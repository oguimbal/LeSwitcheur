//! The switcher panel itself. Owns a [`SwitcherState`] and renders it.

use gpui::{
    canvas, div, linear, percentage, prelude::*, px, svg, uniform_list, Animation, AnimationExt,
    AnyElement, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, ScrollStrategy, SharedString,
    Styled, Subscription, Transformation, UniformListScrollHandle, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};
use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use switcheur_core::{
    DirRef, Item, LlmProvider, PlaybackState, ProgramRef, Section, SwitcherState, WindowRef,
};
use switcheur_i18n::{tr, tr_sub};

use crate::actions::{
    Confirm, Dismiss, FocusNextPane, FocusPrevPane, MoveLeft, MoveRight, SelectNext, SelectPrev,
};
use crate::list::{render_row, OpenClawRow};
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
    /// User clicked the × on a zoxide directory row. Host runs
    /// `zoxide remove <path>` off the UI thread; the row is dropped from the
    /// view optimistically so the UI is immediate.
    RemoveDirRequested(Arc<DirRef>),
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
    /// The state reached the browser-tabs fallback tier and no scan has
    /// been delivered yet. The host responds by running the AppleScript
    /// scrape off the UI thread and feeding the result back via
    /// [`SwitcherView::set_browser_tabs`]. One-shot per switcher session —
    /// emission is gated by a view-level flag reset in `set_items`.
    NeedsBrowserTabs,
    /// The switcher just opened: detect the app currently producing audio
    /// output and feed it back via [`SwitcherView::set_currently_playing`].
    /// Fired once per session at open time, regardless of query state —
    /// the row only renders when the query is empty (see
    /// [`switcheur_core::SwitcherState::suggested_items_visible`]).
    NeedsCurrentlyPlaying,
    /// Any state touching the "Open With" popover just changed: the dir
    /// selection moved, the popover gained/lost keyboard focus, or the
    /// popover index shifted. The host reads [`SwitcherView`] accessors to
    /// decide whether to show/move/hide the popover window and to sync its
    /// view contents.
    OpenWithStateChanged,
    /// User pressed Enter (or clicked a row) while the popover was
    /// keyboard-focused. Carries the 0-based index into the *selectable*
    /// popover rows (default row excluded). The host resolves it to a
    /// bundle id and launches the selected folder with that app.
    OpenWithActivated(usize),
    /// User clicked the play/pause button on a "Currently Playing" row.
    /// Carries the row's full descriptor so the host can pick the right
    /// toggle path (browser tab JS injection vs. AppleScript scripting
    /// dictionary) without re-deriving it from a bundle id.
    TogglePlayPause(Arc<switcheur_core::AudioRowRef>),
    /// Enter on the OpenClaw row while a gateway is configured. The panel
    /// stays open: the host clears the query, sends the instruction off the
    /// UI thread and reports back through [`SwitcherView::instruction_sent`]
    /// or [`SwitcherView::instruction_failed`].
    SendInstruction(String),
    /// The query is an identifier (`ENG-123`) of a connected Linear team.
    /// Host answers through [`SwitcherView::set_linear_issue`].
    NeedsLinearIssue(String),
    /// The query became an absolute path; the host answers with
    /// [`SwitcherView::set_path_hits`].
    NeedsPathHits(String),
    /// Create a Linear issue titled `title` in the host's team choice
    /// `team` (0 = default, MRU first). The panel stays open; the host
    /// opens the issue and closes it, or calls
    /// [`SwitcherView::linear_issue_failed`].
    CreateLinearIssue { title: String, team: usize },
}

/// Progress of the last OpenClaw instruction sent from this panel.
#[derive(Debug, Clone, PartialEq)]
pub enum InstructionPhase {
    Idle,
    Sending,
    /// Enter on an empty query now opens `conversation_url`.
    Sent { conversation_url: Option<Arc<str>> },
    Failed { message: SharedString },
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
    /// Text input widget. Owns its focus, IME, selection, undo/redo,
    /// clipboard handlers via `gpui_component::Input`. Constructed by the
    /// host at panel-open time (needs a `&mut Window`) and passed in.
    input: Entity<InputState>,
    _input_sub: Option<Subscription>,
    theme: Theme,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    _activation_sub: Option<Subscription>,
    last_extras_above_input: f32,
    last_list_shrink: f32,
    nag_phase: NagPhase,
    update_banner: UpdateBannerState,
    /// True when a directory source (zoxide, Spotlight, …) is live. When
    /// false, no `QueryChanged` event is emitted and the right pane stays
    /// empty. The host (main.rs) is the one that actually shells out to the
    /// backing tool — keeps platform code out of the UI crate.
    dirs_enabled: bool,
    /// True while an off-thread dir-source query is in flight. Swaps the
    /// settings cog at the top-right for a rotating spinner so slow
    /// Spotlight queries visibly register. Host toggles via
    /// [`Self::set_dirs_loading`] on either side of the subprocess.
    dirs_loading: bool,
    /// Whether the active source supports removing entries. Zoxide does
    /// (`zoxide remove …`), Spotlight doesn't. Drives the × button render;
    /// host also refuses the action at the `RemoveDirRequested` boundary as
    /// defence-in-depth.
    dirs_removable: bool,
    /// Has a `NeedsBrowserTabs` event already been fired for the current
    /// switcher session? Prevents re-requesting on every subsequent
    /// keystroke after the first fallback-tier hit. Reset in `set_items`
    /// (the switcher is reopened) so each session gets one fresh scan.
    /// Also reset on scan failure so a retry is allowed — rate-limited by
    /// [`SwitcherView::browser_tabs_retry_after`].
    browser_tabs_requested: bool,
    /// Earliest Instant at which a fresh `NeedsBrowserTabs` may fire after
    /// a failed scan (AppleScript timeout / permission error). Throttles
    /// retries so a stuck Chrome doesn't trigger an osascript on every
    /// keystroke. `None` = no cooldown (either no failure yet, or cooldown
    /// elapsed).
    browser_tabs_retry_after: Option<Instant>,
    /// Has a `NeedsCurrentlyPlaying` event already been fired for the
    /// current switcher session? One-shot per open, mirroring the
    /// `browser_tabs_requested` latch. Reset in `set_items`.
    currently_playing_requested: bool,
    /// i18n key of the line shown when handing a pick to another app failed.
    action_error: Option<&'static str>,
    /// Number of *selectable* rows in the "Open With" popover for folder
    /// rows — alternative folder openers, default excluded. Host sets this
    /// whenever its detected-apps list changes. When zero (and the current
    /// dir row is a folder), the popover must not show and keyboard nav
    /// into it is a no-op.
    open_with_folder_count: usize,
    /// Same idea for *file* rows (Spotlight only). Today this is always
    /// either 0 (no file manager detected beyond Finder, which maps to a
    /// single "Show in Finder" action — handled explicitly) or 1.
    open_with_file_count: usize,
    /// Window-local Y (top-down, points) of the dirs panel's top edge,
    /// captured during render via a canvas probe. Invariant across
    /// selection changes inside the panel — only shifts when the banner,
    /// programs section, or eval row appear/disappear. Cleared to `None`
    /// when the dirs panel isn't visible so the host can skip popover
    /// placement until a fresh measurement lands.
    dirs_panel_top_y: Rc<Cell<Option<f32>>>,
    /// Configured (and paired) OpenClaw agent. `None` when it isn't set up
    /// yet: picking the row then goes to the host (setup window).
    openclaw: Option<OpenClawRow>,
    instruction_phase: InstructionPhase,
    /// Bumped per send so the checkmark animation replays.
    instruction_seq: usize,
    /// Set by a send: only the input row is shown until the next key press.
    input_only: bool,
    /// Enter reaches both the input's `PressEnter` and the `Confirm`
    /// action; emit `Confirmed` once per panel.
    confirmed: bool,
    /// Linear teams the create row can target. Above one, the "Open With"
    /// popover lists them next to the row.
    linear_team_count: usize,
    /// Identifier of the last `NeedsLinearIssue`, so each is asked once.
    linear_lookup_requested: Option<String>,
    /// Path of the last `NeedsPathHits`, so each is asked once.
    path_lookup_requested: Option<String>,
    /// Tab cycle over a typed path's completions: kept while the query is
    /// still the candidate Tab last wrote.
    path_cycle: Option<(Vec<String>, usize)>,
    /// Window-local Y of the popover's row (Linear create row, or a path
    /// row in the list), probed while painting.
    row_center_y: Rc<Cell<Option<f32>>>,
    /// Anchor last handed to the host; a paint that moves it re-syncs.
    row_anchor_sent: Option<f32>,
}

