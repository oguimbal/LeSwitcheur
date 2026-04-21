//! Folder-open helpers that honour the user's preferred file manager.
//!
//! Detection piggy-backs on the app catalogue built by [`super::programs`];
//! launching uses `/usr/bin/open -b <bundle_id>` which routes through
//! LaunchServices to the target app.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use switcheur_core::file_manager::{known_folder_openers, FINDER_BUNDLE_ID};

use super::programs;

/// Bundle ids of installed apps that match any known folder opener (file
/// managers + editors). Finder is *not* included — it isn't surfaced by the
/// Applications scan — but callers can assume it's always available on macOS.
pub fn detected_folder_opener_bundle_ids() -> HashSet<String> {
    let known: HashSet<&str> = known_folder_openers()
        .flat_map(|k| k.bundle_ids.iter().copied())
        .collect();
    programs::list_programs_cached()
        .into_iter()
        .filter_map(|p| p.bundle_id)
        .filter(|b| known.contains(b.as_str()))
        .collect()
}

/// Back-compat alias for callers that still think of this list as
/// "file managers". The detection is identical.
pub fn detected_file_manager_bundle_ids() -> HashSet<String> {
    detected_folder_opener_bundle_ids()
}

/// Reveal `path` (typically a file — Spotlight results can include files)
/// inside the given folder-opener app. Uses `/usr/bin/open -R` so the
/// target is selected, not just opened. Callers that don't care about the
/// specific app can pass `None` to land in Finder via the default handler.
///
/// `open -R -b <bundle_id>` ignores apps that don't understand the
/// "selector" concept (most editors), in which case the OS falls back to
/// Finder. That's fine for our use case — "Show in VS Code" doesn't make
/// sense semantically, and the UI only surfaces this action against the
/// user's default *folder opener* (Finder / Marta / ForkLift / …), which
/// all support reveal.
pub fn reveal_file_with(bundle_id: Option<&str>, path: &Path) -> Result<()> {
    let Some(path_str) = path.to_str() else {
        anyhow::bail!("non-utf8 path: {:?}", path);
    };
    let mut cmd = Command::new("/usr/bin/open");
    cmd.arg("-R");
    if let Some(b) = bundle_id {
        if b != FINDER_BUNDLE_ID {
            cmd.args(["-b", b]);
        }
    }
    cmd.arg(path_str);
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            tracing::warn!(
                bundle_id = ?bundle_id,
                exit = ?s.code(),
                "open -R failed, falling back to Finder"
            );
            Command::new("/usr/bin/open")
                .args(["-R", path_str])
                .status()
                .map_err(|e| anyhow::anyhow!(e))
                .and_then(|s| {
                    if s.success() {
                        Ok(())
                    } else {
                        anyhow::bail!("open -R (Finder fallback) exited with {s}")
                    }
                })
        }
        Err(e) => {
            tracing::warn!(bundle_id = ?bundle_id, error = %e, "open -R spawn failed");
            anyhow::bail!(e)
        }
    }
}

/// Open a folder, optionally targeting a specific app by bundle id. Finder
/// (or `None`) goes through LaunchServices' default handler — same channel
/// as before the feature existed. Other bundle ids are dispatched via
/// `open -b`; on failure we log and retry the default handler so the click
/// never ends up as a silent no-op.
pub fn open_folder_with(bundle_id: Option<&str>, path: &Path) -> Result<()> {
    let use_default = bundle_id.map_or(true, |id| id == FINDER_BUNDLE_ID);
    if use_default {
        return open::that(path).map_err(|e| anyhow::anyhow!(e));
    }
    let bundle_id = bundle_id.expect("checked above");
    let Some(path_str) = path.to_str() else {
        anyhow::bail!("non-utf8 path: {:?}", path);
    };

    match Command::new("/usr/bin/open")
        .args(["-b", bundle_id, path_str])
        .status()
    {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            tracing::warn!(
                bundle_id,
                exit = ?s.code(),
                "open -b failed, falling back to default handler"
            );
            open::that(path).map_err(|e| anyhow::anyhow!(e))
        }
        Err(e) => {
            tracing::warn!(bundle_id, error = %e, "open -b spawn failed, falling back");
            open::that(path).map_err(|e| anyhow::anyhow!(e))
        }
    }
}
