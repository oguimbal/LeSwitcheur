//! Window + app enumeration.
//!
//! Window titles are the interesting data, and on macOS 14.4+ `CGWindowList`
//! only returns them to processes with Screen Recording permission. We already
//! require Accessibility (to raise windows), so the primary path uses the
//! Accessibility API to enumerate windows — that gives us titles without a
//! second permission prompt. CGWindowList is kept as a last-resort fallback
//! so the switcher still shows *something* if AX is unavailable.
//!
//! Space filtering: AX `kAXWindows` usually returns windows an app owns
//! across every Space, but macOS can suspend the AX hierarchy of apps
//! living on other Spaces while we're inside a fullscreen Space. The
//! `show_all_spaces` flag drives two decisions: whether to filter AX
//! results against `kCGWindowListOptionOnScreenOnly` (current Space only)
//! and whether the CG supplement pulls from `kCGWindowListOptionAll`
//! (all Spaces) or just the on-screen set.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;

use accessibility_sys::{
    kAXErrorSuccess, kAXMinimizedAttribute, kAXSubroleAttribute, kAXTitleAttribute,
    kAXWindowsAttribute, AXError, AXUIElementCopyAttributeValue, AXUIElementCreateApplication,
    AXUIElementRef,
};
use anyhow::{anyhow, Result};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGWindowListCopyWindowInfo;
use core_graphics::window::{
    kCGNullWindowID, kCGWindowListExcludeDesktopElements, kCGWindowListOptionAll,
    kCGWindowListOptionOnScreenOnly,
};
use switcheur_core::{AppRef, WindowRef};

type CGWindowID = u32;

// Private Accessibility helper. Maps an AX window element to its CGWindowID,
// which is the only stable key we can use to cross-reference AX against
// CGWindowList's on-screen set. Widely used by AltTab, Rectangle, etc.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn _AXUIElementGetWindow(element: AXUIElementRef, window_id: *mut CGWindowID) -> AXError;
}

pub fn list_windows(show_all_spaces: bool) -> Result<Vec<WindowRef>> {
    // Primary: AX gives real titles with only the Accessibility permission.
    // Supplement with CG to catch windows AX didn't surface — most notably
    // windows on another Space when we're currently inside a fullscreen Space,
    // where macOS sometimes suspends the AX hierarchy of apps running
    // elsewhere so `kAXWindows` comes back empty for them. Both paths produce
    // real CGWindowIDs for their entries, so dedup by id is safe.
    let ax_wins = match list_windows_via_ax(show_all_spaces) {
        Ok(ws) => ws,
        Err(e) => {
            tracing::warn!("list_windows via AX failed: {e:#}");
            Vec::new()
        }
    };
    let known_ids: HashSet<u64> = ax_wins.iter().map(|w| w.id).collect();
    // Belt-and-suspenders dedup for *titled* CG entries: AX only falls back
    // to a synthetic id if `_AXUIElementGetWindow` failed (rare now that
    // the private binding is wired up), so id-alone normally suffices —
    // but `(pid, title)` costs nothing and closes any remaining gap.
    let known_pid_title: HashSet<(i32, String)> = ax_wins
        .iter()
        .filter(|w| !w.title.is_empty())
        .map(|w| (w.pid, w.title.clone()))
        .collect();
    let cg_extra = cg_supplement_windows(
        show_all_spaces,
        &known_ids,
        &known_pid_title,
    );
    tracing::debug!(
        ax = ax_wins.len(),
        cg_extra = cg_extra.len(),
        show_all_spaces,
        "list_windows merged"
    );
    let mut out = ax_wins;
    out.extend(cg_extra);
    if out.is_empty() {
        // Nothing from either path — fall back to the raw CG listing so the
        // switcher at least shows something rather than a blank panel.
        return list_windows_via_cg(show_all_spaces);
    }
    Ok(out)
}

