//! Host side of the Linear integration: issue lookup and creation from the
//! switcher panel, account management from the settings.
//!
//! API keys and issue titles are never logged; only content-free error
//! codes are.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext, AsyncApp, Entity};
use switcheur_core::{Item, LinearIssueRef};
use switcheur_platform::linear::{self, Account, LinearError, Store, TeamChoice};
use switcheur_ui::open_with_popover::OpenWithEntry;
use switcheur_ui::settings_view::{LinearAccountRow, SettingsView};
use switcheur_ui::switcher_view::{SwitcherView, SwitcherViewEvent};

use crate::AppState;

/// Typing pause before an identifier is looked up.
const LOOKUP_DEBOUNCE: Duration = Duration::from_millis(250);
/// How long the settings wait for the browser consent.
const OAUTH_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Write back accounts whose OAuth tokens were refreshed (refresh tokens
/// rotate, so losing one logs the workspace out). Removed accounts stay
/// removed.
fn store_refreshed(state: &AppState, refreshed: Vec<Account>) {
    if refreshed.is_empty() {
        return;
    }
    let mut store = state.linear.borrow_mut();
    for account in refreshed {
        if store.accounts.iter().any(|a| a.id == account.id) {
            store.upsert(account);
        }
    }
    if let Err(e) = store.save() {
        tracing::warn!("save linear store: {e:#}");
    }
}

/// What the switcher needs at open time. Also snapshots the team choices
/// so popover indices stay valid for the whole panel session.
pub(crate) fn configure_view(state: &AppState, view: &mut SwitcherView, cx: &mut gpui::Context<SwitcherView>) {
    let store = state.linear.borrow();
    let choices = store.team_choices();
    let several_workspaces = store.accounts.len() > 1;
    let mut keys: Vec<String> = choices.iter().map(|c| c.team.key.clone()).collect();
    keys.sort();
    keys.dedup();
    let default_label = choices.first().map(|c| short_label(c, several_workspaces));
    let entries = choices
        .iter()
        .enumerate()
        .map(|(i, c)| OpenWithEntry {
            id: c.team.id.clone(),
            display_name: long_label(c, several_workspaces),
            bundle_id: String::new(),
            icon_path: None,
            is_default: i == 0,
        })
        .collect();
    let count = choices.len();
    drop(store);
    *state.linear_choices.borrow_mut() = choices;
    *state.linear_entries.borrow_mut() = entries;
    view.set_linear(default_label, keys, count, cx);
}

fn short_label(c: &TeamChoice, several_workspaces: bool) -> String {
    if several_workspaces {
        format!("{} · {}", c.team.key, c.workspace)
    } else {
        c.team.key.clone()
    }
}

fn long_label(c: &TeamChoice, several_workspaces: bool) -> String {
    if several_workspaces {
        format!("{} · {} ({})", c.team.key, c.team.name, c.workspace)
    } else {
        format!("{} · {}", c.team.key, c.team.name)
    }
}

/// `SwitcherViewEvent::NeedsLinearIssue`: debounced lookup in every
/// workspace that has a team with that key.
pub(crate) fn lookup(identifier: String, state: &AppState, cx: &mut App) {
    let Some(entity) = state.current.borrow().as_ref().map(|s| s.entity.clone()) else {
        return;
    };
    let Some((key, number)) = switcheur_core::linear::parse_identifier(&identifier) else {
        return;
    };
    let accounts: Vec<Account> = state
        .linear
        .borrow()
        .accounts_with_team_key(&key)
        .into_iter()
        .cloned()
        .collect();
    if accounts.is_empty() {
        return;
    }
    let generation = state.linear_lookup_gen.get() + 1;
    state.linear_lookup_gen.set(generation);
    let gen_cell = state.linear_lookup_gen.clone();
    let weak = entity.downgrade();
    let state = state.clone();
    cx.spawn(async move |cx: &mut AsyncApp| {
        cx.background_executor().timer(LOOKUP_DEBOUNCE).await;
        if gen_cell.get() != generation {
            return;
        }
        let (found, refreshed) = cx
            .background_executor()
            .spawn(async move {
                let mut refreshed = Vec::new();
                for account in accounts {
                    let result = linear::authorize(&account).and_then(|auth| {
                        refreshed.extend(auth.refreshed);
                        linear::find_issue(&auth.header, &key, number)
                    });
                    match result {
                        Ok(Some(issue)) => return (Some((issue, account.name)), refreshed),
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(code = %error.code(), "linear issue lookup failed")
                        }
                    }
                }
                (None, refreshed)
            })
            .await;
        cx.update(|_| store_refreshed(&state, refreshed));
        let item = found.map(|(issue, workspace)| {
            Item::LinearIssue(Arc::new(LinearIssueRef::new(
                issue.identifier,
                issue.title,
                issue.url,
                issue.state,
                &workspace,
            )))
        });
        let _ = weak.update(cx, |view, cx| view.set_linear_issue(identifier, item, cx));
    })
    .detach();
}

