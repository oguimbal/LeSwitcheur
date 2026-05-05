//! AppleScript probes for well-known media apps that expose `player state` /
//! `current track` via their scripting dictionary. Used to surface paused
//! sources (e.g. Spotify on pause) in the "Currently Playing" rows — CoreAudio
//! only reports `IsRunningOutput == true` processes, which excludes paused
//! sources.
//!
//! Same shape as `browser.rs`: short osascript timeout, per-app one-shot
//! probe, every failure path resolves to `None` so a hung / refused
//! automation prompt can't stall the switcher open.
//!
//! Apps we probe:
//! - **Spotify** (`com.spotify.client`) — exposes `player state`,
//!   `current track`'s `name` / `artist`.
//! - **Apple Music** (`com.apple.Music`) — same shape.
//!
//! Future candidates (not yet wired): Podcasts, TV, VLC, IINA. The pattern
//! is mechanical — add an entry to [`PROBES`].

use std::time::Duration;

use switcheur_core::{Browser, PlaybackState};

use super::browser::run_osascript;

/// Match the timeout used by browser-tab scans — same constraint (a hung
/// automation prompt mustn't block the switcher's first paint).
const SCAN_TIMEOUT: Duration = Duration::from_millis(400);

/// One snapshot of a media-app's playback state via AppleScript.
#[derive(Debug, Clone)]
pub struct MediaAppState {
    /// Display name (e.g. "Spotify").
    pub app_name: String,
    /// Bundle id for icon resolution / dedupe with CoreAudio sources.
    pub bundle_id: String,
    pub state: PlaybackState,
    pub track_title: Option<String>,
    pub track_artist: Option<String>,
    /// Identifies the app as a known browser when applicable. Always
    /// `None` for the media-app probes (Spotify, Music, …) — browsers
    /// don't expose `player state`, they go through the CoreAudio +
    /// audible-tab path instead.
    pub browser: Option<Browser>,
}

/// One known-media-app probe. The AppleScript runs in `osascript`, a short
/// fixed format: `state\x1Ftitle\x1Fartist`. `state` is the literal string
/// "playing" / "paused" / "stopped" — we map back to [`PlaybackState`] in
/// [`probe`].
struct Probe {
    app_name: &'static str,
    bundle_id: &'static str,
    script: &'static str,
}

/// Apps probed in order. The order doesn't matter for correctness — the
/// caller dedupes by `bundle_id` and merges with CoreAudio sources — but it
/// determines presentation order when multiple probes succeed without an
/// overlapping CoreAudio entry.
const PROBES: &[Probe] = &[
    Probe {
        app_name: "Spotify",
        bundle_id: "com.spotify.client",
        script: SPOTIFY_SCRIPT,
    },
    Probe {
        app_name: "Music",
        bundle_id: "com.apple.Music",
        script: MUSIC_SCRIPT,
    },
];

/// Probe every known media app. Returns the snapshot for each app where
/// the probe succeeded *and* the app reports a non-stopped player state
/// (we don't surface "stopped" — too noisy, the app is just sitting idle).
pub fn probe_all() -> Vec<MediaAppState> {
    PROBES
        .iter()
        .filter_map(probe)
        .filter(|m| !matches!(m.state, PlaybackState::Unknown))
        .collect()
}

fn probe(p: &Probe) -> Option<MediaAppState> {
    let raw = match run_osascript(p.script, SCAN_TIMEOUT) {
        Ok(s) => s,
        Err(_) => return None,
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    const US: char = '\u{1F}';
    let mut parts = raw.splitn(3, US);
    let state_str = parts.next()?.trim();
    let title = parts.next()?.trim().to_string();
    let artist = parts.next()?.trim().to_string();
    let state = match state_str {
        "playing" => PlaybackState::Playing,
        "paused" => PlaybackState::Paused,
        // "stopped" / unknown / empty → surface as Unknown so the caller's
        // filter drops it. Same reason as the empty-string short-circuit
        // above: a stopped media app isn't part of "currently playing".
        _ => PlaybackState::Unknown,
    };
    Some(MediaAppState {
        app_name: p.app_name.into(),
        bundle_id: p.bundle_id.into(),
        state,
        track_title: if title.is_empty() { None } else { Some(title) },
        track_artist: if artist.is_empty() { None } else { Some(artist) },
        browser: None,
    })
}

/// Send a play/pause toggle to the named app via its scripting dictionary.
/// Used by the row's optional play/pause button (and Enter on a paused row
/// when the user wants to resume without focusing the app).
///
/// Returns `Ok(())` even when the app isn't running — toggling a stopped
/// app should not error from the caller's perspective; it's just a no-op.
pub fn toggle_play_pause(bundle_id: &str) -> Result<(), String> {
    let script = match bundle_id {
        "com.spotify.client" => SPOTIFY_TOGGLE,
        "com.apple.Music" => MUSIC_TOGGLE,
        _ => return Err(format!("no toggle script for bundle {bundle_id}")),
    };
    run_osascript(script, SCAN_TIMEOUT)
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
}

/// Spotify exposes `player state`, `name of current track`, `artist of
/// current track` via its scripting dictionary. The `if it is running`
/// guard ensures the script never resurrects a quit Spotify — we only
/// want to read state from an already-running session.
const SPOTIFY_SCRIPT: &str = r#"tell application "Spotify"
    if it is not running then return ""
    set sep to (ASCII character 31)
    try
        set s to player state as text
    on error
        return ""
    end try
    try
        set t to name of current track
    on error
        set t to ""
    end try
    try
        set a to artist of current track
    on error
        set a to ""
    end try
    return s & sep & t & sep & a
end tell"#;

const SPOTIFY_TOGGLE: &str = r#"tell application "Spotify"
    if it is not running then return ""
    playpause
    return "ok"
end tell"#;

/// Apple Music (formerly iTunes). Same shape as Spotify but the keys are
/// `player state`, `name`, `artist` on the scripting class. Both apps
/// emit the literal strings "playing" / "paused" / "stopped" for state.
const MUSIC_SCRIPT: &str = r#"tell application "Music"
    if it is not running then return ""
    set sep to (ASCII character 31)
    try
        set s to player state as text
    on error
        return ""
    end try
    try
        set t to name of current track
    on error
        set t to ""
    end try
    try
        set a to artist of current track
    on error
        set a to ""
    end try
    return s & sep & t & sep & a
end tell"#;

const MUSIC_TOGGLE: &str = r#"tell application "Music"
    if it is not running then return ""
    playpause
    return "ok"
end tell"#;
