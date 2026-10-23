//! Lena desktop client (file search + assistant). Lena's UI process serves
//! the local-source transport under source id `lena`; it is request-only
//! (no `list` polling), so it is not registered in `local_sources::SOURCES`.
pub use local_source_ipc::remote_code;
use local_source_ipc::{invalid, Envelope};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

pub const SOURCE: &str = "lena";
pub const MAX_HITS: u32 = 50;
const MAX_FIELD: usize = 1024;
const INFO_TIMEOUT: Duration = Duration::from_millis(750);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(3);
const ACTION_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Deserialize)]
pub struct Info {
    pub instance: String,
    pub signed_in: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Bundle id on macOS when bundled, else the executable path.
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
}

impl Info {
    pub fn can(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Hit {
    pub id: String,
    pub name: String,
    pub folder: String,
    pub is_folder: bool,
    /// Local copy, only when Lena has it on disk without downloading.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// The file type can have a server thumbnail (not a promise: `none`).
    #[serde(default)]
    pub thumbnail: bool,
}

#[derive(Debug, Deserialize)]
struct Hits {
    instance: String,
    items: Vec<Hit>,
}

fn call(method: &str, payload: Value, timeout: Duration) -> io::Result<Value> {
    let envelope = Envelope::new(SOURCE, method, payload);
    #[cfg(unix)]
    {
        use local_source_ipc::unix::{endpoint, request};
        Ok(request(&endpoint(SOURCE)?, &envelope, timeout)?.payload)
    }
    #[cfg(windows)]
    {
        use local_source_ipc::windows::{endpoint, request};
        Ok(request(&endpoint(SOURCE)?, &envelope, timeout)?.payload)
    }
}

pub fn info() -> io::Result<Info> {
    let info: Info =
        serde_json::from_value(call("info", Value::Null, INFO_TIMEOUT)?).map_err(invalid)?;
    if info.instance.is_empty() || info.instance.len() > MAX_FIELD {
        return Err(invalid("invalid lena instance"));
    }
    Ok(info)
}

/// Name search, in Lena's relevance order. Returns the answering instance.
pub fn search(query: &str, limit: u32) -> io::Result<(String, Vec<Hit>)> {
    let payload = json!({"query": query, "limit": limit.min(MAX_HITS)});
    let hits: Hits =
        serde_json::from_value(call("search", payload, SEARCH_TIMEOUT)?).map_err(invalid)?;
    validate(&hits)?;
    Ok((hits.instance, hits.items))
}

fn validate(hits: &Hits) -> io::Result<()> {
    if hits.instance.is_empty()
        || hits.instance.len() > MAX_FIELD
        || hits.items.len() > MAX_HITS as usize
    {
        return Err(invalid("invalid lena search reply"));
    }
    let mut ids = HashSet::new();
    for hit in &hits.items {
        let bounded = |s: &str| s.len() <= MAX_FIELD;
        if hit.id.is_empty()
            || hit.name.is_empty()
            || !bounded(&hit.id)
            || !bounded(&hit.name)
            || !bounded(&hit.folder)
            || hit
                .path
                .as_ref()
                .is_some_and(|p| !p.is_absolute() || p.as_os_str().len() > 4096)
            || !ids.insert(hit.id.as_str())
        {
            return Err(invalid("invalid lena search item"));
        }
    }
    Ok(())
}

fn acknowledged(payload: Value) -> io::Result<()> {
    if payload.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(invalid("lena did not acknowledge"))
    }
}

/// Open a hit (Lena localises / downloads it first), or reveal it in the
/// file manager. `stale` remote code when Lena restarted since the search.
pub fn open(instance: &str, id: &str, reveal: bool) -> io::Result<()> {
    acknowledged(call(
        "open",
        json!({"instance": instance, "id": id, "reveal": reveal}),
        ACTION_TIMEOUT,
    )?)
}

/// Side of the cached thumbnails: icons render at 26 pt, @2x.
pub const THUMBNAIL_SIZE: u32 = 64;
/// Lena's service bound on a decoded thumbnail.
const MAX_THUMBNAIL: usize = 512 * 1024;

#[derive(Deserialize)]
struct Thumbnail {
    data: String,
}

/// Cached PNG of a hit's thumbnail, if already fetched. Keyed by id so it
/// survives Lena restarts; GPUI caches images by path, one file per id.
pub fn cached_thumbnail(id: &str) -> Option<PathBuf> {
    let path = thumbnail_path(id)?;
    path.exists().then_some(path)
}

fn thumbnail_path(id: &str) -> Option<PathBuf> {
    use sha2::{Digest, Sha256};
    let dirs = directories::ProjectDirs::from("fr", "gmbl", "LeSwitcheur")?;
    let hash = Sha256::digest(id.as_bytes());
    let name: String = hash[..12].iter().map(|b| format!("{b:02x}")).collect();
    Some(dirs.cache_dir().join("lena-thumbnails").join(format!("{name}.png")))
}

/// Fetch, downscale and cache a hit's thumbnail. Remote code `none` when
/// Lena has no thumbnail for it.
pub fn thumbnail(instance: &str, id: &str) -> io::Result<PathBuf> {
    use base64::Engine;
    let payload = json!({"instance": instance, "id": id, "size": THUMBNAIL_SIZE});
    let reply: Thumbnail = serde_json::from_value(call("thumbnail", payload, ACTION_TIMEOUT)?).map_err(invalid)?;
    if reply.data.len() > MAX_THUMBNAIL * 4 / 3 + 4 {
        return Err(invalid("lena thumbnail too large"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(reply.data)
        .map_err(invalid)?;
    let image = image::load_from_memory(&bytes).map_err(invalid)?;
    let small = image.thumbnail(THUMBNAIL_SIZE, THUMBNAIL_SIZE);
    let path = thumbnail_path(id).ok_or_else(|| invalid("no cache dir"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write then rename so a reader never sees a partial PNG.
    let partial = path.with_extension("part");
    small
        .save_with_format(&partial, image::ImageFormat::Png)
        .map_err(invalid)?;
    std::fs::rename(&partial, &path)?;
    Ok(path)
}

/// Bring Lena's search window to the front in assistant mode and ask.
pub fn ask(question: &str) -> io::Result<()> {
    acknowledged(call("ask", json!({"question": question}), ACTION_TIMEOUT)?)
}

/// Let Lena take the foreground right after our request. Call on the main
/// thread (macOS) before `open` / `ask`.
pub fn hand_over_focus(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(target_os = "macos")]
    {
        use objc2::{msg_send, sel};
        use objc2_app_kit::{NSApplication, NSRunningApplication};
        use objc2_foundation::{MainThreadMarker, NSObjectProtocol};
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(target) =
            NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32)
        else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        // Cooperative activation is macOS 14+; older systems let Lena activate itself.
        if app.respondsToSelector(sel!(yieldActivationToApplication:)) {
            let _: () = unsafe { msg_send![&*app, yieldActivationToApplication: &*target] };
        }
    }
    #[cfg(target_os = "windows")]
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str) -> Hit {
        Hit {
            id: id.into(),
            name: "Budget.xlsx".into(),
            folder: "Lena/Projets".into(),
            is_folder: false,
            path: None,
            thumbnail: false,
        }
    }

    #[test]
    fn search_replies_are_bounded() {
        let ok = Hits {
            instance: "i".into(),
            items: vec![hit("a"), hit("b")],
        };
        assert!(validate(&ok).is_ok());
        let duplicate = Hits {
            instance: "i".into(),
            items: vec![hit("a"), hit("a")],
        };
        assert!(validate(&duplicate).is_err());
        let too_many = Hits {
            instance: "i".into(),
            items: (0..51).map(|i| hit(&i.to_string())).collect(),
        };
        assert!(validate(&too_many).is_err());
        let long_id = Hits {
            instance: "i".into(),
            items: vec![hit(&"x".repeat(1025))],
        };
        assert!(validate(&long_id).is_err());
        let mut relative = hit("a");
        relative.path = Some("Projets/Budget.xlsx".into());
        assert!(validate(&Hits {
            instance: "i".into(),
            items: vec![relative]
        })
        .is_err());
    }

    #[test]
    fn info_accepts_additive_fields() {
        let info: Info = serde_json::from_value(json!({
            "instance": "i", "signed_in": true, "capabilities": ["search", "ask"],
            "pid": 42, "future": 1
        }))
        .unwrap();
        assert!(info.can("ask") && !info.can("open"));
        assert_eq!(info.app_id, None);
    }
}
