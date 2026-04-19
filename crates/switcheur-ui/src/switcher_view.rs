//! The switcher panel itself. Owns a [`SwitcherState`] and renders it.

use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, App, ClickEvent, Context, EventEmitter,
    FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Render, ScrollStrategy, SharedString, Styled, Subscription, UniformListScrollHandle, Window,
};
use std::sync::Arc;

use switcheur_core::{Item, ProgramRef, Section, SwitcherState, WindowRef};
use switcheur_i18n::tr;

use crate::actions::{
    Backspace, Confirm, Copy, Cut, Delete, Dismiss, ExtendEnd, ExtendHome, ExtendLeft, ExtendRight,
    ExtendWordLeft, ExtendWordRight, FocusNextPane, FocusPrevPane, MoveEnd, MoveHome, MoveLeft,
    MoveRight, MoveWordLeft, MoveWordRight, Paste, SelectAll, SelectNext, SelectPrev,
};
use crate::input::QueryInput;
use crate::list::render_row;
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub enum SwitcherViewEvent {
    Confirmed(Item),
    Dismissed,
    OpenSettings,
    /// Signed frame deltas the host should apply to the switcher window so
    /// the input row's screen position never shifts. Programs-section growth
    /// sends `delta_origin_y = 0` + positive `delta_height` (bottom anchored).
    /// Results-panel suppression sends positive `delta_origin_y` + matching
    /// negative `delta_height` (top anchored).
    FrameDeltaChanged {
        delta_origin_y: f32,
        delta_height: f32,
    },
    /// User clicked "Activate licence" inside the in-panel nag card. Host
    /// starts the activation round-trip and flips the view to
    /// `NagPhase::Activating` for feedback.
    LicenseActivateRequested,
    /// User clicked "Later" — host hides the nag and restores the normal list.
    LicenseDismissed,
    /// User clicked the × on a window row. Host closes the target window via
    /// the platform and refreshes the list; the panel stays open.
    CloseWindowRequested(Arc<WindowRef>),
    /// User clicked "Download" on the update banner. Host starts the DMG
    /// download and flips the banner to `UpdateBannerState::Downloading`.
    UpdateDownloadRequested,
    /// User clicked the × on the update banner. Host marks the update
    /// dismissed for this session (no persistence).
    UpdateDismissed,
    /// The query changed (keystroke, paste, set_items reset). The host
    /// uses this to drive the zoxide subprocess off the UI thread; the
    /// view itself stays platform-agnostic.
    QueryChanged(String),
}

/// Top-of-panel banner shown when the startup update check reported a newer
/// version. Lives above the search input; never blocks the rest of the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateBannerState {
    Hidden,
    Available,
    Downloading,
    Ready,
}

/// Visibility state of the in-panel "support the project" card. Replaces
/// (and suppresses) the result list + programs section when not Hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NagPhase {
    Hidden,
    Visible,
    Activating,
}

pub struct SwitcherView {
    state: SwitcherState,
    input: QueryInput,
    theme: Theme,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    _activation_sub: Option<Subscription>,
    last_programs_extra: f32,
    last_list_shrink: f32,
    nag_phase: NagPhase,
    update_banner: UpdateBannerState,
    /// Mirrors the user's `zoxide_integration` setting. When false, no
    /// `QueryChanged` event is emitted and the right pane stays empty.
    /// The host (main.rs) is the one that actually shells out to zoxide
    /// — keeps platform code out of the UI crate.
    zoxide_enabled: bool,
}

