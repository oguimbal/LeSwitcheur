//! Host side of the OpenClaw provider: sending instructions from the
//! switcher panel, and the setup window that pairs a gateway.
//!
//! Instruction text and the shared gateway token are never logged; only
//! content-free gateway error codes are.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use anyhow::Result;
use gpui::{
    App, AppContext, AsyncApp, Entity, Focusable, WindowBounds, WindowHandle, WindowKind,
    WindowOptions,
};
use std::sync::Arc;
use switcheur_core::{Config, LlmProvider, OpenClawConfig, OpenClawTarget};
use switcheur_platform::openclaw::{self, GatewayError, PairOutcome, SendTarget, Url};
use switcheur_ui::openclaw_setup_view::{
    error_message, AgentAvatar, AgentChoice, OpenClawSetupEvent, OpenClawSetupView, SetupStatus,
};
use switcheur_ui::OpenClawRow;

use crate::{initial_bounds, theme_for, AppState, WindowSlot};

const SETUP_WIDTH: f32 = 480.0;
const SETUP_HEIGHT: f32 = 720.0;

/// Agent row to show in the switcher when a gateway is configured *and* this
/// computer holds a device token for it. `None` sends the row to the setup
/// window instead.
pub(crate) fn ready_row(config: &Config) -> Option<OpenClawRow> {
    let oc = config.openclaw.as_ref()?;
    let url = openclaw::parse_gateway_url(&oc.url).ok()?;
    openclaw::is_paired(&url).then(|| OpenClawRow {
        name: oc.nickname.clone().into(),
        avatar: openclaw::avatar_path().map(Arc::from),
        emoji: oc.emoji.clone().map(Into::into),
    })
}

/// `SwitcherViewEvent::SendInstruction`: clear the field, send off the UI
/// thread, then flip the panel to "sent" (or put the text back on failure).
pub(crate) fn send_from_panel(text: String, state: &AppState, cx: &mut App) {
    let Some((handle, entity)) = state
        .current
        .borrow()
        .as_ref()
        .map(|slot| (slot.handle, slot.entity.clone()))
    else {
        return;
    };
    let Some(oc) = state.config.borrow().openclaw.clone() else {
        return;
    };
    // Float the row up next time, like the other providers.
    let promoted = {
        let mut cfg = state.config.borrow_mut();
        cfg.promote_llm_provider(LlmProvider::OpenClaw);
        cfg.clone()
    };
    if let Err(e) = promoted.save() {
        tracing::warn!("save config after llm promote: {e:#}");
    }
    let _ = cx.update_window(handle, |_, window, cx| {
        entity.update(cx, |view, cx| view.instruction_sending(window, cx));
    });
    let nickname = oc.nickname.clone();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let message = text.clone();
        let result = cx
            .background_executor()
            .spawn(async move {
                let base = openclaw::parse_gateway_url(&oc.url)?;
                let target = match oc.target() {
                    OpenClawTarget::Session(key) => SendTarget::Session(key),
                    OpenClawTarget::NewSession { agent_id } => SendTarget::NewSession { agent_id },
                };
                let session_key = openclaw::send_instruction(&base, &target, &message)?;
                Ok::<_, GatewayError>(openclaw::conversation_url(&base, &session_key))
            })
            .await;
        let _ = cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| match result {
                Ok(url) => {
                    tracing::info!("openclaw instruction sent");
                    view.instruction_sent(url, window, cx);
                }
                Err(error) => {
                    tracing::warn!(code = %error.code(), "openclaw instruction failed");
                    if error.code() == "AUTH_DEVICE_TOKEN_MISMATCH" {
                        // Device token was dropped: next pick goes to setup.
                        view.set_openclaw(None, cx);
                    }
                    let reason = error_message(&error.code());
                    let message = switcheur_i18n::tr_sub(
                        "switcher.instruction_failed",
                        &[("name", &nickname), ("reason", &reason)],
                    );
                    view.instruction_failed(text, message, window, cx);
                }
            });
        });
    })
    .detach();
}

