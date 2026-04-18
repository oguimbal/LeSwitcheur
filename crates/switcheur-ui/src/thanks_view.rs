//! Post-activation confirmation popup.
//!
//! Shown after `leswitcheur://activate` resolves — either as a thanks card
//! with a heart and the verified licence key, or as an error card when the
//! backend refused the key or the signature check failed. One button: OK.

use gpui::{
    div, prelude::*, px, AnyElement, App, Context, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    Styled, Window,
};
use switcheur_i18n::tr as _tr;

use crate::theme::Theme;

fn tr(key: &str) -> SharedString {
    SharedString::from(_tr(key))
}

#[derive(Debug, Clone)]
pub enum ThanksViewEvent {
    Dismissed,
}

#[derive(Debug, Clone)]
pub enum ThanksState {
    Success { key: String },
    Error { key: String, message_i18n: String },
}

pub struct ThanksView {
    state: ThanksState,
    theme: Theme,
    focus: FocusHandle,
}

impl ThanksView {
    pub fn new(state: ThanksState, cx: &mut Context<Self>) -> Self {
        Self {
            state,
            theme: Theme::default(),
            focus: cx.focus_handle(),
        }
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(ThanksViewEvent::Dismissed);
    }

    fn on_mouse_ok(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(cx);
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = ev.keystroke.key.as_str();
        if matches!(k, "escape" | "enter" | "return" | "space") {
            self.dismiss(cx);
        }
    }
}

impl Focusable for ThanksView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<ThanksViewEvent> for ThanksView {}

impl Render for ThanksView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        // Heart on success, warning dot on error. Card colours and copy keys
        // flip accordingly; the rest of the layout is shared.
        let (badge_fg, badge_bg, badge_glyph, title_key, body_key, key_string) = match &self.state {
            ThanksState::Success { key } => (
                gpui::rgb(0xe0245e),
                gpui::rgba(0xe0245e22),
                "♥",
                "thanks.success.title",
                "thanks.success.body",
                key.clone(),
            ),
            ThanksState::Error { key, .. } => (
                theme.destructive,
                gpui::rgba(0xef444422),
                "!",
                "thanks.error.title",
                "thanks.error.body",
                key.clone(),
            ),
        };

        let badge = div()
            .w(px(72.0))
            .h(px(72.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(badge_bg)
            .text_size(px(38.0))
            .text_color(badge_fg)
            .child(SharedString::from(badge_glyph));

        let title = div()
            .text_size(px(22.0))
            .text_color(theme.foreground)
            .child(tr(title_key));

        let body = div()
            .text_size(px(13.5))
            .text_color(theme.muted)
            .text_center()
            .child(tr(body_key));

        let mut children: Vec<AnyElement> = vec![
            badge.into_any_element(),
            title.into_any_element(),
            body.into_any_element(),
        ];

        if let ThanksState::Error { message_i18n, .. } = &self.state {
            let err_pill = div()
                .px_4()
                .py_2()
                .rounded_md()
                .bg(gpui::rgba(0xef444422))
                .text_color(theme.destructive)
                .text_size(px(13.0))
                .child(tr(message_i18n));
            children.push(err_pill.into_any_element());
        }

        if !key_string.is_empty() {
            let key_pill = div()
                .px_4()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.selection)
                .text_size(px(14.0))
                .text_color(theme.foreground)
                .child(SharedString::from(key_string));
            children.push(key_pill.into_any_element());
        }

        let ok_btn = div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(38.0))
            .px_8()
            .rounded_full()
            .bg(theme.accent)
            .text_color(gpui::rgb(0xffffff))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_ok))
            .child(tr("thanks.ok"));
        children.push(ok_btn.into_any_element());

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .items_center()
            .justify_center()
            .gap_4()
            .px_8()
            .py_8()
            .children(children)
    }
}