impl SwitcherView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        Self {
            state: SwitcherState::new(),
            input: QueryInput::new(),
            theme: Theme::default(),
            focus,
            scroll: UniformListScrollHandle::default(),
            _activation_sub: None,
            last_programs_extra: 0.0,
            last_list_shrink: 0.0,
            nag_phase: NagPhase::Hidden,
            update_banner: UpdateBannerState::Hidden,
            zoxide_enabled: false,
        }
    }

    /// Mirror the user's zoxide_integration setting. When flipped off, the
    /// right-pane suggestions are cleared immediately. When flipped on, the
    /// host should emit a fresh `QueryChanged` synthetically (or the user's
    /// next keystroke triggers one).
    pub fn set_zoxide_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.zoxide_enabled == enabled {
            return;
        }
        self.zoxide_enabled = enabled;
        if !enabled {
            self.state.set_dirs(Vec::new());
            cx.notify();
        } else {
            // Ask the host to refresh dirs against the current query so the
            // pane populates without waiting for the next keystroke.
            cx.emit(SwitcherViewEvent::QueryChanged(
                self.state.query().to_string(),
            ));
        }
    }

    pub fn zoxide_enabled(&self) -> bool {
        self.zoxide_enabled
    }

    pub fn set_update_banner(&mut self, state: UpdateBannerState, cx: &mut Context<Self>) {
        self.update_banner = state;
        cx.notify();
    }

    pub fn set_nag_phase(&mut self, phase: NagPhase, cx: &mut Context<Self>) {
        self.nag_phase = phase;
        cx.notify();
    }

    pub fn nag_phase(&self) -> NagPhase {
        self.nag_phase
    }

    pub fn set_items(&mut self, items: Vec<Item>, cx: &mut Context<Self>) {
        self.input.clear();
        self.state.set_items(items);
        self.state.set_query("");
        if self.zoxide_enabled {
            cx.emit(SwitcherViewEvent::QueryChanged(String::new()));
        }
        self.emit_height_delta_if_changed(cx);
        cx.notify();
    }

    /// Refresh the candidate set in place without wiping the query or input.
    /// Used after closing a window from the list so the dead row disappears
    /// while the user's typing is preserved.
    pub fn refresh_items(&mut self, items: Vec<Item>, cx: &mut Context<Self>) {
        self.state.set_items(items);
        self.emit_height_delta_if_changed(cx);
        cx.notify();
    }

    /// Drop the window with the given CGWindowID from the visible list right
    /// away — optimistic so the row vanishes without waiting for the AX close
    /// to propagate through `list_windows`.
    pub fn drop_window(&mut self, id: u64, cx: &mut Context<Self>) {
        self.state.remove_window(id);
        self.emit_height_delta_if_changed(cx);
        cx.notify();
    }

    /// Install the installed-program catalogue. Cheap (Arc clones). Safe to
    /// call on every switcher open.
    pub fn set_programs(&mut self, programs: Vec<Arc<ProgramRef>>, cx: &mut Context<Self>) {
        self.state.set_programs(programs);
        self.emit_height_delta_if_changed(cx);
        cx.notify();
    }

    /// Install the preferred order for the "Ask LLM" fallback rows, as stored
    /// in the user config. Safe to call on every open.
    pub fn set_llm_provider_order(
        &mut self,
        order: Vec<switcheur_core::LlmProvider>,
        cx: &mut Context<Self>,
    ) {
        self.state.set_llm_provider_order(order);
        cx.notify();
    }

    /// Toggle the "Ask LLM" fallback rows. Mirrors the user setting.
    pub fn set_ask_llm_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.state.set_ask_llm_enabled(enabled);
        cx.notify();
    }

    /// Replace the right-pane directory suggestions. Called by the host with
    /// the result of an off-thread zoxide query (debounced per keystroke).
    pub fn set_dirs(&mut self, dirs: Vec<Item>, cx: &mut Context<Self>) {
        self.state.set_dirs(dirs);
        cx.notify();
    }

    /// Clear the right-pane suggestions. Called when the integration is
    /// turned off in settings or zoxide returns an empty list.
    pub fn clear_dirs(&mut self, cx: &mut Context<Self>) {
        self.state.set_dirs(Vec::new());
        cx.notify();
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Append text to the current query from an external source (Quick Type).
    /// Re-runs the fuzzy filter so the visible list updates immediately.
    pub fn append_query(&mut self, text: &str, cx: &mut Context<Self>) {
        self.input.insert_str(text);
        self.sync_query(cx);
    }

    /// Remove the last character from the query (Quick Type's Fn+Backspace).
    pub fn backspace_query(&mut self, cx: &mut Context<Self>) {
        self.input.backspace();
        self.sync_query(cx);
    }

    /// Advance the selection from an external driver (Cmd+Tab cycle).
    pub fn select_next_external(&mut self, cx: &mut Context<Self>) {
        self.state.move_down();
        self.scroll_selection_into_view();
        cx.notify();
    }

    /// Recede the selection from an external driver (Cmd+Shift+Tab cycle).
    pub fn select_prev_external(&mut self, cx: &mut Context<Self>) {
        self.state.move_up();
        self.scroll_selection_into_view();
        cx.notify();
    }

    /// Jump the selection to a specific index. Used when promoting a
    /// grace-period Cmd+Tab cycle into a visible panel so the cursor lands on
    /// the item the invisible cycle had already advanced to.
    pub fn set_selected_external(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.state.set_selected(idx);
        self.scroll_selection_into_view();
        cx.notify();
    }

    /// Confirm the current selection from an external driver (Cmd release).
    pub fn confirm_external(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = self.state.selected().cloned() {
            self._activation_sub = None;
            cx.emit(SwitcherViewEvent::Confirmed(item));
        }
    }

    pub fn dismiss_on_blur(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sub = cx.observe_window_activation(window, |view, window, cx| {
            let active = window.is_window_active();
            tracing::info!(active, "switcher window activation change");
            if active {
                return;
            }
            // Keep the panel alive while the user is in the browser activating
            // their license — otherwise opening the browser blurs us and kills
            // the outstanding poll.
            if view.nag_phase == NagPhase::Activating {
                return;
            }
            cx.emit(SwitcherViewEvent::Dismissed);
        });
        self._activation_sub = Some(sub);
    }

    // --- List navigation ---

    fn on_select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        if self.nag_phase != NagPhase::Hidden {
            return;
        }
        self.select_prev_external(cx);
    }

    fn on_select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.nag_phase != NagPhase::Hidden {
            return;
        }
        self.select_next_external(cx);
    }

    fn scroll_selection_into_view(&self) {
        // The programs section is small and rendered outside the uniform_list,
        // so only the windows list ever needs scrolling.
        if self.state.active_section() != Section::Windows {
            return;
        }
        if !self.state.filtered().is_empty() {
            self.scroll
                .scroll_to_item(self.state.selected_idx(), ScrollStrategy::Nearest);
        }
    }

    fn emit_height_delta_if_changed(&mut self, cx: &mut Context<Self>) {
        let programs = programs_extra_height(&self.state);
        let shrink = self.current_list_shrink();
        let d_programs = programs - self.last_programs_extra;
        let d_shrink = shrink - self.last_list_shrink;
        if d_programs.abs() < f32::EPSILON && d_shrink.abs() < f32::EPSILON {
            return;
        }
        self.last_programs_extra = programs;
        self.last_list_shrink = shrink;
        // Programs section grows above the input (anchor bottom → delta_origin_y
        // stays 0). Results-panel suppression shrinks below the input (anchor
        // top → delta_origin_y matches the shrink so bottom rises by the same
        // amount height drops).
        let delta_height = d_programs - d_shrink;
        let delta_origin_y = d_shrink;
        cx.emit(SwitcherViewEvent::FrameDeltaChanged {
            delta_origin_y,
            delta_height,
        });
    }

    /// Pixels the results list should be trimmed from the bottom of the
    /// window. Non-zero only when we're rendering an eval result with no
    /// matching items (and the nag card isn't covering the list).
    fn current_list_shrink(&self) -> f32 {
        let suppress = self.nag_phase == NagPhase::Hidden
            && self.state.filtered().is_empty()
            && self.state.eval_result().is_some();
        if suppress {
            LIST_AREA_HEIGHT
        } else {
            0.0
        }
    }

    fn on_confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        if self.nag_phase != NagPhase::Hidden {
            return;
        }
        if let Some(item) = self.state.selected().cloned() {
            self._activation_sub = None;
            cx.emit(SwitcherViewEvent::Confirmed(item));
        }
    }

    /// Click on a row: jump the selection there and activate — same end-state
    /// as pressing Enter on that entry.
    fn on_row_click(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.state.set_selected(idx);
        if let Some(item) = self.state.selected().cloned() {
            self._activation_sub = None;
            cx.emit(SwitcherViewEvent::Confirmed(item));
        }
    }

    /// Hover a row: move the selection highlight there, but do not activate.
    fn on_row_hover(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.state.active_section() == Section::Windows
            && self.state.selected_idx() == idx
        {
            return;
        }
        self.state.set_selected(idx);
        cx.notify();
    }

    /// Click on the row's × button: ask the host to close that window. The
    /// parent row's click handler has already been short-circuited via
    /// `stop_propagation` in the button's mouse-down.
    fn on_close_clicked(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(m) = self.state.filtered().get(idx) else {
            return;
        };
        if let Item::Window(w) = &m.item {
            cx.emit(SwitcherViewEvent::CloseWindowRequested(w.clone()));
        }
    }

    fn on_program_row_click(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.state.set_selected_program(idx);
        if let Some(item) = self.state.selected().cloned() {
            self._activation_sub = None;
            cx.emit(SwitcherViewEvent::Confirmed(item));
        }
    }

    fn on_program_row_hover(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.state.active_section() == Section::Programs
            && self.state.selected_program_idx() == idx
        {
            return;
        }
        self.state.set_selected_program(idx);
        cx.notify();
    }

    fn on_dir_row_click(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.state.set_selected_dir(idx);
        if let Some(item) = self.state.selected().cloned() {
            self._activation_sub = None;
            cx.emit(SwitcherViewEvent::Confirmed(item));
        }
    }

    fn on_dir_row_hover(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.state.active_section() == Section::Dirs && self.state.selected_dir_idx() == idx {
            return;
        }
        self.state.set_selected_dir(idx);
        cx.notify();
    }

    fn on_focus_next_pane(&mut self, _: &FocusNextPane, _: &mut Window, cx: &mut Context<Self>) {
        if self.nag_phase != NagPhase::Hidden {
            return;
        }
        if self.state.active_section() == Section::Dirs {
            self.state.focus_windows();
        } else if self.state.dirs_visible() {
            self.state.focus_dirs();
        } else {
            return;
        }
        cx.notify();
    }

    fn on_focus_prev_pane(&mut self, _: &FocusPrevPane, _: &mut Window, cx: &mut Context<Self>) {
        if self.nag_phase != NagPhase::Hidden {
            return;
        }
        if self.state.active_section() == Section::Dirs {
            self.state.focus_windows();
        } else if self.state.dirs_visible() {
            self.state.focus_dirs();
        } else {
            return;
        }
        cx.notify();
    }

    fn on_dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        self._activation_sub = None;
        cx.emit(SwitcherViewEvent::Dismissed);
    }

    fn on_cog_click(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        tracing::info!("cog clicked");
        self._activation_sub = None;
        cx.emit(SwitcherViewEvent::OpenSettings);
    }

    // --- Text editing ---

    fn on_backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.input.backspace();
        self.sync_query(cx);
    }

    fn on_delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.input.delete();
        self.sync_query(cx);
    }

    fn on_move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        // When the dirs pane is focused, Left snaps focus back to the
        // windows pane (the input doesn't have a visible caret while a
        // dir row is selected, so consuming the keystroke here costs
        // nothing). Otherwise behave as a normal text-caret motion.
        if self.state.active_section() == Section::Dirs {
            self.state.focus_windows();
            cx.notify();
            return;
        }
        self.input.move_left(false);
        cx.notify();
    }
    fn on_move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        // From the windows/programs pane, Right at the end of the input
        // jumps focus into the dirs pane (when one is visible). The
        // caret-at-end check preserves normal text editing — typing a
        // word and pressing Right moves through the characters first.
        if self.state.active_section() != Section::Dirs
            && self.state.dirs_visible()
            && self.input.cursor() >= self.input.text().len()
        {
            self.state.focus_dirs();
            cx.notify();
            return;
        }
        self.input.move_right(false);
        cx.notify();
    }
    fn on_move_home(&mut self, _: &MoveHome, _: &mut Window, cx: &mut Context<Self>) {
        self.input.move_home(false);
        cx.notify();
    }
    fn on_move_end(&mut self, _: &MoveEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.input.move_end(false);
        cx.notify();
    }
    fn on_extend_left(&mut self, _: &ExtendLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.input.move_left(true);
        cx.notify();
    }
    fn on_extend_right(&mut self, _: &ExtendRight, _: &mut Window, cx: &mut Context<Self>) {
        self.input.move_right(true);
        cx.notify();
    }
    fn on_extend_home(&mut self, _: &ExtendHome, _: &mut Window, cx: &mut Context<Self>) {
        self.input.move_home(true);
        cx.notify();
    }
    fn on_extend_end(&mut self, _: &ExtendEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.input.move_end(true);
        cx.notify();
    }
    fn on_move_word_left(&mut self, _: &MoveWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.input.move_word_left(false);
        cx.notify();
    }
    fn on_move_word_right(&mut self, _: &MoveWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.input.move_word_right(false);
        cx.notify();
    }
    fn on_extend_word_left(&mut self, _: &ExtendWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.input.move_word_left(true);
        cx.notify();
    }
    fn on_extend_word_right(&mut self, _: &ExtendWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.input.move_word_right(true);
        cx.notify();
    }
    fn on_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.input.select_all();
        cx.notify();
    }

    fn on_copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        // With a live selection, copy that. Otherwise fall back to the eval
        // result so Cmd+C on `2+2` puts `4` on the clipboard.
        if let Some(s) = self.input.selected_text() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(s.to_string()));
        } else if let Some(res) = self.state.eval_result() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(res.to_string()));
        }
    }

    fn on_cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.input.selected_text().map(str::to_string) else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(s));
        // `backspace` deletes the selection when there is one.
        self.input.backspace();
        self.sync_query(cx);
    }

    fn on_paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else {
            return;
        };
        // Single-line input: drop control chars (newlines, tabs, …) so a
        // multi-line paste collapses to a single search query.
        let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
        if cleaned.is_empty() {
            return;
        }
        self.input.insert_str(&cleaned);
        self.sync_query(cx);
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if k.modifiers.control || k.modifiers.platform || k.modifiers.function {
            return;
        }
        // Swallow keystrokes while the nag card is up: the user must click
        // Activate or Later explicitly. Escape still dismisses via
        // `on_dismiss` (actions are wired separately).
        if self.nag_phase != NagPhase::Hidden {
            return;
        }
        // Only consume characters the OS produced for this keystroke. If
        // `key_char` is None, it's a non-character key (arrows, function keys,
        // etc.) — those are handled by the action system, not by this path.
        let Some(ch) = k.key_char.as_deref() else {
            return;
        };
        if ch.is_empty() || ch.chars().any(|c| c.is_control()) {
            return;
        }
        self.input.insert_str(ch);
        self.sync_query(cx);
    }

    fn on_nag_activate(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.nag_phase == NagPhase::Activating {
            return;
        }
        self.nag_phase = NagPhase::Activating;
        cx.emit(SwitcherViewEvent::LicenseActivateRequested);
        cx.notify();
    }

    fn on_nag_dismiss(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.nag_phase == NagPhase::Activating {
            return;
        }
        self.nag_phase = NagPhase::Hidden;
        cx.emit(SwitcherViewEvent::LicenseDismissed);
        cx.notify();
    }

    fn on_update_download(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.update_banner, UpdateBannerState::Available) {
            return;
        }
        self.update_banner = UpdateBannerState::Downloading;
        cx.emit(SwitcherViewEvent::UpdateDownloadRequested);
        cx.notify();
    }

    fn on_update_dismiss(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.update_banner = UpdateBannerState::Hidden;
        cx.emit(SwitcherViewEvent::UpdateDismissed);
        cx.notify();
    }

    fn sync_query(&mut self, cx: &mut Context<Self>) {
        self.state.set_query(self.input.text());
        self.scroll_selection_into_view();
        if self.zoxide_enabled {
            cx.emit(SwitcherViewEvent::QueryChanged(
                self.state.query().to_string(),
            ));
        }
        self.emit_height_delta_if_changed(cx);
        cx.notify();
    }
}

