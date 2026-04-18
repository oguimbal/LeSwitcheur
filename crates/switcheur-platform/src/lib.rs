//! Platform-specific integrations for the switcher: window enumeration,
//! window activation, global hotkey registration, permission checks.
//!
//! The trait is intentionally narrow — if we ever port to Linux or Windows,
//! only this crate needs implementing.

use anyhow::Result;
use switcheur_core::{AppRef, HotkeySpec, LlmProvider, ProgramRef, WindowRef};

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

/// Events delivered when the user presses the registered hotkey.
#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    Pressed,
}

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub use macos::{
    ensure_accessibility, has_screen_recording_permission, prompt_accessibility,
    prompt_input_monitoring, request_accessibility_prompt, request_screen_recording_permission,
    startup, ExclusionCell, FocusedApp, FocusedAppCell, MacHotkeyService, MacPlatform,
    QuickTypeError, QuickTypeEvent, QuickTypeService, RecencyService, ScrollDir,
    SystemSwitcherError, SystemSwitcherEvent, SystemSwitcherService,
};

#[cfg(target_os = "macos")]
pub use macos::panel::adjust_key_window_frame;

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
