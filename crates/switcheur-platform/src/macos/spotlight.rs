//! Spotlight integration for the right-side directory pane.
//!
//! Shells out to `/usr/bin/mdfind` (the Spotlight CLI) to fetch candidate
//! paths matching the current query, then uses `std::fs::metadata` to sort
//! by modification time and to distinguish folders from files. Unlike
//! zoxide, Spotlight has no native frecency signal on results — mtime is
//! the best cheap approximation without shelling out to `mdls` per path.
//!
//! Subprocess dispatch must happen off the UI thread. In the common case
//! `mdfind` completes in a few ms; pathological queries (`a`) can be
//! slower, which is why the caller debounces.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use switcheur_core::DirSourceId;

use crate::{DirHit, DirectorySource};

const MDFIND: &str = "/usr/bin/mdfind";

/// Oversample factor before client-side mtime sort: more candidates = better
/// picks surfaced, but we don't want to stat-spam arbitrarily many files.
const OVERSAMPLE: usize = 4;

pub struct SpotlightSource;

impl SpotlightSource {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SpotlightSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DirectorySource for SpotlightSource {
    fn id(&self) -> DirSourceId {
        DirSourceId::Spotlight
    }

    fn query(&self, terms: &str, limit: usize) -> Vec<DirHit> {
        let trimmed = terms.trim();
        if trimmed.is_empty() {
            // Spotlight has no "top-N most relevant" concept the way zoxide
            // does. Nothing to show until the user starts typing.
            return Vec::new();
        }
        let overfetch = limit.saturating_mul(OVERSAMPLE).max(limit);
        let candidates = match run_mdfind(trimmed, overfetch) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("spotlight mdfind: {e:#}");
                return Vec::new();
            }
        };
        let mut scored: Vec<(std::time::SystemTime, DirHit)> = candidates
            .into_iter()
            .filter(|p| !is_noisy_path(p))
            .filter_map(|path| {
                let meta = std::fs::metadata(&path).ok()?;
                let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                Some((
                    mtime,
                    DirHit {
                        path,
                        is_dir: meta.is_dir(),
                    },
                ))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, h)| h).take(limit).collect()
    }

    fn remove(&self, _path: &Path) -> Result<()> {
        // Spotlight's index is read-only from userland. The UI hides the ×
        // button via `supports_remove()`; this branch is defence-in-depth.
        anyhow::bail!("Spotlight index is read-only")
    }
}

fn run_mdfind(terms: &str, limit: usize) -> Result<Vec<PathBuf>> {
    let expr = build_query(terms);
    // No `-onlyin` scoping: Spotlight indexes every mounted, indexable
    // volume (external drives, /Volumes, work folders under /opt, …).
    // Scoping to $HOME would silently hide those. `mdfind` on macOS has
    // no `-limit` flag either — we cap client-side via `take(limit)`.
    let output = Command::new(MDFIND).arg(&expr).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "mdfind exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .filter(|l| !l.is_empty())
        .take(limit)
        .map(PathBuf::from)
        .collect())
}

/// Paths Spotlight indexes but the user almost never wants listed in a
/// day-to-day search: macOS system locations, package caches, and the
/// contents of `.app` bundles (which already show up via the Programs
/// pane). External volumes and arbitrary user folders pass through
/// unchanged.
fn is_noisy_path(path: &std::path::Path) -> bool {
    let Some(s) = path.to_str() else {
        return true;
    };
    const SYSTEM_PREFIXES: &[&str] = &[
        "/System/",
        "/Library/",
        "/private/",
        "/usr/",
        "/var/",
        "/bin/",
        "/sbin/",
        "/dev/",
        "/opt/X11/",
        "/Applications/",
    ];
    if SYSTEM_PREFIXES.iter().any(|p| s.starts_with(p)) {
        return true;
    }
    // Spotlight indexes caches, Mail, containers, logs under ~/Library —
    // all noise for a "find my stuff" search.
    if let Some(home) = std::env::var_os("HOME").and_then(|h| h.to_str().map(String::from)) {
        let lib = format!("{}/Library/", home);
        if s.starts_with(&lib) {
            return true;
        }
    }
    // Anything inside an `.app` bundle is packaged program internals.
    if s.contains(".app/") {
        return true;
    }
    false
}

/// Build an `mdfind` attribute expression: one case/diacritics-insensitive
/// fuzzy match per whitespace-separated term, ANDed together. The `*…*`
/// wrap produces substring matching; the `cd` modifiers make it case- and
/// accent-insensitive. Quotes in terms are stripped so we can't break out of
/// the literal string.
fn build_query(terms: &str) -> String {
    let mut expr = String::new();
    for (i, term) in terms.split_whitespace().enumerate() {
        if i > 0 {
            expr.push_str(" && ");
        }
        let safe = term.replace('"', "");
        expr.push_str(&format!("kMDItemDisplayName == \"*{safe}*\"cd"));
    }
    expr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_single_term() {
        assert_eq!(
            build_query("hello"),
            "kMDItemDisplayName == \"*hello*\"cd"
        );
    }

    #[test]
    fn query_multi_term_ands() {
        assert_eq!(
            build_query("foo bar"),
            "kMDItemDisplayName == \"*foo*\"cd && kMDItemDisplayName == \"*bar*\"cd"
        );
    }

    #[test]
    fn query_strips_embedded_quotes() {
        assert_eq!(
            build_query("ab\"cd"),
            "kMDItemDisplayName == \"*abcd*\"cd"
        );
    }
}
