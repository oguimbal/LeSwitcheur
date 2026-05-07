//! HID-level hotkey interceptor for system-reserved combos (Cmd+Space →
//! Spotlight, Cmd+Tab → System Switcher, Ctrl+↑/↓/←/→ → Mission Control,
//! etc.). Carbon's `RegisterEventHotKey` registers but never fires for these
//! because macOS dispatches them to the system service first; only a
//! `CGEventTap` at HID level intercepts before that point.
//!
//! Mirrors the lifecycle of `quick_type.rs` / `system_switcher.rs`: dedicated
//! thread, own `CFRunLoop`, callback returns `Drop` on match so the system
//! never sees the keystroke. Permission: Input Monitoring (same gate as the
//! other taps).
//!
//! Two operating modes:
//! - `start(spec)` runs continuously, emitting `HotkeyEvent::Pressed` each
//!   time the bound combo is observed. Used at runtime when the bound spec
//!   is system-reserved.
//! - `record_once()` captures a single `KeyDown` (with any modifiers held)
//!   and stops, used by the Settings/Onboarding "record" flow when the user
//!   wants to bind a system-reserved combo.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use async_channel::{unbounded, Receiver, Sender};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopRef, CFRunLoopStop};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CallbackResult, EventField,
};
use switcheur_core::HotkeySpec;
use thiserror::Error;

use crate::HotkeyEvent;

#[derive(Debug, Error)]
pub enum HotkeyTapError {
    #[error(
        "CGEventTap could not be created — grant Input Monitoring in System Settings \
         (Privacy & Security)"
    )]
    PermissionDenied,
    #[error("hotkey tap start failed: {0}")]
    Start(String),
    #[error("spec cannot be expressed as keycode + flags: {0:?}")]
    UnsupportedSpec(HotkeySpec),
}

/// Bits we compare on. All other CGEventFlags bits (CapsLock, NumericPad,
/// Help, etc.) are masked out so they don't break the match.
fn modifier_mask() -> CGEventFlags {
    CGEventFlags::CGEventFlagCommand
        | CGEventFlags::CGEventFlagControl
        | CGEventFlags::CGEventFlagAlternate
        | CGEventFlags::CGEventFlagShift
}

pub struct HotkeyTapService {
    receiver: Receiver<HotkeyEvent>,
    runloop: Arc<Mutex<Option<SendableRunLoop>>>,
    thread: Option<JoinHandle<()>>,
}

struct SendableRunLoop(CFRunLoopRef);
unsafe impl Send for SendableRunLoop {}

