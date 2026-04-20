//! Observes focus / activation so the sort layer can order by recency.
//!
//! Two observers:
//!   * [`AppActivationObserver`] — always on. Wraps the NSWorkspace block-based
//!     notification for `NSWorkspaceDidActivateApplicationNotification`. Cheap:
//!     one registration, fires only on Cmd+Tab / dock click / our own raises.
//!   * [`FocusedWindowObserver`] — opt-in, started when the user picks
//!     `SortOrder::RecentWindow`. One AXObserver per running app on
//!     `kAXFocusedWindowChangedNotification`. Each observer schedules its
//!     run-loop source on the main thread. Costs ~1% CPU because the kernel
//!     wakes us on every focus flip system-wide.
//!
//! Both observers push into a shared [`RecencyTracker`] behind a mutex. The
//! service is always driven from the main thread — NSWorkspace and AX call
//! their blocks/callbacks on the thread that registered them.
//!
//! The app observer *also* maintains a shared [`FocusedApp`] snapshot so other
//! subsystems (hotkey gating, Quick Type tap) can cheaply check which app is
//! currently frontmost. That snapshot lives behind `ArcSwap` for lock-free
//! reads from the HID tap thread.

use std::ffi::c_void;
use std::os::raw::c_int;
use std::ptr;
use std::sync::{Arc, Mutex};

use accessibility_sys::{
    kAXErrorSuccess, kAXFocusedWindowAttribute, kAXFocusedWindowChangedNotification,
    kAXTitleAttribute, AXError, AXObserverAddNotification, AXObserverCreate,
    AXObserverGetRunLoopSource, AXObserverRef, AXObserverRemoveNotification,
    AXUIElementCopyAttributeValue, AXUIElementCreateApplication, AXUIElementRef,
};
use core_foundation::base::CFType;
use arc_swap::ArcSwap;
use block2::RcBlock;
use core_foundation::base::{CFRelease, TCFType};
use core_foundation::runloop::{
    kCFRunLoopDefaultMode, CFRunLoopAddSource, CFRunLoopGetMain, CFRunLoopRemoveSource,
    CFRunLoopSourceRef,
};
use core_foundation::string::{CFString, CFStringRef};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};
use std::ptr::NonNull;
use switcheur_core::RecencyTracker;

/// Snapshot of whichever non-self app is currently frontmost. Updated by the
/// NSWorkspace activation observer on the main thread and read lock-free from
/// HID tap threads + the hotkey dispatch loop.
#[derive(Debug, Clone, Default)]
pub struct FocusedApp {
    pub pid: i32,
    pub name: String,
    pub bundle_id: Option<String>,
}

pub type FocusedAppCell = Arc<ArcSwap<Option<FocusedApp>>>;

pub fn focused_app_cell() -> FocusedAppCell {
    Arc::new(ArcSwap::from_pointee(None))
}

fn running_app_snapshot(app: &NSRunningApplication) -> FocusedApp {
    let name = app
        .localizedName()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let bundle_id = app.bundleIdentifier().map(|s| s.to_string());
    FocusedApp {
        pid: app.processIdentifier(),
        name,
        bundle_id,
    }
}

/// Observer for NSWorkspace app-activation notifications. Drop unregisters.
pub struct AppActivationObserver {
    center: Retained<NSNotificationCenter>,
    token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl AppActivationObserver {
    pub fn new(
        tracker: Arc<Mutex<RecencyTracker>>,
        focused: FocusedAppCell,
        self_bundle_id: Option<String>,
    ) -> Self {
        let ws = NSWorkspace::sharedWorkspace();
        let center = ws.notificationCenter();
        let name = unsafe { NSWorkspaceDidActivateApplicationNotification };
        let key_static = unsafe { NSWorkspaceApplicationKey };
        let block = RcBlock::new(move |notif: NonNull<NSNotification>| {
            let notif = unsafe { notif.as_ref() };
            let Some(info) = notif.userInfo() else { return };
            let key_obj: &AnyObject = key_static.as_ref();
            let Some(app_obj) = info.objectForKey(key_obj) else {
                return;
            };
            let app: &NSRunningApplication = match app_obj.downcast_ref::<NSRunningApplication>() {
                Some(a) => a,
                None => return,
            };
            let pid = app.processIdentifier();
            // Also note the app's currently-focused window so per-window MRU
            // captures app activations that didn't go through the switcher
            // (Dock click, Cmd-Tab, click-through from another window). Without
            // this, an app brought forward by non-switcher means would leave
            // its focused window stuck at an old window_rank, and the user
            // couldn't alt-tab back to it via the per-window sort.
            let focused_title = ax_focused_window_title(pid);
            if let Ok(mut t) = tracker.lock() {
                t.note_app(pid);
                if let Some(title) = focused_title {
                    t.note_window(pid, &title);
                }
            }
            let snapshot = running_app_snapshot(app);
            // Ignore our own activation (e.g. Settings window coming forward)
            // so the remembered "frontmost" stays the user's real app.
            if let (Some(self_bid), Some(bid)) = (self_bundle_id.as_deref(), snapshot.bundle_id.as_deref()) {
                if self_bid.eq_ignore_ascii_case(bid) {
                    return;
                }
            }
            focused.store(Arc::new(Some(snapshot)));
        });
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
        };
        Self { center, token }
    }
}

