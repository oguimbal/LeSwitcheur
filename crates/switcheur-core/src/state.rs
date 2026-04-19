//! UI-agnostic state machine. The GPUI view reads from and drives this.
//! Keeping it pure Rust makes every transition unit-testable.

use std::sync::Arc;

use crate::matcher::{FuzzyMatcher, MatchResult};
use crate::model::{Item, LlmProvider, ProgramRef};

const MAX_PROGRAMS: usize = 3;

/// Show "Ask <Provider>" rows after browser-tab matches when at most this
/// many tabs matched (and no window matched). Above this, the tab list is a
/// strong enough signal on its own — adding LLM rows just pushes real matches
/// off-screen.
const ASK_LLM_MAX_TABS: usize = 3;

/// Minimum query length (in Unicode scalars, after trimming) before the
/// switcher asks the host to scrape browser tabs. Short queries (1-2 chars)
/// are too generic — the AppleScript fetch can be 50-300 ms and the typical
/// "aa" prefix would pull in dozens of false-positive matches from the tab
/// haystack, clutter the list, and fire osascript on every quick key repeat.
const MIN_QUERY_LEN_FOR_BROWSER_TABS: usize = 3;

/// Which section of the switcher the keyboard cursor currently lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Programs,
    Windows,
    /// Right-side pane fed externally (zoxide today, possibly other sources
    /// later). Independent stream — not produced by rerank.
    Dirs,
}

pub struct SwitcherState {
    items: Vec<Item>,
    programs: Vec<Item>,
    /// Right-pane suggestions (currently zoxide dirs). Populated externally
    /// via [`SwitcherState::set_dirs`] — the subprocess call runs off the UI
    /// thread, so we deliberately keep this out of [`SwitcherState::rerank`].
    dirs: Vec<Item>,
    /// All browser tabs fetched for the current switcher session, unfiltered.
    /// `None` = not yet fetched; `Some(vec)` = fetched (possibly empty, e.g.
    /// browser not running or permission denied). Populated externally via
    /// [`SwitcherState::set_browser_tabs`] after an off-thread AppleScript
    /// call. [`SwitcherState::rerank`] fuzzy-matches against this cache when
    /// the fallback tier is reached.
    browser_tabs_cache: Option<Vec<Item>>,
    browser_tabs_integration: bool,
    query: String,
    filtered: Vec<MatchResult>,
    filtered_programs: Vec<MatchResult>,
    selected_idx: usize,
    selected_program: usize,
    selected_dir: usize,
    active_section: Section,
    matcher: FuzzyMatcher,
    eval_result: Option<String>,
    /// URL extracted from the current query, when the query looks like one.
    /// Populated alongside `eval_result` in `set_query` and consumed in
    /// `rerank` to render the "Open URL" launcher row.
    detected_url: Option<Arc<str>>,
    llm_provider_order: Vec<LlmProvider>,
    ask_llm_enabled: bool,
}