/// `SwitcherViewEvent::CreateLinearIssue`: create off the UI thread, then
/// open the new issue and close the panel, or report the failure in it.
pub(crate) fn create(title: String, team: usize, state: &AppState, cx: &mut App) {
    let Some(entity) = state.current.borrow().as_ref().map(|s| s.entity.clone()) else {
        return;
    };
    let Some(choice) = state.linear_choices.borrow().get(team).cloned() else {
        entity.update(cx, |view, cx| {
            view.linear_issue_failed(failure_message(&LinearError::Api("NO_TEAM".into())), cx)
        });
        return;
    };
    let state = state.clone();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let team_id = choice.team.id.clone();
        let (result, refreshed) = cx
            .background_executor()
            .spawn(async move {
                let mut refreshed = None;
                let result = linear::authorize(&choice.account).and_then(|auth| {
                    refreshed = auth.refreshed;
                    linear::create_issue(&auth.header, &choice.team.id, &title)
                });
                (result, refreshed)
            })
            .await;
        cx.update(|cx| {
            store_refreshed(&state, refreshed.into_iter().collect());
            let still_open = state
                .current
                .borrow()
                .as_ref()
                .is_some_and(|slot| slot.entity.entity_id() == entity.entity_id());
            match result {
                Ok(issue) => {
                    tracing::info!("linear issue created");
                    {
                        let mut store = state.linear.borrow_mut();
                        store.promote_team(&team_id);
                        if let Err(e) = store.save() {
                            tracing::warn!("save linear store: {e:#}");
                        }
                    }
                    if let Err(e) = state.platform.open_url(&issue.url) {
                        tracing::warn!("open linear issue: {e:#}");
                    }
                    if still_open {
                        crate::handle_view_event(&SwitcherViewEvent::Dismissed, &state, cx);
                    }
                }
                Err(error) => {
                    tracing::warn!(code = %error.code(), "linear issue creation failed");
                    if still_open {
                        entity.update(cx, |view, cx| {
                            view.linear_issue_failed(failure_message(&error), cx)
                        });
                    }
                }
            }
        });
    })
    .detach();
}

fn failure_message(error: &LinearError) -> String {
    switcheur_i18n::tr_sub("linear.create_failed", &[("reason", &error_reason(error))])
}

fn error_reason(error: &LinearError) -> String {
    match error {
        LinearError::Unauthorized => switcheur_i18n::tr("linear.error_unauthorized"),
        LinearError::RateLimited => switcheur_i18n::tr("linear.error_rate_limited"),
        LinearError::Network => switcheur_i18n::tr("linear.error_network"),
        LinearError::Denied => switcheur_i18n::tr("linear.error_denied"),
        LinearError::TimedOut => switcheur_i18n::tr("linear.error_timed_out"),
        LinearError::PortInUse => switcheur_i18n::tr("linear.error_port_in_use"),
        LinearError::Api(code) => code.clone(),
        LinearError::Cancelled => String::new(),
    }
}

/// Re-fetch the teams of workspaces whose cached list is old. Fire and
/// forget; a revoked key keeps its account so the user sees it in settings.
pub(crate) fn refresh_stale_accounts(state: &AppState, cx: &mut App) {
    let stale: Vec<_> = state
        .linear
        .borrow()
        .accounts
        .iter()
        .filter(|a| a.teams_stale())
        .cloned()
        .collect();
    if stale.is_empty() || state.linear_refreshing.replace(true) {
        return;
    }
    let state = state.clone();
    cx.spawn(async move |cx: &mut AsyncApp| {
        for account in stale {
            let (result, refreshed) = cx
                .background_executor()
                .spawn({
                    let account = account.clone();
                    async move {
                        match linear::authorize(&account) {
                            Ok(auth) => {
                                let current = auth.refreshed.clone().unwrap_or(account);
                                (linear::refresh(&current), auth.refreshed)
                            }
                            Err(error) => (Err(error), None),
                        }
                    }
                })
                .await;
            store_refreshed(&state, refreshed.into_iter().collect());
            let mut store = state.linear.borrow_mut();
            match result {
                Ok(fresh) => {
                    // Skip if removed meanwhile.
                    if store.accounts.iter().any(|a| a.id == fresh.id) {
                        store.upsert(fresh);
                    }
                }
                Err(error) => {
                    tracing::warn!(code = %error.code(), "linear teams refresh failed");
                    continue;
                }
            }
            if let Err(e) = store.save() {
                tracing::warn!("save linear store: {e:#}");
            }
        }
        state.linear_refreshing.set(false);
    })
    .detach();
}

