//! Scrape open browser tabs via AppleScript and focus a chosen tab.
//!
//! Used by the switcher's fallback tier: when nothing in the window / program
//! / eval lists matches the query, the UI asks this module for a snapshot of
//! every open tab across supported browsers and fuzzy-matches the user's
//! query against it.
//!
//! Supported browsers: Google Chrome, Safari. Each browser is scanned in its
//! own short-lived thread so one hung / unresponsive browser doesn't block
//! the other.
//!
//! Everything runs best-effort. "Browser not running", "automation permission
//! denied" and "osascript hung" all resolve to an empty vec — the caller then
//! falls through to the LLM tier without the user seeing an error.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::NSString;
use switcheur_core::{Browser, BrowserTabRef, WindowRef};

/// AppleScript template for Chrome: lists every (window-id, tab-index, title,
/// url) tuple. Uses ASCII control characters as separators so titles
/// containing pipes or tabs don't confuse the parser:
///   * `\x1F` (US, Unit Separator) between fields on one line
///   * `\x1E` (RS, Record Separator) between records (tabs)
///
/// The `if not running` guard means the script returns an empty string
/// without ever launching the browser — crucial, since we only want to
/// scrape, never resurrect, a quit browser.
const CHROME_LIST_SCRIPT: &str = r#"tell application "Google Chrome"
    if not running then return ""
    set sep to (ASCII character 31)
    set recSep to (ASCII character 30)
    set output to ""
    set wList to windows
    repeat with wi from 1 to count of wList
        set w to item wi of wList
        set wid to id of w
        set tList to tabs of w
        repeat with ti from 1 to count of tList
            set t to item ti of tList
            try
                set ttitle to title of t
            on error
                set ttitle to ""
            end try
            try
                set turl to URL of t
            on error
                set turl to ""
            end try
            set output to output & wid & sep & ti & sep & ttitle & sep & turl & recSep
        end repeat
    end repeat
    return output
end tell"#;

/// AppleScript template for Safari. Safari's Tab class exposes `name` where
/// Chrome's exposes `title`; URL is the same. Window `id` is a stable integer
/// within the Safari session, same contract as Chrome.
const SAFARI_LIST_SCRIPT: &str = r#"tell application "Safari"
    if not running then return ""
    set sep to (ASCII character 31)
    set recSep to (ASCII character 30)
    set output to ""
    set wList to windows
    repeat with wi from 1 to count of wList
        set w to item wi of wList
        try
            set wid to id of w
        on error
            set wid to 0
        end try
        try
            set tList to tabs of w
        on error
            set tList to {}
        end try
        repeat with ti from 1 to count of tList
            set t to item ti of tList
            try
                set ttitle to name of t
            on error
                set ttitle to ""
            end try
            try
                set turl to URL of t
            on error
                set turl to ""
            end try
            if turl is missing value then set turl to ""
            set output to output & wid & sep & ti & sep & ttitle & sep & turl & recSep
        end repeat
    end repeat
    return output
end tell"#;

/// Hard ceiling for each browser's scan. Each browser runs in its own thread,
/// so total wall-clock is still bounded by this — not the sum across
/// browsers. If `osascript` runs longer than this we kill it and return a
/// failure for that browser; the caller may retry on the next keystroke
/// (see [`crate::macos::MacPlatform::list_browser_tabs`]).
///
/// 3s comfortably covers Chrome with ~50+ tabs when the browser thread is
/// busy. The scan runs off the UI thread, so the UI never stalls.
const SCAN_TIMEOUT: Duration = Duration::from_millis(3000);

/// Browsers we try to scan on every fallback tick. Order doesn't matter —
/// results are merged and the UI sorts them via its own fuzzy-match scorer.
const SUPPORTED: &[Browser] = &[Browser::Chrome, Browser::Safari];

