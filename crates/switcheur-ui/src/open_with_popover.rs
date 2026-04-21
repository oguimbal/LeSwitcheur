//! The floating "Open With" popover attached to a selected zoxide dir row.
//! Renders as the root view of a borderless, transparent secondary window
//! (see `switcheur-platform/src/macos/panel.rs`). Lists detected folder
//! openers (file managers + editors), with the current default pinned at
//! the top in a greyed, non-selectable row.

use std::path::PathBuf;

use gpui::{
    canvas, div, img, prelude::*, px, App, ClickEvent, Context, EventEmitter, FocusHandle,
    Focusable, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    Styled, Window,
};
use switcheur_i18n::tr;

use crate::theme::Theme;

/// One row in the popover.
#[derive(Debug, Clone)]
pub struct OpenWithEntry {
    /// Stable id from `switcheur_core::file_manager::known_folder_openers`.
    pub id: String,
    pub display_name: String,
    pub bundle_id: String,
    /// Cached PNG on disk (via `switcheur_platform::macos::icons`). Absent
    /// means we couldn't extract the icon — the row falls back to a text
    /// initial so the popover never shows a blank square.
    pub icon_path: Option<PathBuf>,
    /// True for the row that matches the user's configured default opener.
    /// Rendered greyed + unselectable + "default" tag; the user picks it
    /// implicitly by pressing Enter without entering the popover.
    pub is_default: bool,
}

/// Events bubbled to the host window when the user interacts with the popover.
#[derive(Debug, Clone)]
pub enum OpenWithPopoverEvent {
    /// A popover row was clicked (or Enter pressed with that index focused).
    /// Carries the opener's stable id so the host can resolve a bundle id
    /// and promote it in the MRU list.
    Confirmed(String),
    /// Mouse moved over a row. The host uses this to keep the popover's
    /// keyboard selection in sync with the pointer.
    Hovered(usize),
}

pub struct OpenWithPopoverView {
    entries: Vec<OpenWithEntry>,
    /// Index into the *selectable* rows only (excludes the default row). The
    /// popover exposes that distinction to the host so Right-arrow focus
    /// lands on the first non-default row.
    selected: Option<usize>,
    theme: Theme,
    focus: FocusHandle,
}

impl OpenWithPopoverView {
    pub fn new(theme: Theme, cx: &mut Context<Self>) -> Self {
        Self {
            entries: Vec::new(),
            selected: None,
            theme,
            focus: cx.focus_handle(),
        }
    }

    pub fn set_entries(&mut self, entries: Vec<OpenWithEntry>, cx: &mut Context<Self>) {
        self.entries = entries;
        cx.notify();
    }

    pub fn set_selected(&mut self, idx: Option<usize>, cx: &mut Context<Self>) {
        self.selected = idx;
        cx.notify();
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }

    /// Rows the user can actually pick (everything except the default row).
    /// Exposed so the host can clamp `SwitcherState::open_with_index` against
    /// the live length instead of tracking it separately.
    pub fn selectable_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_default).count()
    }

    /// Total pixel height of the popover content. Used by the host to size
    /// the popover window so it fits exactly, without resorting to
    /// `UniformList` inside a floating panel.
    pub fn preferred_height(&self) -> f32 {
        // Arrow notch (12px) + top padding (8px) + one row per entry
        // (ROW_H each) + bottom padding (8px).
        const ROW_H: f32 = 32.0;
        let rows = self.entries.len().max(1) as f32;
        12.0 + 8.0 + ROW_H * rows + 8.0
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }
}

impl EventEmitter<OpenWithPopoverEvent> for OpenWithPopoverView {}

