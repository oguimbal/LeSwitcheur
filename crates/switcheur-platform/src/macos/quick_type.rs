//! Quick Type: hold Fn and type letters to open/filter the switcher globally.
//!
//! We install a `CGEventTap` at HID level on a dedicated thread with its own
//! `CFRunLoop`. The callback tracks whether the Fn flag is currently held
//! (via `CGEventFlagSecondaryFn` on `FlagsChanged` events) and, when a
//! `KeyDown` arrives while Fn is held, extracts the unicode characters,
//! pushes them on an `async_channel`, and returns `Drop` so the event doesn't
//! reach the focused app.
//!
//! Permission: `CGEventTapCreate` returns NULL when Input Monitoring is not
//! granted. `start()` returns `QuickTypeError::PermissionDenied` in that case
//! so the caller can prompt the user.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use arc_swap::ArcSwap;
use async_channel::{unbounded, Receiver, Sender};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopRef, CFRunLoopStop};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CallbackResult, EventField,
};
use foreign_types::ForeignType;
use switcheur_core::AppMatchSet;
use thiserror::Error;

use crate::macos::recency::FocusedAppCell;

/// Shared per-feature exclusion list. Swapped lock-free when the user edits
/// the list in Settings; read once per event from the tap callback.
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
    #[error(
        "CGEventTap could not be created — grant Input Monitoring in System Settings \
         (Privacy & Security)"
    )]
    PermissionDenied,
    #[error("quick type start failed: {0}")]
    Start(String),
}

/// macOS keycode for delete/backspace (`kVK_Delete`).
const KEYCODE_BACKSPACE: i64 = 51;

/// Pixels of accumulated vertical scroll that correspond to one selection
/// tick (Up/Down). Tuned for Magic Trackpad; adjust after live testing.
const SCROLL_TICK_PIXELS: f64 = 30.0;

pub struct QuickTypeService {
    receiver: Receiver<QuickTypeEvent>,
    runloop: Arc<Mutex<Option<SendableRunLoop>>>,
    thread: Option<JoinHandle<()>>,
}

/// Raw `CFRunLoopRef` wrapped so we can `CFRunLoopStop` it from the main thread.
/// The pointer itself is thread-safe to pass to `CFRunLoopStop`.
struct SendableRunLoop(CFRunLoopRef);
unsafe impl Send for SendableRunLoop {}

