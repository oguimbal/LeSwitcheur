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
    /// Most-recently-activated app first. Cheap: one NSWorkspace observer.
    RecentApp,
    /// Most-recently-focused *window* first. More precise but needs one
    /// AXObserver per app on the focus-changed notification — costs ~1% CPU.
    RecentWindow,
    /// Alphabetical by window title (fallback to app name when title empty).
    Title,
    /// Alphabetical by app name, tie-broken by window title.
    AppName,
}

impl Default for SortOrder {
    fn default() -> Self {
        Self::RecentApp
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
        SortOrder::RecentApp => {
            windows.sort_by_key(|w| Reverse(tracker.app_rank(w.pid)));
        }
        SortOrder::RecentWindow => {
            windows.sort_by_key(|w| {
                Reverse(
                    tracker
                        .window_rank(w.pid, &w.title)
                        .or_else(|| tracker.app_rank(w.pid)),
                )
            });
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
    fn recent_app_puts_last_noted_first() {
        let mut t = RecencyTracker::new();
        t.note_app(1);
        sleep(Duration::from_millis(2));
        t.note_app(2);
        let mut ws = vec![w(1, "Old", "x"), w(2, "New", "y"), w(3, "Unknown", "z")];
        sort_items(&mut ws, SortOrder::RecentApp, &t);
        assert_eq!(
            ws.iter().map(|w| w.pid).collect::<Vec<_>>(),
            vec![2, 1, 3]
        );
    }

    #[test]
    fn recent_window_prefers_specific_window_then_app_fallback() {
        let mut t = RecencyTracker::new();
        t.note_app(1);
        sleep(Duration::from_millis(2));
        t.note_window(2, "focused");
        // pid=2 wins thanks to window note; pid=1 has an app note; pid=3 has nothing.
        let mut ws = vec![
            w(1, "A", "other"),
            w(2, "B", "focused"),
            w(2, "B", "not-focused"),
            w(3, "C", "unseen"),
        ];
        sort_items(&mut ws, SortOrder::RecentWindow, &t);
        let ordered: Vec<_> = ws.iter().map(|w| (w.pid, w.title.as_str())).collect();
        // pid 2 "focused" first (window rank), pid 2 "not-focused" next (app rank
        // inherited from note_window bumping the app too), pid 1 "other" (old
        // app rank), pid 3 last (unknown).
        assert_eq!(ordered[0], (2, "focused"));
        assert_eq!(ordered[3], (3, "unseen"));
    }
}
