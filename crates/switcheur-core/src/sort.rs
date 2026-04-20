//! Result-ordering policy + an in-memory recency tracker.
//!
//! Sorting runs before the fuzzy matcher sees the items, so the user-visible
//! order when the query is empty follows [`SortOrder`]. With a non-empty query,
//! the matcher's score still dominates — recency/alphabetical is a tiebreaker
//! for items of equal fuzzy score, which is rare.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::model::WindowRef;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Most-recently-focused *window* first. Drives the alt-tab-back flow
    /// most users expect: picking a window bumps that single window to the
    /// top, so the "previously used window" is always one step away, even
    /// when the user has ten instances of the same app open.
    ///
    /// The legacy `recent_app` value deserializes into this variant — the
    /// old per-app grouping turned out to be unusable with multi-window
    /// apps like VSCode (ten sibling windows clogging the top of the list
    /// before the previous app could be reached).
    #[serde(alias = "recent_app")]
    RecentWindow,
    /// Alphabetical by window title (fallback to app name when title empty).
    Title,
    /// Alphabetical by app name, tie-broken by window title.
    AppName,
}

impl Default for SortOrder {
    fn default() -> Self {
        // Per-window MRU is the alt-tab-like behavior most users expect:
        // picking a window bumps that specific window to the top without
        // dragging every sibling window of the same app along with it.
        Self::RecentWindow
    }
}

/// Tracks when each app and window was last focused, in the current process's
/// lifetime. Intentionally not persisted — a fresh launch starts empty and
/// refills from the first few switcher interactions.
#[derive(Debug, Default)]
pub struct RecencyTracker {
    apps: HashMap<i32, Instant>,
    windows: HashMap<(i32, String), Instant>,
}

impl RecencyTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn note_app(&mut self, pid: i32) {
        self.apps.insert(pid, Instant::now());
    }

    pub fn note_window(&mut self, pid: i32, title: &str) {
        // Also bump the app — if we just focused one of its windows, the app
        // itself is implicitly active too.
        let now = Instant::now();
        self.apps.insert(pid, now);
        self.windows.insert((pid, title.to_owned()), now);
    }

    pub fn app_rank(&self, pid: i32) -> Option<Instant> {
        self.apps.get(&pid).copied()
    }

    pub fn window_rank(&self, pid: i32, title: &str) -> Option<Instant> {
        self.windows.get(&(pid, title.to_owned())).copied()
    }
}

/// Sort a window list in place according to [`SortOrder`]. Items with no
/// recency data fall to the bottom; the matcher's stable sort preserves their
/// original relative order.
pub fn sort_items(windows: &mut [WindowRef], order: SortOrder, tracker: &RecencyTracker) {
    match order {
        SortOrder::RecentWindow => {
            // No fallback to app_rank: if we fell back, every sibling window
            // of a recently-activated app would tie at the app's rank and
            // cluster at the top — exactly the "10 VSCode windows before the
            // previous window" pain this mode is trying to avoid. Windows
            // never explicitly focused drop to the bottom, and the stable
            // sort preserves their enumeration order down there.
            windows.sort_by_key(|w| Reverse(tracker.window_rank(w.pid, &w.title)));
        }
        SortOrder::Title => {
            windows.sort_by(|a, b| {
                a.display_title()
                    .to_lowercase()
                    .cmp(&b.display_title().to_lowercase())
            });
        }
        SortOrder::AppName => {
            windows.sort_by(|a, b| {
                a.app_name
                    .to_lowercase()
                    .cmp(&b.app_name.to_lowercase())
                    .then_with(|| {
                        a.display_title()
                            .to_lowercase()
                            .cmp(&b.display_title().to_lowercase())
                    })
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    fn w(pid: i32, app: &str, title: &str) -> WindowRef {
        WindowRef {
            id: 0,
            pid,
            title: title.into(),
            app_name: app.into(),
            bundle_id: None,
            icon_path: None,
            minimized: false,
        }
    }

    #[test]
    fn title_sort_is_case_insensitive_alpha() {
        let mut ws = vec![
            w(1, "Z", "banana"),
            w(2, "A", "Apple"),
            w(3, "M", "cherry"),
        ];
        sort_items(&mut ws, SortOrder::Title, &RecencyTracker::new());
        assert_eq!(
            ws.iter().map(|w| w.title.as_str()).collect::<Vec<_>>(),
            vec!["Apple", "banana", "cherry"]
        );
    }

    #[test]
    fn app_name_sort_then_title() {
        let mut ws = vec![
            w(1, "Safari", "b-tab"),
            w(2, "Mail", "inbox"),
            w(3, "Safari", "a-tab"),
        ];
        sort_items(&mut ws, SortOrder::AppName, &RecencyTracker::new());
        let titles: Vec<_> = ws.iter().map(|w| w.title.as_str()).collect();
        assert_eq!(titles, vec!["inbox", "a-tab", "b-tab"]);
    }

    #[test]
    fn legacy_recent_app_config_migrates_to_recent_window() {
        // Users upgrading from the pre-release default (sort_order = "recent_app")
        // must land on RecentWindow seamlessly — the whole point of removing
        // per-app grouping.
        let toml = r#"sort_order = "recent_app""#;
        #[derive(serde::Deserialize)]
        struct Wrap {
            sort_order: SortOrder,
        }
        let w: Wrap = toml::from_str(toml).expect("deserialize");
        assert_eq!(w.sort_order, SortOrder::RecentWindow);
    }

    #[test]
    fn recent_window_ranks_seen_windows_above_unseen() {
        let mut t = RecencyTracker::new();
        t.note_app(1);
        sleep(Duration::from_millis(2));
        t.note_window(2, "focused");
        // Only (2,"focused") has a window_rank. The other rows have none —
        // per-window MRU deliberately does NOT fall back to app_rank, so
        // those sink below, preserving their enumeration order among each
        // other (stable sort).
        let mut ws = vec![
            w(1, "A", "other"),
            w(2, "B", "focused"),
            w(2, "B", "not-focused"),
            w(3, "C", "unseen"),
        ];
        sort_items(&mut ws, SortOrder::RecentWindow, &t);
        let ordered: Vec<_> = ws.iter().map(|w| (w.pid, w.title.as_str())).collect();
        assert_eq!(ordered[0], (2, "focused"));
        assert_eq!(
            &ordered[1..],
            &[(1, "other"), (2, "not-focused"), (3, "unseen")][..]
        );
    }
}
