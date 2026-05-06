//! Installed-application catalogue (Start menu shortcuts + UWP packages).
//!
//! Stubbed for now — returns an empty list, matching the macOS contract that
//! `list_programs` may legitimately come back empty while the catalogue is
//! still being populated. Real walkers (over `%ProgramData%\Microsoft\Windows
//! \Start Menu\Programs` and the per-user variant, plus
//! `Windows.ApplicationModel.PackageManager` for UWP) land later.

use anyhow::Result;
use switcheur_core::ProgramRef;

pub fn list_programs() -> Vec<ProgramRef> {
    Vec::new()
}

pub fn launch(p: &ProgramRef) -> Result<()> {
    open::that_detached(&p.bundle_path).map_err(|e| anyhow::anyhow!("launch: {e}"))
}
