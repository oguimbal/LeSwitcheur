//! Launch-at-startup toggle backed by the `auto-launch` crate.
//!
//! On macOS this writes a plist under `~/Library/LaunchAgents/` pointing at the
//! currently-running executable. When the user moves the `.app` bundle or
//! switches between `cargo run` and the packaged app, they need to toggle the
//! setting off and back on so the plist points at the new path.

use anyhow::{anyhow, Context, Result};
use auto_launch::{AutoLaunch, AutoLaunchBuilder, MacOSLaunchMode};

const APP_NAME: &str = "LeSwitcheur";

/// CLI flag written into the LaunchAgent plist's `ProgramArguments`. Lets
/// `main.rs` distinguish a cold launch kicked off by launchd-at-login from a
/// manual launch (Finder, `open -a`), so the former stays headless while the
/// latter opens the switcher.
pub const LAUNCHED_AT_LOGIN_ARG: &str = "--launched-at-login";

fn builder() -> Result<AutoLaunch> {
    let exe = std::env::current_exe().context("current_exe")?;
    let path = exe
        .to_str()
        .ok_or_else(|| anyhow!("exe path not utf-8: {}", exe.display()))?;
    AutoLaunchBuilder::new()
        .set_app_name(APP_NAME)
        .set_app_path(path)
        .set_args(&[LAUNCHED_AT_LOGIN_ARG])
        .set_macos_launch_mode(MacOSLaunchMode::LaunchAgent)
        .build()
        .map_err(|e| anyhow!("auto-launch build: {e}"))
}

pub fn enable() -> Result<()> {
    builder()?
        .enable()
        .map_err(|e| anyhow!("auto-launch enable: {e}"))
}

pub fn disable() -> Result<()> {
    builder()?
        .disable()
        .map_err(|e| anyhow!("auto-launch disable: {e}"))
}

pub fn is_enabled() -> Result<bool> {
    builder()?
        .is_enabled()
        .map_err(|e| anyhow!("auto-launch is_enabled: {e}"))
}
