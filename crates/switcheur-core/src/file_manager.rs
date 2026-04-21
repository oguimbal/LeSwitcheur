//! Apps that can open a folder picked from the switcher: Finder, alternative
//! file managers (Marta, ForkLift…), and popular editors/IDEs that accept a
//! folder path (VS Code, Cursor, Zed, JetBrains IDEs, Xcode…).
//!
//! The user picks a default in Settings; the switcher also surfaces a floating
//! "open with" popover so any detected app can be used ad-hoc. The setting
//! stores a stable `id` rather than a bundle id so a bundle rename across app
//! versions doesn't invalidate the user's pick.

use std::collections::HashSet;

/// Stable id of the default opener (Finder). Also what [`Config::file_manager`]
/// stores when the preference matches the default.
pub const FINDER_ID: &str = "finder";

/// Apple's bundle id for Finder. Treated specially because Finder is always
/// available on macOS even when the installed-apps scan doesn't surface it.
pub const FINDER_BUNDLE_ID: &str = "com.apple.finder";

/// Category of a folder-opening app. Drives default ordering (file managers
/// first, then editors) and lets the UI label editor rows if it wants to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderOpenerKind {
    FileManager,
    Editor,
}

/// An app we know how to recognize. `bundle_ids` lists every known variant so
/// a single stable `id` works across app renames or paid/free edition splits.
#[derive(Debug, Clone, Copy)]
pub struct KnownFolderOpener {
    pub id: &'static str,
    pub display_name: &'static str,
    pub bundle_ids: &'static [&'static str],
    pub kind: FolderOpenerKind,
}

/// File managers that can replace Finder. Finder is always present even when
/// not in the installed-apps scan.
pub const KNOWN_FILE_MANAGERS: &[KnownFolderOpener] = &[
    KnownFolderOpener {
        id: FINDER_ID,
        display_name: "Finder",
        bundle_ids: &[FINDER_BUNDLE_ID],
        kind: FolderOpenerKind::FileManager,
    },
    KnownFolderOpener {
        id: "marta",
        display_name: "Marta",
        bundle_ids: &["org.yanex.marta", "com.marta.Marta"],
        kind: FolderOpenerKind::FileManager,
    },
    KnownFolderOpener {
        id: "forklift",
        display_name: "ForkLift",
        bundle_ids: &[
            "com.binarynights.ForkLift-3",
            "com.binarynights.ForkLift",
        ],
        kind: FolderOpenerKind::FileManager,
    },
    KnownFolderOpener {
        id: "nimble-commander",
        display_name: "Nimble Commander",
        bundle_ids: &[
            "info.filesmanager.Files",
            "com.magnumbytes.nimble-commander",
        ],
        kind: FolderOpenerKind::FileManager,
    },
    KnownFolderOpener {
        id: "commander-one",
        display_name: "Commander One",
        bundle_ids: &["com.eltima.cmd1mas", "com.eltima.cmd1"],
        kind: FolderOpenerKind::FileManager,
    },
    KnownFolderOpener {
        id: "path-finder",
        display_name: "Path Finder",
        bundle_ids: &["com.cocoatech.PathFinder"],
        kind: FolderOpenerKind::FileManager,
    },
];

