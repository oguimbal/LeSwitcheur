//! OpenClaw gateway setup, in two steps. Connection: Control UI URL and
//! shared token (used once to pair, never stored). Once paired: the agent
//! that receives instructions and the target session. The host
//! runs pairing and the agent listing and reports through the `set_*`
//! methods.

use gpui::{
    div, img, linear, percentage, prelude::*, px, svg, Animation, AnimationExt, AnyElement, App,
    ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, Image, ImageFormat,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    Styled, Subscription, Transformation, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};
use std::sync::Arc;
use std::time::{Duration, Instant};
use switcheur_core::OpenClawSessionMode;
use switcheur_i18n::{tr as _tr, tr_sub};

use crate::theme::Theme;

fn tr(key: &str) -> SharedString {
    SharedString::from(_tr(key))
}

#[derive(Debug, Clone)]
pub enum OpenClawSetupEvent {
    Connect {
        url: String,
        token: String,
    },
    Save {
        agent_id: String,
        /// Name and emoji of the selected agent; `None` when the agent list
        /// couldn't be loaded (keep what was saved before).
        agent_display: Option<(String, Option<String>)>,
        session_mode: OpenClawSessionMode,
        session_key: String,
    },
    /// Approval was paused by the gateway: try pairing again now.
    Retry,
    Cancel,
}

