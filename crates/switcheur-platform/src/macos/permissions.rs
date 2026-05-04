//! Accessibility permission gate.
//!
//! Listing the windows of *other* apps with CGWindowList works without
//! Accessibility, but raising a specific window does not. We prompt on first
//! launch and return whether we're trusted so the app can decide how to behave.

use accessibility_sys::{kAXTrustedCheckOptionPrompt, AXIsProcessTrustedWithOptions};
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;

/// Returns true if this process is trusted by the Accessibility API.
/// When `prompt` is true and the process isn't yet trusted, macOS shows the
/// system dialog that lets the user grant access.
///
/// NOTE: passing `prompt=true` also has the side-effect of refreshing the
/// per-process TCC cache — `prompt=false` reads a cache that does not always
/// pick up grants made while the process is running. Callers polling the
/// trust state from a running process should pass `prompt=true`; macOS only
/// shows the dialog once per TCC decision, so subsequent calls are silent.
pub fn ensure_accessibility(prompt: bool) -> bool {
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let value: CFType = if prompt {
            CFBoolean::true_value().as_CFType()
        } else {
            CFBoolean::false_value().as_CFType()
        };
        let opts = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value)]);
        AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef())
    }
}

/// Trigger the macOS Accessibility prompt without opening System Settings.
/// Used by the onboarding wizard so the user sees a single, native dialog
/// instead of "prompt + Settings window" simultaneously. If the prompt has
/// already been dismissed previously (denied state), this is a no-op — the
/// onboarding step's polling loop is what eventually catches the toggle.
pub fn request_accessibility_prompt() -> bool {
    ensure_accessibility(true)
}

/// Opens System Settings → Privacy & Security → Input Monitoring so the user
/// can toggle this app on. Required by `CGEventTap` on macOS 10.15+.
///
/// Side-effect: also calls `IOHIDRequestAccess` first, which is what triggers
/// the *native* macOS Input Monitoring prompt the very first time the app
/// asks. After the user has answered once (allow or deny), this is a no-op
/// and the open-pane fallback is what gets the user to the toggle.
pub fn prompt_input_monitoring() {
    unsafe {
        let _granted = IOHIDRequestAccess(K_IO_HID_REQUEST_TYPE_LISTEN_EVENT);
    }
    let url = "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent";
    if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
        tracing::warn!("failed to open Input Monitoring pane: {e:#}");
    }
}

/// Non-prompting check: has the user granted Input Monitoring to this bundle?
/// Used by the boot path and the Settings polling loop to decide between the
/// Carbon hotkey path and the CGEventTap path for system-reserved combos.
pub fn has_input_monitoring_permission() -> bool {
    let access = unsafe { IOHIDCheckAccess(K_IO_HID_REQUEST_TYPE_LISTEN_EVENT) };
    let granted = access == K_IO_HID_ACCESS_TYPE_GRANTED;
    tracing::debug!(access, granted, "IOHIDCheckAccess(ListenEvent)");
    granted
}

const K_IO_HID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;
const K_IO_HID_ACCESS_TYPE_GRANTED: u32 = 0;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDCheckAccess(request_type: u32) -> u32;
    fn IOHIDRequestAccess(request_type: u32) -> bool;
}

/// Opens System Settings → Privacy & Security → Accessibility. macOS only
/// shows the `kAXTrustedCheckOptionPrompt` dialog once per TCC decision —
/// if the user previously denied, the next `AXIsProcessTrustedWithOptions`
/// silently returns false. Pairing the call with opening the settings pane
/// means the user always has a clear path to flip the toggle.
pub fn prompt_accessibility() {
    let _ = ensure_accessibility(true);
    let url = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
    if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
        tracing::warn!("failed to open Accessibility pane: {e:#}");
    }
}

// Screen Recording (a.k.a. Screen Capture) gate. Needed so CGWindowList
// returns real `kCGWindowName` titles for windows owned by other processes
// on macOS 14.4+ — we read those titles to surface cross-Space windows in
// the switcher. Neither `core-graphics` 0.25 nor `accessibility-sys`
// expose these functions, so we bind them directly.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Non-prompting check: has the user granted Screen Recording to this
/// bundle? Cheap — call it before reading `kCGWindowName` for other
/// processes.
pub fn has_screen_recording_permission() -> bool {
    let granted = unsafe { CGPreflightScreenCaptureAccess() };
    tracing::debug!(granted, "CGPreflightScreenCaptureAccess");
    granted
}

/// Triggers the native macOS Screen Recording dialog on first call; on
/// subsequent calls it just returns the cached decision (macOS will not
/// re-show the prompt once the user has chosen). When the call returns
/// false we also open System Settings → Privacy → Screen Recording so the
/// user has a path to flip the toggle even after a previous denial. The
/// settings-window polling loop catches the grant once it lands.
pub fn request_screen_recording_permission() -> bool {
    let granted = unsafe { CGRequestScreenCaptureAccess() };
    if !granted {
        open_screen_recording_settings();
    }
    granted
}

/// Opens System Settings → Privacy & Security → Screen Recording.
pub fn open_screen_recording_settings() {
    let url = "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
    if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
        tracing::warn!("failed to open Screen Recording pane: {e:#}");
    }
}
