//! Stable per-machine identifier, used as the `machine_id` field in license
//! activation requests so the backend can attribute each activation to a
//! specific Mac (and not double-count re-activations from the same machine).
//!
//! We read `IOPlatformUUID` via `ioreg` instead of linking the IOKit framework
//! directly — the cost of spawning the tool is irrelevant for a flow that runs
//! at most a few times a year, and it keeps the crate free of extra deps.

use std::process::Command;

/// Return this Mac's stable hardware UUID, or `None` if it can't be read.
pub fn machine_id() -> Option<String> {
    let out = Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    for line in text.lines() {
        if !line.contains("IOPlatformUUID") {
            continue;
        }
        if let Some(eq) = line.find('=') {
            let rest = line[eq + 1..].trim();
            let value = rest.trim_matches(|c: char| c == '"' || c.is_whitespace());
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}
