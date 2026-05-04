//! Unified hotkey service: routes between Carbon (`MacHotkeyService`) and the
//! HID tap (`HotkeyTapService`) depending on whether the bound spec collides
//! with a system shortcut and whether Input Monitoring is granted.
//!
//! Carbon registration "succeeds" for combos like Cmd+Space but never fires
//! because Spotlight wins the dispatch race; only the HID tap can intercept.
//! Tap requires Input Monitoring permission, so for non-conflicting combos
//! we keep the Carbon path to avoid demanding the permission unnecessarily.
//!
//! The wrapper owns a stable bridge channel (`tx_out`/`rx_out`) so that
//! `reregister()` can swap the underlying impl without invalidating the
//! receiver held by the consumer in `main.rs`.

use std::sync::Mutex;
use std::thread;

use anyhow::{anyhow, Result};
use async_channel::{unbounded, Receiver, Sender};
use switcheur_core::HotkeySpec;

use crate::macos::hotkey::MacHotkeyService;
use crate::macos::hotkey_tap::{is_system_reserved, HotkeyTapService};
use crate::HotkeyEvent;

enum Inner {
    Carbon(MacHotkeyService),
    Tap(HotkeyTapService),
}

pub struct HotkeyService {
    inner: Mutex<Inner>,
    tx_out: Sender<HotkeyEvent>,
    rx_out: Receiver<HotkeyEvent>,
}

impl HotkeyService {
    /// Build a service for `spec`. When `spec` is system-reserved AND Input
    /// Monitoring is granted, installs a `HotkeyTapService`; otherwise falls
    /// back to Carbon (which won't fire for reserved combos but stays a
    /// valid no-op so the rest of the app keeps working).
    pub fn start(spec: &HotkeySpec, im_granted: bool) -> Result<Self> {
        let (tx_out, rx_out) = unbounded::<HotkeyEvent>();
        let inner = build_inner(spec, im_granted)?;
        spawn_bridge(&inner, tx_out.clone());
        Ok(Self {
            inner: Mutex::new(inner),
            tx_out,
            rx_out,
        })
    }

    pub fn receiver(&self) -> Receiver<HotkeyEvent> {
        self.rx_out.clone()
    }

    /// Synthesize a hotkey press. Used by `--open` on startup, `on_reopen`,
    /// and the manual cold-launch branch in main.
    pub fn trigger(&self) {
        let _ = self.tx_out.send_blocking(HotkeyEvent::Pressed);
    }

    /// Swap the bound hotkey. May change the underlying variant
    /// (Carbon ↔ Tap) based on the new spec + IM grant state. Tears down
    /// the previous inner before installing the new one; the bridge thread
    /// of the old inner exits naturally when its receiver closes.
    pub fn reregister(&self, spec: &HotkeySpec, im_granted: bool) -> Result<()> {
        let new_inner = build_inner(spec, im_granted)?;
        spawn_bridge(&new_inner, self.tx_out.clone());
        let mut guard = self.inner.lock().unwrap();
        *guard = new_inner;
        Ok(())
    }

    /// True iff the current implementation is the HID tap (i.e. we're
    /// actively intercepting a system-reserved combo). Used by the host to
    /// decide whether a missing Input Monitoring grant is a real problem.
    pub fn is_tap(&self) -> bool {
        matches!(*self.inner.lock().unwrap(), Inner::Tap(_))
    }
}

fn build_inner(spec: &HotkeySpec, im_granted: bool) -> Result<Inner> {
    if is_system_reserved(spec) && im_granted {
        match HotkeyTapService::start(spec) {
            Ok(svc) => return Ok(Inner::Tap(svc)),
            Err(e) => {
                tracing::warn!(
                    "HotkeyTapService failed for reserved spec, falling back to Carbon: {e}"
                );
            }
        }
    }
    let carbon = MacHotkeyService::register(spec)
        .map_err(|e| anyhow!("MacHotkeyService::register: {e}"))?;
    Ok(Inner::Carbon(carbon))
}

fn spawn_bridge(inner: &Inner, tx_out: Sender<HotkeyEvent>) {
    let rx_in = match inner {
        Inner::Carbon(svc) => svc.receiver(),
        Inner::Tap(svc) => svc.receiver(),
    };
    thread::Builder::new()
        .name("leswitcheur-hotkey-bridge".into())
        .spawn(move || {
            while let Ok(ev) = rx_in.recv_blocking() {
                if tx_out.send_blocking(ev).is_err() {
                    break;
                }
            }
        })
        .expect("spawn hotkey bridge thread");
}
