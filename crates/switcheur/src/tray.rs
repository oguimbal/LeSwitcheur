//! Cross-platform tray / menu-bar icon.
//!
//! Renders a `tray-icon` entry whose left-click triggers the global hotkey
//! (same affordance as Ctrl+= or `open -a LeSwitcheur`) and whose right-click
//! menu offers Settings + Quit. Because LeSwitcheur is `LSUIElement` on macOS
//! and `windows_subsystem = "windows"` on Windows, the tray is the only
//! visible UI affordance once the user dismisses onboarding.
//!
//! Lifecycle: the `Tray` value returned by [`install`] owns the underlying
//! `TrayIcon` plus the menu items it points at. Drop it to remove the icon
//! from the system tray. Use `set_visible` for transient hide/show without
//! tearing down the entry.

use anyhow::{anyhow, Context, Result};
use async_channel::Sender as AsyncSender;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use switcheur_platform::HotkeyEvent;

const LOGO_PNG: &[u8] = include_bytes!("../../../brand/logo-256.png");

/// Commands the tray needs the main app to execute. Click-to-open-switcher
/// is handled inline in the drain loop (just calls `HotkeyService::trigger`),
/// so it doesn't appear here.
#[derive(Debug, Clone, Copy)]
pub enum TrayCommand {
    OpenSettings,
    Quit,
}

/// RAII handle for the tray icon. Drop hides the icon and stops the drain
/// thread. Keep it alive in `AppState`.
pub struct Tray {
    icon: TrayIcon,
    // Menu items must outlive the TrayIcon — their MenuIds are referenced by
    // the WM_COMMAND handler and the global MenuEvent dispatch table.
    _items: Vec<MenuItem>,
}

impl Tray {
    pub fn set_visible(&self, visible: bool) -> Result<()> {
        self.icon
            .set_visible(visible)
            .map_err(|e| anyhow!("tray set_visible: {e}"))
    }
}

/// Build the tray icon, wire its events, and start the drain thread.
///
/// `cmd_tx` receives `OpenSettings` / `Quit` from the menu; the caller
/// drains it on the GPUI main thread (where it has `cx` access).
/// Left-click on the icon and the "Open LeSwitcheur" menu item don't go
/// through `cmd_tx` — they push directly onto `hotkey_tx`, the same
/// channel `HotkeyService::trigger` writes to. We can't carry the whole
/// `HotkeyService` here because on Windows `GlobalHotKeyManager` is `!Send`.
pub fn install(
    hotkey_tx: AsyncSender<HotkeyEvent>,
    cmd_tx: async_channel::Sender<TrayCommand>,
) -> Result<Tray> {
    let icon = decode_icon().context("decode tray icon")?;

    let menu = Menu::new();
    let item_open = MenuItem::new("Open LeSwitcheur", true, None);
    let item_settings = MenuItem::new("Settings…", true, None);
    let separator = PredefinedMenuItem::separator();
    let item_quit = MenuItem::new("Quit LeSwitcheur", true, None);

    let id_open = item_open.id().clone();
    let id_settings = item_settings.id().clone();
    let id_quit = item_quit.id().clone();

    menu.append(&item_open).map_err(|e| anyhow!("menu append: {e}"))?;
    menu.append(&item_settings).map_err(|e| anyhow!("menu append: {e}"))?;
    menu.append(&separator).map_err(|e| anyhow!("menu append: {e}"))?;
    menu.append(&item_quit).map_err(|e| anyhow!("menu append: {e}"))?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("LeSwitcheur")
        .with_icon(icon)
        .build()
        .map_err(|e| anyhow!("build tray: {e}"))?;

    spawn_drain_thread(hotkey_tx, cmd_tx, id_open, id_settings, id_quit);

    Ok(Tray {
        icon: tray,
        _items: vec![item_open, item_settings, item_quit],
    })
}

fn spawn_drain_thread(
    hotkey_tx: AsyncSender<HotkeyEvent>,
    cmd_tx: async_channel::Sender<TrayCommand>,
    id_open: MenuId,
    id_settings: MenuId,
    id_quit: MenuId,
) {
    std::thread::Builder::new()
        .name("tray-event-drain".into())
        .spawn(move || {
            let menu_rx = MenuEvent::receiver();
            let tray_rx = TrayIconEvent::receiver();
            loop {
                crossbeam_channel::select! {
                    recv(menu_rx) -> ev => {
                        let Ok(ev) = ev else { return };
                        if ev.id == id_open {
                            let _ = hotkey_tx.send_blocking(HotkeyEvent::Pressed);
                        } else if ev.id == id_settings {
                            let _ = cmd_tx.send_blocking(TrayCommand::OpenSettings);
                        } else if ev.id == id_quit {
                            let _ = cmd_tx.send_blocking(TrayCommand::Quit);
                        }
                    }
                    recv(tray_rx) -> ev => {
                        let Ok(ev) = ev else { return };
                        // Mirror Explorer / Finder behaviour: a left-click-up
                        // on the icon is the "primary" affordance; the
                        // context menu appears on right-click instead.
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = ev
                        {
                            let _ = hotkey_tx.send_blocking(HotkeyEvent::Pressed);
                        }
                    }
                }
            }
        })
        .expect("spawn tray drain thread");
}

fn decode_icon() -> Result<Icon> {
    let img = image::load_from_memory(LOGO_PNG)
        .context("decode brand PNG")?
        .to_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).map_err(|e| anyhow!("Icon::from_rgba: {e}"))
}
