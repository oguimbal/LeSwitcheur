//! Global hotkey on Windows via the cross-platform `global-hotkey` crate.
//!
//! This is the rough equivalent of `macos/hotkey_service.rs`, but without
//! the HID-tap fallback path: Windows doesn't have an equivalent to the
//! "system reserved combo" problem that drives the macOS HotkeyTap, so the
//! single Carbon-style registration path is enough.
//!
//! Surface mirrors `macos::HotkeyService` so `main.rs` can consume it
//! through the cfg-aliased `crate::HotkeyService` re-export with no diff.

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

pub struct HotkeyService {
    manager: Mutex<GlobalHotKeyManager>,
    current: Mutex<Option<HotKey>>,
    active_id: std::sync::Arc<std::sync::atomic::AtomicU32>,
    rx: Receiver<HotkeyEvent>,
    tx: Sender<HotkeyEvent>,
}

impl HotkeyService {
    /// Build a service for `spec`. The `_im_granted` flag is accepted for
    /// API parity with the macOS impl but has no meaning on Windows.
    pub fn start(spec: &HotkeySpec, _im_granted: bool) -> Result<Self> {
        let manager =
            GlobalHotKeyManager::new().map_err(|e| anyhow!("GlobalHotKeyManager::new: {e}"))?;
        let hotkey = build_hotkey(spec)?;
        let hk_id = hotkey.id();
        tracing::info!(
            id = hk_id,
            modifiers = ?spec.modifiers,
            key = %spec.key,
            "registering global hotkey (windows)"
        );
        manager
            .register(hotkey)
            .map_err(|e| anyhow!("register hotkey: {e}"))?;

        let (tx, rx) = unbounded();
        let active_id = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(hk_id));

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
                            let want = active_id_thread
                                .load(std::sync::atomic::Ordering::Relaxed);
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
            manager: Mutex::new(manager),
            current: Mutex::new(Some(hotkey)),
            active_id,
            rx,
            tx,
        })
    }

    pub fn receiver(&self) -> Receiver<HotkeyEvent> {
        self.rx.clone()
    }

    /// Sender into the same `HotkeyEvent` channel as `trigger`. Lets
    /// callers (e.g. the tray-icon drain thread) trigger the switcher
    /// without holding the whole `HotkeyService` — which embeds a
    /// `GlobalHotKeyManager` whose internal HWND makes it `!Send` on
    /// Windows.
    pub fn sender(&self) -> Sender<HotkeyEvent> {
        self.tx.clone()
    }

    pub fn trigger(&self) {
        let _ = self.tx.send_blocking(HotkeyEvent::Pressed);
    }

    /// Swap the bound hotkey at runtime.
    pub fn reregister(&self, spec: &HotkeySpec, _im_granted: bool) -> Result<()> {
        let new = build_hotkey(spec)?;
        let new_id = new.id();
        let manager = self.manager.lock().unwrap();
        let mut guard = self.current.lock().unwrap();
        manager
            .register(new)
            .map_err(|e| anyhow!("register hotkey: {e}"))?;
        self.active_id
            .store(new_id, std::sync::atomic::Ordering::Relaxed);
        if let Some(old) = guard.take() {
            if let Err(e) = manager.unregister(old) {
                tracing::warn!("unregister old hotkey: {e}");
            }
        }
        *guard = Some(new);
        Ok(())
    }

    /// True when the underlying impl is the HID tap. Always `false` on
    /// Windows; here for API parity with the macOS service.
    pub fn is_tap(&self) -> bool {
        false
    }
}

fn build_hotkey(spec: &HotkeySpec) -> Result<HotKey> {
    let mut mods = Modifiers::empty();
    for m in &spec.modifiers {
        match m.to_ascii_lowercase().as_str() {
            "cmd" | "command" | "super" | "meta" | "win" => mods |= Modifiers::SUPER,
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