/// Scan every supported browser's tabs, concurrently. Running the scans in
/// parallel keeps the worst-case wall-clock at [`SCAN_TIMEOUT`] even when
/// one browser hangs.
///
/// `all_failed` is set when every browser we tried returned an error
/// (timeout, permission denied, garbled output). In that case the caller
/// should NOT cache the empty result — a retry on the next keystroke may
/// succeed (Chrome often stutters on the first AppleScript of a switcher
/// session). A browser that's simply not running counts as success (empty
/// tab list, no error), so `all_failed` stays false.
pub fn list_tabs() -> (Vec<BrowserTabRef>, bool) {
    let handles: Vec<_> = SUPPORTED
        .iter()
        .copied()
        .map(|b| std::thread::spawn(move || list_tabs_for(b)))
        .collect();
    let mut out = Vec::new();
    let mut attempted = 0usize;
    let mut failed = 0usize;
    for h in handles {
        attempted += 1;
        match h.join() {
            Ok(Ok(mut v)) => out.append(&mut v),
            Ok(Err(())) => failed += 1,
            Err(_) => failed += 1,
        }
    }
    let all_failed = attempted > 0 && failed == attempted;
    (out, all_failed)
}

/// Run the per-browser list script and parse the result. `Ok(vec)` — scan
/// completed (possibly empty, e.g. browser not running). `Err(())` — scan
/// actually failed (timeout, osascript error); caller may want to retry.
fn list_tabs_for(browser: Browser) -> Result<Vec<BrowserTabRef>, ()> {
    let script = list_script(browser);
    let raw = match run_osascript(script, SCAN_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("{} tab scan failed: {e:#}", browser.display_name());
            return Err(());
        }
    };
    if raw.trim().is_empty() {
        tracing::debug!("{} not running or no tabs", browser.display_name());
        return Ok(Vec::new());
    }
    let icon = browser_icon_path(browser);
    let tabs = parse_tabs(browser, &raw, icon);
    tracing::debug!("{} tab scan: {} tabs", browser.display_name(), tabs.len());
    Ok(tabs)
}

fn list_script(browser: Browser) -> &'static str {
    match browser {
        Browser::Chrome => CHROME_LIST_SCRIPT,
        Browser::Safari => SAFARI_LIST_SCRIPT,
    }
}

/// Parse the `\x1E`/`\x1F`-separated output into [`BrowserTabRef`]s.
/// Malformed lines are skipped rather than aborting the whole batch — one
/// weird URL shouldn't hide every tab.
fn parse_tabs(browser: Browser, raw: &str, icon: Option<PathBuf>) -> Vec<BrowserTabRef> {
    const US: char = '\u{1F}';
    const RS: char = '\u{1E}';
    raw.split(RS)
        .filter_map(|rec| {
            let rec = rec.trim_matches(|c: char| c == '\n' || c == '\r');
            if rec.is_empty() {
                return None;
            }
            let mut parts = rec.splitn(4, US);
            let wid: i64 = parts.next()?.parse().ok()?;
            let ti: i64 = parts.next()?.parse().ok()?;
            let title = parts.next()?.to_string();
            let url = parts.next()?.to_string();
            Some(BrowserTabRef::new(
                browser,
                wid,
                ti,
                title.into(),
                url.into(),
                icon.clone(),
            ))
        })
        .collect()
}

