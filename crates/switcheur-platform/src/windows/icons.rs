//! Resolve and cache Windows icons as PNGs on disk.
//!
//! Mirror of `macos::icons` but using Win32:
//! - `ExtractIconExW` for executables (PE `RT_GROUP_ICON` resource).
//! - `SHGetFileInfoW(SHGFI_ICON | SHGFI_LARGEICON)` for files/folders, which
//!   consults the registered file association the same way Explorer does.
//!
//! Each `HICON` is converted to a 32-bit BGRA buffer via `GetIconInfo` +
//! `GetDIBits`, channel-swapped to RGBA, and encoded with the `image` crate.
//! Cached at `%LOCALAPPDATA%\fr.gmbl.LeSwitcheur\icons\<key>.png` so GPUI's
//! `img(path)` can render them without re-extracting on every frame.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC,
    BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::UI::Shell::{
    ExtractIconExW, SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
    SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// Cached PNG path for the executable's icon, keyed by the exe path itself.
/// `None` if the file has no embedded icon and Shell can't supply one.
pub fn icon_for_exe(exe_path: &str) -> Option<PathBuf> {
    let key = sanitize(exe_path);
    cache_or_extract(&key, || extract_exe_png(exe_path))
}

/// Same as `icon_for_exe` but with an explicit cache key — lets callers share
/// one PNG across multiple processes that share the same executable (which is
/// the common case for browser windows / Office apps).
pub fn icon_for_bundle(bundle_path: &str, cache_key: &str) -> Option<PathBuf> {
    let key = sanitize(cache_key);
    cache_or_extract(&key, || extract_exe_png(bundle_path))
}

/// Cached PNG path for the OS's assigned icon of `path` (file or directory).
/// Files are keyed by extension so `.txt`, `.pdf`, etc. share one cache entry.
/// Directories all share the generic folder icon.
pub fn icon_for_path(path: &Path, is_dir: bool) -> Option<PathBuf> {
    if is_dir {
        return folder_icon_path();
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let key = if ext.is_empty() {
        "ext__none".to_string()
    } else {
        format!("ext_{ext}")
    };
    cache_or_extract(&key, || {
        // SHGFI_USEFILEATTRIBUTES skips the disk hit and returns the icon
        // tied to the type alone — same icon Explorer uses, no I/O.
        let dummy = format!("placeholder.{ext}");
        extract_shell_png(&dummy, FILE_ATTRIBUTE_NORMAL, true)
    })
}

fn folder_icon_path() -> Option<PathBuf> {
    static CACHED: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            cache_or_extract("_folder_", || {
                extract_shell_png("placeholder", FILE_ATTRIBUTE_DIRECTORY, true)
            })
        })
        .clone()
}

fn cache_or_extract<F>(key: &str, extract: F) -> Option<PathBuf>
where
    F: FnOnce() -> Result<Vec<u8>>,
{
    let dir = cache_dir()?;
    let path = dir.join(format!("{key}.png"));
    if path.exists() {
        return Some(path);
    }
    let result = extract().and_then(|bytes| {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
        }
        fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    });
    match result {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::debug!(key, "icon extract failed: {e:#}");
            None
        }
    }
}

fn extract_exe_png(exe_path: &str) -> Result<Vec<u8>> {
    let wide = to_wide(exe_path);
    let mut large: HICON = HICON::default();
    let extracted = unsafe {
        ExtractIconExW(PCWSTR(wide.as_ptr()), 0, Some(&mut large), None, 1)
    };
    if extracted == 0 || large.is_invalid() {
        // Fall back to Shell's view — still picks up an icon for exes that
        // delegate their icon to the OS (UWP wrappers, .lnk shortcuts).
        return extract_shell_png(exe_path, FILE_ATTRIBUTE_NORMAL, false);
    }
    let bytes = hicon_to_png(large);
    unsafe {
        let _ = DestroyIcon(large);
    }
    bytes
}