/// Open (or focus) the setup window, prefilled from the current config.
pub(crate) fn open_setup_window(state: &AppState, cx: &mut App) -> Result<()> {
    // LSUIElement: a Normal window needs explicit activation to come forward.
    cx.activate(true);
    if let Some(handle) = state.openclaw_setup.borrow().as_ref().map(|s| s.handle) {
        if cx
            .update_window(handle, |_, window, _| window.activate_window())
            .is_ok()
        {
            return Ok(());
        }
    }
    state.openclaw_setup.borrow_mut().take();

    let existing = state.config.borrow().openclaw.clone();
    let paired_url = existing
        .as_ref()
        .and_then(|oc| openclaw::parse_gateway_url(&oc.url).ok());
    let theme = theme_for(state.config.borrow().appearance);
    let bounds = initial_bounds(cx, SETUP_WIDTH, SETUP_HEIGHT);
    let options = WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(switcheur_i18n::tr("window.openclaw_title").into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        kind: WindowKind::Normal,
        is_movable: true,
        is_resizable: false,
        focus: true,
        show: true,
        ..Default::default()
    };

    let entity_slot: Rc<RefCell<Option<Entity<OpenClawSetupView>>>> = Rc::new(RefCell::new(None));
    let slot_for_builder = entity_slot.clone();
    let handle: WindowHandle<gpui_component::Root> =
        cx.open_window(options, move |window, cx| {
            use gpui_component::input::InputState;
            let (url, session_key, agent_id, session_mode) = match &existing {
                Some(oc) => (
                    oc.url.clone(),
                    oc.session_key.clone(),
                    oc.agent_id.clone(),
                    oc.session_mode,
                ),
                None => (
                    String::new(),
                    String::new(),
                    OpenClawConfig::default_agent_id(),
                    Default::default(),
                ),
            };
            let url = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("http://host:18789/")
                    .default_value(url)
            });
            let token = cx.new(|cx| InputState::new(window, cx).masked(true));
            let session_key = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("agent:main:inbox")
                    .default_value(session_key)
            });
            let entity = cx.new(|cx| {
                let mut v = OpenClawSetupView::new(
                    url.clone(),
                    token,
                    session_key,
                    agent_id,
                    session_mode,
                    cx,
                );
                v.set_theme(theme, cx);
                v
            });
            *slot_for_builder.borrow_mut() = Some(entity.clone());
            url.read(cx).focus_handle(cx).focus(window, cx);
            cx.new(|cx| gpui_component::Root::new(entity.clone(), window, cx))
        })?;
    let _ = handle.update(cx, |_, window, _| window.activate_window());

    let entity = entity_slot
        .borrow()
        .clone()
        .expect("builder populated slot");
    let state_sub = state.clone();
    let sub = cx.subscribe(
        &entity,
        move |entity, ev: &OpenClawSetupEvent, cx: &mut App| {
            handle_setup_event(ev, entity, &state_sub, cx);
        },
    );
    *state.openclaw_setup.borrow_mut() = Some(WindowSlot {
        handle: handle.into(),
        _sub: sub,
    });
    // Already paired: straight to the agent step.
    if let Some(base) = paired_url
        .filter(openclaw::is_paired)
    {
        show_agent_step(base, entity, state, cx);
    }
    Ok(())
}

