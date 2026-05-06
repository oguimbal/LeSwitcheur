//! Index-style file/folder lookup for the right-side panel on Windows.
//!
//! Windows ships a real content index (`SystemIndex`) accessible only via
//! OLE DB, which would mean a couple hundred lines of unsafe COM. We instead
//! walk the user's standard folders on a background thread and keep an
//! in-memory snapshot — Windows-Search-like UX without the COM cost. The
//! walker refreshes itself on a slow tick so newly created files surface.
//!
//! Two `DirectorySource` variants are exposed by sharing the same walker
//! cache:
//!
//! - [`WalkerKind::Folders`] yields directories only (`DirSourceId::WindowsFolders`).
//! - [`WalkerKind::Files`]   yields directories + files (`DirSourceId::WindowsFiles`).
//!
//! Empty query returns nothing — there's no "frecency" signal to fall back
//! on, and the panel's job in that case is to stay quiet, not enumerate the
//! user's home dir.
//!
//! Hard caps:
//! - Recursion depth: `MAX_DEPTH` (defends against bind mounts / symlink cycles).
//! - Total entries: `MAX_ENTRIES` (memory ceiling, dropped LRU-style by sort).
//! - Skips Windows / Program Files / hidden / system noise via `is_skip`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use arc_swap::ArcSwap;
use switcheur_core::DirSourceId;

use crate::{DirHit, DirectorySource};

const MAX_DEPTH: usize = 8;
const MAX_ENTRIES: usize = 200_000;
const REFRESH_INTERVAL: Duration = Duration::from_secs(120);

/// What the walker is asked to surface. Both kinds share the same cache —
/// the difference is purely a filter applied at query time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkerKind {
    Folders,
    Files,
}

#[derive(Debug, Clone)]
struct Entry {
    path: PathBuf,
    name_lower: String,
    is_dir: bool,
    /// Modification time as a `Duration` since epoch; used for sort. Storing
    /// the duration avoids branching on `SystemTime::duration_since` per row
    /// during the hot query path.
    mtime: Duration,
}

#[derive(Debug, Default)]
struct Snapshot {
    entries: Vec<Entry>,
}

#[derive(Clone)]
struct Cache {
    inner: Arc<ArcSwap<Snapshot>>,
}

impl Cache {
    fn new() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(Snapshot::default())),
        }
    }

    fn store(&self, snapshot: Snapshot) {
        self.inner.store(Arc::new(snapshot));
    }

    fn load(&self) -> Arc<Snapshot> {
        self.inner.load_full()
    }
}

/// Process-wide singleton. Both `Folders` and `Files` adapters share it so we
/// only walk the disk once.
fn shared_cache() -> Cache {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let cache = Cache::new();
            spawn_walker(cache.clone());
            cache
        })
        .clone()
}

fn spawn_walker(cache: Cache) {
    std::thread::Builder::new()
        .name("leswitcheur-search-walker".into())
        .spawn(move || loop {
            let snapshot = build_snapshot();
            tracing::debug!(
                count = snapshot.entries.len(),
                "search walker refreshed"
            );
            cache.store(snapshot);
            std::thread::sleep(REFRESH_INTERVAL);
        })
        .expect("spawn search walker");
}

fn build_snapshot() -> Snapshot {
    let mut entries: Vec<Entry> = Vec::new();
    for root in walk_roots() {
        walk_into(&root, 0, &mut entries);
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }
    // Newest first — used both as the result order when the query is short
    // and as the eviction policy when MAX_ENTRIES is hit.
    entries.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    entries.truncate(MAX_ENTRIES);
    Snapshot { entries }
}

fn walk_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        for sub in [
            "Desktop",
            "Documents",
            "Downloads",
            "Pictures",
            "Videos",
            "Music",
        ] {
            let p = home.join(sub);
            if p.is_dir() {
                roots.push(p);
            }
        }
        // OneDrive — root and any per-tenant subfolders the user signed into.
        for entry in std::fs::read_dir(&home).into_iter().flatten().flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("OneDrive") {
                let p = entry.path();
                if p.is_dir() {
                    roots.push(p);
                }
            }
        }
    }
    roots
}

fn walk_into(dir: &Path, depth: usize, out: &mut Vec<Entry>) {
    if depth > MAX_DEPTH || out.len() >= MAX_ENTRIES {
        return;
    }
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if is_skip(&name_str) {
            continue;
        }
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .unwrap_or_default();
        let is_dir = meta.is_dir();
        out.push(Entry {
            path: path.clone(),
            name_lower: name_str.to_lowercase(),
            is_dir,
            mtime,
        });
        if is_dir {
            walk_into(&path, depth + 1, out);
            if out.len() >= MAX_ENTRIES {
                return;
            }
        }
    }
}

/// Names the walker should treat as opaque: package caches, build outputs,
/// VCS metadata. Skipping at this level avoids spending budget on tens of
/// thousands of useless entries inside `node_modules` etc.
fn is_skip(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | "node_modules"
            | "target"
            | ".cargo"
            | "__pycache__"
            | ".venv"
            | "venv"
            | ".tox"
            | ".idea"
            | ".vscode"
            | ".next"
            | ".nuxt"
            | "dist"
            | "build"
            | ".gradle"
    ) || name.starts_with('.')
}

pub struct WalkerSource {
    kind: WalkerKind,
    cache: Cache,
}

impl WalkerSource {
    pub fn new(kind: WalkerKind) -> Self {
        Self {
            kind,
            cache: shared_cache(),
        }
    }
}

impl DirectorySource for WalkerSource {
    fn id(&self) -> DirSourceId {
        match self.kind {
            WalkerKind::Folders => DirSourceId::WindowsFolders,
            WalkerKind::Files => DirSourceId::WindowsFiles,
        }
    }

    fn query(&self, terms: &str, limit: usize) -> Vec<DirHit> {
        let trimmed = terms.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        let needles: Vec<String> = trimmed
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect();
        let want_files = matches!(self.kind, WalkerKind::Files);
        let snap = self.cache.load();
        snap.entries
            .iter()
            .filter(|e| want_files || e.is_dir)
            .filter(|e| needles.iter().all(|n| e.name_lower.contains(n)))
            .take(limit)
            .map(|e| DirHit {
                path: e.path.clone(),
                is_dir: e.is_dir,
            })
            .collect()
    }

    fn remove(&self, _path: &Path) -> Result<()> {
        // Walker rebuilds itself from disk every refresh — manually removing
        // an entry would just come back. Hide the × button via
        // `supports_remove()`.
        anyhow::bail!("walker source has no mutable index")
    }
}
