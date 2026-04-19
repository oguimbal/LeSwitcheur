//! Scrape open Chrome tabs via AppleScript and focus a chosen tab.
//!
//! Used by the switcher's fallback tier: when nothing in the window / program
//! / eval lists matches the query, the UI asks this module for a snapshot of
//! every open Chrome tab and fuzzy-matches the user's query against it.
//!
//! Everything runs best-effort. "Chrome not running", "automation permission
//! denied" and "osascript hung" all resolve to an empty vec — the caller then
//! falls through to the LLM tier without the user seeing an error.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::NSString;
use switcheur_core::{Browser, BrowserTabRef, WindowRef};

/// AppleScript that lists every (window-id, tab-index, title, url) tuple for
/// every Chrome window. Uses ASCII control characters as separators so titles
/// containing pipes or tabs don't confuse the parser:
///   * `\x1F` (US, Unit Separator) between fields on one line
///   * `\x1E` (RS, Record Separator) between records (tabs)
///
/// The `if not running` guard means the script returns an empty string
/// without ever launching Chrome — crucial, since we only want to scrape,
/// never resurrect, a quit browser.
const LIST_SCRIPT: &str = r#"tell application "Google Chrome"
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

/// Hard ceiling for the scan. If `osascript` runs longer than this we kill it
/// and return an empty vec — the UI silently falls through to the LLM tier
/// rather than freezing or flashing a placeholder.
const SCAN_TIMEOUT: Duration = Duration::from_millis(800);

/// Scan every Chrome window's tabs. Silent on every failure (not running,
/// permission denied, script timeout, garbled output) — returns an empty vec
/// and logs at debug so the fallback tier simply skips to "Ask AI".
pub fn list_tabs() -> Vec<BrowserTabRef> {
    let raw = match run_osascript(LIST_SCRIPT, SCAN_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("chrome tab scan failed: {e:#}");
            return Vec::new();
        }
    };
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let icon = chrome_icon_path();
    parse_tabs(&raw, icon)
}

/// Parse the `\x1E`/`\x1F`-separated output produced by [`LIST_SCRIPT`] into
/// [`BrowserTabRef`]s. Malformed lines are skipped rather than aborting the
/// whole batch — one weird URL shouldn't hide every tab.
fn parse_tabs(raw: &str, icon: Option<PathBuf>) -> Vec<BrowserTabRef> {
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
                Browser::Chrome,
                wid,
                ti,
                title.into(),
                url.into(),
                icon.clone(),
            ))
        })
        .collect()
}