impl Drop for AppActivationObserver {
    fn drop(&mut self) {
        unsafe { self.center.removeObserver(self.token.as_ref()) };
    }
}

/// Per-app observer on `kAXFocusedWindowChangedNotification`. Holds a boxed
/// callback whose raw pointer is passed as AX `refcon`. Drop removes the
/// notification, detaches the run-loop source, and drops the callback box.
pub struct FocusedWindowObserver {
    observer: AXObserverRef,
    app_elem: AXUIElementRef,
    source: CFRunLoopSourceRef,
    notif: CFString,
    // Boxed so the raw pointer passed to AX stays stable for the lifetime of
    // this struct. Dropped in `Drop`.
    _cb_box: Box<AxCallbackContext>,
}

struct AxCallbackContext {
    pid: c_int,
    tracker: Arc<Mutex<RecencyTracker>>,
}

impl FocusedWindowObserver {
    pub fn new(pid: c_int, tracker: Arc<Mutex<RecencyTracker>>) -> Option<Self> {
        let cb_box = Box::new(AxCallbackContext { pid, tracker });
        let refcon = &*cb_box as *const AxCallbackContext as *mut c_void;

        let mut observer: AXObserverRef = ptr::null_mut();
        let err = unsafe { AXObserverCreate(pid, ax_focused_window_cb, &mut observer) };
        if err != kAXErrorSuccess || observer.is_null() {
            tracing::debug!(pid, err, "AXObserverCreate failed");
            return None;
        }

        let app_elem = unsafe { AXUIElementCreateApplication(pid) };
        if app_elem.is_null() {
            unsafe { CFRelease(observer as *const c_void) };
            return None;
        }

        let notif = CFString::from_static_string(kAXFocusedWindowChangedNotification);
        let err = unsafe {
            AXObserverAddNotification(observer, app_elem, notif.as_concrete_TypeRef(), refcon)
        };
        if err != kAXErrorSuccess {
            tracing::debug!(pid, err, "AXObserverAddNotification failed");
            unsafe { CFRelease(app_elem as *const c_void) };
            unsafe { CFRelease(observer as *const c_void) };
            return None;
        }

        let source = unsafe { AXObserverGetRunLoopSource(observer) };
        if source.is_null() {
            unsafe { CFRelease(app_elem as *const c_void) };
            unsafe { CFRelease(observer as *const c_void) };
            return None;
        }
        unsafe {
            CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopDefaultMode);
        }

        Some(Self {
            observer,
            app_elem,
            source,
            notif,
            _cb_box: cb_box,
        })
    }
}

impl Drop for FocusedWindowObserver {
    fn drop(&mut self) {
        unsafe {
            CFRunLoopRemoveSource(CFRunLoopGetMain(), self.source, kCFRunLoopDefaultMode);
            let _ = AXObserverRemoveNotification(
                self.observer,
                self.app_elem,
                self.notif.as_concrete_TypeRef(),
            );
            CFRelease(self.app_elem as *const c_void);
            CFRelease(self.observer as *const c_void);
        }
    }
}

unsafe extern "C" fn ax_focused_window_cb(
    _observer: AXObserverRef,
    element: AXUIElementRef,
    _notification: CFStringRef,
    refcon: *mut c_void,
) {
    if refcon.is_null() || element.is_null() {
        return;
    }
    let ctx = unsafe { &*(refcon as *const AxCallbackContext) };
    let title = unsafe { ax_copy_title(element) }.unwrap_or_default();
    if let Ok(mut t) = ctx.tracker.lock() {
        t.note_window(ctx.pid, &title);
    }
}

