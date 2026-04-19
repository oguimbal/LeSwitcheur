//! Resolve and cache app icons as PNGs on disk.
//!
//! GPUI's `img()` element takes a `PathBuf`, so the simplest integration is to
//! extract each app's icon via AppKit once and write it to
//! `~/Library/Caches/fr.gmbl.LeSwitcheur/icons/<key>.png`. Subsequent lookups
//! just check that the file still exists.
//!
//! "Key" is the bundle identifier when available (stable across launches);
//! we fall back to the pid-prefixed name for stray processes without one.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use objc2::rc::Retained;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSImageNameFolder, NSWorkspace};
use objc2_foundation::{NSDictionary, NSString};

/// Return the on-disk path to a PNG icon for the app with the given bundle path,
/// generating it if the cache doesn't yet have one.
pub fn icon_for_bundle(bundle_path: &str, cache_key: &str) -> Option<PathBuf> {
    let dir = cache_dir()?;
    let path = dir.join(format!("{}.png", sanitize(cache_key)));
    if path.exists() {
        return Some(path);
    }
    match write_png_icon(bundle_path, &path) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::debug!(bundle_path, cache_key, "icon extract failed: {e:#}");
            None
        }
    }
}

/// Path to the cached PNG of the system's generic folder icon, used by every
/// row in the right-side dirs panel. We extract from `NSImageNameFolder`
/// rather than `iconForFile` on a sample directory — `/Applications` and
/// `~/Documents` carry custom Finder icons, the named system image is
/// guaranteed to be the plain manila folder. Cached on disk so repeated
/// renders stay zero-copy through GPUI's `img()`.
///
/// Custom per-directory Finder icons (Icon\r files) aren't honoured here
/// yet — every dir shares one PNG. Trade-off: tiny cache, instant lookup.
pub fn folder_icon_path() -> Option<PathBuf> {
    static CACHED: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let dir = cache_dir()?;
            let path = dir.join("_folder_.png");
            if path.exists() {
                return Some(path);
            }
            match write_folder_icon_png(&path) {
                Ok(()) => Some(path),
                Err(e) => {
                    tracing::warn!("folder icon extract failed: {e:#}");
                    None
                }
            }
        })
        .clone()
}

fn write_folder_icon_png(out: &Path) -> Result<()> {
    let image = unsafe { NSImage::imageNamed(NSImageNameFolder) }
        .ok_or_else(|| anyhow!("NSImageNameFolder returned nil"))?;
    let tiff = image
        .TIFFRepresentation()
        .ok_or_else(|| anyhow!("no TIFF representation"))?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)
        .ok_or_else(|| anyhow!("NSBitmapImageRep::imageRepWithData returned nil"))?;
    let props = NSDictionary::new();
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }
        .ok_or_else(|| anyhow!("representationUsingType(png) returned nil"))?;
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    fs::write(out, png.to_vec()).with_context(|| format!("write {}", out.display()))?;
    Ok(())
}

fn write_png_icon(bundle_path: &str, out: &Path) -> Result<()> {
    let png_bytes = extract_png_bytes(bundle_path)?;
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    fs::write(out, png_bytes).with_context(|| format!("write {}", out.display()))?;
    Ok(())
}

fn extract_png_bytes(bundle_path: &str) -> Result<Vec<u8>> {
    let ns_path = NSString::from_str(bundle_path);
    let workspace = NSWorkspace::sharedWorkspace();
    let image: Retained<NSImage> = workspace.iconForFile(&ns_path);

    let tiff = image
        .TIFFRepresentation()
        .ok_or_else(|| anyhow!("no TIFF representation"))?;

    let rep = NSBitmapImageRep::imageRepWithData(&tiff)
        .ok_or_else(|| anyhow!("NSBitmapImageRep::imageRepWithData returned nil"))?;

    let props = NSDictionary::new();
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }
        .ok_or_else(|| anyhow!("representationUsingType(png) returned nil"))?;

    let bytes = png.to_vec();
    if bytes.is_empty() {
        return Err(anyhow!("NSData was empty"));
    }
    Ok(bytes)
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

/// Keep cache keys filesystem-safe without fancy encoding.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}
