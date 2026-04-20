//! Raise a specific window or focus an app — cross-Space, from fullscreen,
//! through the Sonoma+ activation lock-down.
//!
//! Three cooperating layers are required; removing any one breaks a real
//! use case:
//!
//! 1. `NSRunningApplication::activateFromApplication:options:` — the modern
//!    yield-based activation. It's the only call that crosses the Dock's
//!    "universal owner" gate to switch Spaces (incl. leaving a fullscreen
//!    Space) and the only one that isn't silently neutered by the macOS 14+
//!    "caller must hold activation" rule. Pre-req: we must hold activation,
//!    which is why the switcher panel opens with `cx.activate(true)`.
//! 2. SLPS sequence (`_SLPSSetFrontProcessWithOptions` + two "makeKey"
//!    event records) — picks the specific `CGWindowID` inside the target
//!    app's window stack. Needed whenever the user picks window N-of-M.
//! 3. `AXUIElementPerformAction(kAXRaiseAction)` — final same-Space
//!    z-order nudge when we have the AX element. Skipped when AX doesn't
//!    surface the window (common for cross-Space targets during a
//!    fullscreen Space).

use anyhow::{anyhow, Context, Result};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::string::{CFString, CFStringRef};
use std::ffi::c_void;
use switcheur_core::{AppRef, WindowRef};

use accessibility_sys::{
    kAXCloseButtonAttribute, kAXErrorSuccess, kAXFrontmostAttribute, kAXMinimizedAttribute,
    kAXPressAction, kAXRaiseAction, kAXWindowsAttribute, AXError, AXUIElementCopyAttributeValue,
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

        // Step 1 (see module doc): yield-based activation. Returns `ok=false`
        // if we don't currently hold activation — that's the signal that the
        // switcher panel wasn't opened with `cx.activate(true)`, or the panel
        // was already closed before we got here.
        //
        // Pass empty options (NOT `ActivateAllWindows`): we target one specific
        // window and let SLPS raise it below. With `ActivateAllWindows`, every
        // window of the app is lifted above other apps' windows, so a user
        // with 10 VSCode windows ends up with all 10 stacked above the
        // previous app — breaking the "alt-tab back" flow.
        {
            use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
            if let Some(running) =
                NSRunningApplication::runningApplicationWithProcessIdentifier(win.pid)
            {
                let us = NSRunningApplication::currentApplication();
                let ok = running.activateFromApplication_options(
                    &us,
                    NSApplicationActivationOptions::empty(),
                );
                tracing::debug!(pid = win.pid, ok, "activateFromApplication");
            } else {
                tracing::debug!(pid = win.pid, "no NSRunningApplication for activate-from");
            }
        }

        // Un-minimize BEFORE the SLPS/raise dance. Only works if we have the
        // real AX window element — skip silently otherwise (the target isn't
        // actually minimized from AX's perspective if AX can't see it).
        if win.minimized {
            if let Some(t) = target {
                let min_attr = CFString::from_static_string(kAXMinimizedAttribute);
                let f = CFBoolean::false_value();
                let err = AXUIElementSetAttributeValue(
                    t,
                    min_attr.as_concrete_TypeRef(),
                    f.as_CFTypeRef(),
                );
                if err != kAXErrorSuccess {
                    tracing::warn!("AX un-minimize failed err={err}");
                }
            }
        }

        // Step 2 (see module doc): SLPS — pick the specific CGWindowID. We
        // prefer `win.id` captured at enumeration time over re-deriving via
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
        match wid {
            Some(wid) => cross_space_focus(win.pid, wid),
            None => {
                tracing::debug!(pid = win.pid, "no CGWindowID available, falling back to app-level focus");
                focus_pid(win.pid)?;
            }
        }

        // Step 3 (see module doc): AXRaise — skipped when AX element isn't
        // surfaced, which is normal for cross-Space targets.
        if let Some(t) = target {
            let err = AXUIElementPerformAction(t, raise_action.as_concrete_TypeRef());
            if err != kAXErrorSuccess {
                tracing::debug!(err, "AXRaise failed (SLPS path should still work)");
            }
        } else {
            tracing::debug!(
                pid = win.pid,
                wid = win.id,
                "AX element not in kAXWindows — relying on SLPS-only path"
            );
        }
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