impl Focusable for SwitcherView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<SwitcherViewEvent> for SwitcherView {}

impl Render for SwitcherView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let placeholder: SharedString = SharedString::from(tr("switcher.search_placeholder"));
        let filtered_count = self.state.filtered().len();
        let query_empty = self.input.text().is_empty();
        let nag_phase = self.nag_phase;
        // When a math/JS evaluation result is displayed and no items matched,
        // suppress the "no results" panel entirely — the eval value alone is
        // a complete answer and the empty panel below looks noisy.
        let hide_empty_list =
            filtered_count == 0 && nag_phase == NagPhase::Hidden && self.state.eval_result().is_some();

        let empty_msg: SharedString = if query_empty {
            SharedString::from(tr("switcher.no_windows"))
        } else {
            SharedString::from(tr("switcher.no_results"))
        };

        let list_section: AnyElement = if nag_phase != NagPhase::Hidden {
            render_nag_card(nag_phase, &theme, cx).into_any_element()
        } else if filtered_count == 0 {
            div()
                .px_3()
                .py_2()
                .text_size(px(13.0))
                .text_color(theme.muted)
                .child(empty_msg)
                .into_any_element()
        } else {
            uniform_list(
                "switcher-list",
                filtered_count,
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let selected = this.state.selected_idx();
                    let window_active = this.state.active_section() == Section::Windows;
                    let theme = this.theme;
                    // `filtered()` borrows `this`, but we also need `cx.listener`
                    // which borrows `this` mutably. Build the rows first using
                    // the shared borrow, then attach handlers after it drops.
                    let rows: Vec<_> = range
                        .clone()
                        .map(|i| {
                            let mr = &this.state.filtered()[i];
                            let is_window = matches!(mr.item, Item::Window(_));
                            (
                                i,
                                is_window,
                                render_row(mr, window_active && i == selected, &theme),
                            )
                        })
                        .collect();
                    rows.into_iter()
                        .map(|(i, is_window, row)| {
                            let row = if is_window {
                                row.child(render_close_button(i, &theme, cx))
                            } else {
                                row
                            };
                            row.id(("switcher-row", i))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                                    this.on_row_click(i, cx);
                                }))
                                .on_hover(cx.listener(move |this, hovering: &bool, _w, cx| {
                                    if *hovering {
                                        this.on_row_hover(i, cx);
                                    }
                                }))
                                .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .into_any_element()
        };

        div()
            .key_context("Switcher")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_dismiss))
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_move_left))
            .on_action(cx.listener(Self::on_move_right))
            .on_action(cx.listener(Self::on_move_home))
            .on_action(cx.listener(Self::on_move_end))
            .on_action(cx.listener(Self::on_extend_left))
            .on_action(cx.listener(Self::on_extend_right))
            .on_action(cx.listener(Self::on_extend_home))
            .on_action(cx.listener(Self::on_extend_end))
            .on_action(cx.listener(Self::on_move_word_left))
            .on_action(cx.listener(Self::on_move_word_right))
            .on_action(cx.listener(Self::on_extend_word_left))
            .on_action(cx.listener(Self::on_extend_word_right))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_cut))
            .on_action(cx.listener(Self::on_paste))
            .on_action(cx.listener(Self::on_focus_next_pane))
            .on_action(cx.listener(Self::on_focus_prev_pane))
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .size_full()
            .rounded_lg()
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .text_color(theme.foreground)
            .text_size(px(14.0))
            .children(render_update_banner(self.update_banner.clone(), &theme, cx))
            .children(if nag_phase == NagPhase::Hidden {
                programs_section(&self.state, &theme, cx)
            } else {
                None
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(18.0))
                    .child(
                        div()
                            .flex_1()
                            .child(render_query(&self.input, &placeholder, &theme)),
                    )
                    .child(
                        div()
                            .ml_2()
                            .w(px(24.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(px(16.0))
                            .text_color(theme.muted)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(Self::on_cog_click),
                            )
                            .child("⚙"),
                    ),
            )
            .children(self.state.eval_result().map(|res| {
                let size = if res.len() > 30 { px(16.0) } else { px(22.0) };
                let res_string = res.to_string();
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .mr_2()
                            .text_color(theme.muted)
                            .text_size(px(18.0))
                            .child("="),
                    )
                    .child(
                        div()
                            .text_color(theme.accent)
                            .text_size(size)
                            .child(SharedString::from(res_string.clone())),
                    )
                    .child(
                        div()
                            .id("eval-copy-btn")
                            .ml_2()
                            .w(px(24.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(px(14.0))
                            .text_color(theme.muted)
                            .hover(|s| s.bg(theme.selection).text_color(theme.foreground))
                            .on_click(cx.listener(move |_this, _: &ClickEvent, _, cx| {
                                cx.write_to_clipboard(
                                    gpui::ClipboardItem::new_string(res_string.clone()),
                                );
                                cx.stop_propagation();
                            }))
                            .child("⧉"),
                    )
            }))
            .children(if hide_empty_list {
                None
            } else {
                let dirs_pane = (nag_phase == NagPhase::Hidden && self.state.dirs_visible())
                    .then(|| render_dirs_panel(&self.state, &theme, cx));
                Some(
                    div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .overflow_hidden()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .px_2()
                                .py_2()
                                .overflow_hidden()
                                .child(list_section),
                        )
                        .children(dirs_pane),
                )
            })
    }
}