fn extract_shell_png(
    path: &str,
    attrs: FILE_FLAGS_AND_ATTRIBUTES,
    use_attrs: bool,
) -> Result<Vec<u8>> {
    let wide = to_wide(path);
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let mut flags = SHGFI_ICON | SHGFI_LARGEICON;
    if use_attrs {
        flags |= SHGFI_USEFILEATTRIBUTES;
    }
    let res = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            attrs,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    if res == 0 || info.hIcon.is_invalid() {
        return Err(anyhow!("SHGetFileInfoW returned no icon"));
    }
    let bytes = hicon_to_png(info.hIcon);
    unsafe {
        let _ = DestroyIcon(info.hIcon);
    }
    bytes
}

fn hicon_to_png(hicon: HICON) -> Result<Vec<u8>> {
    unsafe {
        let mut info: ICONINFO = std::mem::zeroed();
        GetIconInfo(hicon, &mut info).context("GetIconInfo")?;
        // Defensive: ensure we always release the bitmaps GetIconInfo allocates.
        struct Bitmaps {
            color: HBITMAP,
            mask: HBITMAP,
        }
        impl Drop for Bitmaps {
            fn drop(&mut self) {
                unsafe {
                    if !self.color.is_invalid() {
                        let _ = DeleteObject(HGDIOBJ(self.color.0));
                    }
                    if !self.mask.is_invalid() {
                        let _ = DeleteObject(HGDIOBJ(self.mask.0));
                    }
                }
            }
        }
        let bitmaps = Bitmaps {
            color: info.hbmColor,
            mask: info.hbmMask,
        };

        if bitmaps.color.is_invalid() {
            return Err(anyhow!("icon has no color bitmap"));
        }

        let mut bm: BITMAP = std::mem::zeroed();
        let got = GetObjectW(
            HGDIOBJ(bitmaps.color.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        );
        if got == 0 {
            return Err(anyhow!("GetObjectW(BITMAP) failed"));
        }
        let width = bm.bmWidth.max(1) as u32;
        let height = bm.bmHeight.max(1) as u32;

        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));

        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = width as i32;
        // Negative height -> top-down rows so iterator order matches PNG.
        bmi.bmiHeader.biHeight = -(height as i32);
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let mut buf = vec![0u8; (width * height * 4) as usize];
        let scan_lines = GetDIBits(
            mem_dc,
            bitmaps.color,
            0,
            height,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );

        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);

        if scan_lines == 0 {
            return Err(anyhow!("GetDIBits returned 0 scan lines"));
        }

        // BGRA -> RGBA for `image::RgbaImage`.
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }

        // Some HICONs come in with alpha == 0 across the board (older 32-bit
        // icons in legacy 24bpp+mask form). Detect and rebuild alpha from the
        // mask bitmap — without this the icon ends up fully transparent.
        let any_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
        if !any_alpha && !bitmaps.mask.is_invalid() {
            apply_icon_mask(&mut buf, width, height, bitmaps.mask)?;
        }

        let img = image::RgbaImage::from_raw(width, height, buf)
            .ok_or_else(|| anyhow!("invalid image dims {width}x{height}"))?;
        let mut out = Vec::with_capacity(4096);
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .context("encode PNG")?;
        Ok(out)
    }
}

unsafe fn apply_icon_mask(rgba: &mut [u8], width: u32, height: u32, mask: HBITMAP) -> Result<()> {
    let screen_dc = unsafe { GetDC(None) };
    let mem_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };

    let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = width as i32;
    bmi.bmiHeader.biHeight = -(height as i32);
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;

    let mut mask_buf = vec![0u8; (width * height * 4) as usize];
    let lines = unsafe {
        GetDIBits(
            mem_dc,
            mask,
            0,
            height,
            Some(mask_buf.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
    }
    if lines == 0 {
        return Err(anyhow!("GetDIBits(mask) failed"));
    }
    // Mask convention: 0 = opaque, !=0 = transparent.
    for (px, m) in rgba.chunks_exact_mut(4).zip(mask_buf.chunks_exact(4)) {
        px[3] = if m[0] == 0 { 255 } else { 0 };
    }
    Ok(())
}

fn cache_dir() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let proj = ProjectDirs::from("fr", "gmbl", "LeSwitcheur")?;
        let d = proj.cache_dir().join("icons");
        if let Err(e) = fs::create_dir_all(&d) {
            tracing::warn!("cannot create icon cache dir {}: {e}", d.display());
            return None;
        }
        Some(d)
    })
    .clone()
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
