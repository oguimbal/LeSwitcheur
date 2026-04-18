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
    kAXErrorSuccess, kAXMinimizedAttribute, kAXPositionAttribute, kAXSizeAttribute,
    kAXSubroleAttribute, kAXTitleAttribute, kAXValueTypeCGPoint, kAXValueTypeCGSize,
    kAXWindowsAttribute, AXError, AXUIElementCopyAttributeValue, AXUIElementCreateApplication,
    AXUIElementRef, AXValueGetType, AXValueGetValue, AXValueRef,
};
use anyhow::{anyhow, Result};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::{CGDisplay, CGWindowListCopyWindowInfo};
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

// Private SkyLight helpers. `SLSCopySpacesForWindows` returns the set of
// Space IDs the given windows live on. Hidden / orderOut windows live on
// no Space — CG still reports them as valid layer-0 drawables with
// plausible bounds, so this is the only reliable way to distinguish a
// genuinely off-Space window from one the app just detached on red-X
// close. Mask 0x7 means "all Space kinds" (user + fullscreen + tiled).
// Approach borrowed from AltTab.
#[link(name = "SkyLight", kind = "framework")]
extern "C" {
    fn SLSMainConnectionID() -> i32;
    fn SLSCopySpacesForWindows(
        cid: i32,
        mask: u32,
        windows: core_foundation::array::CFArrayRef,
    ) -> core_foundation::array::CFArrayRef;
    fn SLSGetActiveSpace(cid: i32) -> u64;
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
    let ax_pids: HashSet<i32> = ax_wins.iter().map(|w| w.pid).collect();
    let cg_extra = cg_supplement_windows(
        show_all_spaces,
        &known_ids,
        &known_pid_title,
        &ax_pids,
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
    ax_pids: &HashSet<i32>,
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
    // nothing about (typically cross-Space windows without Screen
    // Recording, which macOS 14.4+ strips the title from). We collect
    // untitled candidates here and filter them after the pass once we
    // know which pids already have titled coverage: if any titled entry
    // exists for a pid (AX or CG), the untitled siblings are almost
    // always helper surfaces and get dropped. When a pid has no titled
    // coverage at all we keep at most one untitled row — we can't tell
    // real windows apart from OS/app scratch surfaces without a title,
    // so better to show a single indicative row than flood the list.
    let mut titled_out: Vec<WindowRef> = Vec::new();
    let mut titled_seen: HashSet<(i32, String)> = HashSet::new();
    let mut untitled_candidates: Vec<WindowRef> = Vec::new();

    // Active Space — used to reject `orderOut:` ghosts. When CG lists a
    // window off-screen whose only Space is the one we're on, the app
    // closed that window but kept it around internally (classic
    // Keychain Access / Mail / System Settings behavior). Legitimate
    // off-Space windows always have at least one non-active Space in
    // their SkyLight-reported list.
    let active = active_space_id();

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
        // Ghost filter before touching title-based dedup: a CG entry
        // that's off-screen and either lives on no Space or only on
        // the active Space is an `orderOut:` ghost / scratch surface.
        // Empirically Keychain Access keeps 5-6 such entries per pid.
        if is_space_ghost(id as CGWindowID, on_screen, active) {
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
            // Untitled entries: only keep strict-looking candidates. We
            // defer the pid-level filter (no titled coverage) and the
            // one-per-pid collapse to the post-pass so the decision can
            // use the *final* titled_out set, not just what we've seen
            // so far this iteration.
            if ax_pids.contains(&pid) {
                continue;
            }
            if alpha < 0.9 {
                continue;
            }
            if w < 200.0 || h < 150.0 {
                continue;
            }
            untitled_candidates.push(WindowRef {
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

    // Post-pass for untitled: drop any whose pid already has a titled
    // CG row (same mechanism as the ax_pids filter inside the loop, but
    // applied after the whole titled set is known). Collapse the rest
    // to one entry per pid — without a title we can't distinguish the
    // main window from OS/app scratch surfaces, so a single row is the
    // least-lying thing to show.
    let titled_pids: HashSet<i32> = titled_out.iter().map(|w| w.pid).collect();
    let mut untitled_seen_pids: HashSet<i32> = HashSet::new();
    let mut untitled_out: Vec<WindowRef> = Vec::new();
    for u in untitled_candidates {
        if titled_pids.contains(&u.pid) {
            continue;
        }
        if !untitled_seen_pids.insert(u.pid) {
            continue;
        }
        untitled_out.push(u);
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
    // CG's view of "real" drawable layer-0 windows. Apps like Keychain
    // Access keep internal hidden windows alive after the user closes
    // their last visible window — AX still surfaces those as
    // AXStandardWindow so the subrole filter alone can't reject them.
    // Cross-checking the CGWindowID against CG's layer-0-with-bounds
    // set drops the ghosts.
    let valid = valid_cg_window_ids();
    // Union of active display rects (CG top-left coords). Eclipse RCP
    // apps (DBeaver, Eclipse) park disposed UI parts in a hidden
    // "PartRenderingEngine's limbo" NSWindow at coords like
    // `(-10000, -10000)`. It has a valid CGWindowID and a Space, so
    // only a geometry check catches it: a real window always has some
    // overlap with a physical display.
    let screens = active_display_rects();
    let mut out = Vec::with_capacity(apps.len() * 2);
    for app in apps {
        match ax_windows_for(&app, on_screen.as_ref(), &valid, &screens) {
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
    valid_cg_ids: &HashSet<CGWindowID>,
    screens: &[Rect],
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
            // Ghost-window filter. Apps often `orderOut:` their windows
            // on red-X close (Keychain Access, System Settings, Mail's
            // Viewer window, etc.) — the window is detached from the
            // display list but stays in AX's `kAXWindows` collection,
            // and CG still reports it as a valid layer-0 drawable with
            // plausible bounds/alpha, so neither the subrole nor the
            // CG-shape check can reject it. The reliable signal is
            // SkyLight: a hidden window lives on no Space.
            //
            // Checks in order of cheapness:
            //   1. `_AXUIElementGetWindow` returns no id → CG has no
            //      record at all, clearly bogus.
            //   2. CG has the id but classifies it as a non-drawable
            //      (zero bounds / transparent / wrong layer) → bogus.
            //   3. SkyLight reports no Spaces for the id → the window
            //      was detached from the display list (orderOut).
            //
            // Minimized windows stay on their Space even when hidden
            // from view, so they pass 3 naturally — we skip the whole
            // block for them to stay safe against edge-case bookkeeping.
            if !minimized {
                match cg_id {
                    Some(id) if valid_cg_ids.contains(&id) => {
                        // For AX-surfaced windows we only reject windows
                        // truly detached from the Space graph. Active-
                        // Space orderOut detection is unnecessary here:
                        // if AX still lists the window, the app considers
                        // it a real member of its window set.
                        if window_space_ids(id).is_empty() {
                            continue;
                        }
                    }
                    _ => continue,
                }
                // Off-screen "limbo" filter: a real window — even one
                // parked on another Space — sits somewhere inside a
                // physical display rect. Eclipse RCP's
                // `PartRenderingEngine's limbo` shell is stashed at
                // absurd negative coords (~(-10000, -10000)) and never
                // intersects a screen; same pattern for any hidden
                // NSWindow an app moves off-screen as a disposal trick.
                // AXPosition/AXSize give the definitive geometry
                // (CG `kCGWindowBounds` agrees but AX is already in
                // hand so we avoid a second CG dict lookup).
                let pos = ax_copy_point(wref, kAXPositionAttribute);
                let size = ax_copy_size(wref, kAXSizeAttribute);
                if let (Some((x, y)), Some((w, h))) = (pos, size) {
                    let r = Rect { x, y, w, h };
                    if !screens.iter().any(|s| rects_overlap(&r, s)) {
                        continue;
                    }
                }
            }
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

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn rects_overlap(a: &Rect, b: &Rect) -> bool {
    a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y
}

/// Rectangles of every connected display, in CG top-left coords. Used
/// to check that an AX window's frame lives somewhere visible; windows
/// stashed entirely off-display are "limbo" shells apps use to park
/// disposed views.
fn active_display_rects() -> Vec<Rect> {
    let ids = match CGDisplay::active_displays() {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("CGDisplay::active_displays failed (err={e}) — geometry filter disabled");
            return Vec::new();
        }
    };
    ids.into_iter()
        .map(|id| {
            let b = CGDisplay::new(id).bounds();
            Rect {
                x: b.origin.x,
                y: b.origin.y,
                w: b.size.width,
                h: b.size.height,
            }
        })
        .collect()
}

/// Space IDs the given CGWindowID lives on. Empty means the window is
/// not attached to any Space (pure ghost — WindowServer scratch surface
/// or detached placeholder). Mask 0x7 asks for all Space kinds (user +
/// fullscreen + tiled).
fn window_space_ids(cg_id: CGWindowID) -> Vec<i64> {
    unsafe {
        let cid = SLSMainConnectionID();
        let num = CFNumber::from(cg_id as i64);
        let arr: CFArray<CFNumber> = CFArray::from_CFTypes(&[num]);
        let spaces_ref = SLSCopySpacesForWindows(cid, 0x7, arr.as_concrete_TypeRef());
        if spaces_ref.is_null() {
            return Vec::new();
        }
        let spaces: CFArray<CFNumber> = CFArray::wrap_under_create_rule(spaces_ref as _);
        let mut out = Vec::with_capacity(spaces.len() as usize);
        for i in 0..spaces.len() {
            if let Some(n) = spaces.get(i) {
                if let Some(v) = n.to_i64() {
                    out.push(v);
                }
            }
        }
        out
    }
}

fn active_space_id() -> i64 {
    unsafe { SLSGetActiveSpace(SLSMainConnectionID()) as i64 }
}

/// Ghost classification. An off-screen CG entry is a ghost when:
///   - it lives on no Space at all (empty `spaces`), or
///   - every Space it lives on is the currently-active one (the window
///     was `orderOut:`-ed from the active Space; a legitimate cross-
///     Space window always has at least one non-active Space in its
///     list).
/// `on_screen` should come from CG's `kCGWindowIsOnscreen` — the
/// active-Space rule only fires when the window is also off-screen, so
/// a visible focused window isn't mistaken for a ghost.
fn is_space_ghost(cg_id: CGWindowID, on_screen: bool, active: i64) -> bool {
    let spaces = window_space_ids(cg_id);
    if spaces.is_empty() {
        return true;
    }
    if !on_screen && spaces.iter().all(|s| *s == active) {
        return true;
    }
    false
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

/// CGWindowIDs of windows CG considers real layer-0 drawables — non-zero
/// bounds, non-zero alpha. Used to reject AX "ghost" windows (apps like
/// Keychain Access retain invisible placeholder windows internally; AX
/// still reports them with subrole `AXStandardWindow`, but CG exposes
/// them with zero size / zero alpha / a non-normal layer, if at all).
/// Pulls from `kCGWindowListOptionAll` so off-Space windows are kept.
fn valid_cg_window_ids() -> HashSet<CGWindowID> {
    let options = kCGWindowListOptionAll | kCGWindowListExcludeDesktopElements;
    let raw = unsafe { CGWindowListCopyWindowInfo(options, kCGNullWindowID) };
    if raw.is_null() {
        return HashSet::new();
    }
    let cf_array: CFArray<CFDictionary> = unsafe { CFArray::wrap_under_create_rule(raw) };
    let mut ids = HashSet::with_capacity(cf_array.len() as usize);
    for dict in cf_array.iter() {
        let dict: &CFDictionary = &dict;
        let layer = cg_get_i64(dict, "kCGWindowLayer").unwrap_or(-1);
        if layer != 0 {
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
        if let Some(id) = cg_get_i64(dict, "kCGWindowNumber") {
            if id >= 0 {
                ids.insert(id as CGWindowID);
            }
        }
    }
    ids
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

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CgPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CgSize {
    width: f64,
    height: f64,
}

unsafe fn ax_copy_point(elem: AXUIElementRef, attr: &str) -> Option<(f64, f64)> {
    let raw = ax_copy_raw(elem, attr)?;
    let axv = raw as AXValueRef;
    if AXValueGetType(axv) != kAXValueTypeCGPoint {
        return None;
    }
    let mut p = CgPoint::default();
    if !AXValueGetValue(axv, kAXValueTypeCGPoint, &mut p as *mut _ as *mut c_void) {
        return None;
    }
    Some((p.x, p.y))
}

unsafe fn ax_copy_size(elem: AXUIElementRef, attr: &str) -> Option<(f64, f64)> {
    let raw = ax_copy_raw(elem, attr)?;
    let axv = raw as AXValueRef;
    if AXValueGetType(axv) != kAXValueTypeCGSize {
        return None;
    }
    let mut s = CgSize::default();
    if !AXValueGetValue(axv, kAXValueTypeCGSize, &mut s as *mut _ as *mut c_void) {
        return None;
    }
    Some((s.width, s.height))
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