impl QuickTypeService {
    /// Start the tap. `focused` and `excluded` are consulted on each `KeyDown`
    /// while Fn is held: when the frontmost app matches the exclusion list,
    /// the keystroke is passed through to the app instead of being captured.
    pub fn start(
        focused: FocusedAppCell,
        excluded: ExclusionCell,
    ) -> Result<Self, QuickTypeError> {
        let (tx, rx) = unbounded::<QuickTypeEvent>();
        let (start_tx, start_rx) = std::sync::mpsc::channel::<Result<(), QuickTypeError>>();
        let runloop = Arc::new(Mutex::new(None::<SendableRunLoop>));

        let runloop_thread = runloop.clone();
        let tx_thread = tx.clone();
        let thread = std::thread::Builder::new()
            .name("leswitcheur-quicktype".into())
            .spawn(move || run_tap_loop(runloop_thread, tx_thread, start_tx, focused, excluded))
            .map_err(|e| QuickTypeError::Start(format!("spawn thread: {e}")))?;

        // Wait for the thread to report whether the tap installed successfully.
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
                Err(QuickTypeError::Start(format!("start channel: {e}")))
            }
        }
    }

    pub fn receiver(&self) -> Receiver<QuickTypeEvent> {
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

impl Drop for QuickTypeService {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_tap_loop(
    runloop: Arc<Mutex<Option<SendableRunLoop>>>,
    tx: Sender<QuickTypeEvent>,
    start_tx: std::sync::mpsc::Sender<Result<(), QuickTypeError>>,
    focused: FocusedAppCell,
    excluded: ExclusionCell,
) {
    let fn_held = Arc::new(AtomicBool::new(false));
    let scroll_accum = Arc::new(Mutex::new(0.0_f64));
    let scrolled_this_hold = Arc::new(AtomicBool::new(false));
    let fn_held_cb = fn_held.clone();
    let scroll_accum_cb = scroll_accum.clone();
    let scrolled_cb = scrolled_this_hold.clone();
    let tx_cb = tx.clone();
    let focused_cb = focused.clone();
    let excluded_cb = excluded.clone();

    let callback = move |_proxy: CGEventTapProxy, etype: CGEventType, event: &CGEvent| {
        handle_event(
            etype,
            event,
            &fn_held_cb,
            &scroll_accum_cb,
            &scrolled_cb,
            &tx_cb,
            &focused_cb,
            &excluded_cb,
        )
    };

    let tap = match CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![
            CGEventType::FlagsChanged,
            CGEventType::KeyDown,
            CGEventType::ScrollWheel,
        ],
        callback,
    ) {
        Ok(t) => t,
        Err(()) => {
            let _ = start_tx.send(Err(QuickTypeError::PermissionDenied));
            return;
        }
    };

    let source = match tap.mach_port().create_runloop_source(0) {
        Ok(s) => s,
        Err(()) => {
            let _ = start_tx.send(Err(QuickTypeError::Start(
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

    // Keep the tap alive until the run loop ends.
    drop(tap);
}

fn handle_event(
    etype: CGEventType,
    event: &CGEvent,
    fn_held: &AtomicBool,
    scroll_accum: &Mutex<f64>,
    scrolled_this_hold: &AtomicBool,
    tx: &Sender<QuickTypeEvent>,
    focused: &FocusedAppCell,
    excluded: &ExclusionCell,
) -> CallbackResult {
    match etype {
        CGEventType::FlagsChanged => {
            let flags = event.get_flags();
            let held = flags.contains(CGEventFlags::CGEventFlagSecondaryFn);
            let prev = fn_held.swap(held, Ordering::Relaxed);
            match (prev, held) {
                (false, true) => {
                    *scroll_accum.lock().unwrap() = 0.0;
                    scrolled_this_hold.store(false, Ordering::Relaxed);
                }
                (true, false) => {
                    let scrolled = scrolled_this_hold.swap(false, Ordering::Relaxed);
                    *scroll_accum.lock().unwrap() = 0.0;
                    let _ = tx.send_blocking(QuickTypeEvent::FnReleased { scrolled });
                }
                _ => {}
            }
            CallbackResult::Keep
        }
        CGEventType::KeyDown => {
            if !fn_held.load(Ordering::Relaxed) {
                return CallbackResult::Keep;
            }
            if focused_excluded(focused, excluded) {
                return CallbackResult::Keep;
            }
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
            if keycode == KEYCODE_BACKSPACE {
                let _ = tx.send_blocking(QuickTypeEvent::Backspace);
                return CallbackResult::Drop;
            }
            match read_unicode(event) {
                Some(s) if !s.is_empty() && !s.chars().all(char::is_control) => {
                    let _ = tx.send_blocking(QuickTypeEvent::InsertText(s));
                    CallbackResult::Drop
                }
                _ => CallbackResult::Keep,
            }
        }
        CGEventType::ScrollWheel => {
            if !fn_held.load(Ordering::Relaxed) {
                return CallbackResult::Keep;
            }
            if focused_excluded(focused, excluded) {
                return CallbackResult::Keep;
            }
            let delta =
                event.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1);
            let mut accum = scroll_accum.lock().unwrap();
            *accum += delta;
            while accum.abs() >= SCROLL_TICK_PIXELS {
                let dir = if *accum > 0.0 {
                    *accum -= SCROLL_TICK_PIXELS;
                    ScrollDir::Down
                } else {
                    *accum += SCROLL_TICK_PIXELS;
                    ScrollDir::Up
                };
                scrolled_this_hold.store(true, Ordering::Relaxed);
                let _ = tx.send_blocking(QuickTypeEvent::Scroll(dir));
            }
            CallbackResult::Drop
        }
        _ => CallbackResult::Keep,
    }
}

fn focused_excluded(focused: &FocusedAppCell, excluded: &ExclusionCell) -> bool {
    let excl = excluded.load();
    if excl.is_empty() {
        return false;
    }
    let snap = focused.load();
    let Some(app) = snap.as_ref().as_ref() else {
        return false;
    };
    excl.any_match(&app.name, app.bundle_id.as_deref())
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
