pub mod activate;
pub mod app_policy;
pub mod browser;
pub mod file_manager;
pub mod hotkey;
pub mod icons;
pub mod llm;
pub mod machine_id;
pub mod panel;
pub mod permissions;
pub mod programs;
pub mod quick_type;
pub mod recency;
pub mod startup;
pub mod system_switcher;
pub mod windows;

pub use hotkey::MacHotkeyService;
pub use permissions::{
    ensure_accessibility, has_screen_recording_permission, prompt_accessibility,
    prompt_input_monitoring, request_accessibility_prompt, request_screen_recording_permission,
};
pub use quick_type::{ExclusionCell, QuickTypeError, QuickTypeEvent, QuickTypeService, ScrollDir};
pub use recency::{FocusedApp, FocusedAppCell, RecencyService};
pub use system_switcher::{SystemSwitcherError, SystemSwitcherEvent, SystemSwitcherService};

use anyhow::Result;
use switcheur_core::{AppRef, BrowserTabRef, LlmProvider, ProgramRef, WindowRef};

use crate::{BrowserTabSource, LlmLauncher, ProgramSource, WindowSource};

pub struct MacPlatform;

impl MacPlatform {
    pub fn new() -> Result<Self> {
        // Walk the Application directories once at startup so the catalogue
        // is ready by the time the user first opens the switcher. Runs on
        // the main thread — see `programs::prefetch_sync` docs.
        programs::prefetch_sync();
        Ok(Self)
    }

    /// Open the given URL in the user's default browser. Used by the
    /// launcher's "Open URL" row when the query is a pasted http/https link.
    pub fn open_url(&self, url: &str) -> Result<()> {
        llm::open_url(url)
    }
}

impl WindowSource for MacPlatform {
    fn list_windows(&self, show_all_spaces: bool) -> Result<Vec<WindowRef>> {
        windows::list_windows(show_all_spaces)
    }

    fn list_apps(&self) -> Result<Vec<AppRef>> {
        windows::list_apps()
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

impl ProgramSource for MacPlatform {
    fn list_programs(&self) -> Result<Vec<ProgramRef>> {
        Ok(programs::list_programs_cached())
    }

    fn launch_program(&self, p: &ProgramRef) -> Result<()> {
        programs::launch(p)
    }
}

impl LlmLauncher for MacPlatform {
    fn open_llm(&self, provider: LlmProvider, prompt: &str) -> Result<()> {
        llm::open_llm(provider, prompt)
    }
}

impl BrowserTabSource for MacPlatform {
    fn list_browser_tabs(&self) -> Vec<BrowserTabRef> {
        browser::list_tabs()
    }

    fn activate_browser_tab(&self, t: &BrowserTabRef) -> Result<()> {
        browser::activate_tab(t)
    }
}