/// Render the query text with a visible caret and selection highlight.
fn render_query(input: &QueryInput, placeholder: &SharedString, theme: &Theme) -> AnyElement {
    let text = input.text();
    if text.is_empty() {
        return div()
            .flex()
            .flex_row()
            .items_center()
            .child(
                div()
                    .w(px(2.0))
                    .h(px(20.0))
                    .bg(theme.accent),
            )
            .child(
                div()
                    .ml_1()
                    .text_color(theme.muted)
                    .child(placeholder.clone()),
            )
            .into_any_element();
    }

    let mut row = div().flex().flex_row().items_center();

    match input.selection() {
        Some(sel) => {
            let before = &text[..sel.start];
            let selected = &text[sel.clone()];
            let after = &text[sel.end..];
            if !before.is_empty() {
                row = row.child(
                    div()
                        .text_color(theme.foreground)
                        .child(before.to_string()),
                );
            }
            row = row.child(
                div()
                    .px_0p5()
                    .bg(theme.accent)
                    .text_color(gpui::rgb(0xffffff))
                    .child(selected.to_string()),
            );
            if !after.is_empty() {
                row = row.child(
                    div()
                        .text_color(theme.foreground)
                        .child(after.to_string()),
                );
            }
        }
        None => {
            let cursor = input.cursor();
            let (before, after) = text.split_at(cursor);
            if !before.is_empty() {
                row = row.child(
                    div()
                        .text_color(theme.foreground)
                        .child(before.to_string()),
                );
            }
            row = row.child(
                div()
                    .w(px(2.0))
                    .h(px(20.0))
                    .bg(theme.accent),
            );
            if !after.is_empty() {
                row = row.child(
                    div()
                        .text_color(theme.foreground)
                        .child(after.to_string()),
                );
            }
        }
    }

    row.into_any_element()
}