/// Popular editors and IDEs that accept a folder path as their first argument
/// (or can be pointed at one via LaunchServices). Only apps whose "open this
/// folder" UX is standard are listed.
pub const KNOWN_EDITORS: &[KnownFolderOpener] = &[
    KnownFolderOpener {
        id: "vscode",
        display_name: "VS Code",
        bundle_ids: &["com.microsoft.VSCode"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "vscode-insiders",
        display_name: "VS Code Insiders",
        bundle_ids: &["com.microsoft.VSCodeInsiders"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "cursor",
        display_name: "Cursor",
        bundle_ids: &["com.todesktop.230313mzl4w4u92"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "windsurf",
        display_name: "Windsurf",
        bundle_ids: &["com.exafunction.windsurf"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "zed",
        display_name: "Zed",
        bundle_ids: &["dev.zed.Zed", "dev.zed.Zed-Preview"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "sublime-text",
        display_name: "Sublime Text",
        bundle_ids: &["com.sublimetext.4", "com.sublimetext.3"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "intellij-ce",
        display_name: "IntelliJ IDEA CE",
        bundle_ids: &["com.jetbrains.intellij.ce"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "intellij",
        display_name: "IntelliJ IDEA",
        bundle_ids: &["com.jetbrains.intellij"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "pycharm-ce",
        display_name: "PyCharm CE",
        bundle_ids: &["com.jetbrains.pycharm.ce"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "pycharm",
        display_name: "PyCharm",
        bundle_ids: &["com.jetbrains.pycharm"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "webstorm",
        display_name: "WebStorm",
        bundle_ids: &["com.jetbrains.WebStorm"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "rustrover",
        display_name: "RustRover",
        bundle_ids: &["com.jetbrains.rustrover"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "goland",
        display_name: "GoLand",
        bundle_ids: &["com.jetbrains.goland"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "phpstorm",
        display_name: "PhpStorm",
        bundle_ids: &["com.jetbrains.PhpStorm"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "clion",
        display_name: "CLion",
        bundle_ids: &["com.jetbrains.CLion"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "rubymine",
        display_name: "RubyMine",
        bundle_ids: &["com.jetbrains.rubymine"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "rider",
        display_name: "Rider",
        bundle_ids: &["com.jetbrains.rider"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "datagrip",
        display_name: "DataGrip",
        bundle_ids: &["com.jetbrains.datagrip"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "android-studio",
        display_name: "Android Studio",
        bundle_ids: &["com.google.android.studio"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "xcode",
        display_name: "Xcode",
        bundle_ids: &["com.apple.dt.Xcode"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "nova",
        display_name: "Nova",
        bundle_ids: &["com.panic.Nova"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "bbedit",
        display_name: "BBEdit",
        bundle_ids: &["com.barebones.bbedit"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "textmate",
        display_name: "TextMate",
        bundle_ids: &["com.macromates.TextMate"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "fleet",
        display_name: "Fleet",
        bundle_ids: &["com.jetbrains.fleet"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "macvim",
        display_name: "MacVim",
        bundle_ids: &["org.vim.MacVim"],
        kind: FolderOpenerKind::Editor,
    },
    KnownFolderOpener {
        id: "neovide",
        display_name: "Neovide",
        bundle_ids: &["com.neovide.neovide"],
        kind: FolderOpenerKind::Editor,
    },
];

/// Every known folder-opener, file managers first then editors. Used for
/// iteration and id resolution.
pub fn known_folder_openers() -> impl Iterator<Item = &'static KnownFolderOpener> {
    KNOWN_FILE_MANAGERS.iter().chain(KNOWN_EDITORS.iter())
}

/// Concrete choice the user can pick: a known app paired with the bundle id
/// that was found installed. Finder is always included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableFolderOpener {
    pub id: &'static str,
    pub display_name: &'static str,
    pub bundle_id: String,
    pub kind: FolderOpenerKind,
}

/// Filter [`known_folder_openers`] down to entries whose bundle id appears in
/// `installed`. Finder is always returned first; file managers come before
/// editors, each group in their declaration order.
pub fn available_folder_openers(installed: &HashSet<String>) -> Vec<AvailableFolderOpener> {
    let mut out = Vec::new();
    for km in known_folder_openers() {
        let is_finder = km.id == FINDER_ID;
        let matched = km.bundle_ids.iter().find(|b| installed.contains(**b));
        let bundle_id = if is_finder {
            km.bundle_ids[0].to_string()
        } else if let Some(b) = matched {
            (*b).to_string()
        } else {
            continue;
        };
        out.push(AvailableFolderOpener {
            id: km.id,
            display_name: km.display_name,
            bundle_id,
            kind: km.kind,
        });
    }
    out
}

/// Resolve a stored `id` to the installed bundle id to hand off to
/// LaunchServices. Returns `None` for unknown ids or apps whose bundle is no
/// longer present on disk — callers should fall back to the default handler
/// (Finder) in that case.
pub fn resolve_bundle_id(id: &str, installed: &HashSet<String>) -> Option<String> {
    let km = known_folder_openers().find(|k| k.id == id)?;
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
    known_folder_openers()
        .find(|k| k.id == id)
        .map(|k| k.display_name)
        .unwrap_or("Finder")
}

/// Apply the user's MRU order on top of the canonical `available` list, keeping
/// Finder (or whichever id the caller passes as `default_id`) at the very top.
/// Unknown / uninstalled ids in `order` are silently dropped; entries missing
/// from `order` are appended in their original position.
pub fn order_folder_openers(
    available: Vec<AvailableFolderOpener>,
    order: &[String],
    default_id: &str,
) -> Vec<AvailableFolderOpener> {
    if available.is_empty() {
        return available;
    }
    let mut by_id: std::collections::HashMap<&str, AvailableFolderOpener> = available
        .iter()
        .map(|a| (a.id, a.clone()))
        .collect();
    let mut out: Vec<AvailableFolderOpener> = Vec::with_capacity(available.len());
    // Default first.
    if let Some(def) = by_id.remove(default_id) {
        out.push(def);
    }
    // Then MRU.
    for id in order {
        if id == default_id {
            continue;
        }
        if let Some(a) = by_id.remove(id.as_str()) {
            out.push(a);
        }
    }
    // Finally, remaining detected apps in the canonical order.
    for a in available {
        if by_id.contains_key(a.id) {
            out.push(a.clone());
            by_id.remove(a.id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finder_always_available() {
        let list = available_folder_openers(&HashSet::new());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, FINDER_ID);
        assert_eq!(list[0].bundle_id, FINDER_BUNDLE_ID);
    }

    #[test]
    fn detects_installed_by_first_matching_bundle_id() {
        let mut installed = HashSet::new();
        installed.insert("com.binarynights.ForkLift".to_string());
        let list = available_folder_openers(&installed);
        let fl = list.iter().find(|m| m.id == "forklift").unwrap();
        assert_eq!(fl.bundle_id, "com.binarynights.ForkLift");
    }

    #[test]
    fn detects_editor_bundles() {
        let mut installed = HashSet::new();
        installed.insert("com.microsoft.VSCode".to_string());
        installed.insert("dev.zed.Zed".to_string());
        let list = available_folder_openers(&installed);
        assert!(list.iter().any(|a| a.id == "vscode"));
        assert!(list.iter().any(|a| a.id == "zed"));
        // File managers come before editors — Finder first, then VSCode/Zed.
        assert_eq!(list[0].id, FINDER_ID);
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
        assert!(resolve_bundle_id("marta", &HashSet::new()).is_none());
    }

    #[test]
    fn order_pins_default_first_then_mru() {
        let mut installed = HashSet::new();
        installed.insert("com.microsoft.VSCode".to_string());
        installed.insert("dev.zed.Zed".to_string());
        installed.insert("com.todesktop.230313mzl4w4u92".to_string());
        let avail = available_folder_openers(&installed);
        let ordered = order_folder_openers(
            avail,
            &["zed".to_string(), "cursor".to_string()],
            FINDER_ID,
        );
        assert_eq!(ordered[0].id, FINDER_ID);
        assert_eq!(ordered[1].id, "zed");
        assert_eq!(ordered[2].id, "cursor");
        assert!(ordered.iter().any(|a| a.id == "vscode"));
    }
}
