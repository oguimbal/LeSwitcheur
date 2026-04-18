//! Result list rendering.

use gpui::{div, hsla, img, px, rgb, svg, AnyElement, Div, IntoElement, ParentElement, Styled};
use switcheur_core::{Item, LlmProvider, MatchResult};
use switcheur_i18n::tr;

use crate::theme::Theme;

const ICON_SIZE: f32 = 26.0;
/// Every row is forced to this height so `uniform_list` (which only measures
/// the first item) can't clip the two-line rows when the first one happens to
/// only have a primary title.
const ROW_HEIGHT: f32 = 44.0;

/// Returns the row as a concrete [`Div`] so the caller can chain an `.id(...)`
/// + click/hover handlers on top before converting to `AnyElement`.
pub fn render_row(match_result: &MatchResult, selected: bool, theme: &Theme) -> Div {
    let item = &match_result.item;
    let primary = match item {
        Item::AskLlm { provider, .. } => tr(provider.i18n_key()),
        Item::OpenUrl(_) => tr("launcher.open_url"),
        _ => item.primary().to_string(),
    };
    let secondary = item.secondary().map(str::to_string);
    let minimized = item.is_minimized();

    let row_bg = if selected {
        theme.selection
    } else {
        theme.background
    };

    let base_icon: AnyElement = match item {
        Item::AskLlm { provider, .. } => llm_icon(*provider),
        _ => match item.icon_path() {
            Some(path) => img(path.to_path_buf())
                .w(px(ICON_SIZE))
                .h(px(ICON_SIZE))
                .into_any_element(),
            None => placeholder_icon(item.icon_initial(), item.icon_seed(), theme),
        },
    };
    let icon: AnyElement = if minimized {
        // Tag minimized windows with a small down-arrow badge in the corner,
        // visually echoing the Dock's minimize animation target.
        div()
            .relative()
            .w(px(ICON_SIZE))
            .h(px(ICON_SIZE))
            .child(base_icon)
            .child(
                div()
                    .absolute()
                    .bottom(px(-2.0))
                    .right(px(-2.0))
                    .w(px(12.0))
                    .h(px(12.0))
                    .rounded_full()
                    .bg(theme.background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.0))
                    .text_color(theme.foreground)
                    .child("▾"),
            )
            .into_any_element()
    } else {
        base_icon
    };

    let primary_el = div()
        .text_size(px(14.0))
        .text_color(theme.foreground)
        .truncate()
        .child(primary);

    let mut text_col = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .gap_0p5();
    text_col = text_col.child(primary_el);
    if let Some(sub) = secondary {
        text_col = text_col.child(
            div()
                .text_size(px(11.0))
                .text_color(theme.muted)
                .truncate()
                .child(sub),
        );
    }

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .px_3()
        .h(px(ROW_HEIGHT))
        .rounded_md()
        .bg(row_bg)
        .w_full()
        .child(icon)
        .child(text_col)
}

/// Deterministic hue in [0, 1] from a seed string. Used to color the initial
/// placeholder "icon" differently per app.
fn hash_hue(seed: &str) -> f32 {
    // Simple FNV-1a, good enough for palette selection.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h % 360) as f32 / 360.0
}

/// Branded icon for "Ask <Provider>" rows. Each provider has a monochrome
/// SVG mark bundled under `crates/switcheur-ui/assets/llm_icons/` and gets
/// tinted with its brand colour via GPUI's `svg().text_color(...)`. The SVGs
/// are registered with the App through the [`crate::assets::Assets`] source
/// at startup — swap those files to ship updated artwork.
fn llm_icon(provider: LlmProvider) -> AnyElement {
    let (asset, color) = match provider {
        LlmProvider::Mistral => ("llm_icons/mistral.svg", 0xFF7000),
        LlmProvider::Claude => ("llm_icons/claude.svg", 0xCC785C),
        LlmProvider::ChatGpt => ("llm_icons/chatgpt.svg", 0x10A37F),
        LlmProvider::Perplexity => ("llm_icons/perplexity.svg", 0x20808D),
    };
    svg()
        .path(asset)
        .w(px(ICON_SIZE))
        .h(px(ICON_SIZE))
        .text_color(rgb(color))
        .into_any_element()
}

fn placeholder_icon(initial: char, seed: &str, _theme: &Theme) -> AnyElement {
    let hue = hash_hue(seed);
    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(ICON_SIZE))
        .h(px(ICON_SIZE))
        .rounded_md()
        .bg(hsla(hue, 0.55, 0.45, 1.0))
        .text_size(px(13.0))
        .text_color(gpui::rgb(0xffffff))
        .child(initial.to_string())
        .into_any_element()
}