/// Small × button rendered at the right edge of a window row. Clicking it
/// asks the host to close the target window without triggering the row's
/// own click (selection + activate) — the `mouse_down` handler short-circuits
/// propagation to the parent.
fn render_close_button(
    idx: usize,
    theme: &Theme,
    cx: &mut Context<SwitcherView>,
) -> AnyElement {
    let muted = theme.muted;
    let foreground = theme.foreground;
    let hover_bg = theme.border;
    div()
        .id(("switcher-close-btn", idx))
        .w(px(20.0))
        .h(px(20.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .text_size(px(13.0))
        .text_color(muted)
        .cursor_pointer()
        .hover(move |d| d.bg(hover_bg).text_color(foreground))
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, _w, cx| cx.stop_propagation(),
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
            this.on_close_clicked(idx, cx);
        }))
        .child("×")
        .into_any_element()
}

/// Visible only when the query has at least one program match. Returns an
/// `Option<impl IntoElement>` so `.children(...)` quietly collapses when the
/// section should be hidden (no empty placeholder per product spec).
fn programs_section(
    state: &SwitcherState,
    theme: &Theme,
    cx: &mut Context<SwitcherView>,
) -> Option<AnyElement> {
    if !state.programs_visible() {
        return None;
    }
    let selected = state.selected_program_idx();
    let section_active = state.active_section() == Section::Programs;
    let programs = state.filtered_programs();

    let rows: Vec<AnyElement> = programs
        .iter()
        .enumerate()
        .map(|(i, m)| {
            render_row(m, section_active && i == selected, theme)
                .id(("switcher-program-row", i))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                    this.on_program_row_click(i, cx);
                }))
                .on_hover(cx.listener(move |this, hovering: &bool, _w, cx| {
                    if *hovering {
                        this.on_program_row_hover(i, cx);
                    }
                }))
                .into_any_element()
        })
        .collect();

    Some(
        div()
            .flex()
            .flex_col()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .children(rows)
            .into_any_element(),
    )
}

