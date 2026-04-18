//! Thin wrapper over the `global-hotkey` crate so callers talk in terms of our
//! [`HotkeySpec`] and receive [`HotkeyEvent`]s on an async channel.
//!
//! Why async-channel: we want the main GPUI task to `.await` the next event so
//! the app uses zero CPU when idle. A polling loop on a sync `crossbeam` channel
//! would force the executor awake every poll interval.

use std::sync::Mutex;
use std::thread;

use anyhow::{anyhow, Context, Result};
use async_channel::{unbounded, Receiver, Sender};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
use switcheur_core::HotkeySpec;

use crate::HotkeyEvent;

/// Owns the `GlobalHotKeyManager` (keeps it alive) and exposes a channel
/// that fires a [`HotkeyEvent::Pressed`] each time the user triggers the hotkey.
pub struct MacHotkeyService {
    manager: GlobalHotKeyManager,
    /// The currently registered hotkey. `reregister()` replaces it.
    current: Mutex<Option<HotKey>>,
    /// Shared with the background thread so it knows which id to accept.
    active_id: std::sync::Arc<std::sync::atomic::AtomicU32>,
    rx: Receiver<HotkeyEvent>,
    tx: Sender<HotkeyEvent>,
}

impl MacHotkeyService {
    pub fn register(spec: &HotkeySpec) -> Result<Self> {
        let manager = GlobalHotKeyManager::new()
            .map_err(|e| anyhow!("GlobalHotKeyManager::new: {e}"))?;
        let hotkey = build_hotkey(spec)?;
        let hk_id = hotkey.id();
        tracing::info!(
            id = hk_id,
            modifiers = ?spec.modifiers,
            key = %spec.key,
            "registering global hotkey"
        );
        manager
            .register(hotkey)
            .map_err(|e| anyhow!("register hotkey: {e}"))?;

        let (tx, rx) = unbounded();
        let active_id = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(hk_id));

        // `global-hotkey` delivers events on its own global channel; we block
        // on it from a dedicated background thread and bridge each event onto
        // our async channel so the main task can `.await` without polling.
        // The `active_id` atomic lets `reregister()` swap the hotkey without
        // restarting the thread.
        let tx_thread = tx.clone();
        let active_id_thread = active_id.clone();
        thread::Builder::new()
            .name("leswitcheur-hotkey".into())
            .spawn(move || {
                let receiver = GlobalHotKeyEvent::receiver();
                loop {
                    match receiver.recv() {
                        Ok(ev) => {
                            tracing::debug!(?ev, "hotkey event received");
                            let want = active_id_thread.load(std::sync::atomic::Ordering::Relaxed);
                            if ev.id == want
                                && matches!(ev.state, global_hotkey::HotKeyState::Pressed)
                            {
                                let _ = tx_thread.send_blocking(HotkeyEvent::Pressed);
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .context("spawn hotkey thread")?;

        Ok(Self {
            manager,
            current: Mutex::new(Some(hotkey)),
            active_id,
            rx,
            tx,
        })
    }

    pub fn receiver(&self) -> Receiver<HotkeyEvent> {
        self.rx.clone()
    }

    /// Synthesize a hotkey press. Useful for `--open` on startup and tests.
    pub fn trigger(&self) {
        let _ = self.tx.send_blocking(HotkeyEvent::Pressed);
    }

    /// Swap the current hotkey for a new one. Returns an error if the new
    /// spec is invalid or conflicts with an existing system-wide binding;
    /// in that case the previous hotkey remains active.
    pub fn reregister(&self, spec: &HotkeySpec) -> Result<()> {
        let new = build_hotkey(spec)?;
        let new_id = new.id();
        let mut guard = self.current.lock().unwrap();

        self.manager
            .register(new)
            .map_err(|e| anyhow!("register hotkey: {e}"))?;
        // Flip the id the thread listens for before unregistering the old one,
        // so there is no window where neither is active.
        self.active_id
            .store(new_id, std::sync::atomic::Ordering::Relaxed);

        if let Some(old) = guard.take() {
            if let Err(e) = self.manager.unregister(old) {
                tracing::warn!("unregister old hotkey: {e}");
            }
        }
        *guard = Some(new);

        tracing::info!(
            id = new_id,
            modifiers = ?spec.modifiers,
            key = %spec.key,
            "reregistered global hotkey"
        );
        Ok(())
    }
}

fn build_hotkey(spec: &HotkeySpec) -> Result<HotKey> {
    let mut mods = Modifiers::empty();
    for m in &spec.modifiers {
        match m.to_ascii_lowercase().as_str() {
            "cmd" | "command" | "super" | "meta" => mods |= Modifiers::SUPER,
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "alt" | "opt" | "option" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            other => anyhow::bail!("unknown modifier: {other}"),
        }
    }

    let code = match spec.key.to_ascii_lowercase().as_str() {
        "space" => Code::Space,
        "tab" => Code::Tab,
        "escape" | "esc" => Code::Escape,
        "return" | "enter" => Code::Enter,
        "=" | "equal" | "equals" => Code::Equal,
        "-" | "minus" => Code::Minus,
        "[" | "leftbracket" => Code::BracketLeft,
        "]" | "rightbracket" => Code::BracketRight,
        "\\" | "backslash" => Code::Backslash,
        ";" | "semicolon" => Code::Semicolon,
        "'" | "quote" => Code::Quote,
        "`" | "backtick" | "grave" => Code::Backquote,
        "," | "comma" => Code::Comma,
        "." | "period" => Code::Period,
        "/" | "slash" => Code::Slash,
        k if k.len() == 1 && k.chars().next().unwrap().is_ascii_alphabetic() => {
            let c = k.chars().next().unwrap().to_ascii_uppercase();
            match c {
                'A' => Code::KeyA, 'B' => Code::KeyB, 'C' => Code::KeyC, 'D' => Code::KeyD,
                'E' => Code::KeyE, 'F' => Code::KeyF, 'G' => Code::KeyG, 'H' => Code::KeyH,
                'I' => Code::KeyI, 'J' => Code::KeyJ, 'K' => Code::KeyK, 'L' => Code::KeyL,
                'M' => Code::KeyM, 'N' => Code::KeyN, 'O' => Code::KeyO, 'P' => Code::KeyP,
                'Q' => Code::KeyQ, 'R' => Code::KeyR, 'S' => Code::KeyS, 'T' => Code::KeyT,
                'U' => Code::KeyU, 'V' => Code::KeyV, 'W' => Code::KeyW, 'X' => Code::KeyX,
                'Y' => Code::KeyY, 'Z' => Code::KeyZ,
                _ => unreachable!(),
            }
        }
        k if k.len() == 1 && k.chars().next().unwrap().is_ascii_digit() => {
            match k.chars().next().unwrap() {
                '0' => Code::Digit0, '1' => Code::Digit1, '2' => Code::Digit2,
                '3' => Code::Digit3, '4' => Code::Digit4, '5' => Code::Digit5,
                '6' => Code::Digit6, '7' => Code::Digit7, '8' => Code::Digit8,
                '9' => Code::Digit9,
                _ => unreachable!(),
            }
        }
        other => anyhow::bail!("unsupported key: {other}"),
    };

    Ok(HotKey::new(Some(mods), code))
}
