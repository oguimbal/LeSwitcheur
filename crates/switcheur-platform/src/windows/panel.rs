//! Native panel-frame tweaks the GPUI Windows backend doesn't expose.
//!
//! GPUI's Windows backend auto-grows the window when its content needs more
//! room — top-left anchored, so the bottom edge slides down. The switcher's
//! input row needs to stay anchored when sections appear above it (programs,
//! currently-playing). By the time this hook runs the window has *already*
//! grown, so we don't change the height again — we just shift the window up
//! by the same amount, restoring the bottom edge to its previous screen Y.
//!
//! Caller convention: deltas come in NSWindow coordinates (bottom-left origin,
//! y-up).
//! - `delta_height > 0` (grow), `delta_origin_y == 0`: GPUI already pushed
//!   the bottom down by `delta_height`. Shift the window up by `delta_height`
//!   to restore it; height stays the same.
//! - `delta_height < 0` (shrink at bottom), `delta_origin_y == -delta_height`:
//!   GPUI already shrank from the bottom (top stays). Nothing to do — the
//!   formula `Δtop = -Δorigin_y - Δheight` evaluates to zero, no-op.
//!
//! `GetActiveWindow` is enough to identify the panel: this function only runs
//! in response to its content changing, which only happens while it's active.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
};

pub const OPEN_WITH_POPOVER_WIDTH: f64 = 288.0;

pub fn adjust_key_window_frame(delta_origin_y: f32, delta_height: f32) {
    unsafe {
        let hwnd: HWND = GetActiveWindow();
        if hwnd.0.is_null() {
            tracing::warn!("adjust_key_window_frame: no active window");
            return;
        }
        let mut rect = RECT::default();
        if let Err(e) = GetWindowRect(hwnd, &mut rect) {
            tracing::warn!("GetWindowRect: {e:?}");
            return;
        }
        // GPUI emits deltas in *logical* pixels (its own coordinate space);
        // GetWindowRect / SetWindowPos work in *physical* pixels under
        // per-monitor DPI awareness. Scale to keep both sides in the same
        // coordinate system — at 150% DPI a 142 logical-px delta is 213
        // physical px, and undershoot showed up as a one-input-row descent.
        let dpi = GetDpiForWindow(hwnd);
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
        let dx_phys = (delta_origin_y * scale).round() as i32;
        let dh_phys = (delta_height * scale).round() as i32;
        // GPUI already applied the height delta — keep the current height and
        // just compensate the position to anchor the desired edge.
        let new_top = rect.top - dx_phys - dh_phys;
        let new_w = rect.right - rect.left;
        let new_h = rect.bottom - rect.top;
        tracing::info!(
            hwnd = ?hwnd.0,
            dpi,
            cur_top = rect.top,
            cur_bottom = rect.bottom,
            cur_h = new_h,
            delta_origin_y_logical = delta_origin_y,
            delta_height_logical = delta_height,
            dx_phys,
            dh_phys,
            new_top,
            "adjust_key_window_frame"
        );
        if new_top == rect.top {
            return;
        }
        if let Err(e) = SetWindowPos(
            hwnd,
            None,
            rect.left,
            new_top,
            new_w,
            new_h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        ) {
            tracing::warn!("SetWindowPos: {e:?}");
        }
    }
}

pub fn configure_open_with_popover() {}

pub fn set_open_with_popover_frame(_origin_x: f64, _origin_y: f64, _height: f64) {}

pub fn key_window_frame() -> Option<(f64, f64, f64, f64)> {
    None
}
