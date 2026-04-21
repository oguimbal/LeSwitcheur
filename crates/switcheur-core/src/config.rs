//! Persistent user configuration, loaded from `~/Library/Application Support/LeSwitcheur/config.toml`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::app_match::AppMatch;
use crate::exclusions::ExclusionRule;
use crate::model::LlmProvider;
use crate::sort::SortOrder;

const QUALIFIER: &str = "fr";
const ORG: &str = "gmbl";
const APP: &str = "LeSwitcheur";
const FILE_NAME: &str = "config.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub hotkey: HotkeySpec,
    /// When true, typing a query shows a "Programs" suggestion panel above
    /// the switcher list. On by default.
    #[serde(default = "default_true")]
    pub search_apps: bool,
    /// When true, minimized windows are listed alongside visible ones.
    pub include_minimized: bool,
    /// When true, LeSwitcheur registers itself as a LaunchAgent and starts at login.
    pub launch_at_startup: bool,
    pub appearance: Appearance,
    /// Rules that hide matching windows (and optionally app entries) from the switcher.
    #[serde(default)]
    pub exclusions: Vec<ExclusionRule>,
    /// Hold Fn and type letters to open/filter the switcher globally.
    #[serde(default)]
    pub quick_type: bool,
    /// Intercept the OS native app switcher (Cmd+Tab on macOS) and show the
    /// LeSwitcheur panel instead. Requires Input Monitoring permission.
    #[serde(default)]
    pub replace_system_switcher: bool,
    /// How results are ordered when the query is empty.
    #[serde(default)]
    pub sort_order: SortOrder,
    /// Apps (by localized name or bundle id) in which the global popup hotkey
    /// should be silently ignored. The keystroke is still consumed by the
    /// system-wide registration; we just don't open the switcher.
    #[serde(default)]
    pub hotkey_excluded_apps: Vec<AppMatch>,
    /// Apps in which Quick Type (Fn + letter) should pass through to the app
    /// instead of being intercepted.
    #[serde(default)]
    pub quick_type_excluded_apps: Vec<AppMatch>,
    /// When true, the switcher lists windows across every Space (requires
    /// Screen Recording permission to read cross-Space titles). Off by
    /// default — only the current Space is shown.
    #[serde(default)]
    pub show_all_spaces: bool,
    /// Deprecated: pre-release name for the inverse of `show_all_spaces`.
    /// If present in a saved config, [`Config::load_or_default`] inverts
    /// its value into `show_all_spaces` and drops this key on next save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_desktop_only: Option<bool>,
    /// Deprecated: old flag that listed running apps inline with windows.
    /// Accepted for back-compat with existing config.toml files and dropped
    /// on next save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    include_apps: Option<bool>,
    /// Ed25519-signed license token. Absent = unlicensed (nag popup visible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_token: Option<String>,
    /// License key the user entered or received from the purchase flow.
    /// Shown read-only in settings with a copy button. Carried in the token
    /// payload too, but keeping it separately saves parsing on every read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_key: Option<String>,
    /// Epoch seconds when the nag popup was last shown. Throttled to once/day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nag_last_shown_at: Option<u64>,
    /// Count of switcher confirmations since the last nag popup.
    /// Popup re-triggers every 500 uses when unlicensed.
    #[serde(default)]
    pub switcher_uses_since_nag: u32,
    /// Preferred order of the "Ask LLM" fallback rows, most-recently-used
    /// first. Missing providers are appended in their default order on load.
    #[serde(default = "LlmProvider::default_order")]
    pub llm_provider_order: Vec<LlmProvider>,
    /// When true, the switcher shows "Ask <provider>" fallback rows when no
    /// window/program/eval matched the query. On by default.
    #[serde(default = "default_true")]
    pub ask_llm_enabled: bool,
    /// True once the user has been through (or dismissed) the first-launch
    /// onboarding wizard. Missing from existing config.toml files — serde
    /// defaults to true so upgrades don't re-trigger the wizard; new users
    /// get false via [`Config::default`] and see the wizard on first run.
    #[serde(default = "default_true")]
    pub onboarding_completed: bool,
    /// Which backend feeds the right-side directory pane. `Disabled` hides the
    /// pane entirely; `Zoxide` degrades silently to Disabled at runtime when
    /// the binary isn't found. On a fresh install this defaults to `Zoxide`
    /// — existing behaviour — and gets cleared to `Disabled` at boot if
    /// detection fails, so the pane stays hidden until the user picks
    /// another source.
    #[serde(default)]
    pub dir_source: DirSourceId,
    /// Deprecated: pre-multi-source toggle. Migrated into `dir_source` on
    /// next save (`true → Zoxide`, `false → Disabled`) and dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    zoxide_integration: Option<bool>,
    /// When true, if no window / program / URL / eval matches the query, the
    /// switcher scrapes the running browsers (Chrome today) for open tabs and
    /// offers them as results before falling back to "Ask AI". Fetch runs off
    /// the UI thread and is fully lazy — no AppleScript fires until the
    /// fallback tier is actually reached. On by default.
    #[serde(default = "default_true")]
    pub browser_tabs_integration: bool,
    /// Stable id of the app used by default to open folders picked from the
    /// switcher. `None` (or an unknown / uninstalled id) resolves to the
    /// system default (Finder). See
    /// [`crate::file_manager::known_folder_openers`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_manager: Option<String>,
    /// MRU order for the "Open With" popover attached to zoxide rows. Stores
    /// stable `id`s (not bundle ids). Most-recently-used first; the default
    /// opener is always rendered first in the popover regardless of its
    /// position here.
    #[serde(default)]
    pub folder_opener_order: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// A platform-agnostic description of the trigger hotkey. Parsed by the platform
