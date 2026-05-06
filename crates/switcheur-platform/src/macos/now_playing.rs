//! Bridge to MediaRemote.framework's "now playing" info via a bundled
//! Perl-loaded helper framework. Used by the "Currently Playing" feature to
//! identify *which browser tab* is producing audio when the source PID is a
//! browser helper renderer.
//!
//! Why the indirection: macOS 15.4+ added a daemon-side check that rejects
//! MediaRemote XPC calls from any process whose code signature isn't
//! `com.apple.*`. Our app (signed Q966PUVAXJ) hits the rejection path
//! and gets a silent `nil` dict back. The fix mirrors
//! [`ungive/mediaremote-adapter`](https://github.com/ungive/mediaremote-adapter)
//! (BSD-3, sources vendored under `bundle/mediaremote/`): we ship a small
//! `MediaRemoteAdapter.framework` and invoke it from `/usr/bin/perl`. Perl
//! is Apple-signed, so its XPC calls pass the daemon check; the framework
//! reads the now-playing dict and prints JSON to stdout. We parse that JSON
//! and feed the metadata back to `audible_tab_for` for fuzzy-matching
//! against open Chrome / Safari tabs.
//!
//! Falls back to `None` whenever the helper is unavailable (un-bundled
//! `cargo run` build, missing framework, perl missing). The caller then
//! degrades to the existing media-host heuristic.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use objc2_foundation::NSBundle;
use serde::Deserialize;
use switcheur_core::PlaybackState;

/// Subset of the JSON dict the helper prints to stdout. The upstream emits
/// 20+ keys; we deserialize only the ones useful for tab matching plus the
/// `playing` flag for the row badge. Unknown keys are ignored
/// automatically by serde.
#[derive(Debug, Clone, Deserialize)]
struct HelperPayload {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    album: Option<String>,
    #[serde(rename = "bundleIdentifier", default)]
    bundle_id: Option<String>,
    /// `true` when the source is currently producing audio, `false` when
    /// it's a registered-but-paused session, absent when the daemon
    /// returned no info.
    #[serde(default)]
    playing: Option<bool>,
}

/// Parsed view of the helper's primary now-playing item, mapped to the
/// types the rest of the platform layer consumes. Empty fields are
/// surfaced as `None` so callers don't have to special-case empty
/// strings.
#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub bundle_id: Option<String>,
    pub state: PlaybackState,
}

impl NowPlaying {
    pub fn is_blank(&self) -> bool {
        self.title.as_deref().map_or(true, str::is_empty)
            && self.artist.as_deref().map_or(true, str::is_empty)
            && self.album.as_deref().map_or(true, str::is_empty)
    }
}

/// Filesystem paths needed to invoke the helper. Resolved once via
/// `NSBundle::mainBundle()` and cached for the process lifetime — the
/// values don't change while the app is running, and the lookup itself is
/// cheap but not zero-cost.
#[derive(Clone)]
struct HelperPaths {
    perl_script: PathBuf,
    framework: PathBuf,
}

/// Probe MediaRemote for the system "now playing" item. Returns `None` on
/// every failure path:
///
/// - un-bundled build (`cargo run`) — helper paths unresolvable, no fault
/// - bundled build but helper crashed / timed out / printed garbage
/// - macOS 15.4+ daemon refused (in which case the helper itself prints
///   `null` and we treat that as "no current item")
/// - timestamp / artwork keys present but `title`/`artist`/`album` all
///   blank
///
/// `timeout` caps how long we wait for `osascript`-style polling; we
/// match the upstream helper's internal 2 s deadline plus a small slack.
pub fn current_now_playing(timeout: Duration) -> Option<NowPlaying> {
    let paths = helper_paths()?;
    let raw = run_helper(&paths, timeout)?;
    let raw = raw.trim();
    // Helper prints `null` (literal four-byte string) when MediaRemote
    // returned nothing — short-circuit here so serde_json doesn't emit a
    // misleading parse error in the log path below.
    if raw.is_empty() || raw == "null" {
        return None;
    }
    let payload: HelperPayload = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("now_playing: helper output not valid JSON: {e}");
            return None;
        }
    };
    let state = match payload.playing {
        Some(true) => PlaybackState::Playing,
        Some(false) => PlaybackState::Paused,
        None => PlaybackState::Unknown,
    };
    let np = NowPlaying {
        title: payload.title.filter(|s| !s.is_empty()),
        artist: payload.artist.filter(|s| !s.is_empty()),
        album: payload.album.filter(|s| !s.is_empty()),
        bundle_id: payload.bundle_id.filter(|s| !s.is_empty()),
        state,
    };
    if np.is_blank() {
        return None;
    }
    Some(np)
}

