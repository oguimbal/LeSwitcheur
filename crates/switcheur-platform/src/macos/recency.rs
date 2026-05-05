//! Observes focus / activation so the sort layer can order by recency.
//!
//! Three observers:
//!   * [`AppActivationObserver`] — always on. Wraps the NSWorkspace block-based
//!     notification for `NSWorkspaceDidActivateApplicationNotification`. Cheap:
//!     one registration, fires only on Cmd+Tab / dock click / our own raises.
//!   * [`FocusedWindowObserver`] — opt-in, one per running app, on
//!     `kAXFocusedWindowChangedNotification`. Each observer schedules its
//!     run-loop source on the main thread. Costs ~1% CPU because the kernel
//!     wakes us on every focus flip system-wide.
//!   * [`LaunchObserver`] — opt-in, paired with the per-app focus observers.
//!     Listens to `NSWorkspaceDidLaunchApplication` /
//!     `NSWorkspaceDidTerminateApplicationNotification` so apps started after
//!     the switcher boots get an AX focus observer too. Without this, intra-
//!     app window switches inside any later-launched app silently dropped on
//!     the floor and `RecencyTracker` ranked the wrong sibling.
//!
//! All three observers push into a shared [`RecencyTracker`] behind a mutex.
//! The service is always driven from the main thread — NSWorkspace and AX
//! call their blocks/callbacks on the thread that registered them.
//!
//! The app observer *also* maintains a shared [`FocusedApp`] snapshot so other
//! subsystems (hotkey gating, Quick Type tap) can cheaply check which app is
//! currently frontmost. That snapshot lives behind `ArcSwap` for lock-free
//! reads from the HID tap thread.

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::raw::c_int;
use std::ptr;
use std::sync::{Arc, Mutex};

use accessibility_sys::{
    kAXErrorSuccess, kAXFocusedWindowAttribute, kAXFocusedWindowChangedNotification,
    AXError, AXObserverAddNotification, AXObserverCreate, AXObserverGetRunLoopSource,
    AXObserverRef, AXObserverRemoveNotification, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementRef,
};

// Private Accessibility helper — the only way to map an AX window element to
// its CGWindowID. Used here so the recency tracker can key on the stable
// window id instead of the (volatile) window title. Same binding as the one
// in `windows.rs`; duplicated rather than shared to keep modules decoupled.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn _AXUIElementGetWindow(element: AXUIElementRef, window_id: *mut u32) -> AXError;
}
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
    NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification, NSWorkspaceDidLaunchApplicationNotification,
    NSWorkspaceDidTerminateApplicationNotification,
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

/// Type alias for the fan-out subscriber list. The `AppActivationObserver`
/// keeps these around for the lifetime of the process; consumers (e.g. the
/// panel-dismiss loop in main.rs) hold the matching receiver.
type ActivationSubscribers = Arc<Mutex<Vec<async_channel::Sender<FocusedApp>>>>;

/// Observer for NSWorkspace app-activation notifications. Drop unregisters.
pub struct AppActivationObserver {
    center: Retained<NSNotificationCenter>,
    token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    subscribers: ActivationSubscribers,
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
        let self_pid = std::process::id() as i32;
        let subscribers: ActivationSubscribers = Arc::new(Mutex::new(Vec::new()));
        let subs_for_block = subscribers.clone();
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
            let snapshot = running_app_snapshot(app);
            // Filter our own activation (Settings / Onboarding panels coming
            // forward) BEFORE touching the tracker — otherwise the switcher's
            // own pid lands in the recency maps and inflates `app_rank` for a
            // window the user can never see in the list. Pid is the reliable
            // primary check; bundle id is defence in depth.
            if snapshot.pid == self_pid {
                return;
            }
            if let (Some(self_bid), Some(bid)) =
                (self_bundle_id.as_deref(), snapshot.bundle_id.as_deref())
            {
                if self_bid.eq_ignore_ascii_case(bid) {
                    return;
                }
            }
            let pid = app.processIdentifier();
            // Note the app's currently-focused window so per-window MRU
            // captures app activations that didn't go through the switcher
            // (Dock click, Cmd-Tab, click-through from another window). Without
            // this, an app brought forward by non-switcher means would leave
            // its focused window stuck at an old window_rank, and the user
            // couldn't alt-tab back to it via the per-window sort.
            let focused_id = ax_focused_window_id(pid);
            if let Ok(mut t) = tracker.lock() {
                t.note_app(pid);
                if let Some(id) = focused_id {
                    t.note_window(pid, id as u64);
                }
            }
            focused.store(Arc::new(Some(snapshot.clone())));
            // Fan out to subscribers (e.g. the panel-dismiss loop). Drop
            // closed senders. `try_send` on bounded channels never blocks the
            // main thread; a wedged consumer loses dismisses, not the run loop.
            if let Ok(mut subs) = subs_for_block.lock() {
                subs.retain(|s| !s.is_closed());
                for s in subs.iter() {
                    let _ = s.try_send(snapshot.clone());
                }
            }
        });
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
        };
        Self {
            center,
            token,
            subscribers,
        }
    }

    /// Register a subscriber that receives every non-self app activation.
    /// Returns the receiver half; the sender lives inside the observer for the
    /// observer's lifetime.
    pub fn subscribe(&self) -> async_channel::Receiver<FocusedApp> {
        let (tx, rx) = async_channel::bounded(8);
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.push(tx);
        }
        rx
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