/// Paired: show the agent step and fetch the agents (with their avatars) off
/// the UI thread.
fn show_agent_step(base: Url, entity: Entity<OpenClawSetupView>, state: &AppState, cx: &mut App) {
    entity.update(cx, |v, cx| v.set_connected(cx));
    let Some(handle) = state.openclaw_setup.borrow().as_ref().map(|s| s.handle) else {
        return;
    };
    cx.spawn(async move |cx: &mut AsyncApp| {
        let result = cx
            .background_executor()
            .spawn(async move {
                let roster = openclaw::list_agents(&base)?;
                let agents = roster
                    .agents
                    .into_iter()
                    .map(|agent| AgentChoice {
                        avatar: agent.avatar.as_ref().and_then(openclaw::load_avatar).map(
                            |(bytes, ext)| AgentAvatar {
                                bytes: Arc::new(bytes),
                                ext,
                            },
                        ),
                        id: agent.id,
                        name: agent.name,
                        emoji: agent.emoji,
                    })
                    .collect::<Vec<_>>();
                Ok::<_, GatewayError>((agents, roster.default_id))
            })
            .await;
        let _ = cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |v, cx| match result {
                Ok((agents, default_id)) => v.set_agents(agents, default_id, window, cx),
                Err(error) => {
                    tracing::warn!(code = %error.code(), "openclaw agents.list failed");
                    v.set_agents_failed(error.code(), cx);
                }
            });
        });
    })
    .detach();
}

fn close_setup_window(state: &AppState, cx: &mut App) {
    if let Some(slot) = state.openclaw_setup.borrow_mut().take() {
        let _ = cx.update_window(slot.handle, |_, window, _| window.remove_window());
    }
}

fn handle_setup_event(
    ev: &OpenClawSetupEvent,
    entity: Entity<OpenClawSetupView>,
    state: &AppState,
    cx: &mut App,
) {
    match ev {
        OpenClawSetupEvent::Cancel => close_setup_window(state, cx),
        OpenClawSetupEvent::Retry => {
            // The pairing loop parks on a paused approval; flipping the
            // status back to Connecting lets it try again.
            entity.update(cx, |v, cx| v.set_status(SetupStatus::Connecting, cx));
        }
        OpenClawSetupEvent::Connect { url, token } => {
            let fail = |code: &str, cx: &mut App| {
                let code = code.to_string();
                entity.update(cx, |v, cx| v.set_status(SetupStatus::Failed { code }, cx));
            };
            let base = match openclaw::parse_gateway_url(url) {
                Ok(base) => base,
                Err(error) => return fail(&error.code(), cx),
            };
            if token.is_empty() {
                // Already paired: the shared token isn't needed again.
                if openclaw::is_paired(&base) {
                    show_agent_step(base, entity, state, cx);
                } else {
                    fail("missing-token", cx);
                }
                return;
            }
            entity.update(cx, |v, cx| v.set_status(SetupStatus::Connecting, cx));
            spawn_pairing(base, token.clone(), entity, state.clone(), cx);
        }
        OpenClawSetupEvent::Save {
            agent_id,
            agent_display,
            session_mode,
            session_key,
        } => {
            let fail = |code: &str, cx: &mut App| {
                let code = code.to_string();
                entity.update(cx, |v, cx| v.set_status(SetupStatus::Failed { code }, cx));
            };
            let (url, avatar) = entity.read_with(cx, |v, cx| {
                (
                    v.url_input().read(cx).value().trim().to_string(),
                    v.agents_loaded().then(|| v.selected_avatar()),
                )
            });
            let base = match openclaw::parse_gateway_url(&url) {
                Ok(base) => base,
                Err(error) => return fail(&error.code(), cx),
            };
            let previous = state.config.borrow().openclaw.clone();
            let (nickname, emoji) = match agent_display {
                Some((name, emoji)) => (name.clone(), emoji.clone()),
                None => match previous.filter(|oc| oc.agent_id == *agent_id) {
                    Some(oc) => (oc.nickname, oc.emoji),
                    None => (agent_id.clone(), None),
                },
            };
            // Agents that didn't load: keep the avatar already cached.
            if let Some(avatar) = avatar {
                let stored = avatar
                    .as_ref()
                    .map(|a| (a.bytes.as_slice(), a.ext));
                if let Err(e) = openclaw::store_avatar(stored) {
                    tracing::warn!("store openclaw avatar: {e:#}");
                }
            }
            let config = OpenClawConfig {
                url: base.to_string(),
                nickname,
                emoji,
                agent_id: agent_id.clone(),
                session_mode: *session_mode,
                session_key: session_key.clone(),
            };
            save_config(Some(config), state, cx);
            close_setup_window(state, cx);
        }
    }
}

