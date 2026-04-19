//! NSApplication activation-policy helpers.
//!
//! The Info.plist has `LSUIElement = true` so LeSwitcheur should boot as an
//! accessory (no Dock tile, not listed in the system Cmd-Tab switcher). GPUI,
//! however, unconditionally calls `setActivationPolicy(.regular)` inside its
//! `applicationDidFinishLaunching` handler, which overrides the plist at
//! runtime. Call [`set_accessory`] at the start of `app.run` to restore the
//! accessory policy after GPUI has flipped it.

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::MainThreadMarker;

pub fn set_accessory() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}