// Safety: every field is either a CF/AX reference or a Box<…> over Send+Sync
// data. The CF/AX pointers are valid for the lifetime of this struct and we
// never read or write them from anywhere except the main thread (the only
// thread that calls AXObserverCreate / CFRelease / CFRunLoopRemoveSource on
// these handles). The Send+Sync impls exist purely so the observer can sit
// inside an `Arc<Mutex<HashMap<…>>>` shared with NSWorkspace launch /
// terminate blocks (whose `RcBlock` captures must satisfy Send+Sync). All
// access still funnels through the main run-loop in practice.
unsafe impl Send for FocusedWindowObserver {}
unsafe impl Sync for FocusedWindowObserver {}

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
    let Some(cg_id) = (unsafe { ax_window_cg_id(element) }) else {
        // Window has no CGWindowID (extremely rare for a focused window).
        // Without it we can't key recency reliably, so skip — the app_rank
        // fallback still keeps the switcher usable.
        return;
    };
    if let Ok(mut t) = ctx.tracker.lock() {
        t.note_window(ctx.pid, cg_id as u64);
    }
}

unsafe fn ax_window_cg_id(elem: AXUIElementRef) -> Option<u32> {
    let mut id: u32 = 0;
    let err = _AXUIElementGetWindow(elem, &mut id);
    if err == kAXErrorSuccess && id != 0 {
        Some(id)
    } else {
        None
    }
}

/// Query an app's currently-focused window and return its CGWindowID.
/// Returns `None` when the app has no focused window (background app with no
/// open windows) or when AX refuses to hand out the id.
fn ax_focused_window_id(pid: c_int) -> Option<u32> {
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
        ax_window_cg_id(w_elem)
    }
}

/// Shared map of per-pid focus observers. Wrapped in `Arc<Mutex<>>` so the
/// NSWorkspace launch / terminate blocks (which fire on the main thread but
/// require `Send + Sync` captures) can mutate the map alongside
/// [`RecencyService`].
type WindowObservers = Arc<Mutex<HashMap<c_int, FocusedWindowObserver>>>;

/// Owns the always-on app observer and, when enabled, a per-pid window-focus
/// observer plus an NSWorkspace launch/terminate listener that keeps the
/// per-pid map in sync with the running-apps set. Mutating the tracker
/// happens from the main thread (where all callbacks fire).
pub struct RecencyService {
    tracker: Arc<Mutex<RecencyTracker>>,
    focused: FocusedAppCell,
    _app: AppActivationObserver,
    windows: WindowObservers,
    _launch: Option<LaunchObserver>,
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
            windows: Arc::new(Mutex::new(HashMap::new())),
            _launch: None,
        }
    }

    pub fn tracker(&self) -> &Arc<Mutex<RecencyTracker>> {
        &self.tracker
    }

    pub fn focused_app(&self) -> FocusedAppCell {
        self.focused.clone()
    }

    /// Subscribe to non-self app activations. Used by the panel-dismiss loop
    /// to react to another app becoming frontmost while the switcher is open
    /// — the GPUI window-active flip is unreliable for `WindowKind::PopUp`,
    /// but `NSWorkspaceDidActivateApplication` is.
    pub fn subscribe_app_activations(&self) -> async_channel::Receiver<FocusedApp> {
        self._app.subscribe()
    }

    /// Start a window-focus observer for every given pid + a launch/terminate
    /// listener that keeps the observer set in sync with the running-apps
    /// table. Safe to call multiple times: any previous observers are dropped
    /// first.
    ///
    /// Also seeds [`RecencyTracker`] with each app's currently-focused window,
    /// so per-window MRU ordering has something to work with before any focus
    /// change has actually fired. Without the seed, the first switcher open
    /// after launch would have no window ranks at all and fall back to raw
    /// enumeration order — defeating the whole point of the per-window mode.
    pub fn enable_window_tracking(&mut self, pids: &[c_int]) {
        let self_pid = std::process::id() as c_int;
        let observed = {
            let mut map = self.windows.lock().expect("poisoned");
            map.clear();
            for &pid in pids {
                if pid == self_pid {
                    continue;
                }
                if let Some(obs) = FocusedWindowObserver::new(pid, self.tracker.clone()) {
                    map.insert(pid, obs);
                }
            }
            map.len()
        };
        seed_window_ranks(&self.tracker, pids);
        // Drop the existing launch observer (if any) before installing a fresh
        // one — otherwise we'd keep two parallel listeners after a settings-
        // round-trip and double-attach observers for every newly-launched app.
        self._launch = None;
        self._launch = Some(LaunchObserver::new(
            self.windows.clone(),
            self.tracker.clone(),
        ));
        tracing::info!(observed, total = pids.len(), "enabled window-focus tracking");
    }

    pub fn disable_window_tracking(&mut self) {
        // Drop launch observer first so no late notification can sneak in and
        // re-populate the map after we cleared it.
        self._launch = None;
        if let Ok(mut map) = self.windows.lock() {
            map.clear();
        }
        tracing::info!("disabled window-focus tracking");
    }

    pub fn window_tracking_enabled(&self) -> bool {
        self._launch.is_some()
    }
}

