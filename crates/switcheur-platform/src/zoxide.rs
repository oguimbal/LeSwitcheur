//! [zoxide](https://github.com/ajeetdsouza/zoxide) integration.
//!
//! GUI macOS apps inherit a sparse PATH (no shell rc loaded), so we probe
//! explicit candidates rather than relying on `$PATH` alone. Querying is a
//! shell-out to `zoxide query --list --score [terms…]` — measured at ~6 ms
//! for ~160 entries on a Mac, effectively free.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Hit returned by `zoxide query`.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoxideHit {
    pub path: PathBuf,
    pub score: f64,
}

/// Locate the `zoxide` binary. Returns the resolved path, or `None` when
/// zoxide is not installed.
///
/// Probes `$PATH` first (covers most user setups), then well-known absolute
/// candidates (Homebrew on Apple Silicon and Intel, cargo install, Nix
/// profile). Cache the result for the session — re-probing on every query
/// is wasted syscalls.
pub fn detect() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("zoxide");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/zoxide"),
        PathBuf::from("/usr/local/bin/zoxide"),
    ];
    if let Some(h) = home {
        candidates.push(h.join(".cargo/bin/zoxide"));
        candidates.push(h.join(".nix-profile/bin/zoxide"));
    }
    candidates.into_iter().find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Query zoxide for top frecency-ranked directories matching `terms`.
///
/// Empty `terms` returns the top entries by score. `limit` caps the result
/// (zoxide returns the full list otherwise — even a few hundred is cheap to
/// parse, but the panel only shows a handful).
///
/// Errors from the subprocess (binary missing, permission denied, malformed
/// output) collapse to an empty result. The integration is best-effort —
/// the rest of the switcher must keep working.
pub fn query(bin: &Path, terms: &str, limit: usize) -> Vec<ZoxideHit> {
    let mut cmd = Command::new(bin);
    cmd.arg("query").arg("--list").arg("--score");
    for term in terms.split_whitespace() {
        cmd.arg(term);
    }
    let output = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_query_output(&stdout, limit)
}

fn parse_query_output(stdout: &str, limit: usize) -> Vec<ZoxideHit> {
    stdout
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let score: f64 = parts.next()?.parse().ok()?;
            let path = parts.next()?.trim_start();
            if path.is_empty() {
                return None;
            }
            Some(ZoxideHit {
                path: PathBuf::from(path),
                score,
            })
        })
        .take(limit)
        .collect()
}

/// Remove `path` from the zoxide database.
///
/// Shells out to `zoxide remove <path>` — the CLI expects an exact match and
/// exits non-zero if the entry isn't in the database. Callers treat that as a
/// benign race (user clicked × on a row whose entry zoxide had already
/// dropped) and just log; the UI removes the row optimistically regardless.
pub fn remove(bin: &Path, path: &Path) -> Result<()> {
    let status = Command::new(bin).arg("remove").arg(path).status()?;
    if !status.success() {
        anyhow::bail!("zoxide remove exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_zoxide_output() {
        let raw = "  146.0 /Users/oliv/repos-my\n   12.5 /tmp/scratch\n";
        let hits = parse_query_output(raw, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, PathBuf::from("/Users/oliv/repos-my"));
        assert!((hits[0].score - 146.0).abs() < f64::EPSILON);
        assert_eq!(hits[1].path, PathBuf::from("/tmp/scratch"));
    }

    #[test]
    fn parse_handles_paths_with_spaces() {
        let raw = "   3.0 /Users/me/My Documents/notes\n";
        let hits = parse_query_output(raw, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].path,
            PathBuf::from("/Users/me/My Documents/notes")
        );
    }

    #[test]
    fn parse_skips_garbage_lines() {
        let raw = "not-a-number /tmp\n  5.0 /ok\n";
        let hits = parse_query_output(raw, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, PathBuf::from("/ok"));
    }

    #[test]
    fn parse_respects_limit() {
        let raw = "1 /a\n2 /b\n3 /c\n";
        let hits = parse_query_output(raw, 2);
        assert_eq!(hits.len(), 2);
    }
}
