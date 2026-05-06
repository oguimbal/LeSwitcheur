//! Installed-application catalogue on Windows.
//!
//! Walks the Start Menu shortcut directories (per-user + per-machine) and
//! resolves each `.lnk` to its target executable via `IShellLinkW` /
//! `IPersistFile`. Mirrors the macOS pattern: scan once on a background
//! thread, write a snapshot into an in-memory cache, hand out clones from
//! `list_programs`. UWP / Microsoft Store apps that don't have a classic
//! Start Menu shortcut are intentionally out of scope for this first cut.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::{anyhow, Context, Result};
use switcheur_core::ProgramRef;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::MAX_PATH;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

use super::icons;

fn cache() -> &'static Arc<RwLock<Vec<ProgramRef>>> {
    static CACHE: OnceLock<Arc<RwLock<Vec<ProgramRef>>>> = OnceLock::new();
    CACHE.get_or_init(|| Arc::new(RwLock::new(Vec::new())))
}

/// Snapshot from the cache. Empty until [`prefetch_async`] has finished.
pub fn list_programs() -> Vec<ProgramRef> {
    cache().read().map(|g| g.clone()).unwrap_or_default()
}

/// Launch the program. We push the `.lnk` path into the system shell so it
/// honours whatever working directory / arguments the shortcut encodes —
/// matters for entries like "VS Code (Run as administrator)" or batch
/// launchers that wrap an `.exe` with extra flags.
pub fn launch(p: &ProgramRef) -> Result<()> {
    open::that_detached(&p.bundle_path).map_err(|e| anyhow!("launch: {e}"))
}

/// Spawn a one-shot scanner thread. Idempotent: the second call is a no-op.
/// Called once from `WinPlatform::new` so the catalogue is warming up before
/// the first switcher open.
pub fn prefetch_async() {
    static DONE: OnceLock<()> = OnceLock::new();
    let _ = DONE.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("leswitcheur-program-scan".into())
            .spawn(|| {
                let start = std::time::Instant::now();
                let progs = scan_all().unwrap_or_else(|e| {
                    tracing::warn!("program scan failed: {e:#}");
                    Vec::new()
                });
                tracing::info!(
                    programs = progs.len(),
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "program scan complete"
                );
                if let Ok(mut w) = cache().write() {
                    *w = progs;
                }
            });
    });
}

fn scan_all() -> Result<Vec<ProgramRef>> {
    // STA — `IShellLinkW` is apartment-threaded. The thread calling us owns
    // the apartment for its lifetime; `CoUninitialize` runs at the end.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let mut out = Vec::with_capacity(128);
    let mut seen: HashSet<String> = HashSet::new();
    for dir in start_menu_dirs() {
        scan_dir(&dir, &mut out, &mut seen);
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    unsafe {
        CoUninitialize();
    }
    Ok(out)
}

fn start_menu_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("ProgramData") {
        out.push(PathBuf::from(p).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    if let Ok(p) = std::env::var("APPDATA") {
        out.push(PathBuf::from(p).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    out
}

fn scan_dir(dir: &Path, out: &mut Vec<ProgramRef>, seen: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            scan_dir(&path, out, seen);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase())
            != Some("lnk".to_string())
        {
            continue;
        }
        match parse_shortcut(&path) {
            Ok(Some(p)) => {
                let key = p.bundle_path.to_string_lossy().to_lowercase();
                if seen.insert(key) {
                    out.push(p);
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::trace!(path = %path.display(), "skip .lnk: {e:#}");
            }
        }
    }
}

fn parse_shortcut(lnk_path: &Path) -> Result<Option<ProgramRef>> {
    let lnk_str = lnk_path
        .to_str()
        .ok_or_else(|| anyhow!("non-utf8 lnk path"))?;
    let lnk_wide: Vec<u16> = lnk_str.encode_utf16().chain(std::iter::once(0)).collect();

    let target: PathBuf = unsafe {
        let shell_link: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .context("CoCreateInstance(ShellLink)")?;
        let persist: IPersistFile = shell_link.cast().context("cast IPersistFile")?;
        persist
            .Load(PCWSTR(lnk_wide.as_ptr()), STGM_READ)
            .context("IPersistFile::Load")?;

        // GetPath wants a pre-allocated buffer; MAX_PATH * 2 covers extended
        // long-path scenarios that otherwise truncate.
        let mut buf = vec![0u16; (MAX_PATH * 2) as usize];
        shell_link
            .GetPath(&mut buf[..], std::ptr::null_mut(), 0)
            .context("IShellLinkW::GetPath")?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let target = String::from_utf16_lossy(&buf[..len]);
        if target.is_empty() {
            return Ok(None);
        }
        PathBuf::from(target)
    };

    // Drop launchers/uninstallers/help docs. Real apps land on .exe most of
    // the time; .bat / .cmd are rare but legitimate (Visual Studio shells).
    let ext = target
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(ext.as_str(), "exe" | "bat" | "cmd" | "com") {
        return Ok(None);
    }
    if !target.exists() {
        return Ok(None);
    }
    // Heuristic dedup of "Uninstall <App>" / Visual Studio's "Visual Studio
    // Installer" entries; keep the launchable shortcut, drop the meta.
    let lnk_name = lnk_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let lnk_lower = lnk_name.to_ascii_lowercase();
    if lnk_lower.contains("uninstall")
        || lnk_lower.starts_with("readme")
        || lnk_lower.starts_with("release notes")
    {
        return Ok(None);
    }

    let name = if lnk_name.is_empty() {
        target
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("(unknown)")
            .to_string()
    } else {
        lnk_name.to_string()
    };

    let target_str = target.to_str().ok_or_else(|| anyhow!("non-utf8 target"))?;
    let icon_path = icons::icon_for_exe(target_str);

    Ok(Some(ProgramRef {
        name,
        bundle_id: None,
        // The .lnk is what we hand back to `open::that` for launching — it
        // carries any working-dir / argument hints embedded by the installer.
        bundle_path: lnk_path.to_path_buf(),
        icon_path,
    }))
}
