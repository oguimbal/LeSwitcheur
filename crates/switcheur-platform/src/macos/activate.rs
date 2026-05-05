//! Raise a specific window or focus an app — cross-Space, from fullscreen,
//! through the Sonoma+ activation lock-down.
//!
//! Two paths, depending on whether we know the target's `CGWindowID`.
//!
//! Path A — pure SLPS (we have a `CGWindowID`). Mirrors AltTab's
//! `Window.focus()` and yabai's `window_manager_focus_window_with_raise`.
//! `NSRunningApplication::activateFromApplication` is intentionally NOT
//! called: when the caller (us) currently holds activation, that yield
//! makes WindowServer treat the target app as already-frontmost on the
//! *current* Space, so the SLPS pick that should switch Space ends up
//! just updating the front-window pointer — the click looks like a no-op.
//! Both AltTab and yabai work as accessory/LSUIElement callers using SLPS
//! alone, so the macOS 14+ "caller must hold activation" rule is not a
//! blocker for SLPS.
//!
//! 1. `_SLPSSetFrontProcessWithOptions` + two "makeKey" event records —
//!    picks the specific window and (when applicable) triggers the Space
//!    switch.
//! 2. `AXUIElementPerformAction(kAXRaiseAction)` on the AX element when AX
//!    surfaced it. Skipped when AX doesn't surface the window (common for
//!    cross-Space targets during a fullscreen Space).
//!
//! Path B — app-level fallback (no `CGWindowID` available). We can't
//! point SLPS at a specific window, so fall back to AX `kAXFrontmostAttribute`
//! + `NSRunningApplication::activate` to at least bring the app forward.

use anyhow::{anyhow, Context, Result};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::string::{CFString, CFStringRef};
use std::ffi::c_void;
use switcheur_core::{AppRef, WindowRef};

use accessibility_sys::{
    kAXCloseButtonAttribute, kAXErrorSuccess, kAXFrontmostAttribute, kAXPressAction,
    kAXRaiseAction, kAXWindowsAttribute, AXError, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementPerformAction, AXUIElementRef,
    AXUIElementSetAttributeValue,
};

// Private ApplicationServices API — maps an AX window element to its
// `CGWindowID`. We use this for precise per-window activation: when the user
// picks the 3rd Cursor window out of ten, we need to raise that specific
// window, not "the app's frontmost".
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn _AXUIElementGetWindow(element: AXUIElementRef, window_id: *mut u32) -> AXError;
    // Resurrects an AX element by its private (pid, ax_id) "remote token".
    // macOS suspends the AX hierarchy of apps whose windows live on other
    // Spaces — `kAXWindowsAttribute` returns an empty array — but the
    // underlying AX element is still addressable through this private
    // entry point, which AltTab uses for its "windowsByBruteForce" path.
    // Returns +1 retained (or null), so caller must `CFRelease` it.
    fn _AXUIElementCreateWithRemoteToken(
        token: core_foundation::data::CFDataRef,
    ) -> AXUIElementRef;
    // Carbon: deprecated but still functional. Maps a unix pid to the
    // ProcessSerialNumber the WindowServer/SkyLight APIs require.
    fn GetProcessForPID(pid: i32, psn: *mut ProcessSerialNumber) -> i32;
}