/// crate into whatever the OS-specific API needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HotkeySpec {
    /// Modifier names, case-insensitive: one of `cmd`, `ctrl`, `alt`/`opt`, `shift`.
    pub modifiers: Vec<String>,
    /// Key name, e.g. `space`, `tab`, `a`. Case-insensitive.
    pub key: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    System,
    Light,
    Dark,
}

/// Which backend populates the right-side directory pane.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum DirSourceId {
    Disabled,
    Zoxide,
    Spotlight,
}

impl Default for DirSourceId {
    /// New installs start on Zoxide so the existing out-of-the-box behaviour
    /// is preserved. Host code downgrades to `Disabled` at boot if zoxide
    /// isn't actually installed.
    fn default() -> Self {
        Self::Zoxide
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: HotkeySpec::default(),
            search_apps: true,
            include_minimized: true,
            launch_at_startup: false,
            appearance: Appearance::System,
            exclusions: Vec::new(),
            quick_type: false,
            replace_system_switcher: false,
            sort_order: SortOrder::default(),
            hotkey_excluded_apps: Vec::new(),
            quick_type_excluded_apps: Vec::new(),
            show_all_spaces: false,
            current_desktop_only: None,
            include_apps: None,
            license_token: None,
            license_key: None,
            nag_last_shown_at: None,
            switcher_uses_since_nag: 0,
            llm_provider_order: LlmProvider::default_order(),
            ask_llm_enabled: true,
            onboarding_completed: false,
            dir_source: DirSourceId::default(),
            zoxide_integration: None,
            browser_tabs_integration: true,
            file_manager: None,
            folder_opener_order: Vec::new(),
        }
    }
}

impl Default for HotkeySpec {
    fn default() -> Self {
        Self {
            modifiers: vec!["ctrl".into()],
            key: "=".into(),
        }
    }
}

impl Config {
    /// Load from the standard location. If the file is missing, write defaults and return them.
    /// If the file exists but is malformed, log and fall back to defaults (non-fatal).
    pub fn load_or_default() -> Self {
        match Self::try_load() {
            Ok(Some(mut c)) => {
                if c.migrate_legacy_fields() {
                    if let Err(e) = c.save() {
                        tracing::warn!("failed to persist legacy-field migration: {e:#}");
                    }
                }
                c
            }
            Ok(None) => {
                let c = Self::default();
                if let Err(e) = c.save() {
                    tracing::warn!("failed to write default config: {e:#}");
                }
                c
            }
            Err(e) => {
                tracing::warn!("config malformed, using defaults: {e:#}");
                Self::default()
            }
        }
    }

    /// Fold deprecated fields into their canonical replacements. Returns true
    /// if anything changed and the config should be re-saved.
    fn migrate_legacy_fields(&mut self) -> bool {
        let mut changed = false;
        if let Some(legacy) = self.current_desktop_only.take() {
            self.show_all_spaces = !legacy;
            changed = true;
        }
        if self.include_apps.take().is_some() {
            // Deprecated feature removed; drop the field on next save.
            changed = true;
        }
        if let Some(legacy) = self.zoxide_integration.take() {
            self.dir_source = if legacy {
                DirSourceId::Zoxide
            } else {
                DirSourceId::Disabled
            };
            changed = true;
        }
        // Fill in any LLM providers added after the config was first written
        // so the fallback UI always shows every known provider.
        for p in LlmProvider::default_order() {
            if !self.llm_provider_order.contains(&p) {
                self.llm_provider_order.push(p);
                changed = true;
            }
        }
        changed
    }