/// Cooldown between scan-failure retries. Long enough that a slow Chrome
/// gets breathing room, short enough that the user doesn't notice when
/// typing the next character.
const BROWSER_TABS_RETRY_COOLDOWN: Duration = Duration::from_millis(1500);

impl SwitcherView {
    pub fn new(input: Entity<InputState>, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let sub = cx.subscribe(
            &input,
            |this: &mut Self, _state, ev: &InputEvent, cx| match ev {
                InputEvent::Change => this.sync_query(cx),
                InputEvent::PressEnter { .. } => this.confirm_selection(cx),
                _ => {}
            },
        );
        Self {
            state: SwitcherState::new(),
            input,
            _input_sub: Some(sub),
            theme: Theme::default(),
            focus,
            scroll: UniformListScrollHandle::default(),
            _activation_sub: None,
            last_extras_above_input: 0.0,
            last_list_shrink: 0.0,
            nag_phase: NagPhase::Hidden,
            update_banner: UpdateBannerState::Hidden,
            dirs_enabled: false,
            dirs_removable: false,
            dirs_loading: false,
            browser_tabs_requested: false,
            browser_tabs_retry_after: None,
            currently_playing_requested: false,
            action_error: None,
            open_with_folder_count: 0,
            open_with_file_count: 0,
            dirs_panel_top_y: Rc::new(Cell::new(None)),
            openclaw: None,
            instruction_phase: InstructionPhase::Idle,
            instruction_seq: 0,
            input_only: false,
            confirmed: false,
            linear_team_count: 0,
            linear_lookup_requested: None,
            path_lookup_requested: None,
            path_cycle: None,
            row_center_y: Rc::new(Cell::new(None)),
            row_anchor_sent: None,
        }
    }

    /// Linear integration. `default_team` labels the create row (`None`
    /// when no account is connected), `teams` (`Item::LinearTeam`) gate
    /// issue lookups and open on an exact key/name, `team_count` sizes the
    /// team popover.
    pub fn set_linear(
        &mut self,
        default_team: Option<String>,
        teams: Vec<Item>,
        team_count: usize,
        cx: &mut Context<Self>,
    ) {
        self.linear_team_count = team_count;
        self.state.set_linear(default_team.map(Arc::from), teams);
        cx.notify();
    }

    pub fn set_linear_issue(
        &mut self,
        identifier: String,
        issue: Option<Item>,
        cx: &mut Context<Self>,
    ) {
        self.state.set_linear_issue(identifier, issue);
        self.emit_linear_popover_sync(cx);
        cx.notify();
    }

    /// Creation failed: the title stays in the field, the reason above it.
    pub fn linear_issue_failed(&mut self, message: String, cx: &mut Context<Self>) {
        self.instruction_phase = InstructionPhase::Failed {
            message: message.into(),
        };
        self.confirmed = false;
        cx.notify();
    }

    /// True when the selected row is "Create a Linear issue".
    pub fn linear_create_selected(&self) -> bool {
        self.state.active_section() == Section::Windows
            && matches!(self.state.selected(), Some(Item::CreateLinearIssue { .. }))
    }

    /// Existing files/folders for the typed path `target`.
    pub fn set_path_hits(
        &mut self,
        target: String,
        hits: Vec<Item>,
        completions: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.state.set_path_hits(target, hits, completions);
        self.scroll_selection_into_view();
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
        self.emit_height_delta_if_changed(cx);
        cx.notify();
    }

    /// Shell-style Tab on a typed path: complete to the first candidate,
    /// then cycle (`forward` = Tab, else Shift-Tab). False when the query is
    /// not a path, so Tab keeps switching panes.
    fn complete_path(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.state.path_target().is_none() || !self.is_input_focused(window, cx) {
            return false;
        }
        let value = self.input.read(cx).value().to_string();
        let (candidates, index) = match self.path_cycle.take() {
            Some((candidates, index)) if candidates.get(index) == Some(&value) => {
                let n = candidates.len();
                (candidates, if forward { (index + 1) % n } else { (index + n - 1) % n })
            }
            _ => {
                let Some(candidates) = self.state.path_completions().filter(|c| !c.is_empty()) else {
                    return true;
                };
                let index = if forward { 0 } else { candidates.len() - 1 };
                (candidates.to_vec(), index)
            }
        };
        let next = candidates[index].clone();
        // A single candidate completes once; typing the separator then
        // steps into it.
        if candidates.len() > 1 {
            self.path_cycle = Some((candidates, index));
        }
        self.input.update(cx, |s, cx| s.set_value(next, window, cx));
        self.sync_query(cx);
        true
    }

    /// Grey completion shown after the typed path, caret at the end.
    fn path_ghost(&self, window: &Window, cx: &App) -> Option<(String, SharedString)> {
        let ghost = self.state.path_ghost()?;
        let input = self.input.read(cx);
        let value = input.value();
        let cycling = matches!(&self.path_cycle, Some((c, i)) if c.get(*i).map(String::as_str) == Some(value.as_ref()));
        if cycling || input.cursor() < value.len() || !input.focus_handle(cx).is_focused(window) {
            return None;
        }
        Some((value.to_string(), SharedString::from(ghost.to_string())))
    }

    /// Selected file/folder row, from the dirs pane or from a typed path.
    pub fn selected_fs_item(&self) -> Option<&Item> {
        self.state.selected().filter(|it| matches!(it, Item::Dir(_)))
    }

    /// True when a typed path's file/folder row is selected in the list.
    fn list_dir_selected(&self) -> bool {
        self.state.active_section() == Section::Windows && self.selected_fs_item().is_some()
    }

    /// Window-local Y of the row the popover points at.
    pub fn open_with_anchor_y(&self) -> Option<f32> {
        if self.linear_create_selected() || self.list_dir_selected() {
            return self.row_center_y.get();
        }
        self.dirs_panel_top_y
            .get()
            .map(|top| top + self.dirs_row_center_from_panel_top(self.state.selected_dir_idx()))
    }

    /// `choice` indexes the popover's selectable rows; `None` = default team.
    fn start_linear_create(&mut self, choice: Option<usize>, cx: &mut Context<Self>) {
        let Some(Item::CreateLinearIssue { title, .. }) = self.state.selected().cloned() else {
            return;
        };
        if self.instruction_phase == InstructionPhase::Sending {
            return;
        }
        self.instruction_phase = InstructionPhase::Sending;
        cx.emit(SwitcherViewEvent::CreateLinearIssue {
            title: title.to_string(),
            team: choice.map_or(0, |i| i + 1),
        });
        cx.notify();
    }

    /// Rows moved under the team popover: let the host re-anchor or hide it.
    fn emit_linear_popover_sync(&mut self, cx: &mut Context<Self>) {
        if self.linear_team_count > 1 || self.state.path_target().is_some() {
            cx.emit(SwitcherViewEvent::OpenWithStateChanged);
        }
    }

    /// Configured OpenClaw agent, `None` when not set up.
    pub fn set_openclaw(&mut self, openclaw: Option<OpenClawRow>, cx: &mut Context<Self>) {
        self.openclaw = openclaw;
        cx.notify();
    }

    fn openclaw_label(&self) -> SharedString {
        self.openclaw.clone().unwrap_or_default().name
    }