/// Focus the given tab. Dispatches on `t.browser` for the tab-switch
/// AppleScript (Chrome and Safari expose different properties), then hands
/// off to the shared native activation path so cross-Space / fullscreen /
/// un-minimize behavior matches any other window pick.
///
/// Two stages — AppleScript only drives tab-internal content; native AX/
/// SLPS owns window management (unminimize, focus, raise). AppleScript
/// window writes (`miniaturized`, `index`, `activate`) are unreliable
/// across apps and macOS versions.
///
/// 1. Switch tab via AppleScript. This works on miniaturized windows —
///    Chrome/Safari update their internal state (and, empirically,
///    refresh the window's AX title to reflect the newly-active tab)
///    even when the window isn't visible.
/// 2. Enumerate browser windows and match the target by CGWindowID
///    (Safari only — its `id of window` == NSWindow windowNumber), then
///    by `t.title`. Chrome decorates the AX title with suffixes like
///    `" - High memory usage - 2.4 GB - Google Chrome – {profile}"`, so
///    exact equality won't match — fall back to `starts_with` then
///    `contains`. Safari's AX title equals the tab title verbatim, so
///    the exact branch hits first.
/// 3. [`super::activate::activate_window`] — the Cmd+Tab path:
///    kAXMinimizedAttribute write + AXRaise (SLPS skipped on fresh
///    un-miniaturize; see `activate.rs`).
pub fn activate_tab(t: &BrowserTabRef) -> Result<()> {
    let bundle = t.browser.bundle_id();

    // AppleScript tab switch. Safe on miniaturized windows; Chrome and
    // Safari both refresh the window's AX title to the new active tab's
    // page title as part of this call, so post-switch matching against
    // `t.title` below is reliable.
    let script = activate_script(t);
    run_osascript(&script, SCAN_TIMEOUT).with_context(|| {
        format!(
            "applescript for {} tab window={} index={}",
            t.browser.display_name(),
            t.window_id,
            t.tab_index,
        )
    })?;

    // `show_all_spaces=true` surfaces cross-Space and miniaturized windows.
    let browser_windows: Vec<WindowRef> = super::windows::list_windows(true)
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.bundle_id.as_deref() == Some(bundle))
        .collect();

    // Matching order: CGWindowID (Safari exact) → AX title == t.title
    // (Safari exact) → `starts_with(t.title)` (Chrome, whose AX title
    // decorates the page title with app/profile/memory suffixes) →
    // `contains(t.title)`. NO `first()` fallback: picking an arbitrary
    // browser window on mismatch focuses the wrong window (the original
    // complaint). Unmatched degrades to `activate_app` — browser comes
    // forward without yanking focus to a random sibling.
    let tt: &str = t.title.as_ref();
    let target: Option<WindowRef> = browser_windows
        .iter()
        .find(|w| w.id as i64 == t.window_id)
        .or_else(|| browser_windows.iter().find(|w| w.title == tt))
        .or_else(|| {
            (!tt.is_empty()).then(|| browser_windows.iter().find(|w| w.title.starts_with(tt)))?
        })
        .or_else(|| {
            (!tt.is_empty()).then(|| browser_windows.iter().find(|w| w.title.contains(tt)))?
        })
        .cloned();
    tracing::debug!(
        browser = t.browser.display_name(),
        window_id = t.window_id,
        tab_index = t.tab_index,
        t_title = %tt,
        candidates = browser_windows.len(),
        candidate_titles = ?browser_windows.iter().map(|w| &w.title).collect::<Vec<_>>(),
        target_found = target.is_some(),
        target_cg_id = target.as_ref().map(|w| w.id),
        target_minimized = target.as_ref().map(|w| w.minimized),
        "activate_tab target lookup"
    );

    match target {
        Some(w) => super::activate::activate_window(&w),
        None => {
            // Browser quit between scan and click, or AX hasn't surfaced
            // any window for this bundle. Fall back to app-level focus so
            // the user at least lands in the browser.
            tracing::debug!(
                "{} window not found; falling back to activate_app",
                t.browser.display_name()
            );
            let pid = browser_pid(t.browser);
            match pid {
                Some(pid) => super::activate::activate_app(&switcheur_core::AppRef {
                    pid,
                    name: t.browser.display_name().to_string(),
                    bundle_id: Some(bundle.to_string()),
                    icon_path: t.icon_path.clone(),
                }),
                None => Ok(()),
            }
        }
    }
}

/// Build the per-browser "switch to this tab" AppleScript. Chrome uses
/// `active tab index`; Safari uses `current tab` set to a tab reference.
fn activate_script(t: &BrowserTabRef) -> String {
    match t.browser {
        Browser::Chrome => format!(
            r#"tell application "Google Chrome"
    set active tab index of (first window whose id is {wid}) to {ti}
end tell"#,
            wid = t.window_id,
            ti = t.tab_index,
        ),
        Browser::Safari => format!(
            r#"tell application "Safari"
    set targetWindow to (first window whose id is {wid})
    set current tab of targetWindow to tab {ti} of targetWindow
end tell"#,
            wid = t.window_id,
            ti = t.tab_index,
        ),
    }
}

/// Resolve the pid of a running browser instance via NSRunningApplication.
/// Returns `None` when the browser isn't running.
fn browser_pid(browser: Browser) -> Option<i32> {
    use objc2_app_kit::NSWorkspace;
    let ws = NSWorkspace::sharedWorkspace();
    let running = ws.runningApplications();
    let want = browser.bundle_id();
    for i in 0..running.count() {
        let app = running.objectAtIndex(i);
        let bundle = app.bundleIdentifier().map(|s| s.to_string()).unwrap_or_default();
        if bundle == want {
            return Some(app.processIdentifier());
        }
    }
    None
}

