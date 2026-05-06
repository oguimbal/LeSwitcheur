//! Windows has no equivalent to the macOS Accessibility / Input Monitoring
//! / Screen Recording prompts for the operations LeSwitcheur performs
//! today (window enumeration, foreground activation). Every gate below
//! returns "granted" so the host's permission flows skip themselves.

pub fn ensure_accessibility(_prompt: bool) -> bool {
    true
}

pub fn prompt_accessibility() {}

pub fn request_accessibility_prompt() {}

pub fn has_input_monitoring_permission() -> bool {
    true
}

pub fn prompt_input_monitoring() {}

pub fn has_screen_recording_permission() -> bool {
    true
}

pub fn request_screen_recording_permission() -> bool {
    true
}