// Private SkyLight API — the only reliable way for an external process to
// switch macOS to the Space hosting a given window. `AXRaise` alone brings a
// window to the top of *its own* Space without switching the active Space.
// AltTab, Contexts, HyperSwitch, etc. all use this exact pair.
#[link(name = "SkyLight", kind = "framework")]
extern "C" {
    fn _SLPSSetFrontProcessWithOptions(
        psn: *const ProcessSerialNumber,
        wid: u32,
        options: u32,
    ) -> i32;
    fn SLPSPostEventRecordTo(psn: *const ProcessSerialNumber, event_record: *const u8) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessSerialNumber {
    high_long_of_psn: u32,
    low_long_of_psn: u32,
}

// `kCPSUserGenerated` — tells the WindowServer to treat the activation as a
// user-initiated front-most change, which is what triggers the Space switch.
// Value from yabai `window_manager.h` and AltTab `SLPSMode.userGenerated`.
const SLS_USER_GENERATED: u32 = 0x200;

/// SLPS make-key sequence. Byte layout mirrors AltTab `Window.swift#makeKeyWindow`
/// and yabai `window_manager_make_key_window`. The `0xff × 16` block at
/// `0x20..0x30` is NOT optional — without it the WindowServer treats the
/// record as malformed and silently drops it ("window raised but no focus,
/// needs a mouse click to unstick").
unsafe fn cross_space_focus(pid: i32, wid: u32) {
    let mut psn = ProcessSerialNumber::default();
    let err = GetProcessForPID(pid, &mut psn);
    if err != 0 {
        tracing::debug!(pid, err, "GetProcessForPID failed; cross-Space focus skipped");
        return;
    }
    let slps_err = _SLPSSetFrontProcessWithOptions(&psn, wid, SLS_USER_GENERATED);
    let mut bytes = [0u8; 0xf8];
    bytes[0x04] = 0xf8;
    bytes[0x3a] = 0x10;
    bytes[0x20..0x30].fill(0xff);
    let wid_bytes = wid.to_ne_bytes();
    bytes[0x3c..0x40].copy_from_slice(&wid_bytes);
    bytes[0x08] = 0x01;
    let e1 = SLPSPostEventRecordTo(&psn, bytes.as_ptr());
    bytes[0x08] = 0x02;
    let e2 = SLPSPostEventRecordTo(&psn, bytes.as_ptr());
    tracing::debug!(pid, wid, slps_err, e1, e2, "SLPS sequence posted");
}

pub fn activate_window(win: &WindowRef) -> Result<()> {
    unsafe {
        let app_elem = AXUIElementCreateApplication(win.pid);
        if app_elem.is_null() {
            return Err(anyhow!("AXUIElementCreateApplication returned null"));
        }

        let windows_attr = CFString::from_static_string(kAXWindowsAttribute);
        let mut windows_value: *const c_void = std::ptr::null();
        let err: AXError = AXUIElementCopyAttributeValue(
            app_elem,
            windows_attr.as_concrete_TypeRef(),
            &mut windows_value,
        );
        if err != kAXErrorSuccess || windows_value.is_null() {
            return Err(anyhow!(
                "AX windows attribute unavailable (err={err}) — is the Accessibility permission granted?"
            ));
        }

        let windows: CFArray<CFType> = CFArray::wrap_under_create_rule(windows_value as _);
        let raise_action = CFString::from_static_string(kAXRaiseAction);

        // Try to locate the matching AX window element. Cross-Space windows
        // often don't appear in `kAXWindows` (macOS suspends the AX hierarchy
        // of apps whose windows live on other Spaces), so this may legitimately
        // return None — we must NOT fall through to "first window" in that
        // case, because the first element is some sibling on the *current*
        // Space and targeting it for SLPS would no-op (already frontmost on
        // its Space) and hide the real cross-Space behavior.
        let target = find_matching_window(&windows, win);
        tracing::debug!(
            pid = win.pid,
            wid = win.id,
            title = %win.title,
            minimized = win.minimized,
            ax_windows = windows.len(),
            ax_target_found = target.is_some(),
            "activate_window invoked"
        );

        // SLPS — pick the specific CGWindowID and, when the target lives on
        // another Space, kick off the Space switch. SLPS = Dock-icon-click
        // semantics: it raises the window, switches Space, AND restores from
        // the Dock if the window is minimized — all in one round-trip. We
        // intentionally do NOT pre-fire `kAXMinimized=false` ourselves; an
        // earlier version did, and AltTab/yabai don't, and the AX un-minimize
        // started the genie animation on the wrong Space when the window's
        // origin Space != current Space, and on top of that the AXRaise
        // afterwards landed before the animation finished and didn't take.
        //
        // Prefer `win.id` captured at enumeration time over re-deriving via
        // `_AXUIElementGetWindow`, because AX often hides cross-Space windows
        // at activation time. `win.id > u32::MAX` means the enumerator
        // resorted to a synthetic pid-encoded fallback and SLPS can't use it.
        //
        // Do NOT write kAXMain / kAXFocused afterwards: those race the SLPS-
        // driven key-window transition and produce "window forward but
        // keyboard focus still on previous app". AltTab and yabai omit them.
        let wid: Option<u32> = if win.id > 0 && win.id <= u32::MAX as u64 {
            Some(win.id as u32)
        } else {
            target.and_then(|t| ax_window_id(t)).map(|id| id as u32)
        };

        // When `kAXWindowsAttribute` came back empty for the owning app —
        // the cross-Space "AX hierarchy suspended" case (Chess sitting on
        // another Space) — try AltTab's `windowsByBruteForce` workaround.
        // If we recover the matching AX element, the regular SLPS+AXRaise
        // path works exactly like for a normal pick.
        let brute_forced: Option<OwnedAxElem> = if target.is_none() {
            wid.and_then(|w| ax_window_via_remote_token(win.pid, w))
        } else {
            None
        };
        let effective_target = target.or_else(|| brute_forced.as_ref().map(|b| b.0));

        match (effective_target, wid) {
            (Some(t), Some(w)) => {
                // Path A — SLPS + AXRaise. Same flow for normal, cross-Space,
                // minimized-in-Dock, and brute-force-recovered elements.
                cross_space_focus(win.pid, w);
                let err = AXUIElementPerformAction(t, raise_action.as_concrete_TypeRef());
                if err != kAXErrorSuccess {
                    tracing::debug!(err, "AXRaise failed (SLPS already posted)");
                }
            }
            _ => {
                // Path B — no SLPS-targetable handle. Whole-app activation
                // (Dock-icon click without specific window targeting). Last-
                // resort path; should be rare.
                tracing::debug!(
                    pid = win.pid,
                    wid = ?wid,
                    target_found = effective_target.is_some(),
                    "no SLPS handle — using activateWithOptions(.ActivateAllWindows)"
                );
                activate_app_all_windows(win.pid);
            }
        }
        // brute_forced drops → CFRelease.
        drop(brute_forced);
    }
    Ok(())
}

/// Close the given window via Accessibility: locate the AXWindow on the owning
/// process, fetch its `AXCloseButton`, and AX-press it. Mirrors what clicking
/// the red traffic-light dot does, so apps get their normal "save changes?"
/// flow if any.
pub fn close_window(win: &WindowRef) -> Result<()> {
    unsafe {
        let app_elem = AXUIElementCreateApplication(win.pid);
        if app_elem.is_null() {
            return Err(anyhow!("AXUIElementCreateApplication returned null"));
        }

        let windows_attr = CFString::from_static_string(kAXWindowsAttribute);
        let mut windows_value: *const c_void = std::ptr::null();
        let err: AXError = AXUIElementCopyAttributeValue(
            app_elem,
            windows_attr.as_concrete_TypeRef(),
            &mut windows_value,
        );
        if err != kAXErrorSuccess || windows_value.is_null() {
            return Err(anyhow!(
                "AX windows attribute unavailable (err={err}) — is the Accessibility permission granted?"
            ));
        }

        let windows: CFArray<CFType> = CFArray::wrap_under_create_rule(windows_value as _);
        let target = find_matching_window(&windows, win)
            .ok_or_else(|| anyhow!("window not found via AX for pid {}", win.pid))?;

        let btn_attr = CFString::from_static_string(kAXCloseButtonAttribute);
        let mut btn_value: *const c_void = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(
            target,
            btn_attr.as_concrete_TypeRef(),
            &mut btn_value,
        );
        if err != kAXErrorSuccess || btn_value.is_null() {
            return Err(anyhow!("AX close button unavailable (err={err})"));
        }
        let btn: CFType = CFType::wrap_under_create_rule(btn_value as _);
        let btn_elem = btn.as_CFTypeRef() as AXUIElementRef;

        let press_action = CFString::from_static_string(kAXPressAction);
        let err = AXUIElementPerformAction(btn_elem, press_action.as_concrete_TypeRef());
        if err != kAXErrorSuccess {
            return Err(anyhow!("AXUIElementPerformAction(press close) failed (err={err})"));
        }
    }
    Ok(())
}

pub fn activate_app(app: &AppRef) -> Result<()> {
    focus_pid(app.pid).context("activate_app")
}

/// Bring every window of the target app forward, switching Space if needed.
/// Used as the cross-Space fallback when AX has suspended the target's window
/// hierarchy and SLPS-with-specific-wid silently no-ops. AltTab uses the
/// equivalent path (`.activateAllWindows`) for the same reason.
fn activate_app_all_windows(pid: i32) {
    use objc2_app_kit::{NSApplication, NSApplicationActivationOptions, NSRunningApplication};
    use objc2_foundation::MainThreadMarker;
    let Some(running) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
        tracing::debug!(pid, "no NSRunningApplication for activate-all-windows");
        return;
    };
    let was_hidden = running.isHidden();
    let was_active = running.isActive();
    let policy = running.activationPolicy();
    if was_hidden {
        running.unhide();
    }
    // Yield activation explicitly. The switcher panel is opened with
    // `cx.activate(true)` so we currently hold app-active state; macOS 14+
    // silently neuters `activateWithOptions` and SLPS Space switches when
    // they originate from a still-active LSUIElement caller (AltTab avoids
    // this by using a `nonactivatingPanel`, which we can't easily replicate
    // through GPUI). `[NSApp deactivate]` releases that hold so the target's
    // activation actually takes effect.
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).deactivate();
    }
    #[allow(deprecated)]
    let ok = running.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
    tracing::info!(
        pid,
        ok,
        was_hidden,
        was_active,
        ?policy,
        "activateWithOptions(.ActivateAllWindows) after NSApp.deactivate"
    );
}

