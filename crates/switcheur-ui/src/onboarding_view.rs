//! First-launch wizard. Four steps:
//!   1. Request Accessibility permission (explain + trigger system prompt).
//!   2. Replace the native Cmd+Tab switcher (yes/no).
//!   3. Record a global hotkey (or skip).
//!   4. Congratulations + license CTA.
//!
//! Rendered in a blurred popup window with no titlebar (always-on-top, no
//! system close buttons). Dismiss from any step marks onboarding complete.

use gpui::{
    div, img, prelude::*, px, AnyElement, App, Context, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, ParentElement, Render,
    SharedString, Styled, Window,
};
use switcheur_core::HotkeySpec;
use switcheur_i18n::{modifier_symbol, tr as _tr};

use crate::theme::Theme;

fn tr(key: &str) -> SharedString {
    SharedString::from(_tr(key))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    Accessibility,
    Hotkey,
    SystemSwitcher,
    Done,
}

#[derive(Debug, Clone)]
pub enum OnboardingViewEvent {
    /// User clicked "Enable accessibility". Host calls `ensure_accessibility(true)`
    /// which triggers the macOS system prompt. View advances regardless of
    /// whether the user grants — they can always come back later.
    AccessibilityRequested,
    /// User recorded a new hotkey and advanced. Host should call
    /// `hotkey_service.reregister(&spec)` and persist the config.
    HotkeyApplied(HotkeySpec),
    /// User picked yes/no on the Cmd+Tab replacement. Host starts/stops the
    /// `SystemSwitcherService` and persists the config.
    ReplaceSystemSwitcherChosen(bool),
    /// User clicked "Buy a licence" in the final step. Host opens the
    /// marketing page in the default browser.
    LicensePurchaseRequested,
    /// User flipped the launch-at-startup toggle on the final step. Host
    /// enables/disables the LaunchAgent and persists the flag — same handler
    /// as the equivalent toggle in settings.
    LaunchAtStartupChanged(bool),
    /// Wizard closed. Host removes the window (and nothing else — first-launch
    /// state is tracked by config.toml existence, which `load_or_default`
    /// already persisted at boot).
    Finished,
}

pub struct OnboardingView {
    step: OnboardingStep,
    recording: bool,
    recorded: Option<HotkeySpec>,
    /// Flipped to `true` by the host after polling confirms macOS has granted
    /// Accessibility. Gates the "Next" button on the first step.
    accessibility_granted: bool,
    /// Mirrors `Config::launch_at_startup`. Rendered as a toggle on the final
    /// step; flipping emits `LaunchAtStartupChanged` for the host to enact.
    launch_at_startup: bool,
    theme: Theme,
    focus: FocusHandle,
}

impl OnboardingView {
    pub fn new(launch_at_startup: bool, cx: &mut Context<Self>) -> Self {
        Self {
            step: OnboardingStep::Accessibility,
            recording: false,
            recorded: None,
            accessibility_granted: false,
            launch_at_startup,
            theme: Theme::default(),
            focus: cx.focus_handle(),
        }
    }

    fn toggle_launch_at_startup(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.launch_at_startup = !self.launch_at_startup;
        cx.emit(OnboardingViewEvent::LaunchAtStartupChanged(
            self.launch_at_startup,
        ));
        cx.notify();
    }

    fn request_accessibility(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Just emit — do NOT advance. The step only advances once the host
        // pushes `set_accessibility_granted(true)` and the user clicks Next.
        cx.emit(OnboardingViewEvent::AccessibilityRequested);
        cx.notify();
    }

    fn advance_from_accessibility(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.accessibility_granted {
            return;
        }
        self.step = OnboardingStep::SystemSwitcher;
        cx.notify();
    }

    pub fn set_accessibility_granted(&mut self, granted: bool, cx: &mut Context<Self>) {
        if self.accessibility_granted != granted {
            self.accessibility_granted = granted;
            cx.notify();
        }
    }

