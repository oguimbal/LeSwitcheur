pub mod activate;
pub mod app_policy;
pub mod audio;
pub mod browser;
pub mod media_apps;
pub mod now_playing;
pub mod file_manager;
pub mod hotkey;
pub mod hotkey_service;
pub mod hotkey_tap;
pub mod icons;
pub mod llm;
pub mod machine_id;
pub mod panel;
pub mod permissions;
pub mod programs;
pub mod quick_type;
pub mod spotlight;
pub mod recency;
pub mod startup;
pub mod system_switcher;
pub mod windows;

pub use hotkey::MacHotkeyService;
pub use hotkey_service::HotkeyService;
pub use hotkey_tap::{
    is_system_reserved, HotkeyRecordSession, HotkeyTapError, HotkeyTapService, RecordOutcome,
};
pub use permissions::{
    ensure_accessibility, has_input_monitoring_permission, has_screen_recording_permission,
    prompt_accessibility, prompt_input_monitoring, request_accessibility_prompt,
    request_screen_recording_permission,
};
pub use quick_type::{ExclusionCell, QuickTypeError, QuickTypeEvent, QuickTypeService, ScrollDir};
pub use recency::{FocusedApp, FocusedAppCell, RecencyService};
pub use system_switcher::{SystemSwitcherError, SystemSwitcherEvent, SystemSwitcherService};

use anyhow::Result;
use std::time::Duration;
use switcheur_core::{
    AppRef, AudioRowRef, BrowserTabRef, LlmProvider, PlaybackState, ProgramRef, WindowRef,
};

use crate::{BrowserTabSource, CurrentlyPlayingSource, LlmLauncher, ProgramSource, WindowSource};

pub struct MacPlatform;

impl MacPlatform {
    pub fn new() -> Result<Self> {
        // Walk the Application directories once at startup so the catalogue
        // is ready by the time the user first opens the switcher. Runs on
        // the main thread — see `programs::prefetch_sync` docs.
        programs::prefetch_sync();
        Ok(Self)
    }

    /// Open the given URL in the user's default browser. Used by the
    /// launcher's "Open URL" row when the query is a pasted http/https link.
    pub fn open_url(&self, url: &str) -> Result<()> {
        llm::open_url(url)
    }
}

impl WindowSource for MacPlatform {
    fn list_windows(&self, show_all_spaces: bool) -> Result<Vec<WindowRef>> {
        windows::list_windows(show_all_spaces)
    }

    fn list_apps(&self) -> Result<Vec<AppRef>> {
        windows::list_apps()
    }

    fn activate_window(&self, w: &WindowRef) -> Result<()> {
        activate::activate_window(w)
    }

    fn activate_app(&self, a: &AppRef) -> Result<()> {
        activate::activate_app(a)
    }

    fn close_window(&self, w: &WindowRef) -> Result<()> {
        activate::close_window(w)
    }
}

impl ProgramSource for MacPlatform {
    fn list_programs(&self) -> Result<Vec<ProgramRef>> {
        Ok(programs::list_programs_cached())
    }

    fn launch_program(&self, p: &ProgramRef) -> Result<()> {
        programs::launch(p)
    }
}

impl LlmLauncher for MacPlatform {
    fn open_llm(&self, provider: LlmProvider, prompt: &str) -> Result<()> {
        llm::open_llm(provider, prompt)
    }
}

impl BrowserTabSource for MacPlatform {
    fn list_browser_tabs(&self) -> (Vec<BrowserTabRef>, bool) {
        browser::list_tabs()
    }

    fn activate_browser_tab(&self, t: &BrowserTabRef) -> Result<()> {
        browser::activate_tab(t)
    }
}

