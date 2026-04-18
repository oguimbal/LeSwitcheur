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

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
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
