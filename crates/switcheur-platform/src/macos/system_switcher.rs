//! System switcher replacement: intercept Cmd+Tab (and Cmd+Shift+Tab) so the
//! LeSwitcheur panel takes over from the native macOS app switcher.
//!
//! Mirrors `quick_type.rs` — an HID-level `CGEventTap` on its own thread with
//! its own `CFRunLoop`. The callback tracks whether Cmd is held (via
//! `FlagsChanged`) and whether a "Cmd+Tab cycle" is currently in progress.
//!
//! Emitted event stream:
//! - `Open { reverse }`  — first Cmd+Tab press (begins a cycle)
//! - `Cycle { reverse }` — subsequent Tab presses during the same cycle
//! - `Confirm`           — Cmd released while a cycle is in progress
//!
//! When a cycle is active, Tab / Shift+Tab events are swallowed
//! (`CallbackResult::Drop`) so the native switcher never sees them.
//!
//! Permission: same as Quick Type — requires Input Monitoring.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use async_channel::{unbounded, Receiver, Sender};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopRef, CFRunLoopStop};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CallbackResult, EventField,
};
use foreign_types::ForeignType;
use thiserror::Error;

#[derive(Debug, Clone)]
pub enum SystemSwitcherEvent {
    Open { reverse: bool },
    Cycle { reverse: bool },
    Confirm,
    /// The user typed a printable character during an active cycle. Forward it
    /// to the panel's query and stop treating Cmd release as Confirm so the
    /// panel stays open when Cmd is eventually released.
    TypeText(String),
}

#[derive(Debug, Error)]
pub enum SystemSwitcherError {
    #[error(
        "CGEventTap could not be created — grant Input Monitoring in System Settings \
         (Privacy & Security)"
    )]
    PermissionDenied,
    #[error("system switcher start failed: {0}")]
    Start(String),
}

/// macOS keycode for Tab (`kVK_Tab`).
const KEYCODE_TAB: i64 = 48;
/// macOS keycodes for the Command keys (`kVK_Command` / `kVK_RightCommand`).
/// FlagsChanged events carry the keycode of the modifier whose state changed;
/// we only react to Cmd-specific transitions so chording with Shift/Alt does
/// not disturb our Cmd-held tracking.
const KEYCODE_LEFT_CMD: i64 = 55;
const KEYCODE_RIGHT_CMD: i64 = 54;

/// `kCGEventSourceStateHIDSystemState` — the event came from the real
/// keyboard (versus being synthesized by a remapper app).
const HID_SYSTEM_STATE: i64 = 1;

pub struct SystemSwitcherService {
    receiver: Receiver<SystemSwitcherEvent>,
    runloop: Arc<Mutex<Option<SendableRunLoop>>>,
    thread: Option<JoinHandle<()>>,
    /// Shared with the tap callback. The driver flips these when the cycle
    /// ends (panel dismissed/confirmed) so the next Cmd+Tab starts fresh.
    cycle_started: Arc<AtomicBool>,
    confirm_on_release: Arc<AtomicBool>,
}

struct SendableRunLoop(CFRunLoopRef);
unsafe impl Send for SendableRunLoop {}

