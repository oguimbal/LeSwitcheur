//! Platform-specific integrations for the switcher: window enumeration,
//! window activation, global hotkey registration, permission checks.
//!
//! The trait is intentionally narrow — if we ever port to Linux or Windows,
//! only this crate needs implementing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use switcheur_core::{AppRef, BrowserTabRef, DirSourceId, HotkeySpec, LlmProvider, ProgramRef, WindowRef};

/// Source of truth for what's currently runnable and how to focus it.
pub trait WindowSource: Send + Sync {
    /// Enumerate windows. When `show_all_spaces` is false, the result is
    /// restricted to windows on the user's active Space; when true, windows
    /// across every Space are returned (requires Screen Recording permission
    /// to read cross-Space window titles on macOS 14.4+).
    fn list_windows(&self, show_all_spaces: bool) -> Result<Vec<WindowRef>>;
    fn list_apps(&self) -> Result<Vec<AppRef>>;
    fn activate_window(&self, w: &WindowRef) -> Result<()>;
    fn activate_app(&self, a: &AppRef) -> Result<()>;
    /// Ask the window to close itself (AX-press its close button on macOS).
    /// The owning app handles save prompts as normal.
    fn close_window(&self, w: &WindowRef) -> Result<()>;
}

/// Enumeration + launching of **installed** applications (not necessarily
/// currently running). Separate from [`WindowSource`] because the backends and
/// refresh cadence differ wildly across platforms — Spotlight index on macOS,
/// Start-menu / Registry on Windows, .desktop files on Linux.
pub trait ProgramSource: Send + Sync {
    /// Snapshot of installed applications the user can launch. May return an
    /// empty list while a background catalogue is still being populated; the
    /// caller should not treat that as an error.
    fn list_programs(&self) -> Result<Vec<ProgramRef>>;
    /// Launch the given program. If the app is already running, the platform
    /// decides whether to open a new window or focus the existing instance.
    fn launch_program(&self, p: &ProgramRef) -> Result<()>;
}

/// Hand off a free-form query to a well-known LLM provider. The implementation
/// opens the provider's native app if installed (with the query injected when
/// the app supports it, otherwise via clipboard + activate), else falls back
/// to the corresponding web URL with the query as a prefilled prompt.
pub trait LlmLauncher: Send + Sync {
    fn open_llm(&self, provider: LlmProvider, prompt: &str) -> Result<()>;
}

/// Scan running browsers for their open tabs and focus a chosen tab.
/// Supports Chrome and Safari today (macOS AppleScript). The contract is
/// explicitly best-effort:
///
/// - `list_browser_tabs` must never error. Returns the tabs collected plus
///   a boolean `all_failed` flag: `true` when every browser attempted
///   errored out (timeout, permission denied). The caller uses that flag
///   to decide whether to cache the empty result (success) or retry on
///   the next keystroke (failure). A browser that simply isn't running
///   counts as a success returning no tabs.
/// - `activate_browser_tab` may fail (window closed between scan and pick)
///   and the caller reports the error through the normal activation path.
pub trait BrowserTabSource: Send + Sync {
    fn list_browser_tabs(&self) -> (Vec<BrowserTabRef>, bool);
    fn activate_browser_tab(&self, t: &BrowserTabRef) -> Result<()>;
}

/// One result from a [`DirectorySource`] query — a path the UI turns into a
/// `DirRef` row in the right-side pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirHit {
    pub path: PathBuf,
    /// True for directories, false for files. Drives the reduced Open-With
    /// popover for file rows.
    pub is_dir: bool,
}

/// Backend that feeds the right-side directory pane. Implementations must be
/// cheap to `query` (results are fetched on every keystroke off the UI
/// thread) and must degrade silently to an empty vec on error — the rest of
/// the switcher has to keep working.
pub trait DirectorySource: Send + Sync {
    /// Identity of this source. Used for telemetry and to stamp hits with a
    /// `switcheur_core::DirSource` label.
    fn id(&self) -> DirSourceId;

    /// Top-N hits matching `terms`. Empty `terms` returns the source's native
    /// "most relevant" list (e.g. zoxide's top frecency entries).
    fn query(&self, terms: &str, limit: usize) -> Vec<DirHit>;

    /// Remove a path from the source's index (zoxide's "forget this entry"
    /// action, surfaced as the × button on each row). Sources that don't
    /// own a mutable index return an error and advertise `supports_remove()
    /// == false` so the UI hides the button.
    fn remove(&self, path: &Path) -> Result<()>;

