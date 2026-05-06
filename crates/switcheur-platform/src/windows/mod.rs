//! Windows implementation of the platform traits.
//!
//! Mirrors `macos/` but with Win32 (and a handful of WinRT). Many of the
//! richer macOS features (Quick Type, system-switcher takeover, recency
//! observers, currently-playing, browser-tab inspection) ship as stubs for
//! now — the surface matches the macOS one so `main.rs` compiles unchanged,
//! and the disabled branches in the host take the no-op path. Real
//! implementations land iteratively.

pub mod activate;
pub mod app_policy;
pub mod file_manager;
pub mod hotkey;
pub mod list;
pub mod machine_id;
pub mod panel;
pub mod permissions;
pub mod programs;
pub mod services;
pub mod startup;

pub use hotkey::HotkeyService;
pub use list::onscreen_app_window_ids_excluding_pid;
pub use machine_id::machine_id;
pub use panel::{
    adjust_key_window_frame, configure_open_with_popover, key_window_frame,
    set_open_with_popover_frame, OPEN_WITH_POPOVER_WIDTH,
};
pub use permissions::{
    ensure_accessibility, has_input_monitoring_permission, has_screen_recording_permission,
    prompt_accessibility, prompt_input_monitoring, request_accessibility_prompt,
    request_screen_recording_permission,
};
pub use services::{
    is_system_reserved, ExclusionCell, FocusedApp, FocusedAppCell, HotkeyRecordSession,
    HotkeyTapError, QuickTypeError, QuickTypeEvent, QuickTypeService, RecencyService,
    RecordOutcome, ScrollDir, SystemSwitcherError, SystemSwitcherEvent, SystemSwitcherService,
};

use anyhow::Result;
use switcheur_core::{AppRef, AudioRowRef, BrowserTabRef, LlmProvider, ProgramRef, WindowRef};

use crate::{BrowserTabSource, CurrentlyPlayingSource, LlmLauncher, ProgramSource, WindowSource};

pub struct WinPlatform;

impl WinPlatform {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    pub fn open_url(&self, url: &str) -> Result<()> {
        open::that_detached(url).map_err(|e| anyhow::anyhow!("open url: {e}"))
    }
}

impl WindowSource for WinPlatform {
    fn list_windows(&self, _show_all_spaces: bool) -> Result<Vec<WindowRef>> {
        Ok(list::list_windows())
    }

    fn list_apps(&self) -> Result<Vec<AppRef>> {
        Ok(list::list_apps())
    }

    fn activate_window(&self, w: &WindowRef) -> Result<()> {
        activate::activate_window(w)
    }

    fn activate_app(&self, a: &AppRef) -> Result<()> {
        activate::activate_app(a)
    }

    fn close_window(&self, w: &WindowRef) -> Result<()> {
        activate::close_window(w)
    }
}

impl ProgramSource for WinPlatform {
    fn list_programs(&self) -> Result<Vec<ProgramRef>> {
        Ok(programs::list_programs())
    }

    fn launch_program(&self, p: &ProgramRef) -> Result<()> {
        programs::launch(p)
    }
}

impl LlmLauncher for WinPlatform {
    fn open_llm(&self, provider: LlmProvider, prompt: &str) -> Result<()> {
        let url = match provider {
            LlmProvider::ChatGpt => "https://chatgpt.com/",
            LlmProvider::Claude => "https://claude.ai/new",
            LlmProvider::Mistral => "https://chat.mistral.ai/",
            LlmProvider::Perplexity => "https://www.perplexity.ai/",
        };
        let q = url_encode(prompt);
        let full = if q.is_empty() {
            url.to_string()
        } else {
            format!("{url}?q={q}")
        };
        open::that_detached(&full).map_err(|e| anyhow::anyhow!("open llm: {e}"))
    }
}

impl BrowserTabSource for WinPlatform {
    fn list_browser_tabs(&self) -> (Vec<BrowserTabRef>, bool) {
        (Vec::new(), false)
    }

    fn activate_browser_tab(&self, _t: &BrowserTabRef) -> Result<()> {
        anyhow::bail!("browser tabs not supported on Windows yet")
    }
}

impl CurrentlyPlayingSource for WinPlatform {
    fn current_currently_playing(&self) -> Vec<AudioRowRef> {
        Vec::new()
    }

    fn toggle_audio_playback(&self, _row: &AudioRowRef) -> Result<()> {
        anyhow::bail!("currently-playing not supported on Windows yet")
    }
}

fn url_encode(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}