/// Drive `osascript -e <script>` with a hard timeout. Returns stdout (trimmed
/// of no extras) or an error describing what went wrong.
pub(crate) fn run_osascript(script: &str, timeout: Duration) -> Result<String> {
    let mut child = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning osascript")?;

    // Simple sleep-poll — osascript is cheap and one-shot, so we don't need
    // a full async runtime here. Each iteration sleeps 10 ms and re-checks
    // whether the child has exited; at 800 ms timeout that's ≤80 iterations.
    let started = std::time::Instant::now();
    loop {
        match child.try_wait().context("waiting on osascript")? {
            Some(status) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut out) = child.stdout.take() {
                    use std::io::Read;
                    let _ = out.read_to_string(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    use std::io::Read;
                    let _ = err.read_to_string(&mut stderr);
                }
                if !status.success() {
                    anyhow::bail!(
                        "osascript exited {:?}: {}",
                        status.code(),
                        stderr.trim()
                    );
                }
                return Ok(stdout);
            }
            None => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!("osascript timed out after {:?}", timeout);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Cache the resolved browser icon path for the process lifetime, keyed by
/// bundle id. Resolving the bundle URL via NSWorkspace is cheap but not
/// free; the icon itself is on-disk PNG-cached inside
/// [`super::icons::icon_for_bundle`].
fn browser_icon_path(browser: Browser) -> Option<PathBuf> {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, Option<PathBuf>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let bundle = browser.bundle_id();
    if let Ok(map) = cache.lock() {
        if let Some(v) = map.get(bundle) {
            return v.clone();
        }
    }
    let resolved = resolve_icon(bundle);
    if let Ok(mut map) = cache.lock() {
        map.insert(bundle, resolved.clone());
    }
    resolved
}

fn resolve_icon(bundle: &str) -> Option<PathBuf> {
    let ws = NSWorkspace::sharedWorkspace();
    let bundle_id = NSString::from_str(bundle);
    let url = ws.URLForApplicationWithBundleIdentifier(&bundle_id)?;
    let path = url.path()?.to_string();
    super::icons::icon_for_bundle(&path, bundle)
}

/// Hosts (suffix-matched) that virtually always mean "this tab plays
/// audio". When MediaRemote can't pin the tab via title metadata, we use
/// this list to single out the most likely candidate among open tabs.
/// Order doesn't matter; matching is suffix-based on the tab's host.
const MEDIA_HOSTS: &[&str] = &[
    "youtube.com",
    "music.youtube.com",
    "spotify.com",
    "open.spotify.com",
    "soundcloud.com",
    "twitch.tv",
    "vimeo.com",
    "music.apple.com",
    "deezer.com",
    "tidal.com",
    "bandcamp.com",
    "mixcloud.com",
    "music.amazon.com",
    "podcasts.apple.com",
];

/// Identify which tab of `browser` is producing audio. We scan all tabs
/// once, then resolve in order:
///
/// 1. **Metadata match** — substring search of MediaRemote's title /
///    artist / album against each tab's title (case-insensitive). The
///    strongest signal: a YouTube tab titled "(2) Bohemian Rhapsody -
///    Queen - YouTube" matches the title needle "Bohemian Rhapsody".
/// 2. **Media-host fallback** — when no metadata cleared step 1 (silent
///    MediaRemote, or daemon refused on macOS 15.4+, or page title doesn't
///    contain the song name), pick a tab whose host suffix is in
///    [`MEDIA_HOSTS`]. Only use this when it's unambiguous (exactly one
///    such tab); otherwise we'd be guessing.
///
/// Returns `None` when neither tier resolves a tab. The caller treats
/// `None` as "focus the browser app without picking a tab" — the audio
/// row then displays "<Browser> · Now Playing" rather than misleadingly
/// labelling the front-window's active tab.
pub fn audible_tab_for(
    browser: Browser,
    np_title: Option<&str>,
    np_artist: Option<&str>,
    np_album: Option<&str>,
) -> Option<BrowserTabRef> {
    let tabs = list_tabs_for(browser).ok()?;
    if tabs.is_empty() {
        return None;
    }
    if let Some(hit) = match_tab_by_metadata(&tabs, np_title, np_artist, np_album) {
        return Some(hit);
    }
    if let Some(hit) = match_tab_by_media_host(&tabs) {
        return Some(hit);
    }
    // No active-tab fallback by default: when MediaRemote can't pin the
    // tab and there's no unambiguous media-host candidate, the
    // foreground tab is *probably wrong* (the user often opens a new
    // tab while audio plays in another). Better to surface "<Browser> ·
    // Now Playing" with no tab title than to claim a wrong one. Per-tab
    // detection requires either MediaRemote (blocked on 15.4+ from
    // non-Apple callers) or a bundled Apple-signed helper.
    None
}

/// Resolve the (window_id, tab_index) pair of the browser's frontmost
/// window's active tab, then look the row up in the already-fetched
/// `tabs` list so the [`BrowserTabRef`] we return has the same identity
/// as the rows used elsewhere in the switcher.
fn active_tab_of_front_window(browser: Browser, tabs: &[BrowserTabRef]) -> Option<BrowserTabRef> {
    let script = match browser {
        Browser::Chrome => CHROME_ACTIVE_TAB_SCRIPT,
        Browser::Safari => SAFARI_ACTIVE_TAB_SCRIPT,
    };
    let raw = run_osascript(script, SCAN_TIMEOUT).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    const US: char = '\u{1F}';
    let mut parts = raw.splitn(2, US);
    let wid: i64 = parts.next()?.parse().ok()?;
    let ti: i64 = parts.next()?.parse().ok()?;
    tabs.iter()
        .find(|t| t.window_id == wid && t.tab_index == ti)
        .cloned()
}

/// AppleScript: "id<US>active-tab-index" of the front window. Empty
/// string when the browser isn't running. Same separator convention as
/// the list scripts so we can reuse the parse path.
const CHROME_ACTIVE_TAB_SCRIPT: &str = r#"tell application "Google Chrome"
    if not running then return ""
    if (count of windows) = 0 then return ""
    set sep to (ASCII character 31)
    set w to front window
    set wid to id of w
    set ti to active tab index of w
    return wid & sep & ti
end tell"#;

const SAFARI_ACTIVE_TAB_SCRIPT: &str = r#"tell application "Safari"
    if not running then return ""
    if (count of windows) = 0 then return ""
    set sep to (ASCII character 31)
    set w to front window
    try
        set wid to id of w
    on error
        set wid to 0
    end try
    set ti to (index of (current tab of w))
    return wid & sep & ti
end tell"#;

/// Score each tab by how well it matches MediaRemote metadata, then pick
/// the best. A title match weighs more than an artist match, which weighs
/// more than an album match — the title is what's displayed on the page,
/// so it's the strongest signal that this tab is producing the audio.
///
/// Substring (case-insensitive) for each non-blank needle ≥ 4 chars
/// (shorter words match too many tabs). Returns `None` when no tab scores
/// above 0.
fn match_tab_by_metadata(
    tabs: &[BrowserTabRef],
    title: Option<&str>,
    artist: Option<&str>,
    album: Option<&str>,
) -> Option<BrowserTabRef> {
    let title = needle(title);
    let artist = needle(artist);
    let album = needle(album);
    if title.is_none() && artist.is_none() && album.is_none() {
        return None;
    }
    let mut best: Option<(u32, &BrowserTabRef)> = None;
    for t in tabs {
        let hay = t.title.to_lowercase();
        let mut score = 0u32;
        if let Some(n) = &title {
            if hay.contains(n.as_str()) {
                score += 4;
            }
        }
        if let Some(n) = &artist {
            if hay.contains(n.as_str()) {
                score += 2;
            }
        }
        if let Some(n) = &album {
            if hay.contains(n.as_str()) {
                score += 1;
            }
        }
        if score > 0 && best.map_or(true, |(s, _)| score > s) {
            best = Some((score, t));
        }
    }
    best.map(|(_, t)| t.clone())
}

/// Pick exactly one tab whose host is a known media site. Returns `None`
/// when zero or multiple such tabs exist — guessing among ambiguous
/// candidates is what the user reported as wrong; better to surface "no
/// tab" and let the row display the browser name only.
fn match_tab_by_media_host(tabs: &[BrowserTabRef]) -> Option<BrowserTabRef> {
    let mut hits = tabs.iter().filter(|t| is_media_host(t.host()));
    let first = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    Some(first.clone())
}

fn is_media_host(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let host = host.to_lowercase();
    MEDIA_HOSTS
        .iter()
        .any(|m| host == *m || host.ends_with(&format!(".{m}")))
}

fn needle(s: Option<&str>) -> Option<String> {
    let s = s?.trim();
    if s.len() < 4 {
        return None;
    }
    Some(s.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_returns_none() {
        let out = parse_tabs(Browser::Chrome, "", None);
        assert!(out.is_empty());
    }

    #[test]
    fn parse_single_record() {
        let raw = format!(
            "123{US}2{US}Hello World{US}https://example.com/{RS}",
            US = '\u{1F}',
            RS = '\u{1E}',
        );
        let tabs = parse_tabs(Browser::Chrome, &raw, None);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].window_id, 123);
        assert_eq!(tabs[0].tab_index, 2);
        assert_eq!(tabs[0].title.as_ref(), "Hello World");
        assert_eq!(tabs[0].url.as_ref(), "https://example.com/");
        assert_eq!(tabs[0].host(), "example.com");
        assert_eq!(tabs[0].browser, Browser::Chrome);
    }

    #[test]
    fn parse_multiple_records_across_windows() {
        let raw = format!(
            "1{US}1{US}A{US}https://a.test/{RS}1{US}2{US}B{US}https://b.test/{RS}7{US}1{US}C{US}https://c.test/{RS}",
            US = '\u{1F}',
            RS = '\u{1E}',
        );
        let tabs = parse_tabs(Browser::Chrome, &raw, None);
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[0].window_id, 1);
        assert_eq!(tabs[2].window_id, 7);
        assert_eq!(tabs[2].host(), "c.test");
    }

    #[test]
    fn parse_skips_malformed_records() {
        let raw = format!(
            "1{US}1{US}OK{US}https://ok.test/{RS}broken record{RS}2{US}1{US}Good{US}https://g.test/{RS}",
            US = '\u{1F}',
            RS = '\u{1E}',
        );
        let tabs = parse_tabs(Browser::Chrome, &raw, None);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].title.as_ref(), "OK");
        assert_eq!(tabs[1].title.as_ref(), "Good");
    }

    #[test]
    fn parse_preserves_title_with_special_chars() {
        // Titles can contain pipes, dashes, colons — they must survive.
        let raw = format!(
            "9{US}3{US}A | B — C: D{US}https://ex.test/?x=y&z=1{RS}",
            US = '\u{1F}',
            RS = '\u{1E}',
        );
        let tabs = parse_tabs(Browser::Chrome, &raw, None);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].title.as_ref(), "A | B — C: D");
        assert_eq!(tabs[0].url.as_ref(), "https://ex.test/?x=y&z=1");
    }

    #[test]
    fn parse_tags_safari_origin() {
        let raw = format!(
            "5{US}1{US}Safari Page{US}https://apple.com/{RS}",
            US = '\u{1F}',
            RS = '\u{1E}',
        );
        let tabs = parse_tabs(Browser::Safari, &raw, None);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].browser, Browser::Safari);
        assert_eq!(tabs[0].host(), "apple.com");
    }

    #[test]
    fn activate_script_dispatches_per_browser() {
        let chrome = BrowserTabRef::new(
            Browser::Chrome,
            42,
            3,
            std::sync::Arc::from("x"),
            std::sync::Arc::from("https://x.test/"),
            None,
        );
        assert!(activate_script(&chrome).contains(r#"tell application "Google Chrome""#));
        assert!(activate_script(&chrome).contains("active tab index"));

        let safari = BrowserTabRef::new(
            Browser::Safari,
            42,
            3,
            std::sync::Arc::from("x"),
            std::sync::Arc::from("https://x.test/"),
            None,
        );
        assert!(activate_script(&safari).contains(r#"tell application "Safari""#));
        assert!(activate_script(&safari).contains("current tab"));
    }
}