    /// Move `p` to the front of the preferred-order list, preserving the
    /// relative order of the rest. No-op if the list is empty.
    pub fn promote_llm_provider(&mut self, p: LlmProvider) {
        self.llm_provider_order.retain(|x| *x != p);
        self.llm_provider_order.insert(0, p);
    }

    /// Move `id` to the front of the "open with" MRU list, preserving the
    /// relative order of the rest. Called after a successful folder launch so
    /// the next popover render floats the user's habitual editor up top.
    pub fn promote_folder_opener(&mut self, id: &str) {
        self.folder_opener_order.retain(|x| x != id);
        self.folder_opener_order.insert(0, id.to_string());
    }

    fn try_load() -> Result<Option<Self>> {
        let Some(path) = config_path() else {
            return Ok(None);
        };
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).context("parsing config.toml")?;
        Ok(Some(cfg))
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = config_path() else {
            anyhow::bail!("no ProjectDirs on this platform");
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing config")?;
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn path() -> Option<PathBuf> {
        config_path()
    }
}

fn config_path() -> Option<PathBuf> {
    ProjectDirs::from(QUALIFIER, ORG, APP).map(|d| d.config_dir().join(FILE_NAME))
}

/// Escape hatch for tests and advanced callers: load from an explicit file.
pub fn load_from_path(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)?;
    Ok(toml::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip_through_toml() {
        let c = Config::default();
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn unknown_fields_rejected() {
        let text = "include_apps = true\nbogus = 1\n";
        let err = toml::from_str::<Config>(text).unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn default_hotkey_is_ctrl_equal() {
        let h = HotkeySpec::default();
        assert_eq!(h.key, "=");
        assert_eq!(h.modifiers, vec!["ctrl".to_string()]);
    }

    #[test]
    fn config_with_exclusions_roundtrip() {
        let mut c = Config::default();
        c.exclusions.push(ExclusionRule {
            app: "Ghostty".into(),
            title_pattern: String::new(),
        });
        c.exclusions.push(ExclusionRule {
            app: "Safari".into(),
            title_pattern: "^Private".into(),
        });
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn missing_quick_type_defaults_to_false() {
        let text = "hotkey = { modifiers = [\"ctrl\"], key = \"=\" }\n\
            include_apps = false\n\
            include_minimized = false\n\
            launch_at_startup = false\n\
            appearance = \"system\"\n";
        let c: Config = toml::from_str(text).unwrap();
        assert!(!c.quick_type);
    }

    #[test]
    fn missing_replace_system_switcher_defaults_to_false() {
        let text = "hotkey = { modifiers = [\"ctrl\"], key = \"=\" }\n\
            include_apps = false\n\
            include_minimized = false\n\
            launch_at_startup = false\n\
            appearance = \"system\"\n";
        let c: Config = toml::from_str(text).unwrap();
        assert!(!c.replace_system_switcher);
    }

    #[test]
    fn missing_exclusions_defaults_to_empty() {
        // Simulates an older config.toml that predates the feature.
        let text = "hotkey = { modifiers = [\"ctrl\"], key = \"=\" }\n\
            include_apps = false\n\
            include_minimized = false\n\
            launch_at_startup = false\n\
            appearance = \"system\"\n";
        let c: Config = toml::from_str(text).unwrap();
        assert!(c.exclusions.is_empty());
    }

    #[test]
    fn legacy_config_parses_with_empty_new_lists() {
        let text = "hotkey = { modifiers = [\"ctrl\"], key = \"=\" }\n\
            include_apps = false\n\
            include_minimized = false\n\
            launch_at_startup = false\n\
            appearance = \"system\"\n";
        let c: Config = toml::from_str(text).unwrap();
        assert!(c.hotkey_excluded_apps.is_empty());
        assert!(c.quick_type_excluded_apps.is_empty());
    }

    #[test]
    fn missing_show_all_spaces_defaults_to_false() {
        let text = "hotkey = { modifiers = [\"ctrl\"], key = \"=\" }\n\
            include_apps = false\n\
            include_minimized = false\n\
            launch_at_startup = false\n\
            appearance = \"system\"\n";
        let c: Config = toml::from_str(text).unwrap();
        assert!(!c.show_all_spaces);
    }

    #[test]
    fn legacy_current_desktop_only_migrates_inverted() {
        // Pre-release configs had `current_desktop_only = true` meaning
        // "only the current Space". The new canonical flag is
        // `show_all_spaces`, which is the inverse.
        let text = "hotkey = { modifiers = [\"ctrl\"], key = \"=\" }\n\
            include_apps = false\n\
            include_minimized = false\n\
            launch_at_startup = false\n\
            appearance = \"system\"\n\
            current_desktop_only = true\n";
        let mut c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.current_desktop_only, Some(true));
        assert!(!c.show_all_spaces);
        let changed = c.migrate_legacy_fields();
        assert!(changed);
        assert_eq!(c.current_desktop_only, None);
        assert!(!c.show_all_spaces); // !true == false
        // Serialized form no longer carries the deprecated key.
        let out = toml::to_string_pretty(&c).unwrap();
        assert!(!out.contains("current_desktop_only"));
    }

    #[test]
    fn legacy_current_desktop_only_false_migrates_to_show_all_spaces_true() {
        let text = "hotkey = { modifiers = [\"ctrl\"], key = \"=\" }\n\
            include_apps = false\n\
            include_minimized = false\n\
            launch_at_startup = false\n\
            appearance = \"system\"\n\
            current_desktop_only = false\n";
        let mut c: Config = toml::from_str(text).unwrap();
        let changed = c.migrate_legacy_fields();
        assert!(changed);
        assert!(c.show_all_spaces); // !false == true
    }

    #[test]
    fn legacy_config_without_onboarding_field_defaults_completed() {
        // Existing config files on disk predate the onboarding flag. Serde
        // must treat them as already completed so the wizard doesn't pop up
        // on upgrade.
        let text = "hotkey = { modifiers = [\"ctrl\"], key = \"=\" }\n\
            include_minimized = false\n\
            launch_at_startup = false\n\
            appearance = \"system\"\n";
        let c: Config = toml::from_str(text).unwrap();
        assert!(c.onboarding_completed);
    }

    #[test]
    fn default_config_triggers_onboarding_for_new_users() {
        let c = Config::default();
        assert!(!c.onboarding_completed);
    }

    #[test]
    fn legacy_zoxide_integration_true_migrates_to_zoxide() {
        let text = "hotkey = { modifiers = [\"ctrl\"], key = \"=\" }\n\
            include_minimized = false\n\
            launch_at_startup = false\n\
            appearance = \"system\"\n\
            zoxide_integration = true\n";
        let mut c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.zoxide_integration, Some(true));
        let changed = c.migrate_legacy_fields();
        assert!(changed);
        assert_eq!(c.dir_source, DirSourceId::Zoxide);
        assert_eq!(c.zoxide_integration, None);
        let out = toml::to_string_pretty(&c).unwrap();
        assert!(!out.contains("zoxide_integration"));
        assert!(out.contains("dir_source"));
    }

    #[test]
    fn legacy_zoxide_integration_false_migrates_to_disabled() {
        let text = "hotkey = { modifiers = [\"ctrl\"], key = \"=\" }\n\
            include_minimized = false\n\
            launch_at_startup = false\n\
            appearance = \"system\"\n\
            zoxide_integration = false\n";
        let mut c: Config = toml::from_str(text).unwrap();
        let changed = c.migrate_legacy_fields();
        assert!(changed);
        assert_eq!(c.dir_source, DirSourceId::Disabled);
    }

    #[test]
    fn missing_dir_source_defaults_to_zoxide() {
        // No `dir_source` and no legacy `zoxide_integration` — fresh install
        // path. `Default` keeps today's behaviour.
        let text = "hotkey = { modifiers = [\"ctrl\"], key = \"=\" }\n\
            include_minimized = false\n\
            launch_at_startup = false\n\
            appearance = \"system\"\n";
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.dir_source, DirSourceId::Zoxide);
    }

    #[test]
    fn new_exclusion_lists_roundtrip() {
        let mut c = Config::default();
        c.hotkey_excluded_apps.push(AppMatch::new("Safari"));
        c.quick_type_excluded_apps
            .push(AppMatch::new("com.mitchellh.ghostty"));
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
    }
}