/// Right-side panel listing zoxide directory suggestions. Hidden entirely
/// when `state.dirs()` is empty — the existing layout is unchanged in that
/// case. Width is fixed; the windows pane keeps `flex_1` and absorbs the
/// remainder.
fn render_dirs_panel(
    state: &SwitcherState,
    theme: &Theme,
    cx: &mut Context<SwitcherView>,
) -> AnyElement {
    use switcheur_core::MatchResult;

    let section_active = state.active_section() == Section::Dirs;
    let selected = state.selected_dir_idx();
    let dirs = state.dirs();

    let rows: Vec<AnyElement> = dirs
        .iter()
        .enumerate()
        .map(|(i, item)| {
            // `render_row` takes a MatchResult; dirs aren't fuzzy-ranked
            // through the matcher (zoxide already ranks them), so wrap with a
            // zero score and no highlight indices.
            let mr = MatchResult {
                item: item.clone(),
                score: 0,
                indices: Vec::new(),
            };
            render_row(&mr, section_active && i == selected, theme)
                .id(("switcher-dir-row", i))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                    this.on_dir_row_click(i, cx);
                }))
                .on_hover(cx.listener(move |this, hovering: &bool, _w, cx| {
                    if *hovering {
                        this.on_dir_row_hover(i, cx);
                    }
                }))
                .into_any_element()
        })
        .collect();

    div()
        .flex()
        .flex_col()
        .w(px(260.0))
        .border_l_1()
        .border_color(theme.border)
        .px_2()
        .py_2()
        .gap_0p5()
        .overflow_hidden()
        .child(
            div()
                .px_2()
                .pb_1()
                .text_size(px(11.0))
                .text_color(theme.muted)
                .child(SharedString::from(tr("switcher.dirs_header"))),
        )
        .children(rows)
        .into_any_element()
}

