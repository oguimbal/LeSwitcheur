//! Bring a top-level window to the foreground.
//!
//! Win32 enforces a "foreground lock": only the thread that currently owns
//! the foreground window can call `SetForegroundWindow` and have it
//! actually flip focus. The standard workaround used by alt-tab clones
//! (Switcheroo, AltTabbu, etc.) is to attach our input queue to the
//! foreground thread's so we share its foreground rights for the duration
//! of the call. If the target is minimized, restore it first.

use anyhow::{Context, Result};
use switcheur_core::{AppRef, WindowRef};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsIconic, PostMessageW, SetForegroundWindow,
    ShowWindow, SW_RESTORE, WM_CLOSE,
};

use crate::windows::list::list_windows;

pub fn activate_window(w: &WindowRef) -> Result<()> {
    let hwnd = HWND(w.id as *mut _);
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let fg = GetForegroundWindow();
        let mut fg_pid: u32 = 0;
        let fg_thread = GetWindowThreadProcessId(fg, Some(&mut fg_pid));
        let me = GetCurrentThreadId();
        let attached = if fg_thread != 0 && fg_thread != me {
            AttachThreadInput(me, fg_thread, true).as_bool()
        } else {
            false
        };
        let ok = SetForegroundWindow(hwnd).as_bool();
        // Belt-and-braces: focus the window's input queue so keystrokes go
        // to the right control immediately. Ignored when the call fails.
        let _ = SetFocus(Some(hwnd));
        if attached {
            let _ = AttachThreadInput(me, fg_thread, false);
        }
        if !ok {
            anyhow::bail!("SetForegroundWindow rejected (foreground lock)");
        }
    }
    Ok(())
}

pub fn activate_app(a: &AppRef) -> Result<()> {
    let target = list_windows()
        .into_iter()
        .find(|w| w.pid == a.pid)
        .context("no top-level window found for app")?;
    activate_window(&target)
}

pub fn close_window(w: &WindowRef) -> Result<()> {
    let hwnd = HWND(w.id as *mut _);
    unsafe {
        PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0))
            .context("PostMessageW WM_CLOSE failed")?;
    }
    Ok(())
}