    fn go_back(&mut self, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let prev = match self.step {
            OnboardingStep::Accessibility => return,
            OnboardingStep::SystemSwitcher => OnboardingStep::Accessibility,
            OnboardingStep::Hotkey => OnboardingStep::SystemSwitcher,
            OnboardingStep::Done => OnboardingStep::Hotkey,
        };
        self.step = prev;
        self.recording = false;
        if prev == OnboardingStep::Hotkey && self.recorded.is_none() {
            self.recording = true;
            self.focus.focus(window, cx);
        }
        cx.notify();
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn start_recording(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recording = true;
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn go_next_from_hotkey(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(spec) = self.recorded.clone() {
            cx.emit(OnboardingViewEvent::HotkeyApplied(spec));
        }
        self.step = OnboardingStep::Done;
        self.recording = false;
        cx.notify();
    }

    fn skip_hotkey(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.step = OnboardingStep::Done;
        self.recording = false;
        self.recorded = None;
        cx.notify();
    }

    fn pick_switcher(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(OnboardingViewEvent::ReplaceSystemSwitcherChosen(on));
        self.step = OnboardingStep::Hotkey;
        if self.recorded.is_none() {
            self.recording = true;
            self.focus.focus(window, cx);
        }
        cx.notify();
    }

    fn buy_licence(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(OnboardingViewEvent::LicensePurchaseRequested);
        cx.emit(OnboardingViewEvent::Finished);
    }

    fn dismiss(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(OnboardingViewEvent::Finished);
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.recording || self.step != OnboardingStep::Hotkey {
            return;
        }
        let k = &ev.keystroke;
        if let Some(spec) = keystroke_to_spec(k) {
            self.recorded = Some(spec);
            self.recording = false;
            cx.notify();
            return;
        }
        if k.key == "escape" {
            self.recording = false;
            cx.notify();
        }
    }
}

impl Focusable for OnboardingView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<OnboardingViewEvent> for OnboardingView {}

impl Render for OnboardingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let body: AnyElement = match self.step {
            OnboardingStep::Accessibility => {
                self.render_accessibility_step(cx).into_any_element()
            }
            OnboardingStep::Hotkey => self.render_hotkey_step(cx).into_any_element(),
            OnboardingStep::SystemSwitcher => {
                self.render_system_switcher_step(cx).into_any_element()
            }
            OnboardingStep::Done => self.render_done_step(cx).into_any_element(),
        };

        let show_back = !matches!(self.step, OnboardingStep::Accessibility);
        let header = div()
            .relative()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .child(self.render_progress_dots(cx))
            .child(if show_back {
                div()
                    .absolute()
                    .left_0()
                    .top(px(-4.0))
                    .child(self.render_back_button(cx))
                    .into_any_element()
            } else {
                div().into_any_element()
            });

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_size(px(14.0))
            .px_8()
            .py_8()
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(body),
            )
    }
}

impl OnboardingView {
    fn render_progress_dots(&self, cx: &mut Context<Self>) -> AnyElement {
        let _ = cx;
        let theme = self.theme;
        let idx = match self.step {
            OnboardingStep::Accessibility => 0,
            OnboardingStep::SystemSwitcher => 1,
            OnboardingStep::Hotkey => 2,
            OnboardingStep::Done => 3,
        };
        let mut row = div().flex().flex_row().gap_2().justify_center();
        for i in 0..4 {
            let active = i == idx;
            row = row.child(
                div()
                    .w(px(if active { 24.0 } else { 8.0 }))
                    .h(px(8.0))
                    .rounded_full()
                    .bg(if active { theme.accent } else { theme.border }),
            );
        }
        row.into_any_element()
    }

