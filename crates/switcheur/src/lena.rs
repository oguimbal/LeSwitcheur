//! Host side of the Lena integration: file/folder hits in the dirs pane and
//! the "Ask Lena AI" row. Lena is probed once per panel; queries, file names
//! and Lena error messages are never logged, only error codes.

use std::io;
use std::sync::Arc;

use gpui::{App, AsyncApp, Entity};
use switcheur_core::{DirRef, Item, LenaTarget, LlmProvider};
use switcheur_platform::lena::{self, Info};
use switcheur_platform::local_sources::is_disconnection;
use switcheur_ui::switcher_view::{SwitcherView, SwitcherViewEvent};

use crate::AppState;

/// Same row budget as the local dir source.
pub const HITS: u32 = 8;

pub enum Pick {
    Open { target: LenaTarget, reveal: bool },
    Ask(String),
}

fn code(error: &io::Error) -> &str {
    lena::remote_code(error).unwrap_or("transport")
}

/// Signed-in Lena from the last probe, if any.
pub fn ready(state: &AppState) -> Option<Info> {
    state.lena.borrow().clone()
}

pub fn apply(
    view: &mut SwitcherView,
    info: Option<&Info>,
    has_local_dirs: bool,
    cx: &mut gpui::Context<SwitcherView>,
) {
    view.set_lena_ask(info.is_some_and(|i| i.can("ask")), cx);
    view.set_dirs_enabled(has_local_dirs || info.is_some_and(|i| i.can("search")), cx);
}

fn set_ready(state: &AppState, entity: &Entity<SwitcherView>, info: Option<Info>, cx: &mut App) {
    let has_local_dirs = state.dir_source.borrow().is_some();
    *state.lena.borrow_mut() = info.clone();
    entity.update(cx, |view, cx| {
        apply(view, info.as_ref(), has_local_dirs, cx)
    });
}

/// Refresh the cached `info` in the background; the panel was built from the
/// previous answer.
pub fn probe(state: &AppState, entity: &Entity<SwitcherView>, cx: &mut App) {
    let state = state.clone();
    let entity = entity.clone();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let result = cx.background_executor().spawn(async { lena::info() }).await;
        let info = match result {
            Ok(info) => Some(info).filter(|i| i.signed_in),
            Err(error) => {
                if !is_disconnection(&error) {
                    tracing::debug!(code = code(&error), "lena info failed");
                }
                None
            }
        };
        cx.update(|cx| {
            let current = state
                .current
                .borrow()
                .as_ref()
                .map(|slot| slot.entity.clone());
            if current.is_some_and(|e| e.entity_id() == entity.entity_id()) {
                set_ready(&state, &entity, info, cx);
            } else {
                *state.lena.borrow_mut() = info;
            }
        });
    })
    .detach();
}

/// Lena rows plus the hits whose thumbnail is still to fetch.
pub struct Hits {
    pub items: Vec<Item>,
    pub thumbnails: Vec<LenaTarget>,
}

/// Blocking; run off the UI thread. `thumbnails`: Lena advertises them.
pub fn search(query: &str, thumbnails: bool) -> io::Result<Hits> {
    let (instance, hits) = lena::search(query, HITS)?;
    let mut missing = Vec::new();
    let items = hits
        .into_iter()
        .map(|hit| {
            let cached = hit.thumbnail.then(|| lena::cached_thumbnail(&hit.id)).flatten();
            let icon = cached.clone().or_else(|| match &hit.path {
                Some(local) => switcheur_platform::icon_for_path(local, hit.is_folder),
                None => switcheur_platform::icon_for_name(
                    std::path::Path::new(&hit.name),
                    hit.is_folder,
                ),
            });
            let target = LenaTarget {
                instance: instance.clone(),
                id: hit.id,
                local: hit.path,
            };
            if thumbnails && hit.thumbnail && cached.is_none() {
                missing.push(target.clone());
            }
            Item::Dir(Arc::new(DirRef::lena(
                target,
                hit.name,
                hit.folder,
                hit.is_folder,
                icon,
            )))
        })
        .collect();
    Ok(Hits {
        items,
        thumbnails: missing,
    })
}

/// Fetch thumbnails one at a time (Lena serves two requests at once) while
/// `current()` holds; each one replaces its row's type icon. `none` is
/// common: the type allows a thumbnail the server never generated.
pub async fn fetch_thumbnails(
    targets: Vec<LenaTarget>,
    entity: gpui::WeakEntity<SwitcherView>,
    current: impl Fn() -> bool,
    cx: &mut AsyncApp,
) {
    for target in targets {
        if !current() {
            return;
        }
        let LenaTarget { instance, id, .. } = target;
        let result = cx
            .background_executor()
            .spawn({
                let id = id.clone();
                async move { lena::thumbnail(&instance, &id) }
            })
            .await;
        match result {
            Ok(path) => {
                if !current()
                    || entity
                        .update(cx, |view, cx| view.set_lena_icon(&id, path, cx))
                        .is_err()
                {
                    return;
                }
            }
            Err(error) if is_disconnection(&error) => return,
            Err(error) => {
                if !matches!(code(&error), "none" | "stale") {
                    tracing::debug!(code = code(&error), "lena thumbnail failed");
                }
            }
        }
    }
}

/// A search failed: forget a signed-out Lena, stay quiet when it is gone.
pub fn search_failed(
    error: &io::Error,
    state: &AppState,
    entity: &Entity<SwitcherView>,
    cx: &mut App,
) {
    match code(error) {
        "signed_out" => set_ready(state, entity, None, cx),
        _ if is_disconnection(error) => set_ready(state, entity, None, cx),
        "offline" | "failed" => {}
        other => tracing::debug!(code = other, "lena search failed"),
    }
}

/// Hand a pick to Lena. The panel stays open until Lena acknowledges, so a
/// failure can be shown; Lena brings itself to the front.
pub fn confirm(pick: Pick, state: &AppState, cx: &mut App) {
    let entity = {
        let mut current = state.current.borrow_mut();
        let Some(slot) = current.as_mut() else {
            return;
        };
        if slot.local_focus_pending {
            return;
        }
        slot.local_focus_pending = true;
        slot.entity.clone()
    };
    if let Pick::Ask(_) = pick {
        let mut cfg = state.config.borrow_mut();
        cfg.promote_llm_provider(LlmProvider::Lena);
        if let Err(e) = cfg.save() {
            tracing::warn!("save config after llm promote: {e:#}");
        }
    }
    lena::hand_over_focus(ready(state).and_then(|i| i.pid));
    let state = state.clone();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let result = cx
            .background_executor()
            .spawn(async move {
                match pick {
                    Pick::Open { target, reveal } => {
                        lena::open(&target.instance, &target.id, reveal)
                    }
                    Pick::Ask(question) => lena::ask(&question),
                }
            })
            .await;
        cx.update(|cx| {
            let is_original_panel = state
                .current
                .borrow()
                .as_ref()
                .is_some_and(|slot| slot.entity.entity_id() == entity.entity_id());
            if !is_original_panel {
                return;
            }
            match result {
                Ok(()) => crate::handle_view_event(&SwitcherViewEvent::Dismissed, &state, cx),
                Err(error) => {
                    tracing::warn!(code = code(&error), "lena pick failed");
                    if let Some(slot) = state.current.borrow_mut().as_mut() {
                        slot.local_focus_pending = false;
                    }
                    let stale = code(&error) == "stale";
                    entity.update(cx, |view, cx| view.lena_failed(stale, cx));
                }
            }
        });
    })
    .detach();
}
