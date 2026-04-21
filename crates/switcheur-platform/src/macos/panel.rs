//! Low-level tweaks to the switcher panel NSWindow that GPUI doesn't expose.
//!
//! GPUI's `Window::resize` calls `-[NSWindow setContentSize:]` which anchors
//! the top-left corner in place — the window grows *downward*. We never want
//! the input row the user is typing into to shift under the cursor, so all
//! layout changes are translated into signed frame deltas and applied via
//! `-[NSWindow setFrame:display:]`.
//!
//! NSWindow screen coordinates are bottom-left origin:
//! - Adding rows *above* the input (programs section): pass a positive
//!   `delta_height` with `delta_origin_y = 0.0`. The bottom edge stays put,
//!   the top edge grows upward, and the input stays where it is (rows are
//!   added at the new top).
//! - Removing rows *below* the input (results panel suppressed in eval-only
//!   mode): pass a negative `delta_height` with matching positive
//!   `delta_origin_y`. The top edge stays put, the bottom edge moves up.

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSColor, NSWindow};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// Shift the key window's bottom-left origin and resize it by the given
/// deltas. No-op if there's no key window or we're off the main thread.
pub fn adjust_key_window_frame(delta_origin_y: f32, delta_height: f32) {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("adjust_key_window_frame called off main thread");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(win) = app.keyWindow() else {
        tracing::warn!("adjust_key_window_frame: no key window");
        return;
    };
    let frame = win.frame();
    let new_frame = NSRect {
        origin: NSPoint {
            x: frame.origin.x,
            y: frame.origin.y + delta_origin_y as f64,
        },
        size: NSSize {
            width: frame.size.width,
            height: frame.size.height + delta_height as f64,
        },
    };
    win.setFrame_display(new_frame, true);
}

/// Sentinel width — chosen so we can locate the popover among the process's
/// windows without a stable handle. Callers must create the popover window
/// with exactly this width; if they need a different visual width, adjust
/// this constant and their side together.
pub const OPEN_WITH_POPOVER_WIDTH: f64 = 288.0;

/// Locate the "Open With" popover NSWindow by matching its sentinel frame
/// width. Returns `None` before the popover is opened or after it is closed.
fn find_open_with_popover(mtm: MainThreadMarker) -> Option<Retained<NSWindow>> {
    let app = NSApplication::sharedApplication(mtm);
    let windows = app.windows();
    for i in 0..windows.count() {
        let w = windows.objectAtIndex(i);
        let frame = w.frame();
        // Match on width with a tolerance — the float round-trip through
        // setFrame can shift by a sub-pixel on Retina displays.
        if (frame.size.width - OPEN_WITH_POPOVER_WIDTH).abs() < 0.5 {
            return Some(w);
        }
    }
    None
}

/// Polish the popover NSWindow once it's been created: transparent
/// background, no shadow (we render our own), doesn't appear in Cmd-Tab,
/// hides when the app loses focus, stays on top of the main switcher.
///
/// Safe to call more than once; each call re-applies the same tweaks.
pub fn configure_open_with_popover() {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("configure_open_with_popover off main thread");
        return;
    };
    let Some(win) = find_open_with_popover(mtm) else {
        tracing::warn!("configure_open_with_popover: popover window not found");
        return;
    };
    win.setOpaque(false);
    win.setBackgroundColor(Some(&NSColor::clearColor()));
    win.setHasShadow(false);
    win.setHidesOnDeactivate(true);
    // GPUI already sets NSPopUpWindowLevel + CanJoinAllSpaces |
    // FullScreenAuxiliary. Keep those and add IgnoresCycle so the panel
    // never shows up in the Cmd-Tab window cycle for belt-and-braces.
    let behavior = win.collectionBehavior()
        | objc2_app_kit::NSWindowCollectionBehavior::IgnoresCycle
        | objc2_app_kit::NSWindowCollectionBehavior::Transient;
    win.setCollectionBehavior(behavior);
}

/// Move the popover to `(origin_x, origin_y)` with the given height. Keeps
/// the sentinel width. Screen coordinates are bottom-left origin — the caller
/// must translate from "top of selected row" to bottom-left before passing.
pub fn set_open_with_popover_frame(origin_x: f64, origin_y: f64, height: f64) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(win) = find_open_with_popover(mtm) else {
        return;
    };
    let new_frame = NSRect {
        origin: NSPoint {
            x: origin_x,
            y: origin_y,
        },
        size: NSSize {
            width: OPEN_WITH_POPOVER_WIDTH,
            height,
        },
    };
    win.setFrame_display(new_frame, true);
}

/// Current frame of the key window (the main switcher). Used by the host to
/// compute where the popover should sit relative to it.
pub fn key_window_frame() -> Option<(f64, f64, f64, f64)> {
    let mtm = MainThreadMarker::new()?;
    let app = NSApplication::sharedApplication(mtm);
    let win = app.keyWindow()?;
    let f = win.frame();
    Some((f.origin.x, f.origin.y, f.size.width, f.size.height))
}
