//! macOS uses `NSApplicationActivationPolicy::Accessory` to keep
//! LeSwitcheur out of the Dock and the system Cmd+Tab list. The Windows
//! analogue (`WS_EX_TOOLWINDOW`) is set per-window at creation time, not
//! process-wide — and GPUI doesn't surface the option yet, so this is a
//! deliberate no-op until we either add a GPUI window-style option or
//! drop down to Win32 to flip the style after creation.

pub fn set_accessory() {}