impl CurrentlyPlayingSource for MacPlatform {
    fn current_currently_playing(&self) -> Vec<AudioRowRef> {
        let sources = audio::current_audio_sources();
        // Probe MediaRemote once per call; pass the (possibly None) result to
        // every browser source so we don't pay the dispatch_async wait
        // multiple times. macOS 15.4+ rejects callers outside `com.apple.*`
        // — we get None and degrade to active-tab heuristics inside
        // `audible_tab_for`.
        let np = now_playing::current_now_playing(Duration::from_millis(400));
        let (np_title, np_artist, np_album, np_state) = np
            .as_ref()
            .map(|n| (n.title.as_deref(), n.artist.as_deref(), n.album.as_deref(), n.state))
            .unwrap_or((None, None, None, PlaybackState::Unknown));

        let mut rows: Vec<AudioRowRef> = sources
            .into_iter()
            .map(|s| match s {
                audio::AudioSource::App {
                    pid,
                    name,
                    bundle_id,
                } => {
                    let icon_path = bundle_id
                        .as_deref()
                        .and_then(|b| {
                            bundle_path_for(pid).and_then(|p| icons::icon_for_bundle(&p, b))
                        });
                    // When MediaRemote metadata matches this app (e.g.
                    // Spotify producing audio), enrich the row with the
                    // current track. Match by bundle id when available.
                    let mr_match = np
                        .as_ref()
                        .and_then(|n| n.bundle_id.as_deref())
                        .zip(bundle_id.as_deref())
                        .map(|(a, b)| a == b)
                        .unwrap_or(false);
                    AudioRowRef {
                        pid,
                        app_name: name,
                        bundle_id,
                        icon_path,
                        browser_tab: None,
                        browser: None,
                        state: PlaybackState::Playing,
                        track_title: if mr_match { np_title.map(str::to_string) } else { None },
                        track_artist: if mr_match { np_artist.map(str::to_string) } else { None },
                    }
                }
                audio::AudioSource::Browser {
                    browser,
                    app_pid,
                    app_name,
                    bundle_id,
                    ..
                } => {
                    let tab = browser::audible_tab_for(browser, np_title, np_artist, np_album);
                    let icon_path = tab.as_ref().and_then(|t| t.icon_path.clone()).or_else(|| {
                        bundle_id.as_deref().and_then(|b| {
                            bundle_path_for(app_pid).and_then(|p| icons::icon_for_bundle(&p, b))
                        })
                    });
                    AudioRowRef {
                        pid: app_pid,
                        app_name,
                        bundle_id,
                        icon_path,
                        browser_tab: tab,
                        browser: Some(browser),
                        state: PlaybackState::Playing,
                        track_title: np_title
                            .map(str::to_string)
                            .filter(|_| np_state != PlaybackState::Unknown),
                        track_artist: np_artist.map(str::to_string),
                    }
                }
            })
            .collect();

        // Phase B: enrich with paused/registered media apps via per-app
        // AppleScript (Spotify, Music, …). CoreAudio doesn't see paused
        // sources — they're not producing output — so without this the
        // switcher would never surface "Spotify on pause" the way Control
        // Center does. We dedupe against existing CoreAudio rows by
        // bundle id to avoid showing Spotify twice when it's also
        // currently producing.
        for m in media_apps::probe_all() {
            if rows.iter().any(|r| {
                r.bundle_id
                    .as_deref()
                    .map(|b| b == m.bundle_id)
                    .unwrap_or(false)
            }) {
                continue;
            }
            // Resolve a PID for the app even though it's not in CoreAudio
            // — needed for the row's "focus this app" Enter behaviour.
            let pid = pid_for_bundle(&m.bundle_id).unwrap_or(0);
            let icon_path = bundle_path_for(pid)
                .and_then(|p| icons::icon_for_bundle(&p, &m.bundle_id));
            rows.push(AudioRowRef {
                pid,
                app_name: m.app_name,
                bundle_id: Some(m.bundle_id),
                icon_path,
                browser_tab: None,
                browser: m.browser,
                state: m.state,
                track_title: m.track_title,
                track_artist: m.track_artist,
            });
        }

        // MediaRemote fallback: when the helper sees a registered now-playing
        // client (Spotify paused, etc.) but neither CoreAudio nor the
        // AppleScript probe surfaced it (e.g. TCC denied automation for
        // Spotify), inject a row from the helper data so the user still gets
        // parity with Control Center. Skip for browsers — the metadata
        // already enriches the matching CoreAudio browser row above, and a
        // standalone row would double-list Chrome on its own.
        if let Some(np) = np.as_ref() {
            if let Some(mr_bundle) = np.bundle_id.as_deref() {
                let already = rows.iter().any(|r| {
                    r.bundle_id
                        .as_deref()
                        .map(|b| b == mr_bundle)
                        .unwrap_or(false)
                });
                let is_browser = matches!(
                    mr_bundle,
                    "com.google.Chrome" | "com.apple.Safari"
                );
                if !already && !is_browser {
                    let pid = pid_for_bundle(mr_bundle).unwrap_or(0);
                    let app_name = np
                        .bundle_id
                        .as_deref()
                        .and_then(running_app_name)
                        .unwrap_or_else(|| {
                            // Last-resort label: derive from bundle id
                            // (e.g. com.spotify.client → "Spotify"). Not
                            // pretty but better than blank.
                            mr_bundle
                                .rsplit('.')
                                .next()
                                .map(|s| {
                                    let mut c = s.chars();
                                    c.next()
                                        .map(|f| f.to_ascii_uppercase().to_string()
                                            + c.as_str())
                                        .unwrap_or_else(|| s.to_string())
                                })
                                .unwrap_or_else(|| mr_bundle.to_string())
                        });
                    let icon_path = bundle_path_for(pid)
                        .and_then(|p| icons::icon_for_bundle(&p, mr_bundle));
                    rows.push(AudioRowRef {
                        pid,
                        app_name,
                        bundle_id: Some(mr_bundle.to_string()),
                        icon_path,
                        browser_tab: None,
                        browser: None,
                        state: np.state,
                        track_title: np.title.clone(),
                        track_artist: np.artist.clone(),
                    });
                }
            }
        }
        rows
    }