/// CG-driven supplement that adds windows AX didn't return. Restricted to
/// windows belonging to "regular" apps so the result doesn't include Dock,
/// SystemUIServer, WindowServer overlays, etc.
fn cg_supplement_windows(
    show_all_spaces: bool,
    already_have: &HashSet<u64>,
    already_have_pid_title: &HashSet<(i32, String)>,
) -> Vec<WindowRef> {
    let apps_by_pid: HashMap<i32, AppRef> = list_apps()
        .unwrap_or_default()
        .into_iter()
        .map(|a| (a.pid, a))
        .collect();

    let base = if show_all_spaces {
        kCGWindowListOptionAll
    } else {
        kCGWindowListOptionOnScreenOnly
    };
    let options = base | kCGWindowListExcludeDesktopElements;
    let raw = unsafe { CGWindowListCopyWindowInfo(options, kCGNullWindowID) };
    if raw.is_null() {
        return Vec::new();
    }
    let cf_array: CFArray<CFDictionary> = unsafe { CFArray::wrap_under_create_rule(raw) };

    // Titled CG entries go out eagerly, deduped against AX and against
    // each other. Untitled CG entries are the fallback for apps AX knows
    // nothing about (typically cross-Space windows of non-Chromium apps
    // without Screen Recording). Each untitled entry is its own row —
    // collapsing per pid would hide the fact that the user has e.g. ten
    // Cursor windows open and pick the wrong target on activation.
    let mut titled_out: Vec<WindowRef> = Vec::new();
    let mut titled_seen: HashSet<(i32, String)> = HashSet::new();
    let mut untitled_out: Vec<WindowRef> = Vec::new();

    for dict in cf_array.iter() {
        let dict: &CFDictionary = &dict;
        let layer = cg_get_i64(dict, "kCGWindowLayer").unwrap_or(-1);
        if layer != 0 {
            continue;
        }
        let Some(pid) = cg_get_i64(dict, "kCGWindowOwnerPID") else {
            continue;
        };
        let Some(cg_id) = cg_get_i64(dict, "kCGWindowNumber") else {
            continue;
        };
        if cg_id < 0 {
            continue;
        }
        let id = cg_id as u64;
        let pid = pid as i32;
        if already_have.contains(&id) {
            continue;
        }
        let Some(app) = apps_by_pid.get(&pid) else {
            // Skip non-regular apps: menubar helpers, Dock, etc.
            continue;
        };
        // Only accept windows that are *not* on-screen right now — those
        // are the ones AX couldn't surface (other-Space windows while
        // we're inside a fullscreen Space). On-screen layer-0 windows
        // the AX path already handled with its subrole filter.
        let on_screen = cg_get_bool(dict, "kCGWindowIsOnscreen").unwrap_or(false);
        if on_screen {
            continue;
        }
        let alpha = cg_get_f64(dict, "kCGWindowAlpha").unwrap_or(1.0);
        if alpha < 0.01 {
            continue;
        }
        let (w, h) = cg_get_bounds_size(dict).unwrap_or((0.0, 0.0));
        if w < 50.0 || h < 50.0 {
            continue;
        }
        let title = cg_get_string(dict, "kCGWindowName").unwrap_or_default();

        if !title.is_empty() {
            if already_have_pid_title.contains(&(pid, title.clone())) {
                continue;
            }
            if !titled_seen.insert((pid, title.clone())) {
                continue;
            }
            titled_out.push(WindowRef {
                id,
                pid,
                title,
                app_name: app.name.clone(),
                bundle_id: app.bundle_id.clone(),
                icon_path: app.icon_path.clone(),
                // CG doesn't reliably distinguish minimized from other-Space.
                minimized: false,
            });
        } else {
            // Untitled entries: the CG-only view of other-Space windows
            // when Screen Recording isn't granted (14.4+ withholds the
            // title). The `already_have` id dedup above already rejects
            // windows AX covered — and since `_AXUIElementGetWindow`
            // gives AX entries real CGWindowIDs, that dedup is
            // authoritative even for cross-Space apps that also have
            // a current-Space window. We just need strict noise
            // filters: real user windows are near-opaque and a
            // reasonable size; layer-0 scratch buffers fail at least
            // one of these.
            if alpha < 0.9 {
                continue;
            }
            if w < 200.0 || h < 150.0 {
                continue;
            }
            untitled_out.push(WindowRef {
                id,
                pid,
                title: String::new(),
                app_name: app.name.clone(),
                bundle_id: app.bundle_id.clone(),
                icon_path: app.icon_path.clone(),
                minimized: false,
            });
        }
    }

    let mut out = titled_out;
    out.extend(untitled_out);
    out
}

fn list_windows_via_ax(show_all_spaces: bool) -> Result<Vec<WindowRef>> {
    let apps = list_apps()?;
    // Only compute the on-screen CGWindowID set when we actually need to
    // filter — the CG call isn't free, and the unrestricted path doesn't
    // care about the result.
    let on_screen: Option<HashSet<CGWindowID>> = if show_all_spaces {
        None
    } else {
        Some(current_space_window_ids())
    };
    let mut out = Vec::with_capacity(apps.len() * 2);
    for app in apps {
        match ax_windows_for(&app, on_screen.as_ref()) {
            Ok(mut windows) => out.append(&mut windows),
            Err(e) => tracing::debug!(pid = app.pid, "no AX windows: {e:#}"),
        }
    }
    Ok(out)
}

// Real user-facing windows report one of these subroles. Finder's hidden
// desktop window (and similar accessory windows) either have no subrole or
// an unrelated one — activating them is a no-op, so drop them.
const ALLOWED_SUBROLES: &[&str] = &[
    "AXStandardWindow",
    "AXDialog",
    "AXFloatingWindow",
    "AXSystemDialog",
    "AXSystemFloatingWindow",
];