/// Pair until approved, refused, timed out or the window is closed. Each
/// attempt is one blocking connect on the background executor; approvals are
/// polled every [`openclaw::PAIRING_RETRY`] unless the gateway asked to pause.
fn spawn_pairing(
    base: Url,
    token: String,
    entity: Entity<OpenClawSetupView>,
    state: AppState,
    cx: &mut App,
) {
    let weak = entity.downgrade();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let started = Instant::now();
        loop {
            let (b, t) = (base.clone(), token.clone());
            let outcome = cx
                .background_executor()
                .spawn(async move { openclaw::pair(&b, &t) })
                .await;
            let status = match outcome {
                Ok(PairOutcome::Paired) => {
                    let base = base.clone();
                    let _ = cx.update(|cx| {
                        if let Some(entity) = weak.upgrade() {
                            show_agent_step(base, entity, &state, cx);
                        }
                    });
                    return;
                }
                Ok(PairOutcome::AwaitingApproval { .. })
                    if started.elapsed() > openclaw::PAIRING_TIMEOUT =>
                {
                    SetupStatus::Failed {
                        code: "pairing-timeout".into(),
                    }
                }
                Ok(PairOutcome::AwaitingApproval { request_id, paused }) => {
                    SetupStatus::AwaitingApproval { request_id, paused }
                }
                Err(error) => {
                    tracing::warn!(code = %error.code(), "openclaw pairing failed");
                    SetupStatus::Failed { code: error.code() }
                }
            };
            let finished = matches!(status, SetupStatus::Failed { .. });
            if weak.update(cx, |v, cx| v.set_status(status, cx)).is_err() || finished {
                return;
            }
            // Wait for the next attempt. A paused approval only resumes when
            // the user clicks Retry (status flips back to Connecting).
            loop {
                cx.background_executor()
                    .timer(openclaw::PAIRING_RETRY)
                    .await;
                let Ok(status) = weak.read_with(cx, |v, _| v.status().clone()) else {
                    return;
                };
                match status {
                    SetupStatus::AwaitingApproval { paused: true, .. } => continue,
                    SetupStatus::AwaitingApproval { .. } | SetupStatus::Connecting => break,
                    SetupStatus::Idle | SetupStatus::Failed { .. } => return,
                }
            }
        }
    })
    .detach();
}

/// Persist the OpenClaw section and refresh the views that show it.
pub(crate) fn save_config(openclaw: Option<OpenClawConfig>, state: &AppState, cx: &mut App) {
    let summary = openclaw
        .as_ref()
        .map(|oc| (oc.nickname.clone(), oc.url.clone()));
    {
        let mut c = state.config.borrow_mut();
        c.openclaw = openclaw;
        if let Err(e) = c.save() {
            tracing::warn!("save config: {e:#}");
        }
    }
    let row = ready_row(&state.config.borrow());
    if let Some(entity) = state.current.borrow().as_ref().map(|s| s.entity.clone()) {
        entity.update(cx, |v, cx| v.set_openclaw(row, cx));
    }
    if let Some(entity) = state.settings_entity.borrow().clone() {
        entity.update(cx, |v, cx| v.set_openclaw(summary, cx));
    }
}

/// Settings → Disconnect: forget the device token and the config section.
pub(crate) fn disconnect(state: &AppState, cx: &mut App) {
    let url = state
        .config
        .borrow()
        .openclaw
        .as_ref()
        .map(|oc| oc.url.clone());
    if let Some(base) = url.and_then(|u| openclaw::parse_gateway_url(&u).ok()) {
        if let Err(e) = openclaw::forget(&base) {
            tracing::warn!("openclaw forget: {e:#}");
        }
    }
    if let Err(e) = openclaw::store_avatar(None) {
        tracing::warn!("remove openclaw avatar: {e:#}");
    }
    save_config(None, state, cx);
}
