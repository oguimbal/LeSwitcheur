//! UI-agnostic state machine. The GPUI view reads from and drives this.
//! Keeping it pure Rust makes every transition unit-testable.

use std::path::Path;
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
    /// Audio and local-source attention suggestions above the search field.
    Suggestions,
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
    /// Absolute path typed in the query (`crate::path::detect`).
    detected_path: Option<String>,
    /// Latest answer for a path query: the path it answers, its rows (shown
    /// in place of the ranking, kept until the next answer lands so typing
    /// doesn't flicker) and its Tab completions.
    path_hits: Option<(String, Vec<Item>, Vec<String>)>,
    llm_provider_order: Vec<LlmProvider>,
    ask_llm_enabled: bool,
    /// A signed-in Lena answered `info` this panel: list its Ask row.
    lena_ask: bool,
    /// When the Dirs pane is active and the user has pressed Right (or clicked
    /// a popover row), this tracks the selection inside the floating "Open
    /// With" popover. `None` means the popover is passive — shown next to the
    /// selected dir row, but keyboard focus still belongs to the dir list so
    /// Enter opens the folder in the default app.
    open_with_index: Option<usize>,
    /// Empty-query suggestions: audio rows followed by waiting/unread local sessions.
    suggested_items: Vec<Item>,
    local_sources: Vec<Item>,
    selected_suggestion: usize,
    /// Default team label of the "Create a Linear issue" row; `None` while
    /// no Linear account is connected.
    linear_create_team: Option<Arc<str>>,
    /// Upper-case team keys of the connected workspaces. Only identifiers
    /// with one of these prefixes are looked up.
    linear_team_keys: Vec<String>,
    /// `Item::LinearTeam` rows, shown when the query is a team key or name.
    linear_teams: Vec<Item>,
    /// Last lookup result, keyed by the identifier it answers.
    linear_issue: Option<(String, Item)>,
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
            detected_path: None,
            path_hits: None,
            llm_provider_order: LlmProvider::default_order(),
            ask_llm_enabled: true,
            lena_ask: false,
            open_with_index: None,
            suggested_items: Vec::new(),
            local_sources: Vec::new(),
            selected_suggestion: 0,
            linear_create_team: None,
            linear_team_keys: Vec::new(),
            linear_teams: Vec::new(),
            linear_issue: None,
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

    /// Show or hide the "Ask Lena AI" row. Lena is probed after the panel
    /// opens, so the cursor is kept.
    pub fn set_lena_ask(&mut self, available: bool) {
        if self.lena_ask == available {
            return;
        }
        let selected = self.selected().cloned();
        self.lena_ask = available;
        self.rerank_inner(RerankReset::PreserveSelection);
        self.restore_selection(selected);
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
        let selected = self.selected().cloned();
        self.browser_tabs_cache = Some(tabs);
        self.rerank_inner(RerankReset::PreserveSelection);
        self.restore_selection(selected);
    }

    /// Linear integration: `create_team` labels the default team of the
    /// "Create a Linear issue" row (`None` = no account connected);
    /// `teams` (`Item::LinearTeam`) gate which identifiers get looked up
    /// and open on an exact key/name query.
    pub fn set_linear(&mut self, create_team: Option<Arc<str>>, teams: Vec<Item>) {
        debug_assert!(teams.iter().all(|it| matches!(it, Item::LinearTeam(_))));
        self.linear_create_team = create_team;
        self.linear_team_keys = teams
            .iter()
            .filter_map(|it| match it {
                Item::LinearTeam(t) => Some(t.key.to_ascii_uppercase()),
                _ => None,
            })
            .collect();
        self.linear_teams = teams;
        self.rerank();
    }

    /// Absolute path the host should resolve for the current query.
    pub fn path_target(&self) -> Option<&str> {
        self.detected_path.as_deref()
    }

    /// Existing entries (`Item::Dir`) and Tab completions
    /// ([`crate::path::completions`]) for `target`, ignored once the query
    /// moved on.
    pub fn set_path_hits(&mut self, target: String, hits: Vec<Item>, completions: Vec<String>) {
        debug_assert!(hits.iter().all(|it| matches!(it, Item::Dir(_))));
        if self.detected_path.as_deref() != Some(target.as_str()) {
            return;
        }
        self.path_hits = Some((target, hits, completions));
        self.rerank();
    }

    /// Tab completions for the current path query, once its answer landed.
    pub fn path_completions(&self) -> Option<&[String]> {
        match &self.path_hits {
            Some((target, _, completions)) if self.detected_path.as_ref() == Some(target) => {
                Some(completions)
            }
            _ => None,
        }
    }

    /// Grey rest of the first completion, when the query is typed plainly
    /// (no quotes or escapes, so the suffix lines up with the text).
    pub fn path_ghost(&self) -> Option<&str> {
        if self.detected_path.as_deref() != Some(self.query.as_str()) {
            return None;
        }
        crate::path::ghost(&self.query, self.path_completions()?)
    }

    /// Identifier (`ENG-123`) the host should look up for the current
    /// query, when it names one of the connected teams.
    pub fn linear_lookup_target(&self) -> Option<String> {
        let (key, number) = crate::linear::parse_identifier(&self.query)?;
        self.linear_team_keys
            .contains(&key)
            .then(|| format!("{key}-{number}"))
    }

    /// Async lookup result for `identifier`. Ignored once the query moved
    /// on; the cursor stays on the row it was on.
    pub fn set_linear_issue(&mut self, identifier: String, issue: Option<Item>) {
        debug_assert!(issue.iter().all(|it| matches!(it, Item::LinearIssue(_))));
        if self.linear_lookup_target().as_deref() != Some(identifier.as_str()) {
            return;
        }
        let selected = self.selected().cloned();
        self.linear_issue = issue.map(|item| (identifier, item));
        self.rerank_inner(RerankReset::PreserveSelection);
        self.restore_selection(selected);
    }

    /// Keep the cursor on the same windows-list row after an async insert.
    fn restore_selection(&mut self, selected: Option<Item>) {
        if self.active_section != Section::Windows {
            return;
        }
        if let Some(index) = selected.as_ref().and_then(|selected| {
            self.filtered
                .iter()
                .position(|result| same_target(&result.item, selected))
        }) {
            self.selected_idx = index;
        }
    }

    /// Install the "Currently Playing" rows. Like browser tabs the caller
    /// resolves this asynchronously after the panel opens — CoreAudio
    /// enumeration + optional MediaRemote/AppleScript hop. Empty vec means
    /// no source detected. Preserves the user's selection: late delivery
    /// must not yank the cursor. The audio cursor is clamped to the new
    /// list length so a shrinking list doesn't dangle.
    pub fn set_currently_playing(&mut self, rows: Vec<Item>) {
        debug_assert!(rows
            .iter()
            .all(|it| matches!(it, Item::CurrentlyPlaying(_))));
        let selected = self.selected().cloned();
        let mut suggestions = rows;
        let mut attention: Vec<_> = self
            .local_sources
            .iter()
            .filter_map(|item| {
                let Item::LocalSource(source) = item else {
                    return None;
                };
                (source.entry.state == crate::LocalSessionState::Waiting || source.entry.unread)
                    .then_some(item.clone())
            })
            .collect();
        attention.sort_by_key(|item| {
            let Item::LocalSource(source) = item else {
                unreachable!()
            };
            (
                source.entry.state != crate::LocalSessionState::Waiting,
                std::cmp::Reverse(source.entry.last_activity_ms),
            )
        });
        attention.truncate(3);
        suggestions.extend(attention);
        self.suggested_items = suggestions;
        self.selected_suggestion = self
            .selected_suggestion
            .min(self.suggested_items.len().saturating_sub(1));
        if self.active_section == Section::Suggestions {
            if let Some(index) = selected.as_ref().and_then(|selected| {
                self.suggested_items
                    .iter()
                    .position(|item| same_target(item, selected))
            }) {
                self.selected_suggestion = index;
            }
            if !self.suggested_items_visible() {
                self.active_section = Section::Windows;
            }
        }
    }

    /// Replace one provider's live results, preserving the selected target across refreshes.
    pub fn set_local_source(&mut self, source: &str, rows: Vec<Item>) {
        let selected = self.selected().cloned();
        let section = self.active_section;
        self.local_sources
            .retain(|item| !matches!(item, Item::LocalSource(r) if r.source == source));
        self.local_sources.extend(
            rows.into_iter()
                .filter(|item| matches!(item, Item::LocalSource(r) if r.source == source)),
        );
        self.rerank_inner(RerankReset::PreserveSelection);
        if section == Section::Windows {
            if let Some(index) = selected.as_ref().and_then(|selected| {
                self.filtered
                    .iter()
                    .position(|result| same_target(&result.item, selected))
            }) {
                self.selected_idx = index;
            }
        }
        let audio = self
            .suggested_items
            .iter()
            .filter(|item| matches!(item, Item::CurrentlyPlaying(_)))
            .cloned()
            .collect();
        self.set_currently_playing(audio);
    }

    /// Audio and local attention suggestions. Visibility uses
    /// [`Self::suggested_items_visible`] so rows hide on typing.
    pub fn suggested_items(&self) -> &[Item] {
        &self.suggested_items
    }

    /// Cursor index inside the suggestions. Only meaningful when
    /// [`Section::Suggestions`] is active.
    pub fn selected_suggestion_idx(&self) -> usize {
        self.selected_suggestion
    }

    /// True when suggestions should render: query is empty AND at least
    /// one source detected. Mirrors [`Self::programs_visible`] but inverted
    /// on the empty-query check (the two sections are mutually exclusive).
    pub fn suggested_items_visible(&self) -> bool {
        self.query.trim().is_empty() && !self.suggested_items.is_empty()
    }

    /// Move keyboard focus into suggestions (last row — closest to the
    /// windows list, so Up flows naturally from Windows[0]). No-op when
    /// hidden.
    pub fn focus_suggestions(&mut self) {
        if !self.suggested_items_visible() {
            return;
        }
        self.active_section = Section::Suggestions;
        self.selected_suggestion = self.suggested_items.len() - 1;
    }

    /// Click/hover handler for a suggestion. Activates the section and
    /// jumps the cursor to the clicked index.
    pub fn set_selected_suggestion(&mut self, idx: usize) {
        if !self.suggested_items_visible() {
            return;
        }
        self.active_section = Section::Suggestions;
        self.selected_suggestion = idx.min(self.suggested_items.len() - 1);
    }

    /// Flip the playback state of the audio row at `idx` between Playing
    /// and Paused. Used by the UI's play/pause button to update the badge
    /// without waiting for the next probe to land. No-op if `idx` is out
    /// of range or the row's state is `Unknown` (we have no truth to
    /// flip *to*).
    pub fn toggle_audio_row_state(&mut self, idx: usize) {
        let Some(item) = self.suggested_items.get(idx) else {
            return;
        };
        let Item::CurrentlyPlaying(row) = item else {
            return;
        };
        let new_state = match row.state {
            crate::model::PlaybackState::Playing => crate::model::PlaybackState::Paused,
            crate::model::PlaybackState::Paused => crate::model::PlaybackState::Playing,
            crate::model::PlaybackState::Unknown => return,
        };
        // AudioRowRef sits inside an Arc — clone the underlying row and
        // rebuild the Item so we don't mutate a shared reference.
        let mut updated = (**row).clone();
        updated.state = new_state;
        self.suggested_items[idx] = Item::CurrentlyPlaying(Arc::new(updated));
    }

    /// Forget live source data. Called when the switcher closes so the next
    /// open starts with a fresh detection pass.
    pub fn clear_suggestions(&mut self) {
        self.local_sources.clear();
        self.suggested_items.clear();
        self.selected_suggestion = 0;
        if self.active_section == Section::Suggestions {
            self.active_section = Section::Windows;
        }
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

    /// Swap in a fresher window list (async refresh after the panel already
    /// painted from a cached one) without moving the user's cursor off the
    /// row they had selected.
    pub fn refresh_items(&mut self, items: Vec<Item>) {
        let selected = self.selected().cloned();
        let section = self.active_section;
        self.items = items;
        self.rerank_inner(RerankReset::PreserveSelection);
        if section == Section::Windows {
            if let Some(index) = selected.as_ref().and_then(|selected| {
                self.filtered
                    .iter()
                    .position(|result| same_row(&result.item, selected))
            }) {
                self.selected_idx = index;
            }
        }
    }

    /// Drop the window with the given CGWindowID from the candidate set and
    /// re-rank. Used after the user closes a window from the row × button so
    /// the dead row disappears immediately, before the platform's window list
    /// has caught up.
    pub fn remove_window(&mut self, id: u64) {
        self.items.retain(|it| !matches!(it, Item::Window(w) if w.id == id));
        self.rerank();
    }

    /// Drop the directory with the given path from the dirs pane. Used after
    /// the user clicks × on a zoxide row so the row vanishes before the next
    /// zoxide query has been issued.
    pub fn remove_dir(&mut self, path: &Path) {
        let before = self.dirs.len();
        self.dirs
            .retain(|it| !matches!(it, Item::Dir(d) if d.path == path));
        if self.dirs.len() == before {
            return;
        }
        self.open_with_index = None;
        if self.dirs.is_empty() {
            // Emptying the pane: hand focus back to windows and rerank so the
            // "Ask LLM" fallback can come back in (it was suppressed while
            // dirs were visible).
            if self.active_section == Section::Dirs {
                self.active_section = Section::Windows;
            }
            self.rerank();
        } else if self.selected_dir >= self.dirs.len() {
            self.selected_dir = self.dirs.len() - 1;
        }
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
        self.open_with_index = None;
        if was_focus && self.dirs.is_empty() {
            self.active_section = Section::Windows;
        }
        if presence_changed {
            self.rerank_inner(RerankReset::PreserveSelection);
        }
        // When the left list only holds fallback rows (Ask AI, create a
        // Linear issue) but dirs matched, move keyboard focus there so Enter
        // activates a dir row without a manual Tab.
        let only_fallback_rows = self
            .filtered
            .iter()
            .all(|m| matches!(m.item, Item::CreateLinearIssue { .. } | Item::AskLlm { .. }));
        if only_fallback_rows
            && self.filtered_programs.is_empty()
            && self.dirs.iter().any(|it| !is_lena(it))
            && self.active_section == Section::Windows
        {
            self.active_section = Section::Dirs;
        }
    }

    /// Lena rows currently shown, kept below fresh local rows until Lena's
    /// slower answer replaces them.
    pub fn lena_dirs(&self) -> Vec<Item> {
        self.dirs.iter().filter(|it| is_lena(it)).cloned().collect()
    }

    /// Swap a Lena row's icon for its thumbnail once fetched.
    pub fn set_lena_icon(&mut self, id: &str, icon: std::path::PathBuf) {
        for item in &mut self.dirs {
            if let Item::Dir(d) = item {
                if d.lena.as_ref().is_some_and(|t| t.id == id) {
                    let mut updated = (**d).clone();
                    updated.icon_path = Some(icon.clone());
                    *item = Item::Dir(Arc::new(updated));
                }
            }
        }
    }

    /// Replace the Lena rows below the local dirs without moving the pane
    /// cursor (clamped if the pane shrank). Never takes keyboard focus: Lena
    /// answers seconds after typing, when Enter may already target the left
    /// list (e.g. "Ask Lena AI").
    pub fn set_lena_dirs(&mut self, rows: Vec<Item>) {
        self.dirs.retain(|it| !is_lena(it));
        self.dirs.extend(rows);
        if self.dirs.is_empty() {
            self.selected_dir = 0;
            self.open_with_index = None;
            if self.active_section == Section::Dirs {
                self.active_section = Section::Windows;
            }
        } else {
            self.selected_dir = self.selected_dir.min(self.dirs.len() - 1);
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
        self.open_with_index = None;
    }

    /// Jump the dirs selection to a specific index + activate the section.
    /// Used on hover/click of a dir row.
    pub fn set_selected_dir(&mut self, idx: usize) {
        if self.dirs.is_empty() {
            return;
        }
        let changed = self.active_section != Section::Dirs || self.selected_dir != idx;
        self.active_section = Section::Dirs;
        self.selected_dir = idx.min(self.dirs.len() - 1);
        // Reset popover focus whenever the row under the popover changes so
        // the next Right-arrow lands on the top item.
        if changed {
            self.open_with_index = None;
        }
    }

    /// Popover selection index, or `None` while the popover is passive
    /// (keyboard still on the dir row).
    pub fn open_with_index(&self) -> Option<usize> {
        self.open_with_index
    }

    /// Activate the popover on its first row. No-op if `count == 0`.
    pub fn enter_open_with(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.open_with_index = Some(0);
    }

    /// Clear popover focus so Enter falls back to the default opener.
    pub fn exit_open_with(&mut self) {
        self.open_with_index = None;
    }

    /// Jump the popover selection to a specific index. Used when the user
    /// clicks a popover row with the mouse (no prior "enter popover" step).
    pub fn set_open_with_index(&mut self, idx: usize, count: usize) {
        if count == 0 {
            return;
        }
        self.open_with_index = Some(idx.min(count - 1));
    }

    /// Popover up, wrapping at the top. Exits the popover if nothing is there.
    pub fn open_with_prev(&mut self, count: usize) {
        if count == 0 {
            self.open_with_index = None;
            return;
        }
        let i = self.open_with_index.unwrap_or(0);
        let next = if i == 0 { count - 1 } else { i - 1 };
        self.open_with_index = Some(next);
    }

    /// Popover down, wrapping at the bottom.
    pub fn open_with_next(&mut self, count: usize) {
        if count == 0 {
            self.open_with_index = None;
            return;
        }
        let i = self.open_with_index.unwrap_or(0);
        self.open_with_index = Some((i + 1) % count);
    }

    pub fn set_query(&mut self, q: impl Into<String>) {
        self.query = q.into();
        // URL detection short-circuits eval — a pasted URL is never an
        // expression, and running the JS engine on it is pure overhead.
        self.detected_url = crate::url::detect(&self.query).map(Arc::from);
        self.detected_path = crate::path::detect(&self.query);
        if self.detected_path.is_none() {
            self.path_hits = None;
        }
        self.eval_result = if self.detected_url.is_some() || self.detected_path.is_some() {
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
            Section::Suggestions => self.suggested_items.get(self.selected_suggestion),
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
            self.open_with_index = None;
            return;
        }
        // Audio rows sit at the top of the world. Up cycles within the
        // group; the topmost row is a no-op (no wrap-back-to-bottom — that
        // would conflict with the symmetric move_down which exits the
        // group into Windows).
        if self.active_section == Section::Suggestions {
            if self.selected_suggestion > 0 {
                self.selected_suggestion -= 1;
            }
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
                Section::Dirs | Section::Suggestions => unreachable!("handled above"),
            }
        }
        // No programs section: Up from Windows row 0 enters the audio
        // group at its last (closest) row, when one is visible (mutually
        // exclusive with programs — see the empty-query gate in
        // `suggested_items_visible`).
        if self.active_section == Section::Windows
            && self.selected_idx == 0
            && self.suggested_items_visible()
        {
            self.active_section = Section::Suggestions;
            self.selected_suggestion = self.suggested_items.len() - 1;
            return;
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
            self.open_with_index = None;
            return;
        }
        // Down inside the audio group cycles forward; from the last row
        // it drops into the windows list at row 0.
        if self.active_section == Section::Suggestions {
            if self.selected_suggestion + 1 < self.suggested_items.len() {
                self.selected_suggestion += 1;
                return;
            }
            self.active_section = Section::Windows;
            self.selected_idx = 0;
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
                Section::Dirs | Section::Suggestions => unreachable!("handled above"),
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
        let idx = idx.min(self.filtered.len() - 1);
        if self.active_section != Section::Windows || self.selected_idx != idx {
            self.open_with_index = None;
        }
        self.active_section = Section::Windows;
        self.selected_idx = idx;
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
        if let Some((_, hits, _)) = self.path_hits.as_ref().filter(|(_, hits, _)| !hits.is_empty()) {
            self.filtered = hits
                .iter()
                .map(|item| MatchResult {
                    item: item.clone(),
                    score: 0,
                    indices: Vec::new(),
                })
                .collect();
            self.filtered_programs.clear();
            self.selected_program = 0;
            if reset == RerankReset::ResetSelection {
                self.selected_idx = 0;
                self.open_with_index = None;
                self.active_section = Section::Windows;
            }
            self.selected_idx = self.selected_idx.min(self.filtered.len() - 1);
            return;
        }
        self.filtered = self.matcher.rank(&self.query, &self.items);
        let window_count = self.filtered.len();
        let local_matches = if self.query.trim().is_empty() {
            Vec::new()
        } else {
            self.matcher.rank(&self.query, &self.local_sources)
        };
        let local_count = local_matches.len();
        self.filtered.extend(local_matches);
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
        let linear_target = self.linear_lookup_target();
        let linear_hit = match (&linear_target, &self.linear_issue) {
            (Some(target), Some((identifier, item))) if target == identifier => Some(item.clone()),
            _ => None,
        };
        let team_hits: Vec<Item> = self
            .linear_teams
            .iter()
            .filter(|it| matches!(it, Item::LinearTeam(t) if t.matches_exactly(&self.query)))
            .cloned()
            .collect();
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
            && local_count == 0
            && tab_count <= ASK_LLM_MAX_TABS
            && self.filtered_programs.is_empty()
            && self.eval_result.is_none()
            && !browser_tabs_pending
            && linear_hit.is_none()
            && team_hits.is_empty()
        {
            let query: Arc<str> = Arc::from(self.query.as_str());
            let lena_ask = self.lena_ask;
            self.filtered.extend(
                self.llm_provider_order
                    .iter()
                    .copied()
                    .filter(|p| lena_ask || *p != LlmProvider::Lena)
                    .map(|provider| {
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
        for (i, item) in team_hits.into_iter().enumerate() {
            self.filtered.insert(
                i,
                MatchResult {
                    item,
                    score: 0,
                    indices: Vec::new(),
                },
            );
        }
        if let Some(item) = linear_hit {
            self.filtered.insert(
                0,
                MatchResult {
                    item,
                    score: 0,
                    indices: Vec::new(),
                },
            );
        }
        // Last row: a single Up from the input wraps onto it.
        if let Some(team) = &self.linear_create_team {
            let title = self.query.trim();
            if !title.is_empty() && self.eval_result.is_none() && linear_target.is_none() {
                self.filtered.push(MatchResult {
                    item: Item::CreateLinearIssue {
                        title: Arc::from(title),
                        team: team.clone(),
                    },
                    score: 0,
                    indices: Vec::new(),
                });
            }
        }
        match reset {
            RerankReset::ResetSelection => {
                self.selected_idx = 0;
                self.selected_program = 0;
                self.active_section = Section::Windows;
                self.open_with_index = None;
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

fn is_lena(item: &Item) -> bool {
    matches!(item, Item::Dir(d) if d.source == crate::model::DirSource::Lena)
}

/// Like [`same_target`], but a window keeps its identity across a title
/// change between two listings.
fn same_row(a: &Item, b: &Item) -> bool {
    match (a, b) {
        (Item::Window(a), Item::Window(b)) => a.id == b.id && a.pid == b.pid,
        _ => same_target(a, b),
    }
}

fn same_target(a: &Item, b: &Item) -> bool {
    match (a, b) {
        (Item::LocalSource(a), Item::LocalSource(b)) => a.same_target(b),
        _ => a == b,
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
    use crate::model::{DirRef, DirSource, ProgramRef, WindowRef};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn dir(path: &str) -> Item {
        Item::Dir(Arc::new(DirRef::new(
            PathBuf::from(path),
            DirSource::Zoxide,
            None,
        )))
    }

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

    fn win_id(id: u64, title: &str) -> Item {
        Item::Window(Arc::new(WindowRef {
            id,
            pid: 1,
            title: title.into(),
            app_name: "App".into(),
            bundle_id: None,
            icon_path: None,
            minimized: false,
        }))
    }

    #[test]
    fn refresh_items_keeps_selected_row() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win_id(1, "a"), win_id(2, "b"), win_id(3, "c")]);
        s.move_down();
        s.move_down();
        assert_eq!(s.selected().unwrap().primary(), "c");
        // Fresher list: a new window up front, and "c" got retitled.
        s.refresh_items(vec![
            win_id(4, "new"),
            win_id(1, "a"),
            win_id(2, "b"),
            win_id(3, "c2"),
        ]);
        assert_eq!(s.selected_idx(), 3);
        assert_eq!(s.selected().unwrap().primary(), "c2");
    }

    #[test]
    fn refresh_items_keeps_query() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win_id(1, "alpha"), win_id(2, "beta")]);
        s.set_query("bet");
        s.refresh_items(vec![win_id(1, "alpha"), win_id(2, "beta"), win_id(3, "gamma")]);
        assert_eq!(s.query(), "bet");
        assert_eq!(s.selected().unwrap().primary(), "beta");
    }

    #[test]
    fn refresh_items_clamps_when_selected_row_vanishes() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win_id(1, "a"), win_id(2, "b")]);
        s.move_down();
        s.refresh_items(vec![win_id(1, "a")]);
        assert_eq!(s.selected_idx(), 0);
    }

    fn lena_hit(id: &str) -> Item {
        Item::Dir(Arc::new(DirRef::lena(
            crate::model::LenaTarget {
                instance: "i".into(),
                id: id.into(),
                local: None,
            },
            format!("{id}.pdf"),
            "Lena/Projets".into(),
            false,
            None,
        )))
    }

    fn ask_providers(s: &SwitcherState) -> Vec<LlmProvider> {
        s.filtered()
            .iter()
            .filter_map(|m| match &m.item {
                Item::AskLlm { provider, .. } => Some(*provider),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn lena_ask_row_only_while_lena_is_available() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("A", "alpha")]);
        s.set_browser_tabs(Vec::new());
        s.set_query("zzzzzz");
        assert!(!ask_providers(&s).contains(&LlmProvider::Lena));
        s.set_selected(2);
        s.set_lena_ask(true);
        assert_eq!(ask_providers(&s).last(), Some(&LlmProvider::Lena));
        assert_eq!(s.selected_idx(), 2, "late Lena probe keeps the cursor");
        s.set_lena_ask(false);
        assert!(!ask_providers(&s).contains(&LlmProvider::Lena));
    }

    #[test]
    fn ask_rows_stay_next_to_dir_matches() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("A", "alpha")]);
        s.set_browser_tabs(Vec::new());
        s.set_query("zzzzzz");
        s.set_dirs(vec![dir("/zzzzzz")]);
        assert!(!ask_providers(&s).is_empty());
        assert_eq!(s.active_section(), Section::Dirs, "local dir match takes Enter");
    }

    #[test]
    fn lena_hits_append_below_local_dirs_without_moving_the_cursor() {
        let mut s = SwitcherState::new();
        s.set_dirs(vec![dir("/a"), dir("/b")]);
        s.focus_dirs();
        s.set_selected_dir(1);
        s.set_lena_dirs(vec![lena_hit("x"), lena_hit("y")]);
        assert_eq!(s.dirs().len(), 4);
        assert_eq!(s.selected_dir_idx(), 1);
        assert_eq!(s.active_section(), Section::Dirs);
        s.set_selected_dir(3);
        s.set_lena_dirs(vec![lena_hit("z")]);
        assert_eq!(s.dirs().len(), 3, "previous Lena rows are replaced");
        assert_eq!(s.selected_dir_idx(), 2);
        assert_eq!(s.lena_dirs().len(), 1);
        s.set_lena_icon("z", PathBuf::from("/tmp/z.png"));
        assert!(matches!(&s.dirs()[2], Item::Dir(d) if d.icon_path.as_deref() == Some(Path::new("/tmp/z.png"))));

        let mut empty = SwitcherState::new();
        empty.set_items(vec![win("A", "alpha")]);
        empty.set_browser_tabs(Vec::new());
        empty.set_query("zzzzzz");
        empty.set_lena_dirs(vec![lena_hit("x")]);
        assert!(empty.dirs_visible());
        assert_eq!(empty.active_section(), Section::Windows, "late Lena hits leave focus on Ask rows");
        assert!(matches!(empty.selected(), Some(Item::AskLlm { .. })));
        match &empty.dirs()[0] {
            Item::Dir(d) => {
                assert_eq!((d.basename(), d.parent_display()), ("x.pdf", "Lena/Projets"));
                assert!(d.local_path().is_none());
            }
            _ => unreachable!(),
        }
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
        // 5 providers in default order (Mistral first).
        assert_eq!(s.filtered().len(), 5);
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
        // 1 tab + 5 AskLlm rows (thin tab signal → offer AI handoff).
        assert!(matches!(s.filtered()[0].item, Item::BrowserTab(_)));
        assert_eq!(
            s.filtered()
                .iter()
                .filter(|m| matches!(m.item, Item::AskLlm { .. }))
                .count(),
            5
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
        assert_eq!(llms, 5);
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

    // --- remove_dir tests ---

    #[test]
    fn remove_dir_drops_matching_entry() {
        let mut s = SwitcherState::new();
        s.set_dirs(vec![dir("/a"), dir("/b"), dir("/c")]);
        s.remove_dir(Path::new("/b"));
        assert_eq!(s.dirs().len(), 2);
        assert!(s.dirs().iter().all(|it| !matches!(
            it,
            Item::Dir(d) if d.path == PathBuf::from("/b")
        )));
    }

    #[test]
    fn remove_dir_missing_is_noop() {
        let mut s = SwitcherState::new();
        s.set_dirs(vec![dir("/a")]);
        s.remove_dir(Path::new("/missing"));
        assert_eq!(s.dirs().len(), 1);
    }

    #[test]
    fn remove_dir_emptying_pane_returns_focus_to_windows() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_dirs(vec![dir("/only")]);
        s.focus_dirs();
        assert_eq!(s.active_section(), Section::Dirs);
        s.remove_dir(Path::new("/only"));
        assert!(s.dirs().is_empty());
        assert_eq!(s.active_section(), Section::Windows);
    }

    #[test]
    fn remove_dir_clamps_selection() {
        let mut s = SwitcherState::new();
        s.set_dirs(vec![dir("/a"), dir("/b"), dir("/c")]);
        s.focus_dirs();
        s.move_down();
        s.move_down(); // selected_dir = 2
        assert_eq!(s.selected_dir_idx(), 2);
        s.remove_dir(Path::new("/c"));
        assert_eq!(s.selected_dir_idx(), 1);
    }

    // --- Currently Playing audio row tests ---

    fn audio(app: &str) -> Item {
        Item::CurrentlyPlaying(Arc::new(crate::model::AudioRowRef {
            pid: 0,
            app_name: app.into(),
            bundle_id: None,
            icon_path: None,
            browser_tab: None,
            browser: None,
            state: crate::model::PlaybackState::Playing,
            track_title: None,
            track_artist: None,
        }))
    }

    #[test]
    fn audio_row_visible_only_when_query_empty() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        assert!(!s.suggested_items_visible());
        s.set_currently_playing(vec![audio("Spotify")]);
        assert!(s.suggested_items_visible());
        s.set_query("a");
        assert!(!s.suggested_items_visible());
    }

    #[test]
    fn up_from_windows_first_jumps_to_audio() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox"), win("VSCode", "x")]);
        s.set_currently_playing(vec![audio("Spotify")]);
        assert_eq!(s.active_section(), Section::Windows);
        assert_eq!(s.selected_idx(), 0);
        s.move_up();
        assert_eq!(s.active_section(), Section::Suggestions);
        match s.selected().unwrap() {
            Item::CurrentlyPlaying(r) => assert_eq!(r.app_name, "Spotify"),
            _ => panic!("expected audio row"),
        }
    }

    #[test]
    fn down_from_audio_returns_to_windows_first() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox"), win("VSCode", "x")]);
        s.set_currently_playing(vec![audio("Spotify")]);
        s.move_up();
        assert_eq!(s.active_section(), Section::Suggestions);
        s.move_down();
        assert_eq!(s.active_section(), Section::Windows);
        assert_eq!(s.selected_idx(), 0);
    }

    #[test]
    fn move_up_in_audio_section_is_noop() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_currently_playing(vec![audio("Spotify")]);
        s.move_up();
        assert_eq!(s.active_section(), Section::Suggestions);
        s.move_up();
        // Stays on Audio — no wrap to bottom of windows.
        assert_eq!(s.active_section(), Section::Suggestions);
    }

    #[test]
    fn audio_row_arrival_preserves_user_selection() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("A", "a"), win("B", "b"), win("C", "c")]);
        s.move_down();
        s.move_down();
        assert_eq!(s.selected_idx(), 2);
        s.set_currently_playing(vec![audio("Spotify")]);
        // Late delivery must not yank the cursor.
        assert_eq!(s.active_section(), Section::Windows);
        assert_eq!(s.selected_idx(), 2);
    }

    #[test]
    fn typing_clears_audio_selection() {
        // User up-arrows into the audio row, then starts typing. set_query
        // resets active_section to Windows via rerank, so Enter activates
        // the typed-query result, not the stale audio row.
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_currently_playing(vec![audio("Spotify")]);
        s.move_up();
        assert_eq!(s.active_section(), Section::Suggestions);
        s.set_query("ma");
        assert_eq!(s.active_section(), Section::Windows);
    }

    #[test]
    fn audio_section_skipped_when_programs_visible() {
        // Programs and Audio are mutually exclusive: programs need a
        // non-empty query, audio needs an empty one. Sanity-check that the
        // visibility predicates can never both be true at once.
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_programs(vec![prog("Safari")]);
        s.set_currently_playing(vec![audio("Spotify")]);
        s.set_query("saf");
        assert!(s.programs_visible());
        assert!(!s.suggested_items_visible());
    }

    #[test]
    fn clear_suggestions_resets_section() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_currently_playing(vec![audio("Spotify")]);
        s.move_up();
        assert_eq!(s.active_section(), Section::Suggestions);
        s.clear_suggestions();
        assert_eq!(s.active_section(), Section::Windows);
        assert!(!s.suggested_items_visible());
    }

    #[test]
    fn set_currently_playing_to_empty_drops_focus() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_currently_playing(vec![audio("Spotify")]);
        s.move_up();
        s.set_currently_playing(Vec::new());
        assert_eq!(s.active_section(), Section::Windows);
    }

    #[test]
    fn audio_group_navigates_within_then_drops_into_windows() {
        // Two audio rows: Spotify (paused) + Chrome (playing). Up from
        // Windows[0] lands on the *last* audio row (closest to the windows
        // list), Up again moves to the first, Up at the top is a no-op,
        // Down cycles back through the group then exits into Windows[0].
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_currently_playing(vec![audio("Spotify"), audio("Chrome")]);
        s.move_up();
        assert_eq!(s.active_section(), Section::Suggestions);
        assert_eq!(s.selected_suggestion_idx(), 1);
        s.move_up();
        assert_eq!(s.selected_suggestion_idx(), 0);
        s.move_up();
        assert_eq!(s.selected_suggestion_idx(), 0);
        s.move_down();
        assert_eq!(s.selected_suggestion_idx(), 1);
        s.move_down();
        assert_eq!(s.active_section(), Section::Windows);
        assert_eq!(s.selected_idx(), 0);
    }

    #[test]
    fn shrinking_audio_list_clamps_cursor() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox")]);
        s.set_currently_playing(vec![audio("Spotify"), audio("Chrome")]);
        s.move_up();
        assert_eq!(s.selected_suggestion_idx(), 1);
        s.set_currently_playing(vec![audio("Spotify")]);
        assert_eq!(s.selected_suggestion_idx(), 0);
    }

    // --- Linear tests ---

    fn linear_issue(identifier: &str) -> Item {
        Item::LinearIssue(Arc::new(crate::model::LinearIssueRef::new(
            identifier.into(),
            "Fix login".into(),
            format!("https://linear.app/acme/issue/{identifier}"),
            Some("Todo".into()),
            "Acme",
        )))
    }

    fn linear_team(key: &str, name: &str) -> Item {
        Item::LinearTeam(Arc::new(crate::model::LinearTeamRef::new(
            key.into(),
            name.into(),
            format!("https://linear.app/acme/team/{key}/all"),
            "Acme",
        )))
    }

    fn linear_state() -> SwitcherState {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Mail", "Inbox"), win("VSCode", "main.rs")]);
        s.set_browser_tabs(Vec::new());
        s.set_linear(
            Some(Arc::from("ENG")),
            vec![linear_team("ENG", "Engineering"), linear_team("PER", "Perso")],
        );
        s
    }

    #[test]
    fn exact_team_key_or_name_offers_the_team() {
        let mut s = linear_state();
        for q in ["per", "PER", "perso", " Perso "] {
            s.set_query(q);
            assert!(
                matches!(&s.filtered()[0].item, Item::LinearTeam(t) if t.key == "PER"),
                "{q}"
            );
            assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::AskLlm { .. })));
        }
        s.set_query("pers");
        assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::LinearTeam(_))));
    }

    #[test]
    fn create_row_is_last_when_linear_connected() {
        let mut s = linear_state();
        s.set_query("ma");
        let last = &s.filtered().last().unwrap().item;
        assert!(matches!(last, Item::CreateLinearIssue { title, team }
            if &**title == "ma" && &**team == "ENG"));
        assert!(matches!(s.selected(), Some(Item::Window(_))));
    }

    #[test]
    fn create_row_hidden_without_account_or_query() {
        let mut s = linear_state();
        assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::CreateLinearIssue { .. })));
        s.set_linear(None, Vec::new());
        s.set_query("ma");
        assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::CreateLinearIssue { .. })));
    }

    #[test]
    fn create_row_hidden_for_eval_and_urls() {
        let mut s = linear_state();
        s.set_query("1+1");
        assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::CreateLinearIssue { .. })));
        s.set_query("https://example.com");
        assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::CreateLinearIssue { .. })));
    }

    #[test]
    fn lookup_target_requires_known_team() {
        let mut s = linear_state();
        s.set_query("eng-12");
        assert_eq!(s.linear_lookup_target().as_deref(), Some("ENG-12"));
        s.set_query("ops-12");
        assert_eq!(s.linear_lookup_target(), None);
        s.set_query("eng 12");
        assert_eq!(s.linear_lookup_target(), None);
    }

    #[test]
    fn found_issue_is_first_and_replaces_fallbacks() {
        let mut s = linear_state();
        s.set_query("eng-12");
        assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::CreateLinearIssue { .. })));
        s.set_linear_issue("ENG-12".into(), Some(linear_issue("ENG-12")));
        assert!(matches!(s.filtered()[0].item, Item::LinearIssue(_)));
        assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::AskLlm { .. })));
        assert!(matches!(s.selected(), Some(Item::LinearIssue(_))));
    }

    #[test]
    fn stale_issue_result_is_ignored() {
        let mut s = linear_state();
        s.set_query("eng-12");
        s.set_query("eng-123");
        s.set_linear_issue("ENG-12".into(), Some(linear_issue("ENG-12")));
        assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::LinearIssue(_))));
    }

    #[test]
    fn issue_arrival_keeps_cursor_on_its_row() {
        let mut s = linear_state();
        s.set_query("eng-12");
        let n = s.filtered().len();
        s.set_selected(n - 1);
        let before = s.selected().cloned();
        s.set_linear_issue("ENG-12".into(), Some(linear_issue("ENG-12")));
        if let Some(before) = before.filter(|it| s.filtered().iter().any(|m| &m.item == it)) {
            assert_eq!(s.selected(), Some(&before));
        }
    }

    #[test]
    fn dirs_take_focus_when_only_create_row_matches() {
        let mut s = linear_state();
        s.set_query("zzzz");
        s.set_dirs(vec![dir("/zzzz")]);
        assert_eq!(s.active_section(), Section::Dirs);
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

    #[test]
    fn path_hits_replace_the_ranking_until_the_query_leaves_paths() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Finder", "Users")]);
        s.set_query("/Users/me");
        assert_eq!(s.path_target(), Some("/Users/me"));
        assert!(s.eval_result().is_none());
        s.set_path_hits("/Users/other".into(), vec![dir("/Users/other")], Vec::new());
        assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::Dir(_))));
        s.set_path_hits(
            "/Users/me".into(),
            vec![dir("/Users/me"), dir("/Users/meta")],
            vec!["/Users/meta".into()],
        );
        assert_eq!(s.path_ghost(), Some("ta"));
        assert_eq!(s.filtered().len(), 2);
        assert_eq!(s.selected(), Some(&dir("/Users/me")));
        // Next keystroke: previous hits stay until the new answer lands.
        s.set_query("/Users/met");
        assert_eq!(s.filtered().len(), 2);
        assert_eq!(s.path_completions(), None);
        assert_eq!(s.path_ghost(), None);
        s.set_path_hits("/Users/met".into(), vec![dir("/Users/meta")], vec!["/Users/meta".into()]);
        assert_eq!(s.path_ghost(), Some("a"));
        s.set_query(" /Users/met");
        assert_eq!(s.path_ghost(), None);
        s.set_query("/Users/met");
        assert_eq!(s.selected(), Some(&dir("/Users/meta")));
        s.set_query("users");
        assert_eq!(s.path_target(), None);
        assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::Dir(_))));
    }

    #[test]
    fn missing_path_falls_back_to_the_ranking() {
        let mut s = SwitcherState::new();
        s.set_items(vec![win("Finder", "nope")]);
        s.set_query("/nope");
        s.set_path_hits("/nope".into(), Vec::new(), Vec::new());
        assert!(s.filtered().iter().all(|m| !matches!(m.item, Item::Dir(_))));
    }
}
