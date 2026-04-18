//! Raise a specific window or focus an app via the Accessibility API.
//!
//! AX is the only way to bring a *specific* window of another app forward on
//! modern macOS — `NSRunningApplication::activate` only brings the app's
//! frontmost window. This file goes further: it enumerates AX windows of the
//! target process and performs the `raise` action on the matching one.

use anyhow::{anyhow, Context, Result};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use std::ffi::c_void;
use switcheur_core::{AppRef, WindowRef};

use accessibility_sys::{
    kAXCloseButtonAttribute, kAXErrorSuccess, kAXFocusedAttribute, kAXFrontmostAttribute,
    kAXMainAttribute, kAXMinimizedAttribute, kAXPressAction, kAXRaiseAction, kAXWindowsAttribute,
    AXError, AXUIElementCopyAttributeValue, AXUIElementCreateApplication,
    AXUIElementPerformAction, AXUIElementRef, AXUIElementSetAttributeValue,
};

// Private ApplicationServices API — maps an AX window element to its
// `CGWindowID`. We use this for precise per-window activation: when the user
// picks the 3rd Cursor window out of ten, we need to raise that specific
// window, not "the app's frontmost".
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn _AXUIElementGetWindow(element: AXUIElementRef, window_id: *mut u32) -> AXError;
}

pub fn activate_window(win: &WindowRef) -> Result<()> {
    // First, bring the owning app forward. This is necessary or raising a
    // single window has no visible effect when the whole app is hidden.
    focus_pid(win.pid)?;

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

        // Strategy: prefer matching AXWindow by CGWindowID if we can read it,
        // otherwise fall back to title match, otherwise raise the first window.
        let target = find_matching_window(&windows, win).unwrap_or_else(|| {
            if windows.len() > 0 {
                let first = windows.get(0).unwrap();
                first.as_CFTypeRef() as AXUIElementRef
            } else {
                std::ptr::null_mut()
            }
        });

        if target.is_null() {
            return Err(anyhow!("no AXWindows for pid {}", win.pid));
        }

        // Un-minimize first if needed — AX raise on a minimized window does
        // nothing because the window has no on-screen position.
        if win.minimized {
            let min_attr = CFString::from_static_string(kAXMinimizedAttribute);
            let f = CFBoolean::false_value();
            let err = AXUIElementSetAttributeValue(
                target,
                min_attr.as_concrete_TypeRef(),
                f.as_CFTypeRef(),
            );
            if err != kAXErrorSuccess {
                tracing::warn!("AX un-minimize failed err={err}");
            }
        }

        let err = AXUIElementPerformAction(target, raise_action.as_concrete_TypeRef());
        if err != kAXErrorSuccess {
            return Err(anyhow!("AXUIElementPerformAction(raise) failed (err={err})"));
        }

        // Raise brings the window forward, but keyboard focus can still sit on
        // the previous app/window. Mark the target as main+focused so the OS
        // routes keystrokes to it immediately — avoids needing a click.
        let t = CFBoolean::true_value();
        let main_attr = CFString::from_static_string(kAXMainAttribute);
        let err = AXUIElementSetAttributeValue(
            target,
            main_attr.as_concrete_TypeRef(),
            t.as_CFTypeRef(),
        );
        if err != kAXErrorSuccess {
            tracing::debug!(err, "AX set main failed");
        }
        let focused_attr = CFString::from_static_string(kAXFocusedAttribute);
        let err = AXUIElementSetAttributeValue(
            target,
            focused_attr.as_concrete_TypeRef(),
            t.as_CFTypeRef(),
        );
        if err != kAXErrorSuccess {
            tracing::debug!(err, "AX set focused failed");
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

// Silence an unused-import warning if CFNumber is only used conditionally.
#[allow(dead_code)]
fn _keep_import(_: Option<CFNumber>) {}
