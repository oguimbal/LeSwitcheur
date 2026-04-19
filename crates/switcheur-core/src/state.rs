//! UI-agnostic state machine. The GPUI view reads from and drives this.
//! Keeping it pure Rust makes every transition unit-testable.

use std::sync::Arc;

use crate::matcher::{FuzzyMatcher, MatchResult};
use crate::model::{Item, LlmProvider, ProgramRef};

const MAX_PROGRAMS: usize = 3;

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
        self.dirs = dirs;
        self.selected_dir = 0;
        if was_focus && self.dirs.is_empty() {
            self.active_section = Section::Windows;
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
        // "Ask <Provider>" fallback — only when nothing else matched, the
        // query is non-empty, and math/JS eval hasn't answered either. The
        // providers render directly in `filtered` so arrow keys + Enter +
        // click all reuse the existing window-section plumbing.
        if !self.ask_llm_enabled
            || self.query.trim().is_empty()
            || !self.filtered.is_empty()
            || !self.filtered_programs.is_empty()
            || self.eval_result.is_some()
        {
            // leave filtered as-is (empty or populated by items above)
        } else {
            let query: Arc<str> = Arc::from(self.query.as_str());
            self.filtered = self
                .llm_provider_order
                .iter()
                .copied()
                .map(|provider| MatchResult {
                    item: Item::AskLlm {
                        provider,
                        query: query.clone(),
                    },
                    score: 0,
                    indices: Vec::new(),
                })
                .collect();
        }
        self.selected_idx = 0;
        self.selected_program = 0;
        self.active_section = Section::Windows;
    }
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
        // A query that matches nothing now populates the filter with the
        // "Ask LLM" fallback rows, so navigation cycles among providers
        // instead of being a no-op. The previous expectation (None) no
        // longer holds, but navigation must still be safe.
        let mut s = SwitcherState::new();
        s.set_items(vec![win("A", "alpha")]);
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
