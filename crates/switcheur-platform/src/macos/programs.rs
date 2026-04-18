//! Installed-application discovery via direct directory scan.
//!
//! We walk the standard Application directories looking for `.app` bundles,
//! read each bundle's `Info.plist` via `NSBundle` to extract the user-facing
//! display name and bundle identifier, and resolve an icon through the existing
//! icon cache. Spotlight (NSMetadataQuery) would have been more complete — it
//! picks up apps in arbitrary locations — but driving its runloop from a
//! secondary thread proved fragile. A direct fs walk is synchronous, reliable,
//! and covers 99 % of real-world installs.
//!
//! The scan runs once on a background thread at startup and writes a snapshot
//! to an in-memory cache. Subsequent `list_programs()` calls clone from it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::{anyhow, Result};
use objc2_app_kit::{NSWorkspace, NSWorkspaceOpenConfiguration};
use objc2_foundation::{NSBundle, NSNumber, NSString, NSURL};
use switcheur_core::ProgramRef;

use super::icons;

/// Directories scanned for `.app` bundles. Kept small and ordered — first
/// occurrence wins during dedup. User-specific paths are resolved at runtime.
/// `/System/Library/CoreServices` and its `Applications` subfolder host
/// user-facing tools like Keychain Access, Screenshot, Finder; the bulk of
/// that folder is daemons/agents filtered out via `LSUIElement` /
/// `LSBackgroundOnly`.
const SYSTEM_DIRS: &[&str] = &[
    "/Applications",
    "/Applications/Utilities",
    "/System/Applications",
    "/System/Applications/Utilities",
    "/System/Library/CoreServices/Applications",
    "/System/Library/CoreServices",
];

fn cache() -> &'static Arc<RwLock<Vec<ProgramRef>>> {
    static CACHE: OnceLock<Arc<RwLock<Vec<ProgramRef>>>> = OnceLock::new();
    CACHE.get_or_init(|| Arc::new(RwLock::new(Vec::new())))
}

/// Populate the program catalogue synchronously. MUST be called on the main
/// thread because the underlying NSBundle / NSWorkspace calls aren't safe
/// from arbitrary Rust threads on modern macOS (silently crashing the thread).
/// A full walk of the standard Application directories completes in well under
/// 100 ms on typical systems, so blocking the main thread here is fine.
pub fn prefetch_sync() {
    static DONE: OnceLock<()> = OnceLock::new();
    let _ = DONE.get_or_init(|| {
        let start = std::time::Instant::now();
        let list = scan_all().unwrap_or_else(|e| {
            tracing::warn!("program scan failed: {e:#}");
            Vec::new()
        });
        tracing::info!(
            programs = list.len(),
            elapsed_ms = start.elapsed().as_millis() as u64,
            "program scan complete"
        );
        if let Ok(mut w) = cache().write() {
            *w = list;
        }
    });
}

pub fn list_programs_cached() -> Vec<ProgramRef> {
    cache().read().map(|g| g.clone()).unwrap_or_default()
}

pub fn launch(p: &ProgramRef) -> Result<()> {
    let path = p
        .bundle_path
        .to_str()
        .ok_or_else(|| anyhow!("non-utf8 bundle path: {:?}", p.bundle_path))?;
    let ns_path = NSString::from_str(path);
    let url = NSURL::fileURLWithPath(&ns_path);
    let workspace = NSWorkspace::sharedWorkspace();
    let config = NSWorkspaceOpenConfiguration::configuration();
    workspace.openApplicationAtURL_configuration_completionHandler(&url, &config, None);
    Ok(())
}

fn scan_all() -> Result<Vec<ProgramRef>> {
    let mut out: Vec<ProgramRef> = Vec::with_capacity(256);
    for dir in SYSTEM_DIRS {
        scan_dir(Path::new(dir), &mut out);
    }
    if let Some(home) = dirs_home() {
        scan_dir(&home.join("Applications"), &mut out);
    }
    dedup_by_bundle_id(&mut out);
    Ok(out)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn scan_dir(dir: &Path, out: &mut Vec<ProgramRef>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("app") {
            continue;
        }
        if let Some(p) = build_program(&path) {
            out.push(p);
        }
    }
}

fn build_program(bundle_path: &Path) -> Option<ProgramRef> {
    let path_str = bundle_path.to_str()?;
    let ns_path = NSString::from_str(path_str);

    let bundle = NSBundle::bundleWithPath(&ns_path);

    // Skip background agents, UI element helpers, and other non-launchable
    // bundles. CoreServices is full of these.
    if let Some(b) = bundle.as_deref() {
        if info_bool(b, "LSUIElement") || info_bool(b, "LSBackgroundOnly") {
            return None;
        }
    }

    // Prefer the user-visible display name, then fall back to CFBundleName,
    // then the bare file stem. This mirrors what the Finder shows.
    let name = bundle
        .as_deref()
        .and_then(|b| info_string(b, "CFBundleDisplayName"))
        .or_else(|| {
            bundle
                .as_deref()
                .and_then(|b| info_string(b, "CFBundleName"))
        })
        .or_else(|| {
            bundle_path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })?;

    let bundle_id = bundle.as_deref().and_then(|b| {
        b.bundleIdentifier().map(|ns| ns.to_string())
    });

    let cache_key = bundle_id.clone().unwrap_or_else(|| path_str.to_string());
    let icon_path = icons::icon_for_bundle(path_str, &cache_key);

    Some(ProgramRef {
        name,
        bundle_id,
        bundle_path: bundle_path.to_path_buf(),
        icon_path,
    })
}

fn info_bool(bundle: &NSBundle, key: &str) -> bool {
    let key_ns = NSString::from_str(key);
    let Some(value) = bundle.objectForInfoDictionaryKey(&key_ns) else {
        return false;
    };
    let value = match value.downcast::<NSNumber>() {
        Ok(num) => return num.boolValue(),
        Err(v) => v,
    };
    if let Ok(s) = value.downcast::<NSString>() {
        let v = s.to_string().to_ascii_lowercase();
        return v == "yes" || v == "true" || v == "1";
    }
    false
}

fn info_string(bundle: &NSBundle, key: &str) -> Option<String> {
    let key_ns = NSString::from_str(key);
    let value = bundle.objectForInfoDictionaryKey(&key_ns)?;
    let s = value.downcast::<NSString>().ok()?;
    let as_string = s.to_string();
    if as_string.is_empty() {
        None
    } else {
        Some(as_string)
    }
}

fn dedup_by_bundle_id(v: &mut Vec<ProgramRef>) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    v.retain(|p| match &p.bundle_id {
        Some(id) => seen.insert(id.clone()),
        None => true,
    });
}