/// Progress of the connection step.
#[derive(Debug, Clone, PartialEq)]
pub enum SetupStatus {
    Idle,
    Connecting,
    /// Waiting for `openclaw devices approve <id>` on the gateway host.
    AwaitingApproval {
        request_id: Option<String>,
        paused: bool,
    },
    Failed {
        code: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum AgentsState {
    Loading,
    Loaded,
    Failed { code: String },
}

/// Avatar bytes as served by the gateway, with the file extension that
/// matches their format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAvatar {
    pub bytes: Arc<Vec<u8>>,
    pub ext: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentChoice {
    pub id: String,
    pub name: String,
    pub emoji: Option<String>,
    pub avatar: Option<AgentAvatar>,
}

struct AgentEntry {
    choice: AgentChoice,
    image: Option<Arc<Image>>,
}

pub struct OpenClawSetupView {
    _subs: Vec<Subscription>,
    copied_at: Option<Instant>,
    url: Entity<InputState>,
    token: Entity<InputState>,
    session_key: Entity<InputState>,
    status: SetupStatus,
    /// Paired with the gateway: the agent step is shown.
    connected: bool,
    agents_state: AgentsState,
    agents: Vec<AgentEntry>,
    agent_id: String,
    session_mode: OpenClawSessionMode,
    /// Enter reaches both the input subscription and the key listener.
    saved: bool,
    theme: Theme,
    focus: FocusHandle,
}

const SESSION_MODES: [(OpenClawSessionMode, &str); 3] = [
    (OpenClawSessionMode::Main, "openclaw.session_main"),
    (OpenClawSessionMode::New, "openclaw.session_new"),
    (OpenClawSessionMode::Given, "openclaw.session_given"),
];

impl OpenClawSetupView {
    pub fn new(
        url: Entity<InputState>,
        token: Entity<InputState>,
        session_key: Entity<InputState>,
        agent_id: String,
        session_mode: OpenClawSessionMode,
        cx: &mut Context<Self>,
    ) -> Self {
        // Inputs swallow Enter as `PressEnter`; treat it as the main button.
        let mut subs: Vec<Subscription> = [&url, &token]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this: &mut Self, _, ev: &InputEvent, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.submit(cx);
                    }
                })
            })
            .collect();
        subs.push(cx.subscribe(
            &session_key,
            |this: &mut Self, _, ev: &InputEvent, cx| match ev {
                InputEvent::PressEnter { .. } => this.submit(cx),
                InputEvent::Focus => {
                    this.session_mode = OpenClawSessionMode::Given;
                    cx.notify();
                }
                _ => {}
            },
        ));
        Self {
            _subs: subs,
            copied_at: None,
            url,
            token,
            session_key,
            status: SetupStatus::Idle,
            connected: false,
            agents_state: AgentsState::Loading,
            agents: Vec::new(),
            agent_id,
            session_mode,
            saved: false,
            theme: Theme::default(),
            focus: cx.focus_handle(),
        }
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }

    pub fn set_status(&mut self, status: SetupStatus, cx: &mut Context<Self>) {
        // A refused save (status back to Failed) can be retried.
        self.saved = false;
        self.status = status;
        cx.notify();
    }

    pub fn status(&self) -> &SetupStatus {
        &self.status
    }

    pub fn url_input(&self) -> &Entity<InputState> {
        &self.url
    }

    /// Paired: move to the agent step, its list loading.
    pub fn set_connected(&mut self, cx: &mut Context<Self>) {
        self.connected = true;
        self.status = SetupStatus::Idle;
        self.agents_state = AgentsState::Loading;
        cx.notify();
    }

    /// Agents of the gateway. Keeps the configured agent selected when it is
    /// still there, else selects the gateway's default one.
    pub fn set_agents(
        &mut self,
        agents: Vec<AgentChoice>,
        default_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.agents = agents
            .into_iter()
            .map(|choice| {
                let image = choice.avatar.as_ref().and_then(|avatar| {
                    let format = match avatar.ext {
                        "png" => ImageFormat::Png,
                        "jpg" => ImageFormat::Jpeg,
                        "webp" => ImageFormat::Webp,
                        "gif" => ImageFormat::Gif,
                        "svg" => ImageFormat::Svg,
                        _ => return None,
                    };
                    Some(Arc::new(Image::from_bytes(format, avatar.bytes.to_vec())))
                });
                AgentEntry { choice, image }
            })
            .collect();
        self.agents_state = AgentsState::Loaded;
        let keep = self.agents.iter().any(|a| a.choice.id == self.agent_id);
        let target = if keep {
            self.agent_id.clone()
        } else if self.agents.iter().any(|a| a.choice.id == default_id) {
            default_id
        } else {
            match self.agents.first() {
                Some(a) => a.choice.id.clone(),
                None => self.agent_id.clone(),
            }
        };
        self.select_agent(target, window, cx);
    }

    pub fn set_agents_failed(&mut self, code: String, cx: &mut Context<Self>) {
        self.agents_state = AgentsState::Failed { code };
        cx.notify();
    }

    /// The agent list came back: [`Self::selected_avatar`] is meaningful.
    pub fn agents_loaded(&self) -> bool {
        self.agents_state == AgentsState::Loaded
    }

    /// Avatar of the agent that will be saved.
    pub fn selected_avatar(&self) -> Option<AgentAvatar> {
        self.agents
            .iter()
            .find(|a| a.choice.id == self.agent_id)
            .and_then(|a| a.choice.avatar.clone())
    }

    fn agent_name(&self, id: &str) -> Option<&str> {
        self.agents
            .iter()
            .find(|a| a.choice.id == id)
            .map(|a| a.choice.name.as_str())
    }

    fn select_agent(&mut self, id: String, _window: &mut Window, cx: &mut Context<Self>) {
        self.agent_id = id;
        cx.notify();
    }

    fn busy(&self) -> bool {
        matches!(
            self.status,
            SetupStatus::Connecting | SetupStatus::AwaitingApproval { .. }
        )
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let read = |input: &Entity<InputState>, cx: &Context<Self>| {
            input.read(cx).value().trim().to_string()
        };
        if self.connected {
            if std::mem::replace(&mut self.saved, true) {
                return;
            }
            let agent_display = self
                .agents
                .iter()
                .find(|a| a.choice.id == self.agent_id)
                .map(|a| (a.choice.name.clone(), a.choice.emoji.clone()));
            cx.emit(OpenClawSetupEvent::Save {
                agent_id: self.agent_id.clone(),
                agent_display,
                session_mode: self.session_mode,
                session_key: read(&self.session_key, cx),
            });
        } else if !self.busy() {
            cx.emit(OpenClawSetupEvent::Connect {
                url: read(&self.url, cx),
                token: read(&self.token, cx),
            });
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "escape" => cx.emit(OpenClawSetupEvent::Cancel),
            "enter" | "return" => self.submit(cx),
            _ => {}
        }
    }

    fn field(&self, label: &str, hint: Option<&str>, input: &Entity<InputState>) -> AnyElement {
        let theme = self.theme;
        let mut col = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_label(tr(label), &theme))
            .child(Input::new(input).bordered(true));
        if let Some(hint) = hint {
            col = col.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.muted)
                    .child(tr(hint)),
            );
        }
        col.into_any_element()
    }

    fn render_connection(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        if self.connected {
            let url = self.url.read(cx).value().trim().to_string();
            return div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .child(div().size(px(8.0)).flex_none().rounded_full().bg(gpui::rgb(0x30A46C)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_size(px(12.5))
                        .child(SharedString::from(tr_sub(
                            "openclaw.connected_to",
                            &[("url", &url)],
                        ))),
                )
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.accent)
                        .cursor_pointer()
                        .child(tr("openclaw.change_connection"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                this.connected = false;
                                this.status = SetupStatus::Idle;
                                cx.notify();
                            }),
                        ),
                )
                .into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.field("openclaw.url", Some("openclaw.url_hint"), &self.url))
            .child(self.field("openclaw.token", Some("openclaw.token_hint"), &self.token))
            .children(self.render_status(cx))
            .into_any_element()
    }

    fn render_agents(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let body: AnyElement = match &self.agents_state {
            AgentsState::Loading => spinner_row(tr("openclaw.agents_loading"), &theme),
            AgentsState::Failed { code } => div()
                .text_size(px(12.0))
                .text_color(theme.destructive)
                .child(SharedString::from(tr_sub(
                    "openclaw.agents_failed",
                    &[("reason", &error_message(code)), ("agent", &self.agent_id)],
                )))
                .into_any_element(),
            AgentsState::Loaded if self.agents.is_empty() => div()
                .text_size(px(12.0))
                .text_color(theme.muted)
                .child(tr("openclaw.agents_empty"))
                .into_any_element(),
            AgentsState::Loaded => {
                let rows: Vec<_> = self
                    .agents
                    .iter()
                    .enumerate()
                    .map(|(i, agent)| {
                        let selected = agent.choice.id == self.agent_id;
                        let id = agent.choice.id.clone();
                        let mut row = div()
                            .id(("openclaw-agent", i))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .px_2()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.selection))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                    this.select_agent(id.clone(), window, cx);
                                }),
                            )
                            .child(agent_avatar(agent, 28.0, &theme))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_size(px(13.0))
                                            .child(SharedString::from(agent.choice.name.clone())),
                                    )
                                    .when(agent.choice.name != agent.choice.id, |d| {
                                        d.child(
                                            div()
                                                .text_size(px(11.0))
                                                .text_color(theme.muted)
                                                .child(SharedString::from(
                                                    agent.choice.id.clone(),
                                                )),
                                        )
                                    }),
                            );
                        if selected {
                            row = row.bg(theme.selection).child(
                                svg()
                                    .path("browser_icons/check.svg")
                                    .size(px(14.0))
                                    .text_color(theme.accent),
                            );
                        }
                        row
                    })
                    .collect();
                div()
                    .id("openclaw-agents")
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .p_1()
                    .max_h(px(188.0))
                    .overflow_y_scroll()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .children(rows)
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_label(tr("openclaw.agent"), &theme))
            .child(body)
            .into_any_element()
    }

    fn render_session_modes(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let options: Vec<_> = SESSION_MODES
            .iter()
            .map(|(mode, key)| {
                let mode = *mode;
                let selected = self.session_mode == mode;
                let mut row = div()
                    .id(SharedString::from(*key))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_h(px(28.0))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            this.session_mode = mode;
                            if mode == OpenClawSessionMode::Given {
                                this.session_key.read(cx).focus_handle(cx).focus(window, cx);
                            }
                            cx.notify();
                        }),
                    )
                    .child(radio(selected, &theme))
                    .child(div().flex_none().text_size(px(13.0)).child(tr(key)));
                if mode == OpenClawSessionMode::Given {
                    row = row.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.session_key).bordered(true)),
                    );
                }
                row
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_label(tr("openclaw.session"), &theme))
            .children(options)
            .into_any_element()
    }

    fn render_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = self.theme;
        match &self.status {
            SetupStatus::Idle => None,
            SetupStatus::Connecting => Some(spinner_row(tr("openclaw.connecting"), &theme)),
            SetupStatus::AwaitingApproval { request_id, paused } => {
                let command = match request_id {
                    Some(id) => format!("openclaw devices approve {id}"),
                    None => "openclaw devices list".to_string(),
                };
                let copied = self
                    .copied_at
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                let copy_label = if copied {
                    tr("openclaw.copied")
                } else {
                    tr("openclaw.copy")
                };
                let copy_command = |command: String| {
                    cx.listener(move |this: &mut Self, _: &MouseDownEvent, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
                        this.copied_at = Some(Instant::now());
                        cx.notify();
                    })
                };
                // The command gets its own full-width line and may wrap:
                // request ids are long and must stay readable.
                let command_box = div()
                    .id("openclaw-approve-command")
                    .w_full()
                    .min_w_0()
                    .overflow_hidden()
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.selection)
                    .text_size(px(12.0))
                    .font_family("Menlo")
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, copy_command(command.clone()))
                    .child(SharedString::from(command.clone()));
                let mut buttons = div().flex().flex_row().gap_2().child(
                    button(copy_label, &theme)
                        .on_mouse_down(MouseButton::Left, copy_command(command)),
                );
                if *paused {
                    buttons = buttons.child(button(tr("openclaw.retry"), &theme).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _: &MouseDownEvent, _, cx| {
                            cx.emit(OpenClawSetupEvent::Retry);
                        }),
                    ));
                }
                let col = div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .child(spinner_row(tr("openclaw.awaiting_approval"), &theme))
                    .child(command_box)
                    .child(buttons);
                Some(col.into_any_element())
            }
            SetupStatus::Failed { code } => Some(error_box(error_message(code), &theme)),
        }
    }
}