    fn toggle_audio_playback(&self, row: &AudioRowRef) -> Result<()> {
        // Browser tab → JS injection on the captured tab. Standalone media
        // apps → AppleScript scripting dictionary. Order matters: a row
        // for a browser may also have a known bundle id (com.google.Chrome)
        // which doesn't have a meaningful `playpause` script, so the tab
        // path must win.
        if let Some(tab) = row.browser_tab.as_ref() {
            return browser::toggle_tab_play_pause(tab).map_err(|e| anyhow::anyhow!(e));
        }
        match row.bundle_id.as_deref() {
            Some(b) => media_apps::toggle_play_pause(b).map_err(|e| anyhow::anyhow!(e)),
            None => anyhow::bail!("no toggle path for row without bundle id or browser tab"),
        }
    }
}

/// Localised display name for the first running instance of a bundle.
/// Used by the MediaRemote fallback to label the row with the app's
/// proper name (e.g. "Spotify") rather than the bundle id. Returns
/// `None` when the app isn't running or has no localised name.
fn running_app_name(bundle_id: &str) -> Option<String> {
    use objc2_app_kit::NSRunningApplication;
    use objc2_foundation::NSString;
    let key = NSString::from_str(bundle_id);
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&key);
    apps.iter()
        .next()
        .and_then(|a| a.localizedName())
        .map(|s| s.to_string())
}

/// Resolve the running PID for a bundle id via NSRunningApplication. Used
/// for media-app rows surfaced via AppleScript when CoreAudio isn't
/// reporting them (paused sources). Returns the first match — apps with
/// multiple instances are rare for the media apps we probe (Spotify /
/// Music are single-instance by design).
fn pid_for_bundle(bundle_id: &str) -> Option<i32> {
    use objc2_app_kit::NSRunningApplication;
    use objc2_foundation::NSString;
    let key = NSString::from_str(bundle_id);
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&key);
    apps.iter().next().map(|a| a.processIdentifier())
}

/// Look up the on-disk bundle path for a running app PID via
/// NSRunningApplication. Used to feed `icons::icon_for_bundle` so the
/// "Currently Playing" row uses the same cached icon as the rest of the
/// switcher rows.
fn bundle_path_for(pid: i32) -> Option<String> {
    use objc2_app_kit::NSRunningApplication;
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    app.bundleURL()?.path().map(|s| s.to_string())
}