    /// Whether the × button should be rendered on this source's rows.
    fn supports_remove(&self) -> bool {
        false
    }
}

/// Describes a [`DirectorySource`] the user can pick from the Settings
/// dropdown, including whether the backing tool is currently available.
/// `install_url` is only surfaced when `available == false`.
#[derive(Debug, Clone)]
pub struct DirSourceEntry {
    pub id: DirSourceId,
    pub available: bool,
    pub install_url: Option<&'static str>,
}

/// Enumerate every known directory source with its current availability on
/// this machine. `Disabled` is always present and always available.
pub fn detect_dir_sources() -> Vec<DirSourceEntry> {
    let zoxide_available = zoxide::detect().is_some();
    vec![
        DirSourceEntry {
            id: DirSourceId::Disabled,
            available: true,
            install_url: None,
        },
        DirSourceEntry {
            id: DirSourceId::Zoxide,
            available: zoxide_available,
            install_url: Some("https://github.com/ajeetdsouza/zoxide#installation"),
        },
        DirSourceEntry {
            id: DirSourceId::Spotlight,
            available: cfg!(target_os = "macos"),
            install_url: None,
        },
    ]
}

/// Instantiate a concrete [`DirectorySource`] for the given id, or `None`
/// when the backing tool isn't installed (or when `id == Disabled`).
pub fn build_dir_source(id: DirSourceId) -> Option<Arc<dyn DirectorySource>> {
    match id {
        DirSourceId::Disabled => None,
        DirSourceId::Zoxide => zoxide::detect().map(|bin| {
            Arc::new(zoxide::ZoxideSource::new(bin)) as Arc<dyn DirectorySource>
        }),
        #[cfg(target_os = "macos")]
        DirSourceId::Spotlight => {
            Some(Arc::new(macos::spotlight::SpotlightSource::new()) as Arc<dyn DirectorySource>)
        }
        #[cfg(not(target_os = "macos"))]
        DirSourceId::Spotlight => None,
    }
}

/// Events delivered when the user presses the registered hotkey.
#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    Pressed,
}

#[cfg(target_os = "macos")]
pub mod macos;

pub mod zoxide;

#[cfg(target_os = "macos")]
pub use macos::{
    ensure_accessibility, file_manager, has_screen_recording_permission, prompt_accessibility,
    prompt_input_monitoring, request_accessibility_prompt, request_screen_recording_permission,
    startup, ExclusionCell, FocusedApp, FocusedAppCell, MacHotkeyService, MacPlatform,
    QuickTypeError, QuickTypeEvent, QuickTypeService, RecencyService, ScrollDir,
    SystemSwitcherError, SystemSwitcherEvent, SystemSwitcherService,
};

#[cfg(target_os = "macos")]
pub use macos::panel::{
    adjust_key_window_frame, configure_open_with_popover, key_window_frame,
    set_open_with_popover_frame, OPEN_WITH_POPOVER_WIDTH,
};

#[cfg(target_os = "macos")]
pub use macos::app_policy::set_accessory as set_accessory_activation_policy;

#[cfg(not(target_os = "macos"))]
pub fn set_accessory_activation_policy() {}

#[cfg(target_os = "macos")]
pub use macos::machine_id::machine_id;

#[cfg(not(target_os = "macos"))]
pub fn machine_id() -> Option<String> {
    None
}

// Future: Windows mirror (Alt+Tab) would live at crates/switcheur-platform/src/windows/
// and expose the same SystemSwitcher* names behind #[cfg(target_os = "windows")].

/// Convenience factory returning the current platform's implementation.
#[cfg(target_os = "macos")]
pub fn default_platform() -> Result<MacPlatform> {
    MacPlatform::new()
}

#[cfg(not(target_os = "macos"))]
pub fn default_platform() -> Result<()> {
    anyhow::bail!("switcheur-platform currently only supports macOS")
}

/// Parse a [`HotkeySpec`] into the platform-specific representation.
#[cfg(target_os = "macos")]
pub fn register_hotkey(
    spec: &HotkeySpec,
) -> Result<MacHotkeyService> {
    MacHotkeyService::register(spec)
}

#[cfg(not(target_os = "macos"))]
pub fn register_hotkey(_spec: &HotkeySpec) -> Result<()> {
    anyhow::bail!("hotkey registration only supported on macOS");
}