impl SystemSwitcherService {
    pub fn start() -> Result<Self, SystemSwitcherError> {
        let (tx, rx) = unbounded::<SystemSwitcherEvent>();
        let (start_tx, start_rx) =
            std::sync::mpsc::channel::<Result<(), SystemSwitcherError>>();
        let runloop = Arc::new(Mutex::new(None::<SendableRunLoop>));
        let cycle_started = Arc::new(AtomicBool::new(false));
        let confirm_on_release = Arc::new(AtomicBool::new(false));

        let runloop_thread = runloop.clone();
        let tx_thread = tx.clone();
        let cycle_started_thread = cycle_started.clone();
        let confirm_on_release_thread = confirm_on_release.clone();
        let thread = std::thread::Builder::new()
            .name("leswitcheur-sys-switcher".into())
            .spawn(move || {
                run_tap_loop(
                    runloop_thread,
                    tx_thread,
                    start_tx,
                    cycle_started_thread,
                    confirm_on_release_thread,
                )
            })
            .map_err(|e| SystemSwitcherError::Start(format!("spawn thread: {e}")))?;

        match start_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                receiver: rx,
                runloop,
                thread: Some(thread),
                cycle_started,
                confirm_on_release,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(e) => {
                let _ = thread.join();
                Err(SystemSwitcherError::Start(format!("start channel: {e}")))
            }
        }
    }

    pub fn receiver(&self) -> Receiver<SystemSwitcherEvent> {
        self.receiver.clone()
    }

    /// Called by the driver when the switcher panel is definitively closed
    /// (Confirm finalized, Dismissed via Escape/blur, or settings opened).
    /// Clears both cycle flags so the next Cmd+Tab begins a fresh cycle.
    pub fn reset_cycle(&self) {
        self.cycle_started.store(false, Ordering::Relaxed);
        self.confirm_on_release.store(false, Ordering::Relaxed);
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

impl Drop for SystemSwitcherService {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_tap_loop(
    runloop: Arc<Mutex<Option<SendableRunLoop>>>,
    tx: Sender<SystemSwitcherEvent>,
    start_tx: std::sync::mpsc::Sender<Result<(), SystemSwitcherError>>,
    cycle_started: Arc<AtomicBool>,
    confirm_on_release: Arc<AtomicBool>,
) {
    let cmd_held = Arc::new(AtomicBool::new(false));

    let cmd_held_cb = cmd_held.clone();
    let cycle_started_cb = cycle_started.clone();
    let confirm_on_release_cb = confirm_on_release.clone();
    let tx_cb = tx.clone();

    let callback = move |_proxy: CGEventTapProxy, etype: CGEventType, event: &CGEvent| {
        handle_event(
            etype,
            event,
            &cmd_held_cb,
            &cycle_started_cb,
            &confirm_on_release_cb,
            &tx_cb,
        )
    };

    let tap = match CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
        callback,
    ) {
        Ok(t) => t,
        Err(()) => {
            let _ = start_tx.send(Err(SystemSwitcherError::PermissionDenied));
            return;
        }
    };

    let source = match tap.mach_port().create_runloop_source(0) {
        Ok(s) => s,
        Err(()) => {
            let _ = start_tx.send(Err(SystemSwitcherError::Start(
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
}

fn handle_event(
    etype: CGEventType,
    event: &CGEvent,
    cmd_held: &AtomicBool,
    cycle_started: &AtomicBool,
    confirm_on_release: &AtomicBool,
    tx: &Sender<SystemSwitcherEvent>,
) -> CallbackResult {
    match etype {
        CGEventType::FlagsChanged => {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
            let flags = event.get_flags();
            let cmd_bit = flags.contains(CGEventFlags::CGEventFlagCommand);
            tracing::info!(
                keycode,
                cmd_bit,
                cycle_started = cycle_started.load(Ordering::Relaxed),
                confirm_on_release = confirm_on_release.load(Ordering::Relaxed),
                "sys_switcher flags_changed"
            );
            if keycode == KEYCODE_LEFT_CMD || keycode == KEYCODE_RIGHT_CMD {
                let source_state =
                    event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID);
                if !cmd_bit && source_state != HID_SYSTEM_STATE {
                    // Synthesized Cmd-up event from userland (remapper /
                    // macro tool). Not a real hardware release — ignore it
                    // so the cycle keeps going.
                    tracing::info!(
                        source_state,
                        "sys_switcher synthesized cmd-up ignored"
                    );
                    return CallbackResult::Keep;
                }
                let was_held = cmd_held.swap(cmd_bit, Ordering::Relaxed);
                if was_held && !cmd_bit && cycle_started.load(Ordering::Relaxed) {
                    if confirm_on_release.load(Ordering::Relaxed) {
                        tracing::info!("sys_switcher emit Confirm");
                        let _ = tx.send_blocking(SystemSwitcherEvent::Confirm);
                    } else {
                        // User already typed — cycle is over and panel is
                        // user-driven. Clear cycle_started so subsequent keys
                        // pass through as normal input.
                        tracing::info!("sys_switcher cmd release after typing, end cycle");
                        cycle_started.store(false, Ordering::Relaxed);
                    }
                }
            }
            CallbackResult::Keep
        }
        CGEventType::KeyDown => {
            if !cmd_held.load(Ordering::Relaxed) {
                return CallbackResult::Keep;
            }
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
            if keycode == KEYCODE_TAB {
                let reverse = event.get_flags().contains(CGEventFlags::CGEventFlagShift);
                let was_cycling = cycle_started.swap(true, Ordering::Relaxed);
                if !was_cycling {
                    confirm_on_release.store(true, Ordering::Relaxed);
                }
                let ev = if was_cycling {
                    SystemSwitcherEvent::Cycle { reverse }
                } else {
                    SystemSwitcherEvent::Open { reverse }
                };
                tracing::info!(reverse, was_cycling, ?ev, "sys_switcher emit tab");
                let _ = tx.send_blocking(ev);
                return CallbackResult::Drop;
            }
            // Non-Tab key while Cmd held during an active cycle: forward as
            // typed text and disarm Confirm-on-release so the panel behaves
            // like a regular hotkey-opened switcher. Stripping Cmd from the
            // event's flags makes CGEventKeyboardGetUnicodeString return the
            // printable character (Cmd+<letter> otherwise yields nothing).
            if cycle_started.load(Ordering::Relaxed) {
                let orig_flags = event.get_flags();
                event.set_flags(orig_flags & !CGEventFlags::CGEventFlagCommand);
                let text = read_unicode(event);
                event.set_flags(orig_flags);
                tracing::info!(keycode, ?text, "sys_switcher non-tab cmd keydown");
                match text {
                    Some(s) if !s.is_empty() && !s.chars().all(char::is_control) => {
                        confirm_on_release.store(false, Ordering::Relaxed);
                        tracing::info!(text = %s, "sys_switcher emit TypeText");
                        let _ = tx.send_blocking(SystemSwitcherEvent::TypeText(s));
                        return CallbackResult::Drop;
                    }
                    _ => {}
                }
            }
            CallbackResult::Keep
        }
        _ => CallbackResult::Keep,
    }
}

fn read_unicode(event: &CGEvent) -> Option<String> {
    const MAX_CHARS: usize = 16;
    let mut buf = [0u16; MAX_CHARS];
    let mut actual: UniCharCount = 0;
    unsafe {
        CGEventKeyboardGetUnicodeString(
            event.as_ptr(),
            MAX_CHARS as UniCharCount,
            &mut actual,
            buf.as_mut_ptr(),
        );
    }
    if actual == 0 {
        return None;
    }
    let len = (actual as usize).min(MAX_CHARS);
    Some(String::from_utf16_lossy(&buf[..len]))
}

#[allow(non_camel_case_types)]
type UniChar = u16;
#[allow(non_camel_case_types)]
type UniCharCount = std::os::raw::c_ulong;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGEventKeyboardGetUnicodeString(
        event: core_graphics::sys::CGEventRef,
        max_string_length: UniCharCount,
        actual_string_length: *mut UniCharCount,
        unicode_string: *mut UniChar,
    );
}