fn ax_windows_for(
    app: &AppRef,
    on_screen: Option<&HashSet<CGWindowID>>,
) -> Result<Vec<WindowRef>> {
    let mut out = Vec::new();
    unsafe {
        let elem = AXUIElementCreateApplication(app.pid);
        if elem.is_null() {
            return Err(anyhow!("AXUIElementCreateApplication returned null"));
        }
        let Some(windows) = ax_copy_array(elem, kAXWindowsAttribute) else {
            return Ok(out);
        };
        for i in 0..windows.len() {
            let Some(w) = windows.get(i) else { continue };
            let wref = w.as_CFTypeRef() as AXUIElementRef;
            let subrole = ax_copy_string(wref, kAXSubroleAttribute);
            let title = ax_copy_string(wref, kAXTitleAttribute).unwrap_or_default();
            let keep = match subrole.as_deref() {
                Some(s) => ALLOWED_SUBROLES.contains(&s),
                None => !title.is_empty(),
            };
            if !keep {
                continue;
            }
            let minimized = matches!(ax_copy_bool(wref, kAXMinimizedAttribute), Some(true));
            let cg_id = ax_window_id(wref);
            if let Some(on_screen) = on_screen {
                // Current-desktop filter: keep minimized windows (they belong
                // to the current Space conceptually) and anything whose
                // CGWindowID is in the on-screen set. Windows whose id we
                // can't resolve drop out — better than showing off-Space
                // windows when the user asked for the current-only view.
                let keep_space = minimized
                    || cg_id.map_or(false, |id| on_screen.contains(&id));
                if !keep_space {
                    continue;
                }
            }
            out.push(WindowRef {
                // Prefer the real CGWindowID; fall back to the pre-existing
                // synthetic `(pid << 32) | idx` scheme when AX refuses to
                // hand it out. Activation matches on (pid, title) anyway.
                id: cg_id
                    .map(|id| id as u64)
                    .unwrap_or_else(|| ((app.pid as u64) << 32) | (i as u64)),
                pid: app.pid,
                title,
                app_name: app.name.clone(),
                bundle_id: app.bundle_id.clone(),
                icon_path: app.icon_path.clone(),
                minimized,
            });
        }
    }
    Ok(out)
}

unsafe fn ax_window_id(elem: AXUIElementRef) -> Option<CGWindowID> {
    let mut id: CGWindowID = 0;
    let err = _AXUIElementGetWindow(elem, &mut id);
    if err == kAXErrorSuccess && id != 0 {
        Some(id)
    } else {
        None
    }
}

/// CGWindowIDs of on-screen windows — i.e. windows on the user's active Space.
/// Empty on failure so the caller's filter simply keeps nothing (matching the
/// "restrict" intent rather than silently falling back to all Spaces).
fn current_space_window_ids() -> HashSet<CGWindowID> {
    let options = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
    let raw = unsafe { CGWindowListCopyWindowInfo(options, kCGNullWindowID) };
    if raw.is_null() {
        tracing::warn!("CGWindowListCopyWindowInfo returned null while computing Space filter");
        return HashSet::new();
    }
    let cf_array: CFArray<CFDictionary> = unsafe { CFArray::wrap_under_create_rule(raw) };
    let mut ids = HashSet::with_capacity(cf_array.len() as usize);
    for dict in cf_array.iter() {
        let dict: &CFDictionary = &dict;
        if let Some(id) = cg_get_i64(dict, "kCGWindowNumber") {
            if id >= 0 {
                ids.insert(id as CGWindowID);
            }
        }
    }
    ids
}

fn list_windows_via_cg(show_all_spaces: bool) -> Result<Vec<WindowRef>> {
    // On-screen-only for the restricted view; otherwise grab everything CG
    // knows about (including off-screen / other-Space windows).
    let base = if show_all_spaces {
        kCGWindowListOptionAll
    } else {
        kCGWindowListOptionOnScreenOnly
    };
    let options = base | kCGWindowListExcludeDesktopElements;
    let raw = unsafe { CGWindowListCopyWindowInfo(options, kCGNullWindowID) };
    if raw.is_null() {
        return Err(anyhow!("CGWindowListCopyWindowInfo returned null"));
    }
    let cf_array: CFArray<CFDictionary> = unsafe { CFArray::wrap_under_create_rule(raw) };

    let mut out = Vec::with_capacity(cf_array.len() as usize);
    for dict in cf_array.iter() {
        let dict: &CFDictionary = &dict;
        let layer = cg_get_i64(dict, "kCGWindowLayer").unwrap_or(-1);
        if layer != 0 {
            continue;
        }
        let Some(pid) = cg_get_i64(dict, "kCGWindowOwnerPID") else {
            continue;
        };
        let Some(id) = cg_get_i64(dict, "kCGWindowNumber") else {
            continue;
        };
        let Some(app_name) = cg_get_string(dict, "kCGWindowOwnerName") else {
            continue;
        };
        let title = cg_get_string(dict, "kCGWindowName").unwrap_or_default();

        out.push(WindowRef {
            id: id as u64,
            pid: pid as i32,
            title,
            app_name,
            bundle_id: None,
            icon_path: None,
            minimized: false,
        });
    }

    Ok(out)
}

