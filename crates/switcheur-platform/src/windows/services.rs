//! Stubs for macOS-only background services.
//!
//! Each `start()` returns `Err(...::Unsupported)` so the host's match
//! arms in `main.rs` take the disabled path. The types are kept around so
//! `main.rs` can import them unconditionally; the disabled paths produce
//! no behaviour. Real Windows implementations land iteratively.

use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use async_channel::{unbounded, Receiver};
use switcheur_core::{AppMatchSet, HotkeySpec, RecencyTracker};
use thiserror::Error;

#[derive(Debug, Default, Clone)]
pub struct FocusedApp {
    pub pid: i32,
    pub name: String,
    pub bundle_id: Option<String>,
}

pub type FocusedAppCell = Arc<ArcSwap<Option<FocusedApp>>>;
pub type ExclusionCell = Arc<ArcSwap<AppMatchSet>>;

#[derive(Debug, Clone)]
pub enum QuickTypeEvent {
    InsertText(String),
    Backspace,
    Scroll(ScrollDir),
    FnReleased { scrolled: bool },
}

#[derive(Debug, Clone, Copy)]
pub enum ScrollDir {
    Up,
    Down,
}

#[derive(Debug, Error)]
pub enum QuickTypeError {
    #[error("Quick Type is not implemented on Windows yet")]
    PermissionDenied,
    #[error("Quick Type start failed: {0}")]
    Start(String),
}

pub struct QuickTypeService {
    rx: Receiver<QuickTypeEvent>,
}

impl QuickTypeService {
    pub fn start(
        _focused: FocusedAppCell,
        _excluded: ExclusionCell,
    ) -> Result<Self, QuickTypeError> {
        Err(QuickTypeError::PermissionDenied)
    }

    pub fn receiver(&self) -> Receiver<QuickTypeEvent> {
        self.rx.clone()
    }
}

#[derive(Debug, Clone)]
pub enum SystemSwitcherEvent {
    Open { reverse: bool },
    Cycle { reverse: bool },
    Confirm,
    TypeText(String),
}

#[derive(Debug, Error)]
pub enum SystemSwitcherError {
    #[error("Cmd+Tab replacement is not implemented on Windows yet")]
    PermissionDenied,
    #[error("system switcher start failed: {0}")]
    Start(String),
}

pub struct SystemSwitcherService {
    rx: Receiver<SystemSwitcherEvent>,
}

impl SystemSwitcherService {
    pub fn start() -> Result<Self, SystemSwitcherError> {
        Err(SystemSwitcherError::PermissionDenied)
    }

    pub fn receiver(&self) -> Receiver<SystemSwitcherEvent> {
        self.rx.clone()
    }

    pub fn reset_cycle(&self) {}

    pub fn stop(&mut self) {}
}

#[derive(Debug, Error)]
pub enum HotkeyTapError {
    #[error("hotkey HID tap is not implemented on Windows")]
    PermissionDenied,
    #[error("hotkey tap start failed: {0}")]
    Start(String),
    #[error("spec cannot be expressed as keycode + flags: {0:?}")]
    UnsupportedSpec(HotkeySpec),
}

pub enum RecordOutcome {
    Captured(HotkeySpec),
    Cancelled,
}

pub struct HotkeyRecordSession {
    rx: Receiver<RecordOutcome>,
}

impl HotkeyRecordSession {
    pub fn start() -> Result<Self, HotkeyTapError> {
        Err(HotkeyTapError::PermissionDenied)
    }

    pub fn receiver(&self) -> Receiver<RecordOutcome> {
        self.rx.clone()
    }
}

pub fn is_system_reserved(_spec: &HotkeySpec) -> bool {
    false
}

/// No-op recency observer. The macOS impl listens to NSWorkspace events to
/// rerank results by MRU; the Windows equivalent (`SetWinEventHook`
/// `EVENT_SYSTEM_FOREGROUND`) lands in a later milestone.
pub struct RecencyService;

impl RecencyService {
    pub fn start(_tracker: Arc<Mutex<RecencyTracker>>, _focused: FocusedAppCell) -> Self {
        Self
    }

    pub fn enable_window_tracking(&mut self, _pids: &[i32]) {}
    pub fn disable_window_tracking(&mut self) {}
    pub fn refresh_app_tracking(&mut self, _pids: &[i32]) {}
    pub fn window_tracking_enabled(&self) -> bool {
        false
    }

    /// Subscribe to "another app became frontmost" notifications. The
    /// macOS observer fires on NSWorkspace activation events; here it
    /// returns a never-firing receiver until a Win32 `SetWinEventHook`
    /// equivalent is wired up.
    pub fn subscribe_app_activations(&self) -> Receiver<FocusedApp> {
        let (_tx, rx) = unbounded::<FocusedApp>();
        rx
    }
}

// `start()` paths above return Err before they ever build the receivers,
// so the Receiver fields never produce values — but the type still has to
// be inhabited. These helper constructors exist purely so the structs can
// be constructed in tests when a future Windows impl wants to instantiate
// them with a real channel.
#[allow(dead_code)]
fn unused_receiver<T>() -> Receiver<T> {
    let (_tx, rx) = unbounded::<T>();
    rx
}