    fn render_back_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(28.0))
            .px_3()
            .rounded_full()
            .text_size(px(12.5))
            .text_color(theme.muted)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::go_back))
            .child(tr("onboarding.common.back"))
            .into_any_element()
    }

    fn render_accessibility_step(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let granted = self.accessibility_granted;

        let logo = img("brand/logo.png").w(px(72.0)).h(px(72.0));

        let welcome = div()
            .text_size(px(22.0))
            .text_color(theme.foreground)
            .child(tr("onboarding.welcome.title"));

        let body_title = div()
            .text_size(px(16.0))
            .text_color(theme.foreground)
            .child(tr("onboarding.accessibility.title"));

        let body_text = div()
            .text_size(px(13.5))
            .text_color(theme.muted)
            .text_center()
            .child(tr("onboarding.accessibility.body"));

        let action: AnyElement = if granted {
            let success_green = gpui::rgb(0x22c55e);
            let pill = div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(38.0))
                .px_5()
                .rounded_full()
                .bg(gpui::rgba(0x22c55e20))
                .text_color(success_green)
                .child(SharedString::from(format!(
                    "✓  {}",
                    _tr("onboarding.accessibility.granted")
                )));
            let next_btn = div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(38.0))
                .px_6()
                .rounded_full()
                .bg(theme.accent)
                .text_color(gpui::rgb(0xffffff))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(Self::advance_from_accessibility),
                )
                .child(tr("onboarding.common.next"));
            div()
                .flex()
                .flex_row()
                .gap_3()
                .mt_2()
                .child(pill)
                .child(next_btn)
                .into_any_element()
        } else {
            let primary_btn = div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(38.0))
                .px_6()
                .rounded_full()
                .bg(theme.accent)
                .text_color(gpui::rgb(0xffffff))
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, cx.listener(Self::request_accessibility))
                .child(tr("onboarding.accessibility.enable"));
            div().flex().mt_2().child(primary_btn).into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_4()
            .max_w(px(440.0))
            .child(logo)
            .child(welcome)
            .child(body_title)
            .child(body_text)
            .child(action)
            .into_any_element()
    }

    fn render_hotkey_step(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;

        let key_display: SharedString = if self.recording {
            tr("settings.press_combo")
        } else if let Some(spec) = &self.recorded {
            format_spec(spec).into()
        } else {
            SharedString::from("—")
        };
        let key_color = if self.recording {
            theme.muted
        } else if self.recorded.is_some() {
            theme.foreground
        } else {
            theme.muted
        };
        let key_border = if self.recording {
            theme.accent
        } else {
            theme.border
        };

        let keystroke_pill = div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(72.0))
            .min_w(px(260.0))
            .px_8()
            .rounded_xl()
            .border_1()
            .border_color(key_border)
            .bg(theme.selection)
            .text_size(px(28.0))
            .text_color(key_color)
            .child(key_display);

        let has_recorded = self.recorded.is_some();
        let primary_label: SharedString = if has_recorded {
            tr("onboarding.hotkey.next")
        } else if self.recording {
            tr("settings.cancel_record")
        } else {
            tr("settings.record")
        };

        let primary_btn = div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(38.0))
            .px_6()
            .rounded_full()
            .bg(theme.accent)
            .text_color(gpui::rgb(0xffffff))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev, window, cx| {
                    if has_recorded {
                        this.go_next_from_hotkey(ev, window, cx);
                    } else {
                        this.start_recording(ev, window, cx);
                    }
                }),
            )
            .child(primary_label);

        let secondary_label: SharedString = if has_recorded {
            tr("onboarding.hotkey.re_record")
        } else {
            tr("onboarding.hotkey.skip")
        };

        let secondary_btn = div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(38.0))
            .px_6()
            .rounded_full()
            .border_1()
            .border_color(theme.border)
            .text_color(theme.muted)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev, window, cx| {
                    if has_recorded {
                        this.recorded = None;
                        this.start_recording(ev, window, cx);
                    } else {
                        this.skip_hotkey(ev, window, cx);
                    }
                }),
            )
            .child(secondary_label);

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_6()
            .max_w(px(440.0))
            .child(
                div()
                    .text_size(px(20.0))
                    .text_color(theme.foreground)
                    .child(tr("onboarding.hotkey.title")),
            )
            .child(
                div()
                    .text_size(px(13.5))
                    .text_color(theme.muted)
                    .child(tr("onboarding.hotkey.subtitle")),
            )
            .child(
                div()
                    .max_w(px(380.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .text_center()
                    .child(tr("onboarding.hotkey.why")),
            )
            .child(keystroke_pill)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .mt_2()
                    .child(secondary_btn)
                    .child(primary_btn),
            )
            .into_any_element()
    }

    fn render_system_switcher_step(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;

        let kbd = |label: &str, theme: &Theme| -> AnyElement {
            div()
                .flex()
                .items_center()
                .justify_center()
                .min_w(px(36.0))
                .h(px(36.0))
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.selection)
                .text_size(px(18.0))
                .text_color(theme.foreground)
                .child(SharedString::from(label.to_string()))
                .into_any_element()
        };

        let combo = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(kbd(modifier_symbol("cmd"), &theme))
            .child(div().text_color(theme.muted).child("+"))
            .child(kbd("⇥", &theme));

        let no_btn = div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(38.0))
            .px_6()
            .rounded_full()
            .border_1()
            .border_color(theme.border)
            .text_color(theme.muted)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.pick_switcher(false, window, cx);
                }),
            )
            .child(tr("onboarding.system_switcher.no"));

        let yes_btn = div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(38.0))
            .px_6()
            .rounded_full()
            .bg(theme.accent)
            .text_color(gpui::rgb(0xffffff))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.pick_switcher(true, window, cx);
                }),
            )
            .child(tr("onboarding.system_switcher.yes"));

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_5()
            .max_w(px(440.0))
            .child(
                div()
                    .text_size(px(20.0))
                    .text_color(theme.foreground)
                    .child(tr("onboarding.system_switcher.title")),
            )
            .child(combo)
            .child(
                div()
                    .text_size(px(13.5))
                    .text_color(theme.muted)
                    .child(tr("onboarding.system_switcher.body")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .mt_2()
                    .child(no_btn)
                    .child(yes_btn),
            )
            .into_any_element()
    }

    fn render_launch_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let on = self.launch_at_startup;
        let track_bg = if on { theme.accent } else { theme.border };
        let mut track = div()
            .w(px(36.0))
            .h(px(20.0))
            .rounded_full()
            .bg(track_bg)
            .flex()
            .items_center()
            .px(px(2.0));
        track = if on {
            track.justify_end()
        } else {
            track.justify_start()
        };
        let track = track.child(
            div()
                .w(px(16.0))
                .h(px(16.0))
                .rounded_full()
                .bg(gpui::rgb(0xffffff)),
        );
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::toggle_launch_at_startup),
            )
            .child(track)
            .child(
                div()
                    .text_color(theme.foreground)
                    .text_size(px(13.5))
                    .child(tr("settings.launch_at_startup")),
            )
            .into_any_element()
    }

    fn render_done_step(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let accent = theme.accent;
        let badge_bg = gpui::rgba(0xe5ebff20);

        let badge = div()
            .w(px(56.0))
            .h(px(56.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(badge_bg)
            .text_size(px(28.0))
            .text_color(accent)
            .child("✓");

        let buy_btn = div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(38.0))
            .px_6()
            .rounded_full()
            .bg(theme.accent)
            .text_color(gpui::rgb(0xffffff))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::buy_licence))
            .child(tr("onboarding.done.buy"));

        let later_btn = div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(38.0))
            .px_6()
            .rounded_full()
            .border_1()
            .border_color(theme.border)
            .text_color(theme.muted)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::dismiss))
            .child(tr("onboarding.done.later"));

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_4()
            .max_w(px(440.0))
            .child(badge)
            .child(
                div()
                    .text_size(px(20.0))
                    .text_color(theme.foreground)
                    .child(tr("onboarding.done.title")),
            )
            .child(
                div()
                    .text_size(px(13.5))
                    .text_color(theme.muted)
                    .text_center()
                    .child(tr("onboarding.done.body")),
            )
            .child(self.render_launch_toggle(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .mt_2()
                    .child(later_btn)
                    .child(buy_btn),
            )
            .into_any_element()
    }
}