/// Resolve the bundled helper framework + Perl wrapper paths. Cached for
/// the process lifetime: `None` means we're not running from a packaged
/// `.app` (typically `cargo run`), or the bundle layout isn't what
/// `bundle.sh` produced. `Some` means both paths exist on disk.
fn helper_paths() -> Option<&'static HelperPaths> {
    static CACHE: OnceLock<Option<HelperPaths>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            // Allow the integration probe (`cargo run --example probe_audio`)
            // and ad-hoc shell tests to point the resolver at a bundled
            // `.app` even when the Rust binary itself isn't running from
            // inside one. Env var wins over NSBundle when set.
            let bundle_path = if let Ok(p) = std::env::var("LESWITCHEUR_BUNDLE") {
                PathBuf::from(p)
            } else {
                let main = NSBundle::mainBundle();
                PathBuf::from(main.bundlePath().to_string())
            };
            // bundle/bundle.sh layout:
            //   <App>/Contents/Frameworks/MediaRemoteAdapter.framework
            //   <App>/Contents/Resources/mediaremote/mediaremote-adapter.pl
            // For a Rust binary launched via `cargo run`, mainBundle.bundlePath
            // points at the binary's parent dir — Frameworks/Resources don't
            // exist. We require both, otherwise abort.
            let framework = bundle_path
                .join("Contents/Frameworks/MediaRemoteAdapter.framework");
            let perl_script = bundle_path
                .join("Contents/Resources/mediaremote/mediaremote-adapter.pl");
            if !framework.exists() || !perl_script.exists() {
                tracing::debug!(
                    "now_playing helper unavailable (un-bundled run?); \
                     framework={} script={}",
                    framework.display(),
                    perl_script.display()
                );
                return None;
            }
            Some(HelperPaths {
                perl_script,
                framework,
            })
        })
        .as_ref()
}

/// Spawn `/usr/bin/perl <script> <framework> get --no-artwork` and read
/// stdout up to `timeout`. Polls `try_wait` every 10 ms — same pattern as
/// `browser::run_osascript` (`crates/switcheur-platform/src/macos/browser.rs:352`)
/// so the timeout semantics match the rest of the platform layer.
///
/// `--no-artwork` is mandatory: without it the helper emits a 50 KB
/// base64-encoded JPEG inline, which makes the stdout pipe quadratic to
/// drain and adds ~5 ms even when MediaRemote replies fast.
fn run_helper(paths: &HelperPaths, timeout: Duration) -> Option<String> {
    let mut child = match Command::new("/usr/bin/perl")
        .arg(&paths.perl_script)
        .arg(&paths.framework)
        .arg("get")
        .arg("--no-artwork")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("now_playing helper spawn failed: {e}");
            return None;
        }
    };

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let mut buf = String::new();
                if let Some(mut out) = child.stdout.take() {
                    use std::io::Read;
                    let _ = out.read_to_string(&mut buf);
                }
                return Some(buf);
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::debug!(
                        "now_playing helper timed out after {:?}",
                        timeout
                    );
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                tracing::warn!("now_playing helper try_wait failed: {e}");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