    /// Host acknowledged a [`SwitcherViewEvent::SendInstruction`]: clear the
    /// query so the panel is ready for the next instruction.
    pub fn instruction_sending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = tr_sub("switcher.instruction_sending", &[("name", &self.openclaw_label())]);
        self.input_only = true;
        self.input.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_placeholder(placeholder, window, cx);
        });
        self.sync_query(cx);
    }

    pub fn instruction_sent(
        &mut self,
        conversation_url: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.instruction_seq += 1;
        self.instruction_phase = InstructionPhase::Sent {
            conversation_url: conversation_url.map(Arc::from),
        };
        let placeholder = tr_sub("switcher.instruction_open", &[("name", &self.openclaw_label())]);
        self.input
            .update(cx, |s, cx| s.set_placeholder(placeholder, window, cx));
        self.emit_height_delta_if_changed(cx);
        cx.notify();
    }

    /// Put the instruction back in the field so it isn't lost, and show why.
    pub fn instruction_failed(
        &mut self,
        text: String,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.instruction_phase = InstructionPhase::Failed {
            message: message.into(),
        };
        self.input_only = false;
        let placeholder = tr("switcher.search_placeholder");
        self.input.update(cx, |s, cx| {
            s.set_placeholder(placeholder, window, cx);
            if s.value().is_empty() {
                s.set_value(text, window, cx);
            }
        });
        self.sync_query(cx);
    }

    /// Routes a confirmed item: a configured OpenClaw row sends in place,
    /// everything else goes to the host and closes the panel.
    fn confirm_item(&mut self, item: Item, cx: &mut Context<Self>) {
        if matches!(item, Item::CreateLinearIssue { .. }) {
            self.start_linear_create(None, cx);
            return;
        }
        if let Item::AskLlm {
            provider: LlmProvider::OpenClaw,
            query,
        } = &item
        {
            if self.openclaw.is_some() {
                if self.instruction_phase != InstructionPhase::Sending {
                    self.confirmed = false;
                    self.instruction_phase = InstructionPhase::Sending;
                    cx.emit(SwitcherViewEvent::SendInstruction(query.to_string()));
                    cx.notify();
                }
                return;
            }
        }
        self.emit_confirmed(item, cx);
    }

    fn emit_confirmed(&mut self, item: Item, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.confirmed, true) {
            return;
        }
        self._activation_sub = None;
        cx.emit(SwitcherViewEvent::Confirmed(item));
    }

    /// First key press after a send brings the list back.
    fn on_key_down_capture(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.input_only || matches!(ev.keystroke.key.as_str(), "enter" | "escape") {
            return;
        }
        self.input_only = false;
        self.emit_height_delta_if_changed(cx);
        cx.notify();
    }

    /// Install the host-computed counts of "open with" selectable rows for
    /// folder and file dir rows. The view picks which one applies for the
    /// currently selected row via [`Self::current_open_with_count`].
    /// Emitting `OpenWithStateChanged` keeps the popover window in sync
    /// even when the count flips between 0 and non-zero.
    pub fn set_open_with_counts(
        &mut self,
        folder: usize,
        file: usize,
        cx: &mut Context<Self>,
    ) {
        if self.open_with_folder_count == folder && self.open_with_file_count == file {
            return;
        }
        self.open_with_folder_count = folder;
        self.open_with_file_count = file;
        if self.current_open_with_count() == 0 {
            self.state.exit_open_with();
        }
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
        cx.notify();
    }

    /// Which Open-With selectable count applies right now — file vs folder —
    /// based on the currently highlighted dir row. Defaults to the folder
    /// count when the selection isn't a `DirRef` (e.g. no dirs yet), which
    /// keeps the popover sized sensibly for the common case.
    fn current_open_with_count(&self) -> usize {
        if self.linear_create_selected() {
            return self.linear_team_count.saturating_sub(1);
        }
        // A Lena folder not on disk has no path for folder openers; a Lena
        // file keeps "Show in …", which Lena itself performs.
        if matches!(self.selected_fs_item(), Some(Item::Dir(d)) if d.is_dir && d.local_path().is_none()) {
            return 0;
        }
        let is_file = matches!(self.selected_fs_item(), Some(Item::Dir(d)) if !d.is_dir);
        if is_file {
            self.open_with_file_count
        } else {
            self.open_with_folder_count
        }
    }

    /// Mouse hover on a popover row — mirrors the pointer in the popover's
    /// keyboard selection so Enter hits what the cursor is over.
    pub fn on_open_with_hover(&mut self, idx: usize, cx: &mut Context<Self>) {
        let count = self.current_open_with_count();
        if count == 0 {
            return;
        }
        self.state.set_open_with_index(idx, count);
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
        cx.notify();
    }

    /// Mouse click on a popover row — same effect as Enter with that row
    /// selected. The host performs the actual launch.
    pub fn on_open_with_click(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.current_open_with_count() {
            return;
        }
        if self.linear_create_selected() {
            self.start_linear_create(Some(idx), cx);
            return;
        }
        cx.emit(SwitcherViewEvent::OpenWithActivated(idx));
    }

    /// Should the popover window currently be visible? True when the dirs
    /// pane has focus, at least one dir row exists, and at least one
    /// alternative opener was detected for the selected row's kind. The
    /// `nag_phase` guard mirrors the dirs-panel render gate (see `render`)
    /// so the host's `sync_open_with_popover` doesn't try to anchor a
    /// popover against a panel that isn't on screen.
    pub fn open_with_visible(&self) -> bool {
        self.nag_phase == NagPhase::Hidden
            && !self.input_only
            && (self.linear_create_selected()
                || self.list_dir_selected()
                || (self.state.active_section() == Section::Dirs && self.state.dirs_visible()))
            && self.current_open_with_count() > 0
    }

    /// Keyboard index inside the popover. `None` while the popover is
    /// passive (dir row still focused).
    pub fn open_with_index(&self) -> Option<usize> {
        self.state.open_with_index()
    }

    /// Index of the currently highlighted dir row — used by the host to
    /// compute where the popover should be drawn.
    pub fn selected_dir_idx(&self) -> usize {
        self.state.selected_dir_idx()
    }

    /// Window-local Y (top-down, logical points) of the dirs panel's top
    /// edge, or `None` before it has rendered. Captured once per frame via
    /// a canvas probe placed on the panel container. The host combines it
    /// with `selected_dir_idx()` and the known row/header dimensions to
    /// anchor the floating popover — stable across popover-selection
    /// changes, no stale-value races.
    pub fn dirs_panel_top_y(&self) -> Option<f32> {
        self.dirs_panel_top_y.get()
    }

    /// Layout constants of the dirs panel used by the host to translate
    /// `selected_dir_idx()` into a window-local row centre without needing
    /// per-row probes. Mirrors [`list::ROW_HEIGHT`] + the panel's own
    /// padding/header. Kept in sync with [`render_dirs_panel`].
    pub fn dirs_row_center_from_panel_top(&self, selected_dir: usize) -> f32 {
        const HEADER_OFFSET: f32 = 28.0; // py_2 top (8) + header line (~18) + gap_0p5 (2)
        const ROW_HEIGHT: f32 = 44.0;
        HEADER_OFFSET + (selected_dir as f32 + 0.5) * ROW_HEIGHT
    }

    /// Read-only view of the dirs pane contents, used by the host to resolve
    /// the highlighted row's target path without pulling state out.
    pub fn dirs(&self) -> &[Item] {
        self.state.dirs()
    }


    /// Mirror the host's live directory-source state. When flipped off the
    /// right-pane suggestions are cleared immediately. When flipped on, the
    /// host should emit a fresh `QueryChanged` synthetically (or the user's
    /// next keystroke triggers one).
    pub fn set_dirs_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.dirs_enabled == enabled {
            return;
        }
        self.dirs_enabled = enabled;
        if !enabled {
            self.state.set_dirs(Vec::new());
            cx.emit(SwitcherViewEvent::OpenWithStateChanged);
            cx.notify();
        } else {
            // Ask the host to refresh dirs against the current query so the
            // pane populates without waiting for the next keystroke.
            cx.emit(SwitcherViewEvent::QueryChanged(
                self.state.query().to_string(),
            ));
        }
    }

    pub fn dirs_enabled(&self) -> bool {
        self.dirs_enabled
    }

    /// Mirror the active source's `supports_remove()`. Drives the × button
    /// render on each dir row; false hides the button entirely.
    pub fn set_dirs_removable(&mut self, removable: bool, cx: &mut Context<Self>) {
        if self.dirs_removable == removable {
            return;
        }
        self.dirs_removable = removable;
        cx.notify();
    }

    pub fn dirs_removable(&self) -> bool {
        self.dirs_removable
    }

    /// Flip the spinner-in-place-of-cog state from the host. Called on
    /// every keystroke pair: `true` right before the host spawns the
    /// subprocess, `false` when the result lands (or the request is
    /// superseded).
    pub fn set_dirs_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        if self.dirs_loading == loading {
            return;
        }
        self.dirs_loading = loading;
        cx.notify();
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
        // No clear() here — fresh `SwitcherView` always carries an empty
        // `InputState`. set_items runs inside the cx.new builder, before
        // the host installs its event subscriber and any external mutation.
        self.state.set_items(items);
        self.state.set_query("");
        // New switcher session — forget any tabs scanned for the previous
        // one and allow `NeedsBrowserTabs` to be emitted again once the
        // fallback tier is reached.
        self.state.clear_browser_tabs();
        self.browser_tabs_requested = false;
        self.browser_tabs_retry_after = None;
        // Same one-shot reset for the audio row: drop the previous open's
        // detection result so the new open kicks a fresh CoreAudio probe.
        // The actual `NeedsCurrentlyPlaying` emit is deferred to
        // [`Self::request_currently_playing`] — set_items runs inside the
        // `cx.new` builder, before the host installs its event subscriber,
        // so any emit from here is swallowed.
        self.state.clear_suggestions();
        self.currently_playing_requested = false;
        self.action_error = None;
        self.confirmed = false;
        self.linear_lookup_requested = None;
        self.path_lookup_requested = None;
        self.path_cycle = None;
        if self.dirs_enabled {
            cx.emit(SwitcherViewEvent::QueryChanged(String::new()));
        }
        self.emit_height_delta_if_changed(cx);
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
        cx.notify();
    }

    /// Fire a one-shot `NeedsCurrentlyPlaying` event. Called by the host
    /// **after** `cx.subscribe` is installed — emitting from `set_items`
    /// (which runs inside `cx.open_window`'s builder) drops the event on
    /// the floor because the subscription doesn't exist yet.
    pub fn request_currently_playing(&mut self, cx: &mut Context<Self>) {
        if self.currently_playing_requested {
            return;
        }
        self.currently_playing_requested = true;
        cx.emit(SwitcherViewEvent::NeedsCurrentlyPlaying);
    }

    /// Refresh the candidate set in place without wiping the query, input or
    /// selected row. Used when the full window listing lands after the panel
    /// painted from the cached one.
    pub fn refresh_items(&mut self, items: Vec<Item>, cx: &mut Context<Self>) {
        self.state.refresh_items(items);
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

    /// Drop the zoxide dir row matching `path` right away — optimistic,
    /// mirrors [`Self::drop_window`] for the dirs pane.
    pub fn drop_dir(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.state.remove_dir(path);
        self.emit_height_delta_if_changed(cx);
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
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
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
        cx.notify();
    }

    pub fn set_lena_icon(&mut self, id: &str, icon: std::path::PathBuf, cx: &mut Context<Self>) {
        self.state.set_lena_icon(id, icon);
        cx.notify();
    }

    pub fn lena_dirs(&self) -> Vec<Item> {
        self.state.lena_dirs()
    }

    /// Replace the Lena rows below the local dirs once Lena answers.
    pub fn set_lena_dirs(&mut self, rows: Vec<Item>, cx: &mut Context<Self>) {
        self.state.set_lena_dirs(rows);
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
        cx.notify();
    }

    /// Show the "Ask Lena AI" row (a signed-in Lena answered).
    pub fn set_lena_ask(&mut self, available: bool, cx: &mut Context<Self>) {
        self.state.set_lena_ask(available);
        cx.notify();
    }

    /// Deliver the off-thread browser-tab scan back into the state. Rerank
    /// fires automatically so the tabs immediately populate the fallback
    /// tier (or step aside for the LLM row if none match).
    pub fn set_browser_tabs(&mut self, tabs: Vec<Item>, cx: &mut Context<Self>) {
        self.state.set_browser_tabs(tabs);
        self.emit_linear_popover_sync(cx);
        cx.notify();
    }

    /// Deliver the off-thread audio-source detection. Empty vec = no
    /// source detected; clears any stale rows from the previous session.
    pub fn set_currently_playing(&mut self, rows: Vec<Item>, cx: &mut Context<Self>) {
        self.state.set_currently_playing(rows);
        self.emit_height_delta_if_changed(cx);
        cx.notify();
    }

    pub fn local_source_focus_failed(&mut self, cx: &mut Context<Self>) {
        self.handoff_failed("local_sources.focus_failed", cx);
    }

    /// Lena did not take the pick: `stale` = its result list changed since
    /// the search (restart or forgotten id).
    pub fn lena_failed(&mut self, stale: bool, cx: &mut Context<Self>) {
        self.handoff_failed(if stale { "lena.stale" } else { "lena.failed" }, cx);
        if stale {
            // Fresh ids for the next pick.
            let query = self.state.query().to_string();
            cx.emit(SwitcherViewEvent::QueryChanged(query));
        }
    }

    fn handoff_failed(&mut self, key: &'static str, cx: &mut Context<Self>) {
        self.action_error = Some(key);
        // The panel stays open: let the user pick again.
        self.confirmed = false;
        cx.notify();
    }

    pub fn set_local_source(&mut self, source: &str, rows: Vec<Item>, cx: &mut Context<Self>) {
        self.state.set_local_source(source, rows);
        self.emit_height_delta_if_changed(cx);
        cx.notify();
    }

    /// Mirror the user's `browser_tabs_integration` setting. Off → no scan
    /// is ever requested and any previously delivered cache is cleared.
    pub fn set_browser_tabs_integration(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.state.set_browser_tabs_integration(enabled);
        if !enabled {
            self.browser_tabs_requested = false;
            self.browser_tabs_retry_after = None;
        }
        cx.notify();
    }

    /// Signal from the host that the off-thread scan errored out (every
    /// supported browser timed out or the automation prompt was denied).
    /// Clears the `requested` latch and installs a cooldown so the next
    /// keystroke (after the cooldown window) can fire a retry. We
    /// deliberately do NOT touch the state's tab cache: keeping it at
    /// `None` lets `needs_browser_tabs()` stay true so the retry actually
    /// fires.
    pub fn browser_tabs_scan_failed(&mut self, cx: &mut Context<Self>) {
        self.browser_tabs_requested = false;
        self.browser_tabs_retry_after = Some(Instant::now() + BROWSER_TABS_RETRY_COOLDOWN);
        cx.notify();
    }

    /// Clear the right-pane suggestions. Called when the integration is
    /// turned off in settings or zoxide returns an empty list.
    pub fn clear_dirs(&mut self, cx: &mut Context<Self>) {
        self.state.set_dirs(Vec::new());
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
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
    pub fn append_query(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_string();
        self.input.update(cx, |s, cx| s.insert(text, window, cx));
        self.sync_query(cx);
    }

    /// Remove the last character from the query (Quick Type's Fn+Backspace).
    pub fn backspace_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value().to_string();
        if value.is_empty() {
            return;
        }
        // Trim one user-perceived char off the end. Byte-based truncation
        // because the InputState's own setter is byte-indexed and we only
        // care about removing the last grapheme — char_indices is the
        // cheapest way to find that boundary.
        let cut = value.char_indices().last().map(|(i, _)| i).unwrap_or(0);
        let truncated: String = value[..cut].to_string();
        self.input
            .update(cx, |s, cx| s.set_value(truncated, window, cx));
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
            self.confirm_item(item, cx);
        }
    }

    /// Whether the panel should dismiss in response to another app becoming
    /// frontmost. Mirrors the exemptions of `dismiss_on_blur`: stay open
    /// during the license-activation browser flow, and stay open while the
    /// owned "Open With" popover is up. Single source of policy used by both
    /// the GPUI window-activation observer and the NSWorkspace-activation
    /// loop in main.rs.
    pub fn should_dismiss_on_foreign_activation(&self) -> bool {
        self.nag_phase != NagPhase::Activating && !self.open_with_visible()
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
            // Clicking the floating "Open With" popover briefly flips main
            // to inactive even though the popover is a nonactivating panel
            // we own. Without this check, picking an app via the mouse
            // would dismiss the whole switcher before the click even fires.
            if view.open_with_visible() {
                return;
            }
            cx.emit(SwitcherViewEvent::Dismissed);
        });
        self._activation_sub = Some(sub);
    }

    // --- List navigation ---

    /// True when the search field currently owns keyboard focus. The
    /// switcher treats the input as one of the navigable positions: when it
    /// is focused, arrow keys blur it and move into the result list; when
    /// it isn't, arrow keys at the row closest to the input refocus it.
    fn is_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    /// Move keyboard focus off the search field and onto the SwitcherView's
    /// own focus handle so subsequent arrow keys land on the "Switcher"
    /// key context (and the Input's caret-motion bindings stop matching).
    fn blur_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
    }

    /// Move focus back to the search field and select its full content so a
    /// keystroke replaces the previous query — same affordance the user gets
    /// from a fresh switcher session.
    fn refocus_input_select_all(&self, window: &mut Window, cx: &mut Context<Self>) {
        let h = self.input.read(cx).focus_handle(cx);
        h.focus(window, cx);
        window.dispatch_action(
            Box::new(gpui_component::input::SelectAll),
            cx,
        );
    }

    /// True when the active section is touching the input from below (the
    /// windows list directly under the search field, or the dirs pane to
    /// its right) and the cursor sits on its first row — the position from
    /// which Up should refocus the input.
    fn at_top_of_section_below_input(&self) -> bool {
        match self.state.active_section() {
            Section::Windows => self.state.selected_idx() == 0,
            Section::Dirs => self.state.selected_dir_idx() == 0,
            _ => false,
        }
    }

    /// Mirror of [`Self::at_top_of_section_below_input`] for sections
    /// rendered above the input (programs / currently-playing). True when
    /// the cursor sits on the row that visually abuts the search field, so
    /// Down should refocus the input.
    fn at_bottom_of_section_above_input(&self) -> bool {
        match self.state.active_section() {
            Section::Programs => {
                let n = self.state.filtered_programs().len();
                n > 0 && self.state.selected_program_idx() + 1 == n
            }
            Section::Suggestions => {
                let n = self.state.suggested_items().len();
                n > 0 && self.state.selected_suggestion_idx() + 1 == n
            }
            _ => false,
        }
    }

    fn on_select_prev(&mut self, _: &SelectPrev, window: &mut Window, cx: &mut Context<Self>) {
        if self.nag_phase != NagPhase::Hidden {
            return;
        }
        if self.state.open_with_index().is_some() {
            self.state.open_with_prev(self.current_open_with_count());
            cx.emit(SwitcherViewEvent::OpenWithStateChanged);
            cx.notify();
            return;
        }
        if self.is_input_focused(window, cx) {
            // Treat the input as a focusable: Up blurs it and moves the list
            // cursor up by one (which usually advances out of Windows[0]
            // into the closest section above, when one is visible).
            self.blur_input(window, cx);
            self.select_prev_external(cx);
            self.emit_open_with_if_dirs(cx);
            return;
        }
        if self.at_top_of_section_below_input() {
            // First row of the section closest to the input from below —
            // Up moves "into" the input rather than wrapping or jumping
            // over to a section above.
            self.refocus_input_select_all(window, cx);
            return;
        }
        self.select_prev_external(cx);
        self.emit_open_with_if_dirs(cx);
    }

    fn on_select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if self.nag_phase != NagPhase::Hidden {
            return;
        }
        if self.state.open_with_index().is_some() {
            self.state.open_with_next(self.current_open_with_count());
            cx.emit(SwitcherViewEvent::OpenWithStateChanged);
            cx.notify();
            return;
        }
        if self.is_input_focused(window, cx) {
            self.blur_input(window, cx);
            self.select_next_external(cx);
            self.emit_open_with_if_dirs(cx);
            return;
        }
        if self.at_bottom_of_section_above_input() {
            self.refocus_input_select_all(window, cx);
            return;
        }
        self.select_next_external(cx);
        self.emit_open_with_if_dirs(cx);
    }

    /// Helper: every time the user's selection may have moved inside (or
    /// into) the Dirs pane, fire [`SwitcherViewEvent::OpenWithStateChanged`]
    /// so the host can reposition / hide the popover window accordingly.
    fn emit_open_with_if_dirs(&mut self, cx: &mut Context<Self>) {
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
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
        let extras = if self.input_only {
            0.0
        } else {
            extras_above_input_height(&self.state)
        };
        let shrink = self.current_list_shrink();
        let d_extras = extras - self.last_extras_above_input;
        let d_shrink = shrink - self.last_list_shrink;
        if d_extras.abs() < f32::EPSILON && d_shrink.abs() < f32::EPSILON {
            return;
        }
        self.last_extras_above_input = extras;
        self.last_list_shrink = shrink;
        // Sections above the input (programs, currently-playing) grow upward
        // (anchor bottom → delta_origin_y stays 0). Results-panel suppression
        // shrinks below the input (anchor top → delta_origin_y matches the
        // shrink so bottom rises by the same amount height drops).
        let delta_height = d_extras - d_shrink;
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
        if self.input_only {
            return INPUT_ONLY_SHRINK;
        }
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
        self.confirm_selection(cx);
    }

    fn confirm_selection(&mut self, cx: &mut Context<Self>) {
        if self.nag_phase != NagPhase::Hidden || self.confirmed {
            return;
        }
        // Right after a send, Enter on the empty field opens the
        // conversation instead of the top window.
        if self.state.query().is_empty() {
            match &self.instruction_phase {
                InstructionPhase::Sending => return,
                InstructionPhase::Sent {
                    conversation_url: Some(url),
                } => {
                    let item = Item::OpenUrl(url.clone());
                    self.emit_confirmed(item, cx);
                    return;
                }
                _ => {}
            }
        }
        // Popover has keyboard focus → hand off to the host so it launches
        // the dir with the selected app instead of the default handler.
        if let Some(i) = self.state.open_with_index() {
            if self.linear_create_selected() {
                self.start_linear_create(Some(i), cx);
            } else {
                cx.emit(SwitcherViewEvent::OpenWithActivated(i));
            }
            return;
        }
        if let Some(item) = self.state.selected().cloned() {
            self.confirm_item(item, cx);
        }
    }

    /// Click on a row: jump the selection there and activate — same end-state
    /// as pressing Enter on that entry.
    fn on_row_click(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.state.set_selected(idx);
        if let Some(item) = self.state.selected().cloned() {
            self.confirm_item(item, cx);
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
        self.emit_linear_popover_sync(cx);
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

    /// Flip the local playback state of the audio row at `idx` to the
    /// opposite of its current value. Called from the play/pause button so
    /// the badge updates instantly; the host follows up with a fresh
    /// probe a moment later to settle on the truth.
    fn flip_audio_state_optimistic(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.state.toggle_audio_row_state(idx);
        cx.notify();
    }

    fn on_suggestion_row_click(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.state.set_selected_suggestion(idx);
        if let Some(item) = self.state.selected().cloned() {
            self._activation_sub = None;
            cx.emit(SwitcherViewEvent::Confirmed(item));
        }
    }

    fn on_suggestion_row_hover(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.state.active_section() == Section::Suggestions
            && self.state.selected_suggestion_idx() == idx
        {
            return;
        }
        self.state.set_selected_suggestion(idx);
        cx.notify();
    }

    fn on_dir_row_click(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.state.set_selected_dir(idx);
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
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
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
        cx.notify();
    }

    /// Click on the dir row's × button: ask the host to run `zoxide remove`
    /// for that path. Row-level click is short-circuited by the button's
    /// `mouse_down` propagation stop.
    fn on_dir_remove_clicked(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(item) = self.state.dirs().get(idx) else {
            return;
        };
        if let Item::Dir(d) = item {
            cx.emit(SwitcherViewEvent::RemoveDirRequested(d.clone()));
        }
    }

    fn on_focus_next_pane(&mut self, _: &FocusNextPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.nag_phase != NagPhase::Hidden || self.complete_path(true, window, cx) {
            return;
        }
        if self.state.active_section() == Section::Dirs {
            self.state.focus_windows();
        } else if self.state.dirs_visible() {
            self.state.focus_dirs();
        } else {
            return;
        }
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
        cx.notify();
    }

    fn on_focus_prev_pane(&mut self, _: &FocusPrevPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.nag_phase != NagPhase::Hidden || self.complete_path(false, window, cx) {
            return;
        }
        if self.state.active_section() == Section::Dirs {
            self.state.focus_windows();
        } else if self.state.dirs_visible() {
            self.state.focus_dirs();
        } else {
            return;
        }
        cx.emit(SwitcherViewEvent::OpenWithStateChanged);
        cx.notify();
    }

    fn on_dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        // Escape while the popover is focused just closes the popover.
        if self.state.open_with_index().is_some() {
            self.state.exit_open_with();
            cx.emit(SwitcherViewEvent::OpenWithStateChanged);
            cx.notify();
            return;
        }
        self._activation_sub = None;
        cx.emit(SwitcherViewEvent::Dismissed);
    }

    fn on_cog_click(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        tracing::info!("cog clicked");
        self._activation_sub = None;
        cx.emit(SwitcherViewEvent::OpenSettings);
    }

    // --- Text editing ---

    fn on_move_left(&mut self, _: &MoveLeft, window: &mut Window, cx: &mut Context<Self>) {
        // Left while the popover is focused exits the popover and returns
        // focus to the dir row. Keeps the input caret untouched.
        if self.state.open_with_index().is_some() {
            self.state.exit_open_with();
            cx.emit(SwitcherViewEvent::OpenWithStateChanged);
            cx.notify();
            return;
        }
        // While the search field has focus, Left is plain caret motion —
        // hand off to the Input's own MoveLeft binding via propagation.
        if self.is_input_focused(window, cx) {
            cx.propagate();
            return;
        }
        // List has focus. From the dirs pane, Left snaps back to the
        // windows pane (the symmetric counterpart of Right going the other
        // way). Anywhere else there's nothing to the left of the cursor
        // position, so propagate (and the Input would only catch it if it
        // were focused, which we've already ruled out).
        if self.state.active_section() == Section::Dirs {
            self.state.focus_windows();
            cx.emit(SwitcherViewEvent::OpenWithStateChanged);
            cx.notify();
            return;
        }
        cx.propagate();
    }

    fn on_move_right(&mut self, _: &MoveRight, window: &mut Window, cx: &mut Context<Self>) {
        // In the Dirs pane (input not focused), Right enters the "Open
        // With" popover when one is available.
        let owc = self.current_open_with_count();
        let input_focused = self.is_input_focused(window, cx);
        let caret_at_end = !input_focused || {
            let input = self.input.read(cx);
            input.cursor() >= input.value().len()
        };
        let list_row = (self.linear_create_selected() && !input_focused)
            || (self.list_dir_selected() && caret_at_end);
        if (self.state.active_section() == Section::Dirs || list_row)
            && owc > 0
            && self.state.open_with_index().is_none()
        {
            self.state.enter_open_with(owc);
            cx.emit(SwitcherViewEvent::OpenWithStateChanged);
            cx.notify();
            return;
        }
        if self.is_input_focused(window, cx) {
            // Caret at end of query + dirs available → blur the input and
            // hop into the dirs pane. Anywhere else, Right is plain caret
            // motion and we let the Input's own binding move the cursor.
            let cursor = self.input.read(cx).cursor();
            let len = self.input.read(cx).value().len();
            if cursor >= len && self.state.dirs_visible() {
                self.blur_input(window, cx);
                self.state.focus_dirs();
                cx.emit(SwitcherViewEvent::OpenWithStateChanged);
                cx.notify();
                return;
            }
            cx.propagate();
            return;
        }
        // List has focus. Outside the dirs pane, Right hops into it when
        // it's visible — same shortcut as the input-focused path above.
        if self.state.active_section() != Section::Dirs && self.state.dirs_visible() {
            self.state.focus_dirs();
            cx.emit(SwitcherViewEvent::OpenWithStateChanged);
            cx.notify();
            return;
        }
        cx.propagate();
    }

    fn on_audio_space(
        &mut self,
        _: &crate::actions::AudioToggle,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Space on a selected Now-Playing row toggles play/pause instead of
        // typing into the query. Audio rows only render with an empty query,
        // so this can't shadow search input. When the section isn't audio,
        // propagate so the Input widget inserts a literal space.
        if self.nag_phase != NagPhase::Hidden {
            cx.propagate();
            return;
        }
        if self.state.active_section() == Section::Suggestions {
            let idx = self.state.selected_suggestion_idx();
            if let Some(Item::CurrentlyPlaying(r)) = self.state.suggested_items().get(idx) {
                if r.supports_toggle() {
                    let row = r.clone();
                    cx.emit(SwitcherViewEvent::TogglePlayPause(row));
                    self.flip_audio_state_optimistic(idx, cx);
                    return;
                }
            }
        }
        cx.propagate();
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
        self.state.set_query(self.input.read(cx).value().to_string());
        self.scroll_selection_into_view();
        if self.dirs_enabled {
            cx.emit(SwitcherViewEvent::QueryChanged(
                self.state.query().to_string(),
            ));
        }
        // Kick off the browser-tabs scan the first time the fallback tier
        // becomes active this session. Gated so the host doesn't spawn an
        // osascript on every keystroke — one scan per switcher opening,
        // plus one retry per cooldown window after a scan failure.
        if !self.browser_tabs_requested && self.state.needs_browser_tabs() {
            let ready = self
                .browser_tabs_retry_after
                .map(|t| Instant::now() >= t)
                .unwrap_or(true);
            if ready {
                self.browser_tabs_requested = true;
                self.browser_tabs_retry_after = None;
                cx.emit(SwitcherViewEvent::NeedsBrowserTabs);
            }
        }
        let linear_target = self.state.linear_lookup_target();
        if linear_target != self.linear_lookup_requested {
            self.linear_lookup_requested = linear_target.clone();
            if let Some(identifier) = linear_target {
                cx.emit(SwitcherViewEvent::NeedsLinearIssue(identifier));
            }
        }
        let path_target = self.state.path_target().map(str::to_string);
        if path_target != self.path_lookup_requested {
            self.path_lookup_requested = path_target.clone();
            if let Some(target) = path_target {
                cx.emit(SwitcherViewEvent::NeedsPathHits(target));
            }
        }
        self.emit_linear_popover_sync(cx);
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let filtered_count = self.state.filtered().len();
        self.row_center_y.set(None);
        // Selection/scroll events reach the host before this paint fills the
        // probe: hand it the fresh anchor once the frame is out.
        if (self.linear_team_count > 1 && self.linear_create_selected()) || self.list_dir_selected() {
            cx.on_next_frame(window, |this, _window, cx| {
                let y = this.row_center_y.get();
                if y.is_some() && y != this.row_anchor_sent {
                    this.row_anchor_sent = y;
                    cx.emit(SwitcherViewEvent::OpenWithStateChanged);
                }
            });
        } else {
            self.row_anchor_sent = None;
        }
        let query_empty = self.input.read(cx).value().is_empty();
        let path_ghost = self.path_ghost(window, cx);
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
        let browser_tabs_loading = self.state.browser_tabs_loading();

        let list_section: AnyElement = if nag_phase != NagPhase::Hidden {
            render_nag_card(nag_phase, &theme, cx).into_any_element()
        } else if filtered_count == 0 && browser_tabs_loading {
            // Only the spinner is shown here because no window, program, or
            // tab has matched yet. The scan is in flight — rendering "No
            // results" would be a false negative the moment before tabs
            // arrive.
            render_browser_tabs_loading(&theme)
        } else if filtered_count == 0 {
            div()
                .px_3()
                .py_2()
                .text_size(px(13.0))
                .text_color(theme.muted)
                .child(empty_msg)
                .into_any_element()
        } else {
            let list = uniform_list(
                "switcher-list",
                filtered_count,
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let selected = this.state.selected_idx();
                    let window_active = this.state.active_section() == Section::Windows;
                    let theme = this.theme;
                    let openclaw_row = this.openclaw.clone().unwrap_or_default();
                    // `filtered()` borrows `this`, but we also need `cx.listener`
                    // which borrows `this` mutably. Build the rows first using
                    // the shared borrow, then attach handlers after it drops.
                    let rows: Vec<_> = range
                        .clone()
                        .map(|i| {
                            let mr = &this.state.filtered()[i];
                            let is_window = matches!(mr.item, Item::Window(_));
                            let mut row = render_row(
                                mr,
                                window_active && i == selected,
                                &theme,
                                &openclaw_row,
                            );
                            let anchors_popover = match mr.item {
                                Item::CreateLinearIssue { .. } => true,
                                Item::Dir(_) => window_active && i == selected,
                                _ => false,
                            };
                            if anchors_popover {
                                row = row
                                    .relative()
                                    .child(row_center_probe(this.row_center_y.clone()));
                            }
                            (i, is_window, row)
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
            .flex_1();
            if browser_tabs_loading {
                // Results are visible but the tab scan isn't back yet —
                // append a small spinner row below the list so the user
                // knows more results may still arrive.
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .child(list)
                    .child(render_browser_tabs_loading(&theme))
                    .into_any_element()
            } else {
                list.into_any_element()
            }
        };

        div()
            .key_context("Switcher")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_dismiss))
            .on_action(cx.listener(Self::on_move_left))
            .on_action(cx.listener(Self::on_move_right))
            .on_action(cx.listener(Self::on_audio_space))
            .on_action(cx.listener(Self::on_focus_next_pane))
            .on_action(cx.listener(Self::on_focus_prev_pane))
            .capture_key_down(cx.listener(Self::on_key_down_capture))
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
            .when_some(
                match &self.instruction_phase {
                    InstructionPhase::Failed { message } => Some(message.clone()),
                    _ => None,
                },
                |root, message| {
                    root.child(
                        div()
                            .px_3()
                            .py_1()
                            .text_size(px(11.0))
                            .text_color(theme.destructive)
                            .child(message),
                    )
                },
            )
            .when_some(self.action_error, |root, key| {
                root.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_size(px(11.0))
                        .text_color(theme.muted)
                        .child(tr(key)),
                )
            })
            .children(if nag_phase == NagPhase::Hidden && !self.input_only {
                suggestions_section(&self.state, &theme, cx)
            } else {
                None
            })
            .children(if nag_phase == NagPhase::Hidden && !self.input_only {
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
                            .relative()
                            .child(Input::new(&self.input).appearance(false).bordered(false))
                            .children(path_ghost.map(|(typed, ghost)| {
                                // Laid out like the Input's own text (medium
                                // size: 12 px inset, text_sm), the typed part
                                // invisible so the grey rest starts at the caret.
                                div()
                                    .absolute()
                                    .top_0()
                                    .bottom_0()
                                    .left(px(12.0))
                                    .right_0()
                                    .flex()
                                    .items_center()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_sm()
                                    .line_height(gpui::rems(1.25))
                                    .child(div().text_color(gpui::transparent_black()).child(typed))
                                    .child(div().text_color(theme.muted).child(ghost))
                            })),
                    )
                    .children(render_instruction_status(
                        &self.instruction_phase,
                        self.instruction_seq,
                        &theme,
                    ))
                    .child(render_cog_or_spinner(self.dirs_loading, &theme, cx)),
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
            .children(if hide_empty_list || self.input_only {
                None
            } else {
                let dirs_visible = nag_phase == NagPhase::Hidden && self.state.dirs_visible();
                // When the windows list has nothing to show but dirs do, the
                // dirs pane takes the full row width and the "Ask LLM"
                // fallback is suppressed (see `rerank`).
                let dirs_full_width = dirs_visible && filtered_count == 0;
                if dirs_full_width {
                    Some(
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .overflow_hidden()
                            .child(render_dirs_panel(&self.state, &theme, cx, true, self.dirs_removable, self.dirs_panel_top_y.clone())),
                    )
                } else {
                    let dirs_pane = dirs_visible
                        .then(|| render_dirs_panel(&self.state, &theme, cx, false, self.dirs_removable, self.dirs_panel_top_y.clone()));
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
                }
            })
    }
}

/// Zero-area canvas recording its row's vertical centre (window-local) on
/// every paint; anchors the Open With / team popover with scrolling accounted for.
fn row_center_probe(cell: Rc<Cell<Option<f32>>>) -> impl IntoElement {
    canvas(
        move |bounds, _w, _cx| {
            let top: f32 = bounds.origin.y.into();
            cell.set(Some(top + ROW_CENTER));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .left(px(0.0))
    .top(px(0.0))
    .w(px(1.0))
    .h(px(1.0))
}

/// Half of `list::ROW_HEIGHT`.
const ROW_CENTER: f32 = 22.0;

/// Render the query text with a visible caret and selection highlight.
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

/// Play/pause button for "Currently Playing" rows whose source app exposes
/// a known toggle path (Spotify, Music, …). Clicking emits
/// [`SwitcherViewEvent::TogglePlayPause`] with the bundle id; the host
/// dispatches the AppleScript and re-runs the audio probe so the badge
/// flips. `stop_propagation` on mouse-down prevents the click from bubbling
/// up and triggering row activation (which would close the switcher).
fn render_play_pause_button(
    idx: usize,
    row: Arc<switcheur_core::AudioRowRef>,
    state: PlaybackState,
    theme: &Theme,
    cx: &mut Context<SwitcherView>,
) -> AnyElement {
    let muted = theme.muted;
    let foreground = theme.foreground;
    let hover_bg = theme.border;
    let glyph = match state {
        PlaybackState::Playing => "⏸",
        PlaybackState::Paused | PlaybackState::Unknown => "▶",
    };
    div()
        .id(("switcher-audio-toggle-btn", idx))
        .w(px(22.0))
        .h(px(22.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_size(px(11.0))
        .text_color(muted)
        .cursor_pointer()
        .hover(move |d| d.bg(hover_bg).text_color(foreground))
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, _w, cx| cx.stop_propagation(),
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
            cx.emit(SwitcherViewEvent::TogglePlayPause(row.clone()));
            // Optimistic flip: invert the row's local state so the glyph
            // changes immediately without waiting for the host to re-run
            // the probe (which can take ~400 ms via osascript).
            this.flip_audio_state_optimistic(idx, cx);
        }))
        .child(glyph)
        .into_any_element()
}

/// Mirror of [`render_close_button`] for the dirs pane: clicking it asks the
/// host to run `zoxide remove` against the row's path. Distinct id tag so
/// GPUI's interaction state doesn't collide with the window-row close button.
fn render_dir_remove_button(
    idx: usize,
    theme: &Theme,
    cx: &mut Context<SwitcherView>,
) -> AnyElement {
    let muted = theme.muted;
    let foreground = theme.foreground;
    let hover_bg = theme.border;
    div()
        .id(("switcher-dir-remove-btn", idx))
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
            this.on_dir_remove_clicked(idx, cx);
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
            render_row(m, section_active && i == selected, theme, &OpenClawRow::default())
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

fn suggestion_group(item: &Item) -> &str {
    match item {
        Item::LocalSource(source) => &source.source,
        _ => "audio",
    }
}

fn suggestions_section(
    state: &SwitcherState,
    theme: &Theme,
    cx: &mut Context<SwitcherView>,
) -> Option<AnyElement> {
    use switcheur_core::MatchResult;

    if !state.suggested_items_visible() {
        return None;
    }
    let items = state.suggested_items();
    if items.is_empty() {
        return None;
    }
    let section_active = state.active_section() == Section::Suggestions;
    let selected = state.selected_suggestion_idx();

    let rows: Vec<AnyElement> = items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let mr = MatchResult {
                item: item.clone(),
                score: 0,
                indices: Vec::new(),
            };
            let active = section_active && idx == selected;
            // Pull out the toggle button data while we still have the
            // item — render_row consumes the MatchResult.
            let toggle_data: Option<(Arc<switcheur_core::AudioRowRef>, PlaybackState)> =
                if let Item::CurrentlyPlaying(r) = item {
                    if r.supports_toggle() {
                        Some((r.clone(), r.state))
                    } else {
                        None
                    }
                } else {
                    None
                };
            let mut row = render_row(&mr, active, theme, &OpenClawRow::default());
            if let Some((row_ref, state)) = toggle_data {
                row = row.child(render_play_pause_button(idx, row_ref, state, theme, cx));
            }
            let row = row
                .id(SharedString::from(format!("switcher-suggestion-row-{idx}")))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                    this.on_suggestion_row_click(idx, cx);
                }))
                .on_hover(cx.listener(move |this, hovering: &bool, _w, cx| {
                    if *hovering {
                        this.on_suggestion_row_hover(idx, cx);
                    }
                }))
                .into_any_element();
            let starts_group =
                idx == 0 || suggestion_group(&items[idx - 1]) != suggestion_group(item);
            let mut container = div().flex().flex_col();
            if starts_group {
                let title = match item {
                    Item::LocalSource(source) => source.source_name.clone(),
                    _ => tr("audio.section_header"),
                };
                container = container.child(
                    div()
                        .px_3()
                        .pt_1()
                        .pb_0p5()
                        .text_size(px(11.0))
                        .text_color(theme.muted)
                        .child(title),
                );
            }
            container.child(row).into_any_element()
        })
        .collect();

    Some(
        div()
            .flex()
            .flex_col()
            .pb_1()
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
    full_width: bool,
    removable: bool,
    dirs_panel_top_y: Rc<Cell<Option<f32>>>,
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
            // through the matcher (the source already ranks them), so wrap
            // with a zero score and no highlight indices.
            let mr = MatchResult {
                item: item.clone(),
                score: 0,
                indices: Vec::new(),
            };
            let mut row = render_row(
                &mr,
                section_active && i == selected,
                theme,
                &OpenClawRow::default(),
            );
            let from_lena = matches!(item, Item::Dir(d) if d.source == switcheur_core::DirSource::Lena);
            if removable && !from_lena {
                row = row.child(render_dir_remove_button(i, theme, cx));
            }
            row.id(("switcher-dir-row", i))
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

    let mut panel = div().relative().flex().flex_col();
    if full_width {
        panel = panel.flex_1().min_w_0();
    } else {
        panel = panel.w(px(260.0)).border_l_1().border_color(theme.border);
    }
    // Zero-area canvas pinned at the panel's top-left whose prepaint fires
    // every frame with the panel's actual bounds — a single, selection-
    // independent probe the host uses to anchor the "Open With" popover.
    let cell = dirs_panel_top_y.clone();
    let probe = canvas(
        move |bounds, _w, _cx| {
            cell.set(Some(bounds.origin.y.into()));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .left(px(0.0))
    .top(px(0.0))
    .w(px(1.0))
    .h(px(1.0));
    panel
        .px_2()
        .py_2()
        .gap_0p5()
        .overflow_hidden()
        .child(probe)
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

/// Top-right affordance: settings cog normally, rotating spinner while a
/// dir-source query is in flight. Same footprint both ways so the layout
/// doesn't jitter between states.
fn render_cog_or_spinner(
    loading: bool,
    theme: &Theme,
    cx: &mut Context<SwitcherView>,
) -> AnyElement {
    let base = div()
        .ml_2()
        .w(px(24.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_size(px(16.0))
        .text_color(theme.muted);
    if loading {
        let spinner = svg()
            .path("browser_icons/spinner.svg")
            .w(px(14.0))
            .h(px(14.0))
            .text_color(theme.muted)
            .with_animation(
                "dirs_search_spinner",
                Animation::new(std::time::Duration::from_millis(900))
                    .repeat()
                    .with_easing(linear),
                |s, delta| s.with_transformation(Transformation::rotate(percentage(delta))),
            );
        base.child(spinner).into_any_element()
    } else {
        base.cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(SwitcherView::on_cog_click),
            )
            .child("⚙")
            .into_any_element()
    }
}

/// Spinner while an OpenClaw instruction is in flight, then a checkmark that
/// pops in (scale overshoot + fade) once the gateway accepted it.
fn render_instruction_status(
    phase: &InstructionPhase,
    seq: usize,
    theme: &Theme,
) -> Option<AnyElement> {
    let base = div()
        .ml_2()
        .w(px(24.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_center();
    match phase {
        InstructionPhase::Sending => Some(
            base.child(
                svg()
                    .path("browser_icons/spinner.svg")
                    .w(px(14.0))
                    .h(px(14.0))
                    .text_color(theme.muted)
                    .with_animation(
                        "instruction_spinner",
                        Animation::new(Duration::from_millis(900))
                            .repeat()
                            .with_easing(linear),
                        |s, delta| s.with_transformation(Transformation::rotate(percentage(delta))),
                    ),
            )
            .into_any_element(),
        ),
        InstructionPhase::Sent { .. } => Some(
            base.child(
                div()
                    .size(px(22.0))
                    .with_animation(
                        ("instruction_sent", seq),
                        Animation::new(Duration::from_millis(700)).with_easing(linear),
                        |d, delta| d.child(sent_check_canvas(delta)),
                    ),
            )
            .into_any_element(),
        ),
        InstructionPhase::Idle | InstructionPhase::Failed { .. } => None,
    }
}

/// Green check that draws itself: the circle sweeps around, its fill fades
/// in, then the tick is traced. `t` runs 0 → 1 over the animation.
fn sent_check_canvas(t: f32) -> impl IntoElement {
    const GREEN: u32 = 0x30A46C;
    let ease = |x: f32| 1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3);
    let ring = ease(t / 0.5);
    let fill = ease((t - 0.3) / 0.4);
    let tick = ease((t - 0.45) / 0.55);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let side = bounds.size.width.min(bounds.size.height);
            let unit = side / 22.0;
            let at = |x: f32, y: f32| {
                gpui::point(bounds.origin.x + unit * x, bounds.origin.y + unit * y)
            };
            let green: gpui::Hsla = gpui::rgb(GREEN).into();
            let (cx, cy, r) = (11.0_f32, 11.0_f32, 9.5_f32);
            let circle_point = |a: f32| at(cx + r * a.cos(), cy + r * a.sin());
            let start = -std::f32::consts::FRAC_PI_2;

            if fill > 0.0 {
                let mut path = gpui::PathBuilder::fill();
                path.move_to(circle_point(start));
                for i in 1..=48 {
                    path.line_to(circle_point(start + std::f32::consts::TAU * i as f32 / 48.0));
                }
                path.close();
                if let Ok(path) = path.build() {
                    window.paint_path(path, green.opacity(0.18 * fill));
                }
            }
            if ring > 0.0 {
                let steps = ((48.0 * ring).ceil() as usize).max(1);
                let sweep = std::f32::consts::TAU * ring;
                let mut path = gpui::PathBuilder::stroke(unit * 1.8);
                path.move_to(circle_point(start));
                for i in 1..=steps {
                    path.line_to(circle_point(start + sweep * i as f32 / steps as f32));
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, green);
                }
            }
            if tick > 0.0 {
                // Two legs, traced in proportion to their lengths.
                let (a, b, c) = ((6.6, 11.4), (9.7, 14.4), (15.6, 8.2));
                let leg = |p: (f32, f32), q: (f32, f32)| (q.0 - p.0).hypot(q.1 - p.1);
                let (l1, l2) = (leg(a, b), leg(b, c));
                let mut done = tick * (l1 + l2);
                let mut path = gpui::PathBuilder::stroke(unit * 2.2);
                path.move_to(at(a.0, a.1));
                let k = (done / l1).min(1.0);
                path.line_to(at(a.0 + (b.0 - a.0) * k, a.1 + (b.1 - a.1) * k));
                done -= l1;
                if done > 0.0 {
                    let k = (done / l2).min(1.0);
                    path.line_to(at(b.0 + (c.0 - b.0) * k, b.1 + (c.1 - b.1) * k));
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, green);
                }
            }
        },
    )
    .size_full()
}

/// Inline "scanning browser tabs" row shown while the AppleScript fetch is
/// in flight. Replaces the "No results" label so the user isn't briefly told
/// nothing matched when we're still looking. Uses GPUI's animation API to
/// rotate the spinner SVG — linear easing keeps the motion feeling like a
/// loader rather than a bounce.
fn render_browser_tabs_loading(theme: &Theme) -> AnyElement {
    let spinner = svg()
        .path("browser_icons/spinner.svg")
        .w(px(14.0))
        .h(px(14.0))
        .text_color(theme.muted)
        .with_animation(
            "browser_tabs_spinner",
            Animation::new(std::time::Duration::from_millis(900))
                .repeat()
                .with_easing(linear),
            |s, delta| s.with_transformation(Transformation::rotate(percentage(delta))),
        );
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .text_size(px(13.0))
        .text_color(theme.muted)
        .child(spinner)
        .child(SharedString::from(tr("switcher.searching_browser_tabs")))
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

/// Shrink leaving only the input row (base height 400 minus ~60 px).
const INPUT_ONLY_SHRINK: f32 = 340.0;

/// Total extra vertical pixels added above the input row by all sections
/// that grow upward (programs section + currently-playing section). The
/// host uses this to resize the NSWindow upward, keeping the input row's
/// screen position fixed when sections appear/disappear async.
fn extras_above_input_height(state: &SwitcherState) -> f32 {
    const ROW: f32 = 44.0;
    let mut total = 0.0;
    if state.programs_visible() {
        const SECTION_PADDING: f32 = 10.0; // py_1 top + py_1 bottom + border
        total += state.filtered_programs().len() as f32 * ROW + SECTION_PADDING;
    }
    if state.suggested_items_visible() {
        let n = state.suggested_items().len();
        if n > 0 {
            // Header (~18px: pt_1 + 11px text + pb_0p5) + N rows + pb_1 + border
            const HEADER: f32 = 18.0;
            const SECTION_PADDING: f32 = 5.0; // pb_1 + border_b_1
            let groups = state
                .suggested_items()
                .windows(2)
                .filter(|pair| suggestion_group(&pair[0]) != suggestion_group(&pair[1]))
                .count()
                + 1;
            total += groups as f32 * HEADER + n as f32 * ROW + SECTION_PADDING;
        }
    }
    total
}
