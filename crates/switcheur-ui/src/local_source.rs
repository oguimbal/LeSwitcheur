use crate::theme::Theme;
use gpui::{
    canvas, div, point, px, rgb, AnyElement, Hsla, IntoElement, ParentElement, PathBuilder, Styled,
};
use switcheur_core::{LocalSessionState, LocalSourceRef};
use switcheur_i18n::tr;

pub fn state_color(state: &LocalSessionState, theme: &Theme) -> Hsla {
    match state {
        LocalSessionState::Active => rgb(0x43c46b).into(),
        LocalSessionState::Waiting => rgb(0xf0a33a).into(),
        LocalSessionState::Compacting => rgb(0x4f8df7).into(),
        LocalSessionState::Idle | LocalSessionState::Unknown => theme.muted.into(),
    }
}

pub fn status_color(state: &LocalSessionState, theme: &Theme) -> Hsla {
    let mut color = state_color(state, theme);
    if Hsla::from(theme.background).l > 0.5 && color.s > 0.0 {
        color.l = color.l.min(0.34);
    }
    color
}

pub fn status(source: &LocalSourceRef) -> String {
    tr(match source.entry.state {
        LocalSessionState::Active => "local_sources.active",
        LocalSessionState::Waiting => "local_sources.waiting",
        LocalSessionState::Idle => "local_sources.idle",
        LocalSessionState::Compacting => "local_sources.compacting",
        LocalSessionState::Unknown => "local_sources.unknown",
    })
}

/// The provider ring, context pie and unread dot use AgentsMon's colours at 26px.
pub fn icon(source: &LocalSourceRef, theme: &Theme) -> AnyElement {
    let accent = source.entry.accent_rgb.unwrap_or(0x686b75);
    let percentage = source.entry.context_percent;
    let unread = source.entry.unread;
    let background = theme.background;
    let pie = canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let center = point(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            );
            let circle = |radius: f32| {
                let radius = px(radius);
                let mut path = PathBuilder::fill();
                path.move_to(point(center.x + radius, center.y));
                path.arc_to(
                    point(radius, radius),
                    px(0.0),
                    false,
                    true,
                    point(center.x - radius, center.y),
                );
                path.arc_to(
                    point(radius, radius),
                    px(0.0),
                    false,
                    true,
                    point(center.x + radius, center.y),
                );
                path.build()
            };
            if let Ok(path) = circle(12.0) {
                window.paint_path(path, rgb(accent));
            }
            if let Ok(path) = circle(9.0) {
                window.paint_path(path, rgb(0x686b75));
            }
            if let Some(percent) = percentage {
                let color = if percent <= 33 {
                    0xf1f2f4
                } else if percent <= 66 {
                    0xf0a33a
                } else {
                    0xe45353
                };
                if percent == 100 {
                    if let Ok(path) = circle(9.0) {
                        window.paint_path(path, rgb(color));
                    }
                } else if percent > 0 {
                    let angle = -std::f32::consts::FRAC_PI_2
                        + percent as f32 / 100.0 * std::f32::consts::TAU;
                    let mut path = PathBuilder::fill();
                    path.move_to(center);
                    path.line_to(point(center.x, center.y - px(9.0)));
                    path.arc_to(
                        point(px(9.0), px(9.0)),
                        px(0.0),
                        percent > 50,
                        true,
                        point(
                            center.x + px(9.0) * angle.cos(),
                            center.y + px(9.0) * angle.sin(),
                        ),
                    );
                    path.line_to(center);
                    if let Ok(path) = path.build() {
                        window.paint_path(path, rgb(color));
                    }
                }
            }
        },
    )
    .size(px(26.0));
    let mut icon = div().relative().flex_shrink_0().size(px(26.0)).child(pie);
    if unread {
        icon = icon.child(
            div()
                .absolute()
                .top(px(-1.0))
                .right(px(-1.0))
                .size(px(8.0))
                .rounded_full()
                .border_1()
                .border_color(background)
                .bg(rgb(0xf2b84b)),
        );
    }
    icon.into_any_element()
}