impl Focusable for OpenWithPopoverView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for OpenWithPopoverView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let selected = self.selected;

        // `selectable_idx` is the index within the non-default subset; it
        // matches what the host stores in `SwitcherState::open_with_index`.
        let mut selectable_idx: usize = 0;
        let rows: Vec<_> = self
            .entries
            .iter()
            .cloned()
            .enumerate()
            .map(|(row_idx, entry)| {
                let this_selectable = if entry.is_default {
                    None
                } else {
                    let i = selectable_idx;
                    selectable_idx += 1;
                    Some(i)
                };
                render_popover_row(
                    row_idx,
                    entry,
                    this_selectable,
                    selected,
                    &theme,
                    cx,
                )
            })
            .collect();

        // Arrow notch pointing left into the selected dir row. Drawn as a
        // filled triangle via the `canvas` low-level paint API so we don't
        // need an SVG asset. Tip sits at x=0 of the popover window so the
        // caller can place the window flush with (or slightly overlapping)
        // the main switcher's right edge and the arrow visually bridges the
        // two.
        let arrow_fill = theme.elevated_background;
        let border_color = theme.border;
        const ARROW_DEPTH: f32 = 12.0;
        const ARROW_HALF_HEIGHT: f32 = 10.0;
        let notch = canvas(
            |_, _, _| (),
            move |bounds, _state, window, _app| {
                use gpui::PathBuilder;
                let x_tip = bounds.origin.x;
                let x_base = bounds.origin.x + px(ARROW_DEPTH);
                let cy = bounds.origin.y + bounds.size.height / 2.0;
                let half = px(ARROW_HALF_HEIGHT);
                let tip = gpui::point(x_tip, cy);
                let top = gpui::point(x_base, cy - half);
                let bot = gpui::point(x_base, cy + half);
                // Fill first so the border stroke sits on top of the shared
                // edge with the bubble (drawn last).
                let mut path = PathBuilder::fill();
                path.move_to(top);
                path.line_to(tip);
                path.line_to(bot);
                path.close();
                if let Ok(p) = path.build() {
                    window.paint_path(p, arrow_fill);
                }
                // Stroke only the two outer edges (tip → top, tip → bottom)
                // so the bubble's left border continues smoothly into the
                // notch without drawing a vertical line at the base.
                let mut outline = PathBuilder::stroke(px(1.0));
                outline.move_to(top);
                outline.line_to(tip);
                outline.line_to(bot);
                if let Ok(p) = outline.build() {
                    window.paint_path(p, border_color);
                }
            },
        )
        .absolute()
        .left(px(0.0))
        .top(px(0.0))
        .w(px(ARROW_DEPTH))
        .h_full();

        let bubble = div()
            .flex()
            .flex_col()
            .ml(px(ARROW_DEPTH))
            .px_1()
            .py_2()
            .bg(theme.elevated_background)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_lg()
            .w(px(270.0))
            .children(rows);

        div()
            .track_focus(&self.focus)
            .relative()
            .w_full()
            .h_full()
            .child(notch)
            .child(bubble)
    }
}

fn render_popover_row(
    row_idx: usize,
    entry: OpenWithEntry,
    selectable_idx: Option<usize>,
    selected: Option<usize>,
    theme: &Theme,
    cx: &mut Context<OpenWithPopoverView>,
) -> gpui::AnyElement {
    let is_default = entry.is_default;
    let active = match (selectable_idx, selected) {
        (Some(si), Some(cur)) => si == cur,
        _ => false,
    };
    let fg = if is_default {
        theme.muted
    } else {
        theme.foreground
    };
    let row_bg = if active {
        theme.elevated_selection
    } else {
        theme.elevated_background
    };

    let icon: gpui::AnyElement = match &entry.icon_path {
        Some(p) => img(p.clone())
            .w(px(18.0))
            .h(px(18.0))
            .into_any_element(),
        None => div()
            .w(px(18.0))
            .h(px(18.0))
            .rounded_sm()
            .bg(theme.border)
            .into_any_element(),
    };

    let default_tag = if is_default {
        Some(
            div()
                .px_1p5()
                .rounded_sm()
                .bg(theme.border)
                .text_size(px(10.0))
                .text_color(theme.muted)
                .child(SharedString::from(tr("open_with.default"))),
        )
    } else {
        None
    };

    let name_el = div()
        .flex_1()
        .truncate()
        .text_size(px(13.0))
        .text_color(fg)
        .child(SharedString::from(entry.display_name.clone()));

    let base = |bg: gpui::Rgba| {
        let mut r = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .h(px(28.0))
            .rounded_sm()
            .bg(bg)
            .child(icon)
            .child(name_el);
        if let Some(tag) = default_tag {
            r = r.child(tag);
        }
        r
    };
    // The closure can only be called once because `icon`, `name_el`, and
    // `default_tag` are moved — which is fine since we only take one of the
    // two paths below.
    match (is_default, selectable_idx) {
        (true, _) | (_, None) => base(row_bg).into_any_element(),
        (false, Some(si)) => {
            let id = entry.id.clone();
            base(row_bg)
                .id(("open-with-row", row_idx))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    |_: &MouseDownEvent, _w, cx| cx.stop_propagation(),
                )
                .on_hover(cx.listener(move |_, hovering: &bool, _w, cx| {
                    if *hovering {
                        cx.emit(OpenWithPopoverEvent::Hovered(si));
                    }
                }))
                .on_click(cx.listener(move |_, _: &ClickEvent, _w, cx| {
                    cx.emit(OpenWithPopoverEvent::Confirmed(id.clone()));
                }))
                .into_any_element()
        }
    }
}