/// Localized explanation of a content-free failure code, the code kept for
/// support.
pub fn error_message(code: &str) -> String {
    let key = match code {
        "invalid-url" | "unsupported-url-scheme" => "openclaw.err.invalid_url",
        "missing-token" | "AUTH_TOKEN_MISSING" => "openclaw.err.missing_token",
        "not-paired" | "AUTH_DEVICE_TOKEN_MISMATCH" => "openclaw.err.not_paired",
        "AUTH_TOKEN_MISMATCH" | "AUTH_UNAUTHORIZED" | "AUTH_PASSWORD_MISMATCH" => {
            "openclaw.err.token_refused"
        }
        "AUTH_RATE_LIMITED" => "openclaw.err.rate_limited",
        "resolve" | "connect" | "timeout" | "websocket-handshake" | "closed" | "read" | "send" => {
            "openclaw.err.unreachable"
        }
        "no-device-token" => "openclaw.err.no_device_token",
        "save-failed" => "openclaw.err.save_failed",
        "pairing-timeout" => "openclaw.err.pairing_timeout",
        "missing-write-scope" | "OPERATOR_ACCESS_DENIED" => "openclaw.err.missing_scope",
        "PROTOCOL_MISMATCH" | "CLIENT_VERSION_MISMATCH" | "protocol-version" => {
            "openclaw.err.protocol"
        }
        "INVALID_REQUEST" => "openclaw.err.invalid_session",
        code if code.starts_with("DEVICE_AUTH_") => "openclaw.err.device_proof",
        _ => "openclaw.err.refused",
    };
    format!("{} ({code})", _tr(key))
}

