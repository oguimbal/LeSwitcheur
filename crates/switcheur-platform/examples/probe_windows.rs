//! Window enumeration probe. Dumps AX + CG view of every running regular
//! app so we can see what distinguishes a real window from a ghost one.
//!
//! Run:
//!   cargo run -p switcheur-platform --example probe_windows
//!
//! Filter to one app:
//!   PROBE_APP=Keychain cargo run -p switcheur-platform --example probe_windows
//!
//! Needs Accessibility permission like the main binary.

use std::collections::HashMap;
use std::ffi::c_void;

use accessibility_sys::{
    kAXErrorSuccess, kAXMinimizedAttribute, kAXPositionAttribute, kAXSizeAttribute,
    kAXSubroleAttribute, kAXTitleAttribute, kAXValueTypeCGPoint, kAXValueTypeCGSize,
    kAXWindowsAttribute, AXError, AXUIElementCopyAttributeValue, AXUIElementCreateApplication,
    AXUIElementRef, AXValueGetType, AXValueGetValue, AXValueRef,
};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGWindowListCopyWindowInfo;
use core_graphics::window::{
    kCGNullWindowID, kCGWindowListExcludeDesktopElements, kCGWindowListOptionAll,
};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn _AXUIElementGetWindow(element: AXUIElementRef, window_id: *mut u32) -> AXError;
}

#[link(name = "SkyLight", kind = "framework")]
extern "C" {
    fn SLSMainConnectionID() -> i32;
    fn SLSCopySpacesForWindows(
        cid: i32,
        mask: u32,
        windows: core_foundation::array::CFArrayRef,
    ) -> core_foundation::array::CFArrayRef;
}

fn spaces_for_window(cg_id: u32) -> usize {
    unsafe {
        let cid = SLSMainConnectionID();
        let num = CFNumber::from(cg_id as i64);
        let arr: CFArray<CFNumber> = CFArray::from_CFTypes(&[num]);
        let spaces_ref = SLSCopySpacesForWindows(cid, 0x7, arr.as_concrete_TypeRef());
        if spaces_ref.is_null() {
            return 0;
        }
        let spaces: CFArray<CFType> = CFArray::wrap_under_create_rule(spaces_ref);
        spaces.len() as usize
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

fn main() {
    let filter = std::env::var("PROBE_APP").ok().map(|s| s.to_lowercase());

    let apps = list_regular_apps();
    let cg_by_pid = build_cg_index();

    for app in &apps {
        if let Some(f) = &filter {
            if !app.name.to_lowercase().contains(f) {
                continue;
            }
        }
        println!("\n=== {} (pid={}) ===", app.name, app.pid);

        let ax_windows = ax_dump(app.pid);
        if ax_windows.is_empty() {
            println!("  AX: (no windows)");
        } else {
            println!("  AX: {} window(s)", ax_windows.len());
            for (i, w) in ax_windows.iter().enumerate() {
                let spaces = w.cg_id.map(spaces_for_window);
                println!(
                    "    [{i}] subrole={:<20} title={:?}\n        min={}  cg_id={}  spaces={}  pos={:?}  size={:?}",
                    w.subrole.as_deref().unwrap_or("<none>"),
                    w.title,
                    w.minimized,
                    w.cg_id.map(|id| id.to_string()).unwrap_or_else(|| "<none>".to_string()),
                    spaces.map(|n| n.to_string()).unwrap_or_else(|| "<no cg_id>".to_string()),
                    w.position,
                    w.size,
                );
            }
        }

        let cg_wins: Vec<&CgWin> = cg_by_pid.get(&app.pid).map(|v| v.iter().collect()).unwrap_or_default();
        if cg_wins.is_empty() {
            println!("  CG: (no layer-0 windows)");
        } else {
            println!("  CG: {} layer-0 window(s)", cg_wins.len());
            for (i, c) in cg_wins.iter().enumerate() {
                println!(
                    "    [{i}] id={:<8} layer={:<3} title={:?}\n        alpha={:<5} onscreen={:<5} spaces={} bounds=({:.0}x{:.0} @ {:.0},{:.0})  sharing={} store={}",
                    c.id,
                    c.layer,
                    c.title,
                    c.alpha,
                    c.on_screen,
                    spaces_for_window(c.id),
                    c.bounds.width,
                    c.bounds.height,
                    c.bounds.x,
                    c.bounds.y,
                    c.sharing_state
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "<none>".to_string()),
                    c.store_type
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "<none>".to_string()),
                );
            }
        }
    }
}

struct AppInfo {
    pid: i32,
    name: String,
}

fn list_regular_apps() -> Vec<AppInfo> {
    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
    let mut out = Vec::new();
    let ws = NSWorkspace::sharedWorkspace();
    let running = ws.runningApplications();
    for i in 0..running.count() {
        let app = running.objectAtIndex(i);
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            continue;
        }
        let name = app
            .localizedName()
            .map(|s| s.to_string())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        out.push(AppInfo {
            pid: app.processIdentifier(),
            name,
        });
    }
    out
}

#[derive(Debug)]
struct AxWin {
    subrole: Option<String>,
    title: String,
    minimized: bool,
    cg_id: Option<u32>,
    position: Option<(f64, f64)>,
    size: Option<(f64, f64)>,
}

