//! Authenticode check for downloaded installers: valid, timestamped and
//! signed by Oganbo's Azure Artifact Signing identity. Artifact Signing
//! certificates last a few days, so the identity is the per-organisation
//! EKU, never a thumbprint; validity past the leaf's expiry comes from the
//! timestamp (no `WTD_LIFETIME_SIGNING_FLAG`).

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Security::Cryptography::{
    CertGetEnhancedKeyUsage, CERT_FIND_EXT_ONLY_ENHKEY_USAGE_FLAG, CTL_USAGE,
};
use windows::Win32::Security::WinTrust::{
    WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY,
    WTD_UI_NONE,
};

/// Every Artifact Signing certificate.
const ARTIFACT_SIGNING_EKU: &str = "1.3.6.1.4.1.311.97.1.0";
/// Oganbo's validated identity (same as Lena's binaries).
pub const OGANBO_IDENTITY_EKU: &str = "1.3.6.1.4.1.311.97.520439368.510019483.389024596.527382922";

/// Whether `path` carries a valid, timestamped Oganbo signature.
pub fn signed_by_oganbo(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe {
        WinVerifyTrust(HWND::default(), &mut action, &mut data as *mut _ as *mut _)
    };
    let result = if status != 0 {
        Err(format!("untrusted signature (0x{:08x})", status as u32))
    } else {
        unsafe { check_signer(data.hWVTStateData) }
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(HWND::default(), &mut action, &mut data as *mut _ as *mut _);
    }
    result
}

unsafe fn check_signer(state: HANDLE) -> Result<(), String> {
    let provider = unsafe { WTHelperProvDataFromStateData(state) };
    if provider.is_null() {
        return Err("no provider data".into());
    }
    let signer = unsafe { WTHelperGetProvSignerFromChain(provider, 0, false, 0) };
    if signer.is_null() {
        return Err("no signer".into());
    }
    let signer = unsafe { &*signer };
    if signer.csCounterSigners == 0 {
        return Err("signature not timestamped".into());
    }
    if signer.csCertChain == 0 || signer.pasCertChain.is_null() {
        return Err("no signer certificate".into());
    }
    let cert = unsafe { (*signer.pasCertChain).pCert };
    let usages = unsafe { enhanced_key_usages(cert) }?;
    if usages.iter().any(|u| u == ARTIFACT_SIGNING_EKU)
        && usages.iter().any(|u| u == OGANBO_IDENTITY_EKU)
    {
        Ok(())
    } else {
        Err("unexpected publisher".into())
    }
}

unsafe fn enhanced_key_usages(
    cert: *const windows::Win32::Security::Cryptography::CERT_CONTEXT,
) -> Result<Vec<String>, String> {
    let flags = CERT_FIND_EXT_ONLY_ENHKEY_USAGE_FLAG.0;
    let mut size = 0u32;
    unsafe { CertGetEnhancedKeyUsage(cert, flags, None, &mut size) }
        .map_err(|e| format!("read EKU: {e}"))?;
    // CTL_USAGE plus the strings it points into, pointer-aligned.
    let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
    let usage = buffer.as_mut_ptr() as *mut CTL_USAGE;
    unsafe { CertGetEnhancedKeyUsage(cert, flags, Some(usage), &mut size) }
        .map_err(|e| format!("read EKU: {e}"))?;
    let usage = unsafe { &*usage };
    let ids = unsafe {
        std::slice::from_raw_parts(usage.rgpszUsageIdentifier, usage.cUsageIdentifier as usize)
    };
    Ok(ids
        .iter()
        .filter_map(|id| unsafe { id.to_string() }.ok())
        .collect())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_binary_is_refused() {
        let exe = std::env::current_exe().unwrap();
        assert!(signed_by_oganbo(&exe).is_err());
    }

    /// `LESWITCHEUR_SIGNED_SETUP=<path to a released setup> cargo test`.
    #[test]
    fn released_setup_is_accepted() {
        let Some(path) = std::env::var_os("LESWITCHEUR_SIGNED_SETUP") else {
            return;
        };
        assert_eq!(signed_by_oganbo(std::path::Path::new(&path)), Ok(()));
    }
}