/// Centred support-the-project card shown in place of the result list when
/// the app is unlicensed and the nag threshold has been crossed.
fn render_nag_card(
    phase: NagPhase,
    theme: &Theme,
    cx: &mut Context<SwitcherView>,
) -> AnyElement {
    let accent = theme.accent;
    let heart_bg = gpui::rgba(0xe5ebff20);
    let activating = phase == NagPhase::Activating;

    let primary_label: SharedString = if activating {
        SharedString::from(tr("license.activating"))
    } else {
        SharedString::from(tr("license.activate"))
    };
    let secondary_label: SharedString = SharedString::from(tr("license.later"));
    let title: SharedString = SharedString::from(tr("license.title"));
    let body: SharedString = SharedString::from(tr("license.body"));

    let primary = div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(36.0))
        .px_5()
        .rounded_full()
        .bg(accent)
        .text_color(gpui::rgb(0xffffff))
        .text_size(px(13.5))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(SwitcherView::on_nag_activate),
        )
        .child(primary_label);

    let secondary = div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(36.0))
        .px_5()
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .text_color(theme.muted)
        .text_size(px(13.5))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(SwitcherView::on_nag_dismiss),
        )
        .child(secondary_label);

    let heart_badge = div()
        .w(px(48.0))
        .h(px(48.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(heart_bg)
        .text_size(px(24.0))
        .text_color(accent)
        .child("♥");

    let card = div()
        .flex()
        .flex_col()
        .items_center()
        .gap_3()
        .max_w(px(360.0))
        .child(heart_badge)
        .child(
            div()
                .text_size(px(18.0))
                .text_color(theme.foreground)
                .child(title),
        )
        .child(
            div()
                .text_size(px(13.5))
                .text_color(theme.muted)
                .text_center()
                .child(body),
        )
        .child(
            div()
                .mt_2()
                .flex()
                .flex_row()
                .gap_2()
                .child(primary)
                .child(secondary),
        );

    div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .px_6()
        .py_8()
        .child(card)
        .into_any_element()
}

/// Thin top-of-panel banner announcing a new release. Hidden when the
/// startup update check found nothing or when the user clicked ×.
fn render_update_banner(
    state: UpdateBannerState,
    theme: &Theme,
    cx: &mut Context<SwitcherView>,
) -> Option<AnyElement> {
    if matches!(state, UpdateBannerState::Hidden) {
        return None;
    }
    let accent = theme.accent;
    let label: SharedString = SharedString::from(tr("update.available"));
    let (action_label_key, action_enabled) = match state {
        UpdateBannerState::Available => ("update.download", true),
        UpdateBannerState::Downloading => ("update.downloading", false),
        UpdateBannerState::Ready => ("update.ready", false),
        UpdateBannerState::Hidden => unreachable!(),
    };
    let action_label: SharedString = SharedString::from(tr(action_label_key));

    let action = {
        let base = div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(22.0))
            .px_3()
            .rounded_full()
            .text_size(px(12.0));
        if action_enabled {
            base.bg(accent)
                .text_color(gpui::rgb(0xffffff))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(SwitcherView::on_update_download),
                )
                .child(action_label)
        } else {
            base.border_1()
                .border_color(theme.border)
                .text_color(theme.muted)
                .child(action_label)
        }
    };

    let dismiss = div()
        .ml_2()
        .w(px(20.0))
        .h(px(20.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .text_size(px(13.0))
        .text_color(theme.muted)
        .hover(|s| s.bg(theme.selection).text_color(theme.foreground))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(SwitcherView::on_update_dismiss),
        )
        .child("×");

    Some(
        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(30.0))
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.selection)
            .child(
                div()
                    .mr_2()
                    .text_size(px(13.0))
                    .text_color(accent)
                    .child("⤓"),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.5))
                    .text_color(theme.foreground)
                    .child(label),
            )
            .child(action)
            .child(dismiss)
            .into_any_element(),
    )
}

/// Pixels the main results list occupies at base window height. When the
/// list is suppressed (eval-only mode) the window shrinks by this amount.
/// Rough budget: base HEIGHT minus input row + eval row + borders/padding.
const LIST_AREA_HEIGHT: f32 = 300.0;

/// Extra vertical pixels the programs section adds. Mirrors the padding +
/// row heights used in [`programs_section`] so the host window can resize
/// to fit. Returns 0 when the section is hidden.
fn programs_extra_height(state: &SwitcherState) -> f32 {
    if !state.programs_visible() {
        return 0.0;
    }
    const ROW: f32 = 44.0;
    const SECTION_PADDING: f32 = 10.0; // py_1 top + py_1 bottom + border
    state.filtered_programs().len() as f32 * ROW + SECTION_PADDING
}