/// Focus the given tab: switch Chrome to the right tab, then drive the
/// owning window through the same three-layer native activation path the
/// switcher uses for any other window pick (SLPS + AXRaise + un-minimize
/// via AX). Using the existing dance gives us cross-Space / fullscreen-
/// Space / un-minimize behavior for free — AppleScript alone couldn't
/// reliably do any of those on macOS 14+.
///
/// Two stages:
/// 1. AppleScript: only Chrome knows how to switch tabs. We set the active
///    tab index on the target window (identified by Chrome's AppleScript
///    `id`, which is distinct from any CGWindowID). We deliberately do
///    **not** try to un-minimize or reorder through AppleScript — Chrome's
///    scripting dictionary doesn't always honor those on minimized
///    windows, and the native path below handles both cleanly.
/// 2. Native activation: list Chrome's windows via the regular enumerator,
///    match the target by the tab title that AppleScript just made
///    current, and call [`super::activate::activate_window`]. That's the
///    same code path a normal Cmd+Tab pick goes through, so minimize,
///    cross-Space, and fullscreen-Space behavior matches what users
///    already expect.
pub fn activate_tab(t: &BrowserTabRef) -> Result<()> {
    let script = format!(
        r#"tell application "Google Chrome"
    set active tab index of (first window whose id is {wid}) to {ti}
end tell"#,
        wid = t.window_id,
        ti = t.tab_index,
    );
    run_osascript(&script, SCAN_TIMEOUT).with_context(|| {
        format!(
            "applescript for chrome tab window={} index={}",
            t.window_id, t.tab_index
        )
    })?;

    // After switching the tab, the target Chrome window's AX title reflects
    // the tab title we captured at scan time. Match it to find the real
    // CGWindowID + minimized state, then hand off to the shared activator.
    //
    // `show_all_spaces=true` so cross-Space and fullscreen-Space targets
    // still surface — without it the AX layer only reports windows on the
    // current Space. Minimized Chrome windows show up with `minimized=true`
    // so `activate_window` knows to AX-un-minimize before the SLPS dance.
    let bundle = t.browser.bundle_id();
    let all = super::windows::list_windows(true).unwrap_or_default();
    let title_snapshot = t.title.as_ref();
    let mut chrome_windows = all
        .into_iter()
        .filter(|w| w.bundle_id.as_deref() == Some(bundle));
    // Prefer an exact title match (covers the common case even when
    // Chrome has multiple windows on multiple Spaces). If no title
    // matches — possible when the page title hasn't updated yet, or
    // collides — fall back to Chrome's frontmost window (first one
    // enumerated in AX order), which is the best guess available.
    let first_chrome: Option<WindowRef> = chrome_windows.next();
    let target = if first_chrome
        .as_ref()
        .map(|w| w.title == title_snapshot)
        .unwrap_or(false)
    {
        first_chrome
    } else {
        let mut matched: Option<WindowRef> = None;
        for w in chrome_windows {
            if w.title == title_snapshot {
                matched = Some(w);
                break;
            }
        }
        matched.or(first_chrome)
    };
    match target {
        Some(w) => super::activate::activate_window(&w),
        None => {
            // No Chrome window surfaced by either AX or CG — fall back to
            // activating the app as a whole so the user at least lands in
            // Chrome. Happens when Chrome was quit between scan and click.
            tracing::debug!(
                "chrome window not found post-applescript; falling back to activate_app"
            );
            let pid = chrome_pid();
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

/// Resolve the pid of a running Chrome instance via NSRunningApplication.
/// Returns `None` when Chrome isn't running.
fn chrome_pid() -> Option<i32> {
    use objc2_app_kit::NSWorkspace;
    let ws = NSWorkspace::sharedWorkspace();
    let running = ws.runningApplications();
    for i in 0..running.count() {
        let app = running.objectAtIndex(i);
        let bundle = app.bundleIdentifier().map(|s| s.to_string()).unwrap_or_default();
        if bundle == Browser::Chrome.bundle_id() {
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

/// Cache the resolved Chrome icon path for the process lifetime. Resolving
/// the bundle URL via NSWorkspace is cheap but not free; the icon itself is
/// on-disk PNG-cached inside [`super::icons::icon_for_bundle`].
fn chrome_icon_path() -> Option<PathBuf> {
    static CACHED: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let ws = NSWorkspace::sharedWorkspace();
            let bundle_id = NSString::from_str(Browser::Chrome.bundle_id());
            let url = ws.URLForApplicationWithBundleIdentifier(&bundle_id)?;
            let path = url.path()?.to_string();
            super::icons::icon_for_bundle(&path, Browser::Chrome.bundle_id())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_returns_none() {
        let out = parse_tabs("", None);
        assert!(out.is_empty());
    }

    #[test]
    fn parse_single_record() {
        let raw = format!(
            "123{US}2{US}Hello World{US}https://example.com/{RS}",
            US = '\u{1F}',
            RS = '\u{1E}',
        );
        let tabs = parse_tabs(&raw, None);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].window_id, 123);
        assert_eq!(tabs[0].tab_index, 2);
        assert_eq!(tabs[0].title.as_ref(), "Hello World");
        assert_eq!(tabs[0].url.as_ref(), "https://example.com/");
        assert_eq!(tabs[0].host(), "example.com");
    }

    #[test]
    fn parse_multiple_records_across_windows() {
        let raw = format!(
            "1{US}1{US}A{US}https://a.test/{RS}1{US}2{US}B{US}https://b.test/{RS}7{US}1{US}C{US}https://c.test/{RS}",
            US = '\u{1F}',
            RS = '\u{1E}',
        );
        let tabs = parse_tabs(&raw, None);
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
        let tabs = parse_tabs(&raw, None);
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
        let tabs = parse_tabs(&raw, None);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].title.as_ref(), "A | B — C: D");
        assert_eq!(tabs[0].url.as_ref(), "https://ex.test/?x=y&z=1");
    }
}
