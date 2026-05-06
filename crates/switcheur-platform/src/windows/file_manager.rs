//! Folder-open helpers, Windows edition.
//!
//! For now there is no third-party file-manager catalogue: every action
//! routes through the system default (Explorer). The UI hides the Open
//! With popover entirely when this set is empty, so the existing helpers
//! degrade gracefully.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;

pub fn detected_folder_opener_bundle_ids() -> HashSet<String> {
    HashSet::new()
}

pub fn detected_file_manager_bundle_ids() -> HashSet<String> {
    HashSet::new()
}

/// Reveal `path` in Explorer (`explorer /select,<path>`). The `bundle_id`
/// argument is accepted for API parity but ignored — every reveal goes
/// through Explorer until we plug in a third-party catalogue.
pub fn reveal_file_with(_bundle_id: Option<&str>, path: &Path) -> Result<()> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 path: {:?}", path))?;
    std::process::Command::new("explorer")
        .arg(format!("/select,{path_str}"))
        .spawn()
        .map_err(|e| anyhow::anyhow!("explorer /select spawn: {e}"))?;
    Ok(())
}

/// Open a folder via the system default handler. The `bundle_id` argument
/// is accepted for API parity but ignored.
pub fn open_folder_with(_bundle_id: Option<&str>, path: &Path) -> Result<()> {
    open::that(path).map_err(|e| anyhow::anyhow!(e))
}
