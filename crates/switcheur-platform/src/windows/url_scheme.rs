//! Register the `leswitcheur://` URL scheme under `HKCU\Software\Classes`.
//!
//! No admin needed — per-user keys are sufficient for Shell to dispatch the
//! protocol. Idempotent: the values point at the current `current_exe()` path
//! every call, so dropping a fresh build into `target/release` and launching
//! it self-heals the registration to that binary.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};

/// Write the four registry values that bind `<scheme>://` to this binary.
pub fn ensure_registered(scheme: &str) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let exe_str = exe
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 exe path: {:?}", exe))?;
    let cmd = format!("\"{exe_str}\" \"%1\"");

    let scheme_root = format!("Software\\Classes\\{scheme}");
    let cmd_subkey = format!("Software\\Classes\\{scheme}\\shell\\open\\command");

    write_value(&scheme_root, "", &format!("URL:{scheme}"))?;
    write_value(&scheme_root, "URL Protocol", "")?;
    write_value(&cmd_subkey, "", &cmd)?;
    Ok(())
}

fn write_value(subkey: &str, value_name: &str, value: &str) -> Result<()> {
    let subkey_w = wide(subkey);
    let value_name_w = wide(value_name);
    let value_w = wide(value);
    // REG_SZ wants UTF-16 bytes including the trailing NUL.
    let data_bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(value_w.as_ptr() as *const u8, value_w.len() * 2) };

    let mut hkey: HKEY = HKEY::default();
    unsafe {
        let res = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_w.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        );
        if res.0 != 0 {
            anyhow::bail!("RegCreateKeyExW({subkey}): error {}", res.0);
        }
        let res = RegSetValueExW(
            hkey,
            PCWSTR(value_name_w.as_ptr()),
            None,
            REG_SZ,
            Some(data_bytes),
        );
        let _ = RegCloseKey(hkey);
        if res.0 != 0 {
            anyhow::bail!("RegSetValueExW({value_name}): error {}", res.0);
        }
    }
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