fn focus_pid(pid: i32) -> Result<()> {
    // Primary path: ask the Accessibility API to make the target app frontmost.
    // This works from an LSUIElement accessory app, whereas
    // `NSRunningApplication::activateWithOptions_` has been unreliable on
    // macOS 14+ for accessory callers (and is deprecated).
    unsafe {
        let app_elem = AXUIElementCreateApplication(pid);
        if !app_elem.is_null() {
            let attr = CFString::from_static_string(kAXFrontmostAttribute);
            let t = CFBoolean::true_value();
            let err = AXUIElementSetAttributeValue(
                app_elem,
                attr.as_concrete_TypeRef(),
                t.as_CFTypeRef(),
            );
            if err == kAXErrorSuccess {
                return Ok(());
            }
            tracing::debug!(pid, err, "AX frontmost failed, falling back to NSRunningApplication");
        }
    }

    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    let running = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .ok_or_else(|| anyhow!("no NSRunningApplication for pid {pid}"))?;
    #[allow(deprecated)]
    let _ok = running.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
    Ok(())
}

unsafe fn find_matching_window(
    windows: &CFArray<CFType>,
    target: &WindowRef,
) -> Option<AXUIElementRef> {
    // Window number match first.
    for i in 0..windows.len() {
        let w = windows.get(i)?;
        let elem = w.as_CFTypeRef() as AXUIElementRef;
        if let Some(id) = ax_window_id(elem) {
            if id == target.id {
                return Some(elem);
            }
        }
    }
    // Fallback: title match.
    if !target.title.is_empty() {
        for i in 0..windows.len() {
            let w = windows.get(i)?;
            let elem = w.as_CFTypeRef() as AXUIElementRef;
            if let Some(title) = ax_window_title(elem) {
                if title == target.title {
                    return Some(elem);
                }
            }
        }
    }
    None
}