impl SwitcherState {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            programs: Vec::new(),
            dirs: Vec::new(),
            browser_tabs_cache: None,
            browser_tabs_integration: true,
            query: String::new(),
            filtered: Vec::new(),
            filtered_programs: Vec::new(),
            selected_idx: 0,
            selected_program: 0,
            selected_dir: 0,
            active_section: Section::Windows,
            matcher: FuzzyMatcher::new(),
            eval_result: None,
            detected_url: None,
            llm_provider_order: LlmProvider::default_order(),
            ask_llm_enabled: true,
        }
    }

    /// Replace the preferred order used to populate the "Ask LLM" fallback
    /// rows. Rerank runs so any currently-shown fallback reflects the new
    /// order immediately.
    pub fn set_llm_provider_order(&mut self, order: Vec<LlmProvider>) {
        self.llm_provider_order = order;
        self.rerank();
    }

    /// Toggle the "Ask LLM" fallback rows on/off. When disabled, the
    /// fallback section is suppressed even if no other items match.
    pub fn set_ask_llm_enabled(&mut self, enabled: bool) {
        self.ask_llm_enabled = enabled;
        self.rerank();
    }

    /// Toggle the browser-tabs fallback tier. When disabled the state never
    /// reports [`SwitcherState::needs_browser_tabs`] as true and any cached
    /// tabs are ignored in [`SwitcherState::rerank`].
    pub fn set_browser_tabs_integration(&mut self, enabled: bool) {
        if self.browser_tabs_integration == enabled {
            return;
        }
        self.browser_tabs_integration = enabled;
        if !enabled {
            self.browser_tabs_cache = None;
        }
        self.rerank();
    }

    /// Install the browser tabs cache for the current switcher session. The
    /// caller resolves this asynchronously via AppleScript, then calls in
    /// with the (possibly empty) result. Rerank runs so tabs immediately
    /// factor into the fallback tier.
    ///
    /// Unlike every other rerank trigger, this one **preserves** the user's
    /// current selection: the delivery is async and happens at an
    /// unpredictable moment (possibly while the user is arrow-keying through
    /// the window matches), so resetting the cursor would feel like the UI
    /// snatched focus for no reason. The selection is only clamped when the
    /// new list is shorter than the old cursor position.
    pub fn set_browser_tabs(&mut self, tabs: Vec<Item>) {
        self.browser_tabs_cache = Some(tabs);
        self.rerank_inner(RerankReset::PreserveSelection);
    }

    /// Forget any fetched browser tabs. Called when the switcher closes so
    /// the next open starts with a fresh scan.
    pub fn clear_browser_tabs(&mut self) {
        if self.browser_tabs_cache.is_none() {
            return;
        }
        self.browser_tabs_cache = None;
        self.rerank();
    }

    /// True when the host should kick off an AppleScript scan: the user has
    /// typed enough to make a tab search useful, the integration is on, and
    /// no scan has been delivered for this switcher session yet.
    pub fn needs_browser_tabs(&self) -> bool {
        self.browser_tabs_integration
            && self.browser_tabs_cache.is_none()
            && self.query_long_enough_for_browser_tabs()
    }

    /// True while the scan is in flight — triggers the spinner that the view
    /// appends below the results list so the user sees motion while we wait.
    pub fn browser_tabs_loading(&self) -> bool {
        self.needs_browser_tabs()
    }

    /// Does the current query clear the minimum length we require before
    /// scraping browser tabs? Counts Unicode scalars after trimming so a
    /// leading space doesn't throw the threshold off.
    fn query_long_enough_for_browser_tabs(&self) -> bool {
        self.query.trim().chars().count() >= MIN_QUERY_LEN_FOR_BROWSER_TABS
    }

    /// Replace the candidate set (typically called each time the switcher opens).
    pub fn set_items(&mut self, items: Vec<Item>) {
        self.items = items;
        self.rerank();
    }

    /// Drop the window with the given CGWindowID from the candidate set and
    /// re-rank. Used after the user closes a window from the row × button so
    /// the dead row disappears immediately, before the platform's window list
    /// has caught up.
    pub fn remove_window(&mut self, id: u64) {
        self.items.retain(|it| !matches!(it, Item::Window(w) if w.id == id));
        self.rerank();
    }

    /// Replace the installed-program catalogue. Called once on startup (and on
    /// each open, cheap since it's an Arc clone).
    pub fn set_programs(&mut self, programs: Vec<Arc<ProgramRef>>) {
        self.programs = programs.into_iter().map(Item::Program).collect();
        self.rerank();
    }

    /// Replace the right-pane directory suggestions (zoxide today). The
    /// caller resolves these asynchronously; we just store the result and
    /// reset the per-pane cursor. If the user had focus in the dirs pane
    /// and the new list is empty, focus snaps back to the windows pane so
    /// `selected()` keeps returning something useful.
    pub fn set_dirs(&mut self, dirs: Vec<Item>) {
        let was_focus = self.active_section == Section::Dirs;
        let presence_changed = self.dirs.is_empty() != dirs.is_empty();
        self.dirs = dirs;
        self.selected_dir = 0;
        if was_focus && self.dirs.is_empty() {
            self.active_section = Section::Windows;
        }
        // The "Ask LLM" fallback is suppressed when dirs are present, so the
        // right pane can take over the full width. Rerank when dirs toggle
        // between empty and non-empty so the fallback rows appear/disappear.
        if presence_changed {
            self.rerank();
        }
        // When the left list has nothing to show but dirs do, move keyboard
        // focus there so Enter activates a dir row without a manual Tab.
        if self.filtered.is_empty()
            && self.filtered_programs.is_empty()
            && !self.dirs.is_empty()
            && self.active_section == Section::Windows
        {
            self.active_section = Section::Dirs;
        }
    }

    pub fn dirs(&self) -> &[Item] {
        &self.dirs
    }

    pub fn selected_dir_idx(&self) -> usize {
        self.selected_dir
    }

    /// True when the dirs pane has any content to show. Used by the view to
    /// gate rendering — the right column collapses entirely when empty.
    pub fn dirs_visible(&self) -> bool {
        !self.dirs.is_empty()
    }

    /// Move keyboard focus into the dirs pane (no-op if it's empty). Used by
    /// the Tab / Right-arrow handlers in the view.
    pub fn focus_dirs(&mut self) {
        if self.dirs.is_empty() {
            return;
        }
        self.active_section = Section::Dirs;
        if self.selected_dir >= self.dirs.len() {
            self.selected_dir = 0;
        }
    }

    /// Move keyboard focus back to the windows pane (the default home).
    pub fn focus_windows(&mut self) {
        self.active_section = Section::Windows;
    }

    /// Jump the dirs selection to a specific index + activate the section.
    /// Used on hover/click of a dir row.
    pub fn set_selected_dir(&mut self, idx: usize) {
        if self.dirs.is_empty() {
            return;
        }
        self.active_section = Section::Dirs;
        self.selected_dir = idx.min(self.dirs.len() - 1);
    }

    pub fn set_query(&mut self, q: impl Into<String>) {
        self.query = q.into();
        // URL detection short-circuits eval — a pasted URL is never an
        // expression, and running the JS engine on it is pure overhead.
        self.detected_url = crate::url::detect(&self.query).map(Arc::from);
        self.eval_result = if self.detected_url.is_some() {
            None
        } else {
            crate::math::try_eval(&self.query)
                .or_else(|| crate::js::try_eval(&self.query))
        };
        self.rerank();
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn eval_result(&self) -> Option<&str> {
        self.eval_result.as_deref()
    }

    pub fn filtered(&self) -> &[MatchResult] {
        &self.filtered
    }

    pub fn filtered_programs(&self) -> &[MatchResult] {
        &self.filtered_programs
    }

    pub fn selected_idx(&self) -> usize {
        self.selected_idx
    }

    pub fn selected_program_idx(&self) -> usize {
        self.selected_program
    }

    pub fn active_section(&self) -> Section {
        self.active_section
    }

    /// True when the programs section should be rendered in the UI: query is
    /// non-empty AND at least one program matched.
    pub fn programs_visible(&self) -> bool {
        !self.query.trim().is_empty() && !self.filtered_programs.is_empty()
    }

    pub fn selected(&self) -> Option<&Item> {
        match self.active_section {
            Section::Programs => self
                .filtered_programs
                .get(self.selected_program)
                .map(|m| &m.item),
            Section::Windows => self.filtered.get(self.selected_idx).map(|m| &m.item),
            Section::Dirs => self.dirs.get(self.selected_dir),
        }
    }

    pub fn move_up(&mut self) {
        if self.active_section == Section::Dirs {
            if self.dirs.is_empty() {
                return;
            }
            self.selected_dir = if self.selected_dir == 0 {
                self.dirs.len() - 1
            } else {
                self.selected_dir - 1
            };
            return;
        }
        if self.programs_visible() {
            match self.active_section {
                Section::Windows => {
                    if self.selected_idx == 0 {
                        // Jump up into the programs section — land on its last row
                        // (the one closest to the input).
                        self.active_section = Section::Programs;
                        self.selected_program = self.filtered_programs.len() - 1;
                        return;
                    }
                    self.selected_idx -= 1;
                    return;
                }
                Section::Programs => {
                    if self.filtered_programs.is_empty() {
                        return;
                    }
                    self.selected_program = if self.selected_program == 0 {
                        self.filtered_programs.len() - 1
                    } else {
                        self.selected_program - 1
                    };
                    return;
                }
                Section::Dirs => unreachable!("handled above"),
            }
        }
        if self.filtered.is_empty() {
            return;
        }
        self.selected_idx = if self.selected_idx == 0 {
            self.filtered.len() - 1
        } else {
            self.selected_idx - 1
        };
    }

    pub fn move_down(&mut self) {
        if self.active_section == Section::Dirs {
            if self.dirs.is_empty() {
                return;
            }
            self.selected_dir = (self.selected_dir + 1) % self.dirs.len();
            return;
        }
        if self.programs_visible() {
            match self.active_section {
                Section::Programs => {
                    if self.selected_program + 1 >= self.filtered_programs.len() {
                        // Fall back into the windows section on its first row.
                        self.active_section = Section::Windows;
                        self.selected_idx = 0;
                        return;
                    }
                    self.selected_program += 1;
                    return;
                }
                Section::Windows => {
                    if self.filtered.is_empty() {
                        return;
                    }
                    self.selected_idx = (self.selected_idx + 1) % self.filtered.len();
                    return;
                }
                Section::Dirs => unreachable!("handled above"),
            }
        }
        if self.filtered.is_empty() {
            return;
        }
        self.selected_idx = (self.selected_idx + 1) % self.filtered.len();
    }

    /// Jump the windows selection to a specific filtered-index. Out-of-range
    /// indices are clamped; calling on an empty list is a no-op.
    pub fn set_selected(&mut self, idx: usize) {
        if self.filtered.is_empty() {
            return;
        }
        self.active_section = Section::Windows;
        self.selected_idx = idx.min(self.filtered.len() - 1);
    }

    /// Jump the programs selection to a specific index + activate the section.
    /// Used on hover/click of a program row.
    pub fn set_selected_program(&mut self, idx: usize) {
        if self.filtered_programs.is_empty() {
            return;
        }
        self.active_section = Section::Programs;
        self.selected_program = idx.min(self.filtered_programs.len() - 1);
    }

    fn rerank(&mut self) {
        self.rerank_inner(RerankReset::ResetSelection);
    }

    fn rerank_inner(&mut self, reset: RerankReset) {
        // URL launcher short-circuits everything else — a pasted URL is never
        // a fuzzy match for a window/program, and eval rows don't apply.
        if let Some(url) = &self.detected_url {
            self.filtered = vec![MatchResult {
                item: Item::OpenUrl(url.clone()),
                score: 0,
                indices: Vec::new(),
            }];
            self.filtered_programs.clear();
            self.selected_idx = 0;
            self.selected_program = 0;
            self.active_section = Section::Windows;
            return;
        }
        self.filtered = self.matcher.rank(&self.query, &self.items);
        let window_count = self.filtered.len();
        if self.query.trim().is_empty() {
            self.filtered_programs.clear();
        } else {
            let mut ranked = self.matcher.rank(&self.query, &self.programs);
            ranked.truncate(MAX_PROGRAMS);
            // The programs section sits above the query input. Reversing so
            // the best match renders at the bottom puts the strongest match
            // closest to the input the user is typing into.
            ranked.reverse();
            self.filtered_programs = ranked;
        }
        // Browser tabs are not a fallback — they're a complementary source,
        // appended after the window matches so the user can jump to a tab
        // even when a window also matches. Only kicks in past the minimum
        // query length (see [`MIN_QUERY_LEN_FOR_BROWSER_TABS`]) and when a
        // scan has actually been delivered for this session.
        let mut tab_count = 0usize;
        if self.browser_tabs_integration && self.query_long_enough_for_browser_tabs() {
            if let Some(cache) = &self.browser_tabs_cache {
                if !cache.is_empty() {
                    let tab_matches = self.matcher.rank(&self.query, cache);
                    tab_count = tab_matches.len();
                    self.filtered.extend(tab_matches);
                }
            }
        }
        // "Ask <Provider>" rows — appended after tabs when the window tier
        // is empty and the tab signal is thin (≤ ASK_LLM_MAX_TABS). Thin tab
        // matches are easy to overshoot, so offering an AI handoff keeps the
        // flow going without a retry. The zero-tab case subsumes the legacy
        // "nothing matched" fallback. Suppressed while a scan is in flight
        // so the rows don't flash in and out when tabs are about to arrive.
        let browser_tabs_pending = self.browser_tabs_integration
            && self.browser_tabs_cache.is_none()
            && self.query_long_enough_for_browser_tabs();
        if self.ask_llm_enabled
            && !self.query.trim().is_empty()
            && window_count == 0
            && tab_count <= ASK_LLM_MAX_TABS
            && self.filtered_programs.is_empty()
            && self.eval_result.is_none()
            && self.dirs.is_empty()
            && !browser_tabs_pending
        {
            let query: Arc<str> = Arc::from(self.query.as_str());
            self.filtered
                .extend(self.llm_provider_order.iter().copied().map(|provider| {
                    MatchResult {
                        item: Item::AskLlm {
                            provider,
                            query: query.clone(),
                        },
                        score: 0,
                        indices: Vec::new(),
                    }
                }));
        }
        match reset {
            RerankReset::ResetSelection => {
                self.selected_idx = 0;
                self.selected_program = 0;
                self.active_section = Section::Windows;
            }
            RerankReset::PreserveSelection => {
                // Clamp to the new lengths — the cursor may have been beyond
                // the last row if the pool shrank (rare here, but possible if
                // a later refactor calls this from a non-append path).
                if self.filtered.is_empty() {
                    self.selected_idx = 0;
                } else if self.selected_idx >= self.filtered.len() {
                    self.selected_idx = self.filtered.len() - 1;
                }
                if self.filtered_programs.is_empty() {
                    self.selected_program = 0;
                } else if self.selected_program >= self.filtered_programs.len() {
                    self.selected_program = self.filtered_programs.len() - 1;
                }
                // Leave `active_section` alone: the user chose it.
            }
        }
    }
}