pub(crate) fn account_rows(store: &Store) -> Vec<LinearAccountRow> {
    store
        .accounts
        .iter()
        .map(|a| LinearAccountRow {
            id: a.id.clone().into(),
            label: if a.user.is_empty() {
                a.name.clone().into()
            } else {
                format!("{} · {}", a.name, a.user).into()
            },
        })
        .collect()
}

/// `SettingsViewEvent::LinearConnect`: personal API key.
pub(crate) fn connect(key: String, state: &AppState, settings: Entity<SettingsView>, cx: &mut App) {
    finish_connect(state, settings, cx, move || linear::connect(&key));
}

/// `SettingsViewEvent::LinearOAuthStart`: open the consent page, wait for
/// the loopback redirect off the UI thread.
pub(crate) fn start_oauth(state: &AppState, settings: Entity<SettingsView>, cx: &mut App) {
    cancel_oauth(state);
    let flow = match linear::begin_oauth() {
        Ok(flow) => flow,
        Err(error) => {
            tracing::warn!(code = %error.code(), "linear oauth start failed");
            let message = connect_failure(&error);
            settings.update(cx, |v, cx| v.linear_connect_failed(message, cx));
            return;
        }
    };
    if let Err(e) = state.platform.open_url(&flow.authorize_url) {
        tracing::warn!("open linear consent page: {e:#}");
    }
    let cancel = Arc::new(AtomicBool::new(false));
    *state.linear_oauth_cancel.borrow_mut() = Some(cancel.clone());
    let page = done_page();
    finish_connect(state, settings, cx, move || {
        linear::finish_oauth(flow, &cancel, OAUTH_TIMEOUT, &page)
    });
}

pub(crate) fn cancel_oauth(state: &AppState) {
    if let Some(cancel) = state.linear_oauth_cancel.borrow_mut().take() {
        cancel.store(true, Ordering::Relaxed);
    }
}

fn done_page() -> String {
    let title = switcheur_i18n::tr("linear.oauth_done_title");
    let body = switcheur_i18n::tr("linear.oauth_done_body");
    format!(
        "<!doctype html><meta charset=utf-8><title>LeSwitcheur</title>\
         <body style=\"font:16px -apple-system,system-ui,sans-serif;text-align:center;margin-top:20vh\">\
         <h2>{title}</h2><p>{body}</p></body>"
    )
}

fn connect_failure(error: &LinearError) -> Option<String> {
    (*error != LinearError::Cancelled).then(|| {
        switcheur_i18n::tr_sub("linear.connect_failed", &[("reason", &error_reason(error))])
    })
}

fn finish_connect(
    state: &AppState,
    settings: Entity<SettingsView>,
    cx: &mut App,
    work: impl FnOnce() -> Result<Account, LinearError> + Send + 'static,
) {
    let state = state.clone();
    let Some(handle) = state.settings.borrow().as_ref().map(|s| s.handle) else {
        return;
    };
    cx.spawn(async move |cx: &mut AsyncApp| {
        let result = cx.background_executor().spawn(async move { work() }).await;
        // Saved even if the settings window was closed meanwhile.
        let rows = cx.update(|_| {
            let account = result.as_ref().ok()?.clone();
            tracing::info!(teams = account.teams.len(), "linear workspace connected");
            let mut store = state.linear.borrow_mut();
            store.upsert(account);
            if let Err(e) = store.save() {
                tracing::warn!("save linear store: {e:#}");
            }
            Some(account_rows(&store))
        });
        let _ = cx.update_window(handle, |_, window, cx| match (result, rows) {
            (Ok(_), Some(rows)) => {
                settings.update(cx, |v, cx| v.linear_connected(rows, window, cx));
                // Back from the browser.
                cx.activate(true);
                window.activate_window();
            }
            (Ok(_), None) => {}
            (Err(error), _) => {
                tracing::warn!(code = %error.code(), "linear connect failed");
                let message = connect_failure(&error);
                settings.update(cx, |v, cx| v.linear_connect_failed(message, cx));
            }
        });
    })
    .detach();
}

/// `SettingsViewEvent::LinearRemove`.
pub(crate) fn remove(account_id: &str, state: &AppState, settings: &Entity<SettingsView>, cx: &mut App) {
    let removed = state
        .linear
        .borrow()
        .accounts
        .iter()
        .find(|a| a.id == account_id)
        .cloned();
    if let Some(account) = removed {
        cx.background_executor()
            .spawn(async move { linear::revoke(&account) })
            .detach();
    }
    let rows = {
        let mut store = state.linear.borrow_mut();
        store.remove(account_id);
        if let Err(e) = store.save() {
            tracing::warn!("save linear store: {e:#}");
        }
        account_rows(&store)
    };
    settings.update(cx, |v, cx| v.set_linear_accounts(rows, cx));
}
