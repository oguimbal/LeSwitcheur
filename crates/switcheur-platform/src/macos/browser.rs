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
/// Two stages:
/// 1. AppleScript: only the browser knows how to switch tabs. We set the
///    active tab on the target window (identified by the browser's
///    AppleScript `id`, which is distinct from any CGWindowID). We
///    deliberately do **not** try to un-minimize or reorder through
///    AppleScript — scripting dictionaries don't always honor those on
///    minimized windows, and the native path below handles both cleanly.
/// 2. Native activation: list the browser's windows via the regular
///    enumerator, match the target by the tab title that AppleScript just
///    made current, and call [`super::activate::activate_window`]. That's
///    the same code path a normal Cmd+Tab pick goes through, so minimize,
///    cross-Space, and fullscreen-Space behavior matches what users
///    already expect.
pub fn activate_tab(t: &BrowserTabRef) -> Result<()> {
    let script = activate_script(t);
    run_osascript(&script, SCAN_TIMEOUT).with_context(|| {
        format!(
            "applescript for {} tab window={} index={}",
            t.browser.display_name(),
            t.window_id,
            t.tab_index,
        )
    })?;

    // After switching the tab, the target window's AX title reflects the
    // tab title we captured at scan time. Match it to find the real
    // CGWindowID + minimized state, then hand off to the shared activator.
    //
    // `show_all_spaces=true` so cross-Space and fullscreen-Space targets
    // still surface — without it the AX layer only reports windows on the
    // current Space. Minimized windows show up with `minimized=true` so
    // `activate_window` knows to AX-un-minimize before the SLPS dance.
    let bundle = t.browser.bundle_id();
    let all = super::windows::list_windows(true).unwrap_or_default();
    let title_snapshot = t.title.as_ref();
    let mut browser_windows = all
        .into_iter()
        .filter(|w| w.bundle_id.as_deref() == Some(bundle));
    // Prefer an exact title match (covers the common case even when the
    // browser has multiple windows on multiple Spaces). If no title matches
    // — possible when the page title hasn't updated yet, or collides —
    // fall back to the frontmost window (first one enumerated in AX
    // order), which is the best guess available.
    let first: Option<WindowRef> = browser_windows.next();
    let target = if first
        .as_ref()
        .map(|w| w.title == title_snapshot)
        .unwrap_or(false)
    {
        first
    } else {
        let mut matched: Option<WindowRef> = None;
        for w in browser_windows {
            if w.title == title_snapshot {
                matched = Some(w);
                break;
            }
        }
        matched.or(first)
    };
    match target {
        Some(w) => super::activate::activate_window(&w),
        None => {
            // No browser window surfaced by either AX or CG — fall back to
            // activating the app as a whole so the user at least lands in
            // the browser. Happens when the browser was quit between scan
            // and click.
            tracing::debug!(
                "{} window not found post-applescript; falling back to activate_app",
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
fn run_osascript(script: &str, timeout: Duration) -> Result<String> {
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