pub fn list_apps() -> Result<Vec<AppRef>> {
    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};

    let mut out = Vec::new();
    let ws = NSWorkspace::sharedWorkspace();
    let running = ws.runningApplications();
    for i in 0..running.count() {
        let app = running.objectAtIndex(i);
        let name = app.localizedName().map(|s| s.to_string()).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            continue;
        }
        let bundle_id = app.bundleIdentifier().map(|s| s.to_string());
        let pid = app.processIdentifier();
        let bundle_path = app
            .bundleURL()
            .and_then(|u| u.path())
            .map(|s| s.to_string());
        let icon_key = bundle_id.clone().unwrap_or_else(|| format!("pid-{pid}"));
        let icon_path = bundle_path
            .as_deref()
            .and_then(|p| super::icons::icon_for_bundle(p, &icon_key));
        out.push(AppRef {
            pid,
            name,
            bundle_id,
            icon_path,
        });
    }
    Ok(out)
}

// --- AX attribute helpers ---

unsafe fn ax_copy_raw(elem: AXUIElementRef, attr: &str) -> Option<*const c_void> {
    let attr_cf = CFString::new(attr);
    let mut value: *const c_void = std::ptr::null();
    let err: AXError =
        AXUIElementCopyAttributeValue(elem, attr_cf.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    Some(value)
}

unsafe fn ax_copy_string(elem: AXUIElementRef, attr: &str) -> Option<String> {
    let raw = ax_copy_raw(elem, attr)?;
    let s: CFString = CFString::wrap_under_create_rule(raw as CFStringRef);
    Some(s.to_string())
}

unsafe fn ax_copy_bool(elem: AXUIElementRef, attr: &str) -> Option<bool> {
    let raw = ax_copy_raw(elem, attr)?;
    let b: CFBoolean = CFBoolean::wrap_under_create_rule(raw as _);
    Some(b.into())
}

unsafe fn ax_copy_array(elem: AXUIElementRef, attr: &str) -> Option<CFArray<CFType>> {
    let raw = ax_copy_raw(elem, attr)?;
    Some(CFArray::wrap_under_create_rule(raw as _))
}

// --- CG dict helpers ---

fn cg_get_i64(dict: &CFDictionary, key: &str) -> Option<i64> {
    let cf_key = CFString::new(key);
    let value = dict.find(cf_key.as_CFTypeRef() as *const _)?;
    let number: CFNumber = unsafe { CFNumber::wrap_under_get_rule(*value as *const _) };
    number.to_i64()
}

fn cg_get_string(dict: &CFDictionary, key: &str) -> Option<String> {
    let cf_key = CFString::new(key);
    let value = dict.find(cf_key.as_CFTypeRef() as *const _)?;
    let s: CFString = unsafe { CFString::wrap_under_get_rule(*value as *const _) };
    Some(s.to_string())
}

fn cg_get_f64(dict: &CFDictionary, key: &str) -> Option<f64> {
    let cf_key = CFString::new(key);
    let value = dict.find(cf_key.as_CFTypeRef() as *const _)?;
    let number: CFNumber = unsafe { CFNumber::wrap_under_get_rule(*value as *const _) };
    number.to_f64()
}

fn cg_get_bool(dict: &CFDictionary, key: &str) -> Option<bool> {
    let cf_key = CFString::new(key);
    let value = dict.find(cf_key.as_CFTypeRef() as *const _)?;
    let b: CFBoolean = unsafe { CFBoolean::wrap_under_get_rule(*value as *const _) };
    Some(b.into())
}

/// Pull `(width, height)` out of CG's `kCGWindowBounds` dict (the `X`, `Y`,
/// `Width`, `Height` floats packed inside). Returns `None` if the key is
/// missing or malformed.
fn cg_get_bounds_size(dict: &CFDictionary) -> Option<(f64, f64)> {
    let cf_key = CFString::new("kCGWindowBounds");
    let value = dict.find(cf_key.as_CFTypeRef() as *const _)?;
    let bounds: CFDictionary = unsafe { CFDictionary::wrap_under_get_rule(*value as *const _) };
    let w = cg_get_f64(&bounds, "Width")?;
    let h = cg_get_f64(&bounds, "Height")?;
    Some((w, h))
}
