//! Bridges our local [`Theme`] palette into `gpui_component`'s global
//! [`gpui_component::Theme`] / [`gpui_component::ThemeColor`] so that bundled
//! widgets (Input, popovers, context menu, ...) pick up the same colors as
//! the rest of the app.
//!
//! Switching back to gpui-component's bundled themes is just deleting the
//! `apply_overrides` call below.
//!
//! Call `apply(&theme, cx)` after `gpui_component::init(cx)`, and again on
//! appearance change.
//!
//! `match_highlight` has no equivalent token in `ThemeColor` and stays a
//! local-only field consumed by our list rendering.

use gpui::{App, Hsla};
use gpui_component::{Theme as GcTheme, ThemeMode};

use crate::Theme;

/// Sync our `Theme` into `gpui_component::Theme::global`.
pub fn apply(theme: &Theme, cx: &mut App) {
    let mode = if is_dark_palette(theme) {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    GcTheme::change(mode, None, cx);
    apply_overrides(theme, cx);
}

fn apply_overrides(theme: &Theme, cx: &mut App) {
    let bg: Hsla = theme.background.into();
    let fg: Hsla = theme.foreground.into();
    let muted: Hsla = theme.muted.into();
    let accent: Hsla = theme.accent.into();
    let selection: Hsla = theme.selection.into();
    let border: Hsla = theme.border.into();
    let destructive: Hsla = theme.destructive.into();
    let elevated_bg: Hsla = theme.elevated_background.into();
    let elevated_sel: Hsla = theme.elevated_selection.into();

    let g = GcTheme::global_mut(cx);
    g.colors.background = bg;
    g.colors.foreground = fg;
    g.colors.muted = muted;
    g.colors.muted_foreground = muted;
    g.colors.popover = elevated_bg;
    g.colors.popover_foreground = fg;
    g.colors.primary = accent;
    g.colors.primary_foreground = fg;
    g.colors.accent = accent;
    g.colors.accent_foreground = fg;
    g.colors.secondary = selection;
    g.colors.secondary_foreground = fg;
    g.colors.ring = accent;
    g.colors.caret = accent;
    g.colors.selection = accent.alpha(0.3);
    g.colors.border = border;
    g.colors.input = border;
    g.colors.danger = destructive;
    g.colors.danger_foreground = fg;
    g.colors.list = bg;
    g.colors.list_active = elevated_sel;
    g.colors.list_hover = selection;
}

fn is_dark_palette(theme: &Theme) -> bool {
    let bg: Hsla = theme.background.into();
    bg.l < 0.5
}