/// Avatar image, else emoji, else the name's initial on a tinted disc.
fn agent_avatar(agent: &AgentEntry, size: f32, theme: &Theme) -> AnyElement {
    if let Some(image) = &agent.image {
        return img(image.clone())
            .size(px(size))
            .flex_none()
            .rounded_full()
            .into_any_element();
    }
    let (text, text_size) = match &agent.choice.emoji {
        Some(emoji) => (emoji.clone(), size * 0.6),
        None => (
            agent
                .choice
                .name
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default(),
            size * 0.45,
        ),
    };
    div()
        .size(px(size))
        .flex_none()
        .rounded_full()
        .bg(theme.selection)
        .border_1()
        .border_color(theme.border)
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(text_size))
        .child(SharedString::from(text))
        .into_any_element()
}

fn radio(selected: bool, theme: &Theme) -> AnyElement {
    let ring = div()
        .size(px(16.0))
        .flex_none()
        .rounded_full()
        .border_1()
        .flex()
        .items_center()
        .justify_center();
    if selected {
        ring.border_color(theme.accent)
            .child(div().size(px(8.0)).rounded_full().bg(theme.accent))
            .into_any_element()
    } else {
        ring.border_color(theme.border).into_any_element()
    }
}

fn section_label(label: SharedString, theme: &Theme) -> gpui::Div {
    div()
        .text_size(px(12.5))
        .text_color(theme.foreground)
        .child(label)
}

