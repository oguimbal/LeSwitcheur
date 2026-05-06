//! Stable machine identifier on Windows. Sourced from
//! `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`, which Windows
//! generates at install time and keeps for the lifetime of the OS install.
//! Returned verbatim (as the registry stores it). On read failure we
//! return `None` and the host falls back to its no-machine-id branch.

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY,
    REG_VALUE_TYPE,
};

pub fn machine_id() -> Option<String> {
    let subkey: Vec<u16> = "SOFTWARE\\Microsoft\\Cryptography"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value: Vec<u16> = "MachineGuid"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let mut hkey = HKEY::default();
        let r = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_READ | KEY_WOW64_64KEY,
            &mut hkey,
        );
        if r != ERROR_SUCCESS {
            return None;
        }
        let mut buf = [0u16; 64];
        let mut size: u32 = (buf.len() * 2) as u32;
        let mut value_type = REG_VALUE_TYPE::default();
        let r = RegQueryValueExW(
            hkey,
            PCWSTR(value.as_ptr()),
            None,
            Some(&mut value_type),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        if r != ERROR_SUCCESS {
            return None;
        }
        // size is bytes including the trailing null. Convert to a UTF-16
        // length and trim the terminator.
        let len = (size as usize / 2).saturating_sub(1);
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}
