//! Launch-at-startup toggle on Windows. `auto-launch` writes the entry under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

use anyhow::{anyhow, Context, Result};
use auto_launch::{AutoLaunch, AutoLaunchBuilder};

const APP_NAME: &str = "LeSwitcheur";

/// CLI flag tagged onto the registry entry's command line so `main.rs` can
/// distinguish a cold launch driven by the user (Start menu / shell) from
/// one driven by Windows at logon.
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