// Mirrors `settings_view::keystroke_to_spec` — kept private there, duplicated
// here to avoid widening that module's surface for a single call site.
fn keystroke_to_spec(k: &Keystroke) -> Option<HotkeySpec> {
    let mut modifiers = Vec::new();
    if k.modifiers.control {
        modifiers.push("ctrl".to_string());
    }
    if k.modifiers.alt {
        modifiers.push("alt".to_string());
    }
    if k.modifiers.shift {
        modifiers.push("shift".to_string());
    }
    if k.modifiers.platform {
        modifiers.push("cmd".to_string());
    }
    if modifiers.is_empty() {
        return None;
    }
    let key = k.key.to_lowercase();
    if matches!(
        key.as_str(),
        "shift" | "control" | "alt" | "platform" | "function" | "" | "escape"
    ) {
        return None;
    }
    Some(HotkeySpec { modifiers, key })
}

fn format_spec(spec: &HotkeySpec) -> String {
    let mut mods = String::new();
    for m in &spec.modifiers {
        mods.push_str(modifier_symbol(m));
    }
    if mods.is_empty() {
        pretty_key(&spec.key)
    } else {
        format!("{mods} {}", pretty_key(&spec.key))
    }
}

fn pretty_key(k: &str) -> String {
    match k {
        "space" => _tr("keys.space"),
        "tab" => _tr("keys.tab"),
        "return" | "enter" => _tr("keys.enter"),
        "escape" | "esc" => _tr("keys.esc"),
        other if other.len() == 1 => other.to_ascii_uppercase(),
        other => other.to_string(),
    }
}
