//! Stubs for the macOS panel-tweaking helpers. The Open With popover and
//! the dynamic panel resize anchored on the input row are macOS-specific
//! NSWindow tweaks that don't have a direct equivalent under GPUI's
//! Windows backend yet — first pass falls back to GPUI's default sizing.

pub const OPEN_WITH_POPOVER_WIDTH: f64 = 288.0;

pub fn adjust_key_window_frame(_delta_origin_y: f32, _delta_height: f32) {}

pub fn configure_open_with_popover() {}

pub fn set_open_with_popover_frame(_origin_x: f64, _origin_y: f64, _height: f64) {}

pub fn key_window_frame() -> Option<(f64, f64, f64, f64)> {
    None
}
