//! Folder-open helpers that honour the user's preferred file manager.
//!
//! Detection piggy-backs on the app catalogue built by [`super::programs`];
//! launching uses `/usr/bin/open -b <bundle_id>` which routes through
//! LaunchServices to the target app.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use switcheur_core::file_manager::{FINDER_BUNDLE_ID, KNOWN_FILE_MANAGERS};

use super::programs;

/// Bundle ids of installed apps that match any [`KNOWN_FILE_MANAGERS`]
/// entry. Finder is *not* included — it isn't surfaced by the Applications
/// scan — but callers can assume it's always available on macOS.
pub fn detected_file_manager_bundle_ids() -> HashSet<String> {
    let known: HashSet<&str> = KNOWN_FILE_MANAGERS
        .iter()
        .flat_map(|k| k.bundle_ids.iter().copied())
        .collect();
    programs::list_programs_cached()
        .into_iter()
        .filter_map(|p| p.bundle_id)
        .filter(|b| known.contains(b.as_str()))
        .collect()
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