/// Whether a [`SwitcherState::rerank_inner`] call should blow away the
/// current selection (default, because the input that drove the rerank
/// changed) or keep it wherever the user parked the arrow-key cursor
/// (used by async deliveries like [`SwitcherState::set_browser_tabs`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RerankReset {
    ResetSelection,
    PreserveSelection,
}

impl Default for SwitcherState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProgramRef, WindowRef};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn win(app: &str, title: &str) -> Item {
        Item::Window(Arc::new(WindowRef {
            id: 0,
            pid: 0,
            title: title.into(),
            app_name: app.into(),
            bundle_id: None,
            icon_path: None,
            minimized: false,
        }))
    }

    fn prog(name: &str) -> Arc<ProgramRef> {
        Arc::new(ProgramRef {
            name: name.into(),
            bundle_id: None,
            bundle_path: PathBuf::from(format!("/Applications/{}.app", name)),
            icon_path: None,
        })
    }

    #[test]
    fn empty_state_has_no_selection() {
        let s = SwitcherState::new();
        assert!(s.selected().is_none());
    }

    #[test]
    fn set_items_selects_first() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("A", "a"), win("B", "b")]);
        assert_eq!(s.selected_idx(), 0);
        assert_eq!(s.selected().unwrap().primary(), "a");
    }

    #[test]
    fn move_down_wraps() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("A", "a"), win("B", "b")]);
        s.move_down();
        assert_eq!(s.selected_idx(), 1);
        s.move_down();
        assert_eq!(s.selected_idx(), 0);
    }

    #[test]
    fn move_up_wraps() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("A", "a"), win("B", "b")]);
        s.move_up();
        assert_eq!(s.selected_idx(), 1);
    }

    #[test]
    fn query_resets_selection() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("A", "alpha"), win("B", "beta")]);
        s.move_down();
        assert_eq!(s.selected_idx(), 1);
        s.set_query("a");
        assert_eq!(s.selected_idx(), 0);
    }

    #[test]
    fn set_selected_clamps_to_last() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("A", "a"), win("B", "b")]);
        s.set_selected(99);
        assert_eq!(s.selected_idx(), 1);
    }

    #[test]
    fn set_selected_on_empty_is_noop() {
        let mut s = SwitcherState::new();
        s.set_selected(0);
        assert!(s.selected().is_none());
    }

    #[test]
    fn no_moves_on_empty_filter() {
        // A query that matches nothing populates the filter with the
        // "Ask LLM" fallback rows once the browser-tabs tier has finished
        // its scan (here simulated as an empty cache). Navigation cycles
        // among providers instead of being a no-op.
        let mut s = SwitcherState::new();
        s.set_items(vec![win("A", "alpha")]);
        s.set_browser_tabs(Vec::new());
        s.set_query("zzzzzz");
        s.move_down();
        s.move_up();
        assert!(matches!(s.selected(), Some(Item::AskLlm { .. })));
    }

    // --- Programs section tests ---

    #[test]
    fn programs_hidden_when_query_empty() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_programs(vec![prog("Safari"), prog("Mail")]);
        assert!(!s.programs_visible());
        assert_eq!(s.active_section(), Section::Windows);
    }

    #[test]
    fn programs_visible_when_query_matches() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_programs(vec![prog("Safari"), prog("Mail")]);
        s.set_query("saf");
        assert!(s.programs_visible());
    }

    #[test]
    fn up_from_windows_first_jumps_to_programs_last() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Safari", "page")]);
        s.set_programs(vec![prog("Safari"), prog("Safe Sleep")]);
        s.set_query("saf");
        assert_eq!(s.active_section(), Section::Windows);
        assert_eq!(s.selected_idx(), 0);
        s.move_up();
        assert_eq!(s.active_section(), Section::Programs);
        assert_eq!(s.selected_program_idx(), s.filtered_programs().len() - 1);
    }

    #[test]
    fn down_from_programs_last_returns_to_windows_first() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Safari", "page"), win("Safari", "other")]);
        s.set_programs(vec![prog("Safari")]);
        s.set_query("saf");
        s.move_up();
        assert_eq!(s.active_section(), Section::Programs);
        s.move_down();
        assert_eq!(s.active_section(), Section::Windows);
        assert_eq!(s.selected_idx(), 0);
    }

    #[test]
    fn rerank_resets_to_windows_section() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Safari", "page")]);
        s.set_programs(vec![prog("Safari")]);
        s.set_query("saf");
        s.move_up();
        assert_eq!(s.active_section(), Section::Programs);
        s.set_query("safa");
        assert_eq!(s.active_section(), Section::Windows);
    }

    #[test]
    fn programs_capped_at_three() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_programs(vec![
            prog("Safari"),
            prog("Safari Tech"),
            prog("Safe"),
            prog("Safes"),
            prog("SafeBox"),
        ]);
        s.set_query("saf");
        assert_eq!(s.filtered_programs().len(), 3);
    }

    // --- LLM fallback tests ---

    #[test]
    fn llm_fallback_shown_when_no_match() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        // Simulate a completed (empty) browser-tab scan so the LLM tier
        // isn't held back by an in-flight fetch.
        s.set_browser_tabs(Vec::new());
        s.set_query("xyznomatchatall");
        // 4 providers in default order (Mistral first).
        assert_eq!(s.filtered().len(), 4);
        match s.selected().unwrap() {
            Item::AskLlm { provider, query } => {
                assert_eq!(*provider, LlmProvider::Mistral);
                assert_eq!(query.as_ref(), "xyznomatchatall");
            }
            _ => panic!("expected AskLlm"),
        }
    }

    #[test]
    fn llm_fallback_respects_configured_order() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_browser_tabs(Vec::new());
        s.set_llm_provider_order(vec![
            LlmProvider::Claude,
            LlmProvider::ChatGpt,
            LlmProvider::Mistral,
            LlmProvider::Perplexity,
        ]);
        s.set_query("nomatch");
        match s.selected().unwrap() {
            Item::AskLlm { provider, .. } => assert_eq!(*provider, LlmProvider::Claude),
            _ => panic!("expected AskLlm"),
        }
    }

    #[test]
    fn llm_fallback_hidden_when_query_empty() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        assert!(s
            .filtered()
            .iter()
            .all(|m| !matches!(m.item, Item::AskLlm { .. })));
    }

    #[test]
    fn llm_fallback_hidden_when_a_window_matches() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_query("Mail");
        assert!(s
            .filtered()
            .iter()
            .all(|m| !matches!(m.item, Item::AskLlm { .. })));
    }

    #[test]
    fn llm_fallback_hidden_when_a_program_matches() {
        let mut s = SwitcherState::new();
        // No windows that match, but a program does: fallback must stay off.
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_programs(vec![prog("Safari")]);
        s.set_query("safa");
        assert!(!s.filtered_programs().is_empty());
        assert!(s
            .filtered()
            .iter()
            .all(|m| !matches!(m.item, Item::AskLlm { .. })));
    }

    #[test]
    fn llm_fallback_hidden_when_disabled() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_ask_llm_enabled(false);
        s.set_browser_tabs(Vec::new());
        s.set_query("xyznomatchatall");
        assert!(s.filtered().is_empty());
    }

    #[test]
    fn llm_fallback_hidden_when_eval_result_present() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_query("1+1");
        assert!(s.eval_result().is_some());
        assert!(s
            .filtered()
            .iter()
            .all(|m| !matches!(m.item, Item::AskLlm { .. })));
    }

    #[test]
    fn url_query_shows_open_url_row() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_programs(vec![prog("Safari")]);
        s.set_query("https://example.com/path");
        // Single OpenUrl row, no windows, no programs.
        assert_eq!(s.filtered().len(), 1);
        match &s.filtered()[0].item {
            Item::OpenUrl(u) => assert_eq!(&**u, "https://example.com/path"),
            other => panic!("expected OpenUrl, got {other:?}"),
        }
        assert!(s.filtered_programs().is_empty());
        assert!(s.eval_result().is_none());
    }

    // --- Browser tabs fallback tier tests ---

    fn tab(title: &str, url: &str) -> Item {
        Item::BrowserTab(Arc::new(crate::model::BrowserTabRef::new(
            crate::model::Browser::Chrome,
            1,
            1,
            Arc::from(title),
            Arc::from(url),
            None,
        )))
    }

    #[test]
    fn browser_tabs_needed_past_min_query_len() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_query("gh"); // 2 chars — too short
        assert!(!s.needs_browser_tabs());
        s.set_query("git"); // 3 chars — threshold
        assert!(s.needs_browser_tabs());
    }

    #[test]
    fn browser_tabs_not_needed_when_query_empty() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        assert!(!s.needs_browser_tabs());
    }

    #[test]
    fn browser_tabs_not_needed_when_integration_off() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_browser_tabs_integration(false);
        s.set_query("github");
        assert!(!s.needs_browser_tabs());
    }

    #[test]
    fn browser_tabs_not_needed_once_scan_completed() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_browser_tabs(Vec::new()); // scan finished, no tabs
        s.set_query("github");
        assert!(!s.needs_browser_tabs());
    }

    #[test]
    fn browser_tabs_appended_after_window_matches() {
        // Windows still come first; tabs are appended so the user never loses
        // a running-window match to a tab match.
        let mut s = SwitcherState::new();
        s.set_items(vec![win("GitHub Desktop", "")]);
        s.set_browser_tabs(vec![tab("GitHub", "https://github.com/")]);
        s.set_query("github");
        assert_eq!(s.filtered().len(), 2);
        assert!(matches!(s.filtered()[0].item, Item::Window(_)));
        assert!(matches!(s.filtered()[1].item, Item::BrowserTab(_)));
    }

    #[test]
    fn browser_tabs_appear_even_without_window_match() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_browser_tabs(vec![tab("GitHub", "https://github.com/")]);
        s.set_query("github");
        // 1 tab + 4 AskLlm rows (thin tab signal → offer AI handoff).
        assert!(matches!(s.filtered()[0].item, Item::BrowserTab(_)));
        assert_eq!(
            s.filtered()
                .iter()
                .filter(|m| matches!(m.item, Item::AskLlm { .. }))
                .count(),
            4
        );
    }

    #[test]
    fn llm_rows_appear_after_thin_tab_matches() {
        // <= ASK_LLM_MAX_TABS tab matches and zero window matches: LLM rows
        // show up after the tabs so the user can hand off to AI without a
        // second query.
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_browser_tabs(vec![
            tab("GitHub", "https://github.com/"),
            tab("GitHub Docs", "https://docs.github.com/"),
            tab("GitHub Status", "https://githubstatus.com/"),
        ]);
        s.set_query("github");
        let items = s.filtered();
        let tabs = items
            .iter()
            .filter(|m| matches!(m.item, Item::BrowserTab(_)))
            .count();
        let llms = items
            .iter()
            .filter(|m| matches!(m.item, Item::AskLlm { .. }))
            .count();
        assert_eq!(tabs, 3);
        assert_eq!(llms, 4);
        // Tabs first, LLM rows appended at the end.
        assert!(matches!(items[0].item, Item::BrowserTab(_)));
        assert!(matches!(items.last().unwrap().item, Item::AskLlm { .. }));
    }

    #[test]
    fn llm_rows_hidden_when_tab_matches_exceed_threshold() {
        // Above ASK_LLM_MAX_TABS, the tab list is its own answer.
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_browser_tabs(vec![
            tab("GitHub", "https://github.com/a"),
            tab("GitHub Docs", "https://github.com/b"),
            tab("GitHub Status", "https://github.com/c"),
            tab("GitHub Blog", "https://github.com/d"),
        ]);
        s.set_query("github");
        assert!(s
            .filtered()
            .iter()
            .all(|m| !matches!(m.item, Item::AskLlm { .. })));
    }

    #[test]
    fn browser_tabs_ignored_below_min_query_len() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_browser_tabs(vec![tab("GitHub", "https://github.com/")]);
        s.set_query("gh"); // 2 chars — below threshold
        // Below the threshold we must never surface a tab, even when the
        // cache is warm and would match.
        assert!(!s
            .filtered()
            .iter()
            .any(|m| matches!(m.item, Item::BrowserTab(_))));
    }

    #[test]
    fn browser_tabs_arrival_preserves_user_selection() {
        // User typed, arrow-keyed down to row 2, then tabs arrive. Row 2
        // must stay selected — async delivery shouldn't yank the cursor.
        let mut s = SwitcherState::new();
        s.set_items(vec![
            win("VSCode", "foo.rs"),
            win("VSCode", "bar.rs"),
            win("VSCode", "baz.rs"),
        ]);
        s.set_query("vs");
        s.move_down();
        s.move_down();
        assert_eq!(s.selected_idx(), 2);
        s.set_browser_tabs(vec![tab("Some VSCode tab", "https://code.vsc/")]);
        assert_eq!(s.selected_idx(), 2);
    }

    #[test]
    fn browser_tabs_arrival_clamps_when_cursor_would_overflow() {
        // User types, arrow-keys to the last row, then tabs arrive. Normally
        // tabs *append* so clamping isn't needed — but we still guarantee
        // that the cursor can never end up past the end of the list.
        let mut s = SwitcherState::new();
        s.set_items(vec![
            win("VSCode", "a.rs"),
            win("VSCode", "b.rs"),
            win("VSCode", "c.rs"),
        ]);
        s.set_query("vsc");
        // Move to the last filtered row.
        s.move_down();
        s.move_down();
        assert_eq!(s.selected_idx(), 2);
        // Tabs arrive with a match — filtered grows, cursor unchanged.
        s.set_browser_tabs(vec![tab("VSCode tab", "https://vscode.dev/")]);
        assert!(s.filtered().len() >= 3);
        assert_eq!(s.selected_idx(), 2);
        // Simulate an unusual shrink (cleared cache then refilled with
        // nothing). Selection must clamp into the new range.
        s.clear_browser_tabs();
        s.set_browser_tabs(Vec::new());
        // filtered now only holds the 3 window matches.
        assert_eq!(s.filtered().len(), 3);
        assert!(s.selected_idx() <= 2);
    }

    #[test]
    fn browser_tabs_match_by_host_not_only_title() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_browser_tabs(vec![tab("Some Page", "https://github.com/a/b")]);
        s.set_query("github");
        match &s.filtered()[0].item {
            Item::BrowserTab(t) => assert_eq!(t.host(), "github.com"),
            _ => panic!("expected BrowserTab"),
        }
    }

    #[test]
    fn llm_fallback_suppressed_while_scan_in_flight() {
        // At or past the 3-char threshold the scan is in flight — don't flash
        // the LLM rows only to replace them once tabs arrive.
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_query("nomatch");
        assert!(s.filtered().is_empty());
        assert!(s.needs_browser_tabs());
    }

    #[test]
    fn llm_fallback_shown_below_min_query_len_even_without_scan() {
        // Under 3 chars we never scan, so the LLM fallback kicks in
        // immediately once windows/programs/eval have nothing to offer.
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_query("xx"); // 2 chars, below threshold
        assert!(s
            .filtered()
            .iter()
            .all(|m| matches!(m.item, Item::AskLlm { .. })));
    }

    #[test]
    fn wrap_inside_programs() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_programs(vec![prog("Safari"), prog("SafeBoot"), prog("Safety")]);
        s.set_query("saf");
        s.move_up();
        assert_eq!(s.active_section(), Section::Programs);
        let last = s.filtered_programs().len() - 1;
        assert_eq!(s.selected_program_idx(), last);
        s.move_up();
        assert_eq!(s.selected_program_idx(), last - 1);
    }
}