/// NSWorkspace listener for application launch + termination. When per-window
/// tracking is on, attaches a [`FocusedWindowObserver`] to every newly-
/// launched regular app and detaches it when the app quits.
///
/// Without this, intra-app window switches inside any app launched after the
/// switcher booted (e.g. the user starts Chrome ten minutes after login)
/// silently dropped on the floor — the switcher would still rank the *first*
/// focused Chrome window correctly because the activation observer always
/// seeds it, but subsequent cmd-` flips inside Chrome would not. The user
/// would then alt-tab and land on the previous app instead of the previous
/// Chrome window.
struct LaunchObserver {
    center: Retained<NSNotificationCenter>,
    launch_token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    terminate_token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl LaunchObserver {
    fn new(windows: WindowObservers, tracker: Arc<Mutex<RecencyTracker>>) -> Self {
        let ws = NSWorkspace::sharedWorkspace();
        let center = ws.notificationCenter();
        let launch_name = unsafe { NSWorkspaceDidLaunchApplicationNotification };
        let terminate_name = unsafe { NSWorkspaceDidTerminateApplicationNotification };
        let key_static = unsafe { NSWorkspaceApplicationKey };
        let self_pid = std::process::id() as c_int;

        let launch_block = {
            let windows = windows.clone();
            let tracker = tracker.clone();
            RcBlock::new(move |notif: NonNull<NSNotification>| {
                let notif = unsafe { notif.as_ref() };
                let Some(info) = notif.userInfo() else { return };
                let key_obj: &AnyObject = key_static.as_ref();
                let Some(app_obj) = info.objectForKey(key_obj) else { return };
                let app: &NSRunningApplication =
                    match app_obj.downcast_ref::<NSRunningApplication>() {
                        Some(a) => a,
                        None => return,
                    };
                if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
                    return;
                }
                let pid = app.processIdentifier();
                if pid == self_pid {
                    return;
                }
                let Some(obs) = FocusedWindowObserver::new(pid, tracker.clone()) else {
                    tracing::debug!(pid, "could not attach focus observer for new app");
                    return;
                };
                if let Ok(mut map) = windows.lock() {
                    map.insert(pid, obs);
                    tracing::info!(pid, "attached focus observer for launched app");
                }
            })
        };
        let launch_token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(launch_name),
                None,
                None,
                &launch_block,
            )
        };

        let terminate_block = {
            let windows = windows.clone();
            RcBlock::new(move |notif: NonNull<NSNotification>| {
                let notif = unsafe { notif.as_ref() };
                let Some(info) = notif.userInfo() else { return };
                let key_obj: &AnyObject = key_static.as_ref();
                let Some(app_obj) = info.objectForKey(key_obj) else { return };
                let app: &NSRunningApplication =
                    match app_obj.downcast_ref::<NSRunningApplication>() {
                        Some(a) => a,
                        None => return,
                    };
                let pid = app.processIdentifier();
                if let Ok(mut map) = windows.lock() {
                    if map.remove(&pid).is_some() {
                        tracing::debug!(pid, "detached focus observer for terminated app");
                    }
                }
            })
        };
        let terminate_token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(terminate_name),
                None,
                None,
                &terminate_block,
            )
        };

        Self {
            center,
            launch_token,
            terminate_token,
        }
    }
}

impl Drop for LaunchObserver {
    fn drop(&mut self) {
        unsafe {
            self.center.removeObserver(self.launch_token.as_ref());
            self.center.removeObserver(self.terminate_token.as_ref());
        }
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
        if let Some(id) = ax_focused_window_id(pid) {
            guard.note_window(pid, id as u64);
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