fn error_box(message: String, theme: &Theme) -> AnyElement {
    div()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(gpui::rgba(0xef444422))
        .text_color(theme.destructive)
        .text_size(px(12.5))
        .child(SharedString::from(message))
        .into_any_element()
}

fn spinner_row(label: SharedString, theme: &Theme) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .text_size(px(12.5))
        .text_color(theme.muted)
        .child(
            svg()
                .path("browser_icons/spinner.svg")
                .w(px(14.0))
                .h(px(14.0))
                .text_color(theme.muted)
                .with_animation(
                    "openclaw_setup_spinner",
                    Animation::new(Duration::from_millis(900))
                        .repeat()
                        .with_easing(linear),
                    |s, delta| s.with_transformation(Transformation::rotate(percentage(delta))),
                ),
        )
        .child(div().flex_1().min_w_0().child(label))
        .into_any_element()
}

fn button(label: SharedString, theme: &Theme) -> gpui::Div {
    div()
        .px_3()
        .py_1p5()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.selection)
        .text_color(theme.foreground)
        .text_size(px(12.5))
        .cursor_pointer()
        .child(label)
}

impl Focusable for OpenClawSetupView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<OpenClawSetupEvent> for OpenClawSetupView {}

impl Render for OpenClawSetupView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let busy = self.busy();
        let title = match self.agent_name(&self.agent_id) {
            Some(name) if self.connected => {
                SharedString::from(tr_sub("openclaw.setup_title_named", &[("name", name)]))
            }
            _ => tr("openclaw.setup_title"),
        };

        let primary = {
            let label = if self.connected {
                tr("openclaw.save")
            } else {
                tr("openclaw.connect")
            };
            let b = div()
                .px_4()
                .py_1p5()
                .rounded_md()
                .text_size(px(13.0))
                .child(label);
            if busy && !self.connected {
                b.bg(theme.selection).text_color(theme.muted)
            } else {
                b.bg(theme.accent)
                    .text_color(gpui::rgb(0xffffff))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| this.submit(cx)),
                    )
            }
        };
        let cancel = button(tr("openclaw.cancel"), &theme).on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _: &MouseDownEvent, _, cx| cx.emit(OpenClawSetupEvent::Cancel)),
        );

        let mut body = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.render_connection(cx));
        if self.connected {
            body = body
                .child(self.render_agents(cx))
                .child(self.render_session_modes(cx))
                .children(match &self.status {
                    SetupStatus::Failed { code } => Some(error_box(error_message(code), &theme)),
                    _ => None,
                });
        }

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_size(px(13.0))
            .gap_3()
            .px_6()
            .py_5()
            .child(div().text_size(px(18.0)).child(title))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(tr("openclaw.setup_intro")),
            )
            .child(body)
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(cancel)
                    .child(primary),
            )
    }
}
