//! Preferred file manager for opening folders picked from the switcher.
//!
//! Users can replace Finder with alternatives (Marta, ForkLift, Nimble
//! Commander, Commander One, Path Finder). The setting stores a stable `id`
//! rather than a bundle id so a bundle rename across app versions doesn't
//! invalidate the user's pick.

use std::collections::HashSet;

/// Stable id of the default manager (Finder). Also what [`Config::file_manager`]
/// stores when the preference matches the default.
pub const FINDER_ID: &str = "finder";

/// Apple's bundle id for Finder. Treated specially because Finder is always
/// available on macOS even when the installed-apps scan doesn't surface it.
pub const FINDER_BUNDLE_ID: &str = "com.apple.finder";

/// A file manager we know how to recognize. `bundle_ids` lists every known
/// variant so a single stable `id` works across app renames or paid/free
/// edition splits.
#[derive(Debug, Clone, Copy)]
pub struct KnownFileManager {
    pub id: &'static str,
    pub display_name: &'static str,
    pub bundle_ids: &'static [&'static str],
}

pub const KNOWN_FILE_MANAGERS: &[KnownFileManager] = &[
    KnownFileManager {
        id: FINDER_ID,
        display_name: "Finder",
        bundle_ids: &[FINDER_BUNDLE_ID],
    },
    KnownFileManager {
        id: "marta",
        display_name: "Marta",
        bundle_ids: &["org.yanex.marta", "com.marta.Marta"],
    },
    KnownFileManager {
        id: "forklift",
        display_name: "ForkLift",
        bundle_ids: &[
            "com.binarynights.ForkLift-3",
            "com.binarynights.ForkLift",
        ],
    },
    KnownFileManager {
        id: "nimble-commander",
        display_name: "Nimble Commander",
        bundle_ids: &[
            "info.filesmanager.Files",
            "com.magnumbytes.nimble-commander",
        ],
    },
    KnownFileManager {
        id: "commander-one",
        display_name: "Commander One",
        bundle_ids: &["com.eltima.cmd1mas", "com.eltima.cmd1"],
    },
    KnownFileManager {
        id: "path-finder",
        display_name: "Path Finder",
        bundle_ids: &["com.cocoatech.PathFinder"],
    },
];

/// Concrete choice the user can pick: a known file manager paired with the
/// bundle id that was found installed. Finder is always included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableFileManager {
    pub id: &'static str,
    pub display_name: &'static str,
    pub bundle_id: String,
}

/// Filter [`KNOWN_FILE_MANAGERS`] down to entries whose bundle id appears in
/// `installed`. Finder is always returned first, even when absent from the
/// scan — it ships with macOS.
pub fn available_file_managers(installed: &HashSet<String>) -> Vec<AvailableFileManager> {
    let mut out = Vec::with_capacity(KNOWN_FILE_MANAGERS.len());
    for km in KNOWN_FILE_MANAGERS {
        let is_finder = km.id == FINDER_ID;
        let matched = km.bundle_ids.iter().find(|b| installed.contains(**b));
        let bundle_id = if is_finder {
            km.bundle_ids[0].to_string()
        } else if let Some(b) = matched {
            (*b).to_string()
        } else {
            continue;
        };
        out.push(AvailableFileManager {
            id: km.id,
            display_name: km.display_name,
            bundle_id,
        });
    }
    out
}

/// Resolve a stored `id` to the installed bundle id to hand off to
/// LaunchServices. Returns `None` for unknown ids or managers whose bundle
/// is no longer present on disk — callers should fall back to the default
/// handler (Finder) in that case.
pub fn resolve_bundle_id(id: &str, installed: &HashSet<String>) -> Option<String> {
    let km = KNOWN_FILE_MANAGERS.iter().find(|k| k.id == id)?;
    if km.id == FINDER_ID {
        return Some(FINDER_BUNDLE_ID.to_string());
    }
    km.bundle_ids
        .iter()
        .find(|b| installed.contains(**b))
        .map(|s| (*s).to_string())
}

/// Display name for a stored `id`, falling back to "Finder" for unknown
/// values so stale config never leaves the UI blank.
pub fn display_name_for(id: &str) -> &'static str {
    KNOWN_FILE_MANAGERS
        .iter()
        .find(|k| k.id == id)
        .map(|k| k.display_name)
        .unwrap_or("Finder")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finder_always_available() {
        let list = available_file_managers(&HashSet::new());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, FINDER_ID);
        assert_eq!(list[0].bundle_id, FINDER_BUNDLE_ID);
    }

    #[test]
    fn detects_installed_by_first_matching_bundle_id() {
        let mut installed = HashSet::new();
        installed.insert("com.binarynights.ForkLift".to_string());
        let list = available_file_managers(&installed);
        let fl = list.iter().find(|m| m.id == "forklift").unwrap();
        assert_eq!(fl.bundle_id, "com.binarynights.ForkLift");
    }

    #[test]
    fn resolve_unknown_id_returns_none() {
        assert!(resolve_bundle_id("not-a-thing", &HashSet::new()).is_none());
    }

    #[test]
    fn resolve_finder_even_when_not_in_set() {
        let id = resolve_bundle_id(FINDER_ID, &HashSet::new()).unwrap();
        assert_eq!(id, FINDER_BUNDLE_ID);
    }

    #[test]
    fn resolve_missing_alternative_returns_none() {
        // User picked Marta, then uninstalled it.
        assert!(resolve_bundle_id("marta", &HashSet::new()).is_none());
    }
}