impl HotkeyTapService {
    /// Runtime mode: install a tap that emits `HotkeyEvent::Pressed` and
    /// drops every event matching `spec`.
    pub fn start(spec: &HotkeySpec) -> Result<Self, HotkeyTapError> {
        let target = spec_to_target(spec)
            .ok_or_else(|| HotkeyTapError::UnsupportedSpec(spec.clone()))?;
        let (tx, rx) = unbounded::<HotkeyEvent>();
        let (start_tx, start_rx) = std::sync::mpsc::channel::<Result<(), HotkeyTapError>>();
        let runloop = Arc::new(Mutex::new(None::<SendableRunLoop>));

        let runloop_thread = runloop.clone();
        let tx_thread = tx.clone();
        let thread = std::thread::Builder::new()
            .name("leswitcheur-hotkey-tap".into())
            .spawn(move || run_match_tap(runloop_thread, tx_thread, start_tx, target))
            .map_err(|e| HotkeyTapError::Start(format!("spawn thread: {e}")))?;

        match start_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                receiver: rx,
                runloop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(e) => {
                let _ = thread.join();
                Err(HotkeyTapError::Start(format!("start channel: {e}")))
            }
        }
    }

    pub fn receiver(&self) -> Receiver<HotkeyEvent> {
        self.receiver.clone()
    }

    pub fn stop(&mut self) {
        if let Some(rl) = self.runloop.lock().unwrap().take() {
            unsafe { CFRunLoopStop(rl.0) };
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HotkeyTapService {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Recording mode: capture the next non-modifier `KeyDown` (with whatever
/// modifiers are held), drop the event so Spotlight et al. don't fire, then
/// stop the tap. Returns the captured spec on the receiver, or nothing if
/// the user pressed Esc to cancel (Esc is not a valid hotkey on its own).
pub struct HotkeyRecordSession {
    receiver: Receiver<RecordOutcome>,
    runloop: Arc<Mutex<Option<SendableRunLoop>>>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone)]
pub enum RecordOutcome {
    Captured(HotkeySpec),
    Cancelled,
}

impl HotkeyRecordSession {
    pub fn start() -> Result<Self, HotkeyTapError> {
        let (tx, rx) = unbounded::<RecordOutcome>();
        let (start_tx, start_rx) = std::sync::mpsc::channel::<Result<(), HotkeyTapError>>();
        let runloop = Arc::new(Mutex::new(None::<SendableRunLoop>));

        let runloop_thread = runloop.clone();
        let tx_thread = tx.clone();
        let thread = std::thread::Builder::new()
            .name("leswitcheur-hotkey-record".into())
            .spawn(move || run_record_tap(runloop_thread, tx_thread, start_tx))
            .map_err(|e| HotkeyTapError::Start(format!("spawn thread: {e}")))?;

        match start_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                receiver: rx,
                runloop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(e) => {
                let _ = thread.join();
                Err(HotkeyTapError::Start(format!("start channel: {e}")))
            }
        }
    }

    pub fn receiver(&self) -> Receiver<RecordOutcome> {
        self.receiver.clone()
    }

    pub fn stop(&mut self) {
        if let Some(rl) = self.runloop.lock().unwrap().take() {
            unsafe { CFRunLoopStop(rl.0) };
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HotkeyRecordSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_match_tap(
    runloop: Arc<Mutex<Option<SendableRunLoop>>>,
    tx: Sender<HotkeyEvent>,
    start_tx: std::sync::mpsc::Sender<Result<(), HotkeyTapError>>,
    target: (CGEventFlags, i64),
) {
    let (target_flags, target_keycode) = target;
    let mask = modifier_mask();
    let tx_cb = tx.clone();
    let fired_recently = Arc::new(AtomicBool::new(false));
    let fired_cb = fired_recently.clone();

    let callback = move |_proxy: CGEventTapProxy, etype: CGEventType, event: &CGEvent| {
        match etype {
            CGEventType::KeyDown => {
                let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                let flags = event.get_flags() & mask;
                if keycode == target_keycode && flags == target_flags {
                    // Auto-repeat fires another KeyDown; we still drop those
                    // so Spotlight doesn't sneak in, but we only emit the
                    // press once per physical hold. The flag is cleared by
                    // the matching KeyUp below.
                    if !fired_cb.swap(true, Ordering::Relaxed) {
                        let _ = tx_cb.send_blocking(HotkeyEvent::Pressed);
                    }
                    return CallbackResult::Drop;
                }
                CallbackResult::Keep
            }
            CGEventType::KeyUp => {
                let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                if keycode == target_keycode {
                    fired_cb.store(false, Ordering::Relaxed);
                    // Drop the matching KeyUp too, otherwise Spotlight may
                    // see an orphan KeyUp depending on its own state machine.
                    return CallbackResult::Drop;
                }
                CallbackResult::Keep
            }
            _ => CallbackResult::Keep,
        }
    };

    let tap = match CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![CGEventType::KeyDown, CGEventType::KeyUp],
        callback,
    ) {
        Ok(t) => t,
        Err(()) => {
            let _ = start_tx.send(Err(HotkeyTapError::PermissionDenied));
            return;
        }
    };

    let source = match tap.mach_port().create_runloop_source(0) {
        Ok(s) => s,
        Err(()) => {
            let _ = start_tx.send(Err(HotkeyTapError::Start(
                "create_runloop_source failed".into(),
            )));
            return;
        }
    };

    let rl = CFRunLoop::get_current();
    unsafe {
        use core_foundation::base::TCFType;
        core_foundation::runloop::CFRunLoopAddSource(
            rl.as_concrete_TypeRef(),
            source.as_concrete_TypeRef(),
            kCFRunLoopCommonModes,
        );
    }
    tap.enable();

    {
        use core_foundation::base::TCFType;
        *runloop.lock().unwrap() = Some(SendableRunLoop(rl.as_concrete_TypeRef()));
    }
    let _ = start_tx.send(Ok(()));

    CFRunLoop::run_current();

    drop(tap);
    let _ = fired_recently;
}

fn run_record_tap(
    runloop: Arc<Mutex<Option<SendableRunLoop>>>,
    tx: Sender<RecordOutcome>,
    start_tx: std::sync::mpsc::Sender<Result<(), HotkeyTapError>>,
) {
    let mask = modifier_mask();
    let tx_cb = tx.clone();
    let runloop_cb = runloop.clone();
    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_cb = stopped.clone();

    let callback = move |_proxy: CGEventTapProxy, etype: CGEventType, event: &CGEvent| {
        if stopped_cb.load(Ordering::Relaxed) {
            return CallbackResult::Keep;
        }
        if !matches!(etype, CGEventType::KeyDown) {
            return CallbackResult::Keep;
        }
        let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
        let flags = event.get_flags() & mask;
        // Escape (keycode 53) cancels recording.
        if keycode == 53 {
            stopped_cb.store(true, Ordering::Relaxed);
            let _ = tx_cb.send_blocking(RecordOutcome::Cancelled);
            stop_runloop(&runloop_cb);
            return CallbackResult::Drop;
        }
        let outcome = match keycode_to_key(keycode) {
            Some(key) if !flags.is_empty() => {
                let modifiers = flags_to_modifiers(flags);
                if modifiers.is_empty() {
                    return CallbackResult::Keep;
                }
                Some(HotkeySpec { modifiers, key })
            }
            _ => None,
        };
        if let Some(spec) = outcome {
            stopped_cb.store(true, Ordering::Relaxed);
            let _ = tx_cb.send_blocking(RecordOutcome::Captured(spec));
            stop_runloop(&runloop_cb);
            return CallbackResult::Drop;
        }
        CallbackResult::Keep
    };

    let tap = match CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![CGEventType::KeyDown],
        callback,
    ) {
        Ok(t) => t,
        Err(()) => {
            let _ = start_tx.send(Err(HotkeyTapError::PermissionDenied));
            return;
        }
    };

    let source = match tap.mach_port().create_runloop_source(0) {
        Ok(s) => s,
        Err(()) => {
            let _ = start_tx.send(Err(HotkeyTapError::Start(
                "create_runloop_source failed".into(),
            )));
            return;
        }
    };

    let rl = CFRunLoop::get_current();
    unsafe {
        use core_foundation::base::TCFType;
        core_foundation::runloop::CFRunLoopAddSource(
            rl.as_concrete_TypeRef(),
            source.as_concrete_TypeRef(),
            kCFRunLoopCommonModes,
        );
    }
    tap.enable();

    {
        use core_foundation::base::TCFType;
        *runloop.lock().unwrap() = Some(SendableRunLoop(rl.as_concrete_TypeRef()));
    }
    let _ = start_tx.send(Ok(()));

    CFRunLoop::run_current();

    drop(tap);
    let _ = stopped;
}

fn stop_runloop(runloop: &Arc<Mutex<Option<SendableRunLoop>>>) {
    if let Some(rl) = runloop.lock().unwrap().take() {
        unsafe { CFRunLoopStop(rl.0) };
    }
}

/// Re-export of [`HotkeySpec::is_system_reserved`] — the canonical
/// definition lives in `switcheur-core` so the UI crate can also consult it
/// without depending on this platform crate.
pub fn is_system_reserved(spec: &HotkeySpec) -> bool {
    spec.is_system_reserved()
}

fn spec_to_target(spec: &HotkeySpec) -> Option<(CGEventFlags, i64)> {
    let mut flags = CGEventFlags::empty();
    for m in &spec.modifiers {
        match m.to_ascii_lowercase().as_str() {
            "cmd" | "command" | "super" | "meta" => flags |= CGEventFlags::CGEventFlagCommand,
            "ctrl" | "control" => flags |= CGEventFlags::CGEventFlagControl,
            "alt" | "opt" | "option" => flags |= CGEventFlags::CGEventFlagAlternate,
            "shift" => flags |= CGEventFlags::CGEventFlagShift,
            _ => return None,
        }
    }
    let keycode = key_to_keycode(&spec.key.to_ascii_lowercase())?;
    Some((flags, keycode))
}

/// macOS keycodes (kVK_*).
fn key_to_keycode(key: &str) -> Option<i64> {
    Some(match key {
        "a" => 0, "s" => 1, "d" => 2, "f" => 3, "h" => 4, "g" => 5, "z" => 6, "x" => 7,
        "c" => 8, "v" => 9, "b" => 11, "q" => 12, "w" => 13, "e" => 14, "r" => 15,
        "y" => 16, "t" => 17, "1" => 18, "2" => 19, "3" => 20, "4" => 21, "6" => 22,
        "5" => 23, "=" | "equal" | "equals" => 24, "9" => 25, "7" => 26,
        "-" | "minus" => 27, "8" => 28, "0" => 29, "]" | "rightbracket" => 30,
        "o" => 31, "u" => 32, "[" | "leftbracket" => 33, "i" => 34, "p" => 35,
        "l" => 37, "j" => 38, "'" | "quote" => 39, "k" => 40, ";" | "semicolon" => 41,
        "\\" | "backslash" => 42, "," | "comma" => 43, "/" | "slash" => 44,
        "n" => 45, "m" => 46, "." | "period" => 47, "tab" => 48, "space" => 49,
        "`" | "backtick" | "grave" => 50, "return" | "enter" => 36,
        "left" => 123, "right" => 124, "down" => 125, "up" => 126,
        _ => return None,
    })
}

fn keycode_to_key(keycode: i64) -> Option<String> {
    Some(match keycode {
        0 => "a", 1 => "s", 2 => "d", 3 => "f", 4 => "h", 5 => "g", 6 => "z", 7 => "x",
        8 => "c", 9 => "v", 11 => "b", 12 => "q", 13 => "w", 14 => "e", 15 => "r",
        16 => "y", 17 => "t", 18 => "1", 19 => "2", 20 => "3", 21 => "4", 22 => "6",
        23 => "5", 24 => "=", 25 => "9", 26 => "7", 27 => "-", 28 => "8", 29 => "0",
        30 => "]", 31 => "o", 32 => "u", 33 => "[", 34 => "i", 35 => "p", 36 => "return",
        37 => "l", 38 => "j", 39 => "'", 40 => "k", 41 => ";", 42 => "\\", 43 => ",",
        44 => "/", 45 => "n", 46 => "m", 47 => ".", 48 => "tab", 49 => "space",
        50 => "`", 123 => "left", 124 => "right", 125 => "down", 126 => "up",
        _ => return None,
    }
    .to_string())
}

fn flags_to_modifiers(flags: CGEventFlags) -> Vec<String> {
    let mut out = Vec::new();
    if flags.contains(CGEventFlags::CGEventFlagControl) {
        out.push("ctrl".into());
    }
    if flags.contains(CGEventFlags::CGEventFlagAlternate) {
        out.push("alt".into());
    }
    if flags.contains(CGEventFlags::CGEventFlagShift) {
        out.push("shift".into());
    }
    if flags.contains(CGEventFlags::CGEventFlagCommand) {
        out.push("cmd".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(mods: &[&str], key: &str) -> HotkeySpec {
        HotkeySpec {
            modifiers: mods.iter().map(|s| s.to_string()).collect(),
            key: key.to_string(),
        }
    }

    #[test]
    fn reserved_combos_recognised() {
        assert!(is_system_reserved(&spec(&["cmd"], "space")));
        assert!(is_system_reserved(&spec(&["cmd"], "Space")));
        assert!(is_system_reserved(&spec(&["alt", "cmd"], "space")));
        assert!(is_system_reserved(&spec(&["cmd", "alt"], "space")));
        assert!(is_system_reserved(&spec(&["ctrl"], "space")));
        assert!(is_system_reserved(&spec(&["ctrl", "shift"], "space")));
        assert!(is_system_reserved(&spec(&["cmd"], "tab")));
        assert!(is_system_reserved(&spec(&["cmd", "shift"], "tab")));
        assert!(is_system_reserved(&spec(&["cmd"], "`")));
        assert!(is_system_reserved(&spec(&["ctrl"], "up")));
        assert!(is_system_reserved(&spec(&["ctrl"], "down")));
        assert!(is_system_reserved(&spec(&["ctrl"], "left")));
        assert!(is_system_reserved(&spec(&["ctrl"], "right")));
    }

    #[test]
    fn non_reserved_combos_pass_through() {
        assert!(!is_system_reserved(&spec(&["ctrl"], "=")));
        assert!(!is_system_reserved(&spec(&["cmd"], "e")));
        assert!(!is_system_reserved(&spec(&["alt"], "space")));
        assert!(!is_system_reserved(&spec(&["ctrl", "shift"], "up")));
    }

    #[test]
    fn spec_to_target_roundtrips_common_keys() {
        let (flags, keycode) = spec_to_target(&spec(&["cmd"], "space")).unwrap();
        assert!(flags.contains(CGEventFlags::CGEventFlagCommand));
        assert_eq!(keycode, 49);
    }
}