/// Owns an `AXUIElementRef` returned with +1 retain (e.g. from
/// `_AXUIElementCreateWithRemoteToken`). Drops via `CFRelease`.
struct OwnedAxElem(AXUIElementRef);

impl Drop for OwnedAxElem {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { core_foundation::base::CFRelease(self.0 as _) };
        }
    }
}

/// Resurrect the AX element for a window that `kAXWindowsAttribute` doesn't
/// surface. macOS suspends the AX hierarchy of apps whose visible windows
/// live on another Space, so the normal lookup returns an empty array even
/// though the window is fully alive (CGWindowList still sees it). AltTab's
/// `windowsByBruteForce` works around this by iterating private AX element
/// IDs and asking the system for each. Returns the matching element on
/// success — the caller wraps it in `OwnedAxElem` for the rest of the
/// activation pass and drops it after.
///
/// Cost: up to 1000 IPC round-trips per call (capped at ~100 ms wall-clock
/// per AltTab's heuristic). Only invoked when the standard path failed —
/// roughly the cross-Space-with-AX-suspended case, which is rare per pick.
///
/// Token layout (20 bytes):
///   [ 0..4]  pid (i32 LE)
///   [ 4..8]  reserved, zero
///   [ 8..12] magic 0x636f636f ("coco")
///   [12..20] AXUIElementID (u64 LE) — opaque counter, starts at 0 per app
unsafe fn ax_window_via_remote_token(pid: i32, target_wid: u32) -> Option<OwnedAxElem> {
    use core_foundation::data::CFData;

    let mut token = [0u8; 20];
    token[0..4].copy_from_slice(&pid.to_ne_bytes());
    let magic: i32 = 0x636f636f;
    token[8..12].copy_from_slice(&magic.to_ne_bytes());

    let started = std::time::Instant::now();
    for ax_id in 0u64..1000 {
        if started.elapsed() > std::time::Duration::from_millis(100) {
            tracing::debug!(
                pid,
                ax_id,
                "remote-token brute-force aborted on timeout"
            );
            return None;
        }
        token[12..20].copy_from_slice(&ax_id.to_ne_bytes());
        let cf = CFData::from_buffer(&token);
        let elem = _AXUIElementCreateWithRemoteToken(cf.as_concrete_TypeRef());
        if elem.is_null() {
            continue;
        }
        let owned = OwnedAxElem(elem);
        let mut wid: u32 = 0;
        let err = _AXUIElementGetWindow(owned.0, &mut wid);
        if err == kAXErrorSuccess && wid == target_wid {
            tracing::debug!(pid, ax_id, wid, "remote-token brute-force matched");
            return Some(owned);
        }
        // owned drops → CFRelease
    }
    tracing::debug!(pid, target_wid, "remote-token brute-force exhausted 1000 ids");
    None
}

unsafe fn ax_window_id(elem: AXUIElementRef) -> Option<u64> {
    let mut id: u32 = 0;
    let err = _AXUIElementGetWindow(elem, &mut id);
    if err == kAXErrorSuccess && id != 0 {
        Some(id as u64)
    } else {
        None
    }
}

unsafe fn ax_window_title(elem: AXUIElementRef) -> Option<String> {
    let attr = CFString::from_static_string("AXTitle");
    let mut value: *const c_void = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(elem, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let s: CFString = CFString::wrap_under_create_rule(value as CFStringRef);
    Some(s.to_string())
}