fn ax_dump(pid: i32) -> Vec<AxWin> {
    let mut out = Vec::new();
    unsafe {
        let app_elem = AXUIElementCreateApplication(pid);
        if app_elem.is_null() {
            return out;
        }
        let Some(windows) = ax_copy_array(app_elem, kAXWindowsAttribute) else {
            return out;
        };
        for i in 0..windows.len() {
            let Some(w) = windows.get(i) else { continue };
            let wref = w.as_CFTypeRef() as AXUIElementRef;
            let subrole = ax_copy_string(wref, kAXSubroleAttribute);
            let title = ax_copy_string(wref, kAXTitleAttribute).unwrap_or_default();
            let minimized = matches!(ax_copy_bool(wref, kAXMinimizedAttribute), Some(true));
            let mut cg_id: u32 = 0;
            let err = _AXUIElementGetWindow(wref, &mut cg_id);
            let cg_id = if err == kAXErrorSuccess && cg_id != 0 {
                Some(cg_id)
            } else {
                None
            };
            let position = ax_copy_point(wref, kAXPositionAttribute).map(|p| (p.x, p.y));
            let size = ax_copy_size(wref, kAXSizeAttribute).map(|s| (s.width, s.height));
            out.push(AxWin {
                subrole,
                title,
                minimized,
                cg_id,
                position,
                size,
            });
        }
    }
    out
}

#[derive(Debug)]
struct CgWin {
    id: u32,
    pid: i32,
    layer: i64,
    title: String,
    alpha: f64,
    on_screen: bool,
    bounds: Bounds,
    sharing_state: Option<i64>,
    store_type: Option<i64>,
}

#[derive(Debug, Default, Clone, Copy)]
struct Bounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn build_cg_index() -> HashMap<i32, Vec<CgWin>> {
    let mut out: HashMap<i32, Vec<CgWin>> = HashMap::new();
    let options = kCGWindowListOptionAll | kCGWindowListExcludeDesktopElements;
    let raw = unsafe { CGWindowListCopyWindowInfo(options, kCGNullWindowID) };
    if raw.is_null() {
        return out;
    }
    let cf_array: CFArray<CFDictionary> = unsafe { CFArray::wrap_under_create_rule(raw) };
    for dict in cf_array.iter() {
        let dict: &CFDictionary = &dict;
        let layer = cg_get_i64(dict, "kCGWindowLayer").unwrap_or(-1);
        // Keep everything layer-0 + layer-near-0 so we see what might bleed in.
        if layer > 3 {
            continue;
        }
        let Some(pid) = cg_get_i64(dict, "kCGWindowOwnerPID") else {
            continue;
        };
        let Some(id) = cg_get_i64(dict, "kCGWindowNumber") else {
            continue;
        };
        if id < 0 {
            continue;
        }
        let title = cg_get_string(dict, "kCGWindowName").unwrap_or_default();
        let alpha = cg_get_f64(dict, "kCGWindowAlpha").unwrap_or(1.0);
        let on_screen = cg_get_bool(dict, "kCGWindowIsOnscreen").unwrap_or(false);
        let bounds = cg_get_bounds(dict).unwrap_or_default();
        let sharing_state = cg_get_i64(dict, "kCGWindowSharingState");
        let store_type = cg_get_i64(dict, "kCGWindowStoreType");
        out.entry(pid as i32).or_default().push(CgWin {
            id: id as u32,
            pid: pid as i32,
            layer,
            title,
            alpha,
            on_screen,
            bounds,
            sharing_state,
            store_type,
        });
    }
    out
}

// --- AX helpers ---

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

unsafe fn ax_copy_point(elem: AXUIElementRef, attr: &str) -> Option<CGPoint> {
    let raw = ax_copy_raw(elem, attr)?;
    let axv = raw as AXValueRef;
    if AXValueGetType(axv) != kAXValueTypeCGPoint {
        return None;
    }
    let mut p = CGPoint::default();
    if !AXValueGetValue(axv, kAXValueTypeCGPoint, &mut p as *mut _ as *mut c_void) {
        return None;
    }
    Some(p)
}

unsafe fn ax_copy_size(elem: AXUIElementRef, attr: &str) -> Option<CGSize> {
    let raw = ax_copy_raw(elem, attr)?;
    let axv = raw as AXValueRef;
    if AXValueGetType(axv) != kAXValueTypeCGSize {
        return None;
    }
    let mut s = CGSize::default();
    if !AXValueGetValue(axv, kAXValueTypeCGSize, &mut s as *mut _ as *mut c_void) {
        return None;
    }
    Some(s)
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

fn cg_get_bounds(dict: &CFDictionary) -> Option<Bounds> {
    let cf_key = CFString::new("kCGWindowBounds");
    let value = dict.find(cf_key.as_CFTypeRef() as *const _)?;
    let b: CFDictionary = unsafe { CFDictionary::wrap_under_get_rule(*value as *const _) };
    let x = cg_get_f64(&b, "X").unwrap_or(0.0);
    let y = cg_get_f64(&b, "Y").unwrap_or(0.0);
    let width = cg_get_f64(&b, "Width").unwrap_or(0.0);
    let height = cg_get_f64(&b, "Height").unwrap_or(0.0);
    Some(Bounds { x, y, width, height })
}