unsafe fn ax_copy_title(elem: AXUIElementRef) -> Option<String> {
    let attr = CFString::from_static_string(kAXTitleAttribute);
    let mut value: *const c_void = ptr::null();
    let err: AXError =
        AXUIElementCopyAttributeValue(elem, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let s: CFString = CFString::wrap_under_create_rule(value as CFStringRef);
    Some(s.to_string())
}

/// Query an app's current focused window title via AX. Returns `None` when
/// the app exposes no focused window (background app, no windows open) or
/// when the focused window has an empty/missing title.
fn ax_focused_window_title(pid: c_int) -> Option<String> {
    unsafe {
        let app_elem = AXUIElementCreateApplication(pid);
        if app_elem.is_null() {
            return None;
        }
        // Own the retain returned by the `Create` call so it's released
        // regardless of which branch we exit through.
        let app_ref: CFType = CFType::wrap_under_create_rule(app_elem as _);
        let raw_app = app_ref.as_CFTypeRef() as AXUIElementRef;
        let attr = CFString::from_static_string(kAXFocusedWindowAttribute);
        let mut value: *const c_void = ptr::null();
        let err: AXError =
            AXUIElementCopyAttributeValue(raw_app, attr.as_concrete_TypeRef(), &mut value);
        if err != kAXErrorSuccess || value.is_null() {
            return None;
        }
        let w: CFType = CFType::wrap_under_create_rule(value as _);
        let w_elem = w.as_CFTypeRef() as AXUIElementRef;
        ax_copy_title(w_elem).filter(|t| !t.is_empty())
    }
}

/// Owns the always-on app observer and, when enabled, a per-pid window-focus
/// observer. Mutating the tracker happens from the main thread (where all
/// callbacks fire).
pub struct RecencyService {
    tracker: Arc<Mutex<RecencyTracker>>,
    focused: FocusedAppCell,
    _app: AppActivationObserver,
    windows: Vec<FocusedWindowObserver>,
}

impl RecencyService {
    pub fn start(tracker: Arc<Mutex<RecencyTracker>>, focused: FocusedAppCell) -> Self {
        let self_bundle_id = current_process_bundle_id();
        // Seed with the current frontmost app — no activation notification
        // fires for the app that was already foreground when we launched.
        seed_focused(&focused, self_bundle_id.as_deref());
        Self {
            _app: AppActivationObserver::new(
                tracker.clone(),
                focused.clone(),
                self_bundle_id,
            ),
            tracker,
            focused,
            windows: Vec::new(),
        }
    }

    pub fn tracker(&self) -> &Arc<Mutex<RecencyTracker>> {
        &self.tracker
    }

    pub fn focused_app(&self) -> FocusedAppCell {
        self.focused.clone()
    }

    /// Start a window-focus observer for every given pid. Safe to call
    /// multiple times: any previous observers are dropped first.
    ///
    /// Also seeds [`RecencyTracker`] with each app's currently-focused window,
    /// so per-window MRU ordering has something to work with before any focus
    /// change has actually fired. Without the seed, the first switcher open
    /// after launch would have no window ranks at all and fall back to raw
    /// enumeration order — defeating the whole point of the per-window mode.
    pub fn enable_window_tracking(&mut self, pids: &[c_int]) {
        self.windows.clear();
        for &pid in pids {
            if let Some(obs) = FocusedWindowObserver::new(pid, self.tracker.clone()) {
                self.windows.push(obs);
            }
        }
        seed_window_ranks(&self.tracker, pids);
        tracing::info!(
            observed = self.windows.len(),
            total = pids.len(),
            "enabled window-focus tracking"
        );
    }

    pub fn disable_window_tracking(&mut self) {
        self.windows.clear();
        tracing::info!("disabled window-focus tracking");
    }

    pub fn window_tracking_enabled(&self) -> bool {
        !self.windows.is_empty()
    }
}

/// Ask AX for each app's currently-focused window and stamp it into the
/// tracker at the current instant. Called when per-window tracking is
/// (re-)enabled so the very first switcher open after a mode change has
/// a usable starting order.
fn seed_window_ranks(tracker: &Arc<Mutex<RecencyTracker>>, pids: &[c_int]) {
    let mut seeded = 0usize;
    let mut guard = match tracker.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    for &pid in pids {
        if let Some(title) = ax_focused_window_title(pid) {
            guard.note_window(pid, &title);
            seeded += 1;
        }
    }
    tracing::debug!(seeded, pids = pids.len(), "seeded window recency ranks");
}

fn current_process_bundle_id() -> Option<String> {
    let app = NSRunningApplication::currentApplication();
    app.bundleIdentifier().map(|s| s.to_string())
}

fn seed_focused(focused: &FocusedAppCell, self_bundle_id: Option<&str>) {
    let ws = NSWorkspace::sharedWorkspace();
    let Some(app) = ws.frontmostApplication() else {
        return;
    };
    let snapshot = running_app_snapshot(&app);
    if let (Some(self_bid), Some(bid)) = (self_bundle_id, snapshot.bundle_id.as_deref()) {
        if self_bid.eq_ignore_ascii_case(bid) {
            return;
        }
    }
    focused.store(Arc::new(Some(snapshot)));
}
