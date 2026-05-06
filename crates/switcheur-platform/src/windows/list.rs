//! Window + app enumeration on Windows.
//!
//! Strategy: `EnumWindows` for the top-level set, then filter per the
//! "alt-tab window" criteria Microsoft documents (visible, non-toolwindow,
//! not cloaked by DWM, no owner). Each surviving HWND becomes a `WindowRef`
//! keyed by the HWND value (cast to u64) — stable for the window's lifetime.

use std::collections::HashMap;

use switcheur_core::{AppRef, WindowRef};
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, MAX_PATH};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE, GW_OWNER, WS_EX_TOOLWINDOW,
};

pub fn list_windows() -> Vec<WindowRef> {
    let mut buf: Vec<WindowRef> = Vec::with_capacity(64);
    let lparam = LPARAM(&mut buf as *mut _ as isize);
    unsafe {
        let _ = EnumWindows(Some(enum_proc), lparam);
    }
    buf
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let buf = unsafe { &mut *(lparam.0 as *mut Vec<WindowRef>) };
    if !is_alttab_window(hwnd) {
        return BOOL(1);
    }
    let title = window_title(hwnd);
    if title.is_empty() {
        return BOOL(1);
    }
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return BOOL(1);
    }
    let app_name = process_name(pid).unwrap_or_else(|| String::from("Unknown"));
    buf.push(WindowRef {
        id: hwnd.0 as u64,
        pid: pid as i32,
        title,
        app_name,
        bundle_id: None,
        icon_path: None,
        minimized: unsafe { IsIconic(hwnd).as_bool() },
    });
    BOOL(1)
}

fn is_alttab_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        // Skip owned windows — only true top-level entries belong in the list.
        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
            if owner.0 as isize != 0 {
                return false;
            }
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if (ex as u32) & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        // DWM cloaks UWP windows that live on a different virtual desktop —
        // skip them so the switcher only shows the current desktop.
        let mut cloaked: u32 = 0;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return false;
        }
    }
    true
}

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let copied = GetWindowTextW(hwnd, &mut buf);
        if copied <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..copied as usize])
    }
}

fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let handle: HANDLE = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )
        .ok()?;
        let mut buf = [0u16; MAX_PATH as usize];
        let len = GetModuleBaseNameW(handle, None, &mut buf);
        let _ = CloseHandle(handle);
        if len == 0 {
            return None;
        }
        let name = String::from_utf16_lossy(&buf[..len as usize]);
        Some(
            name.trim_end_matches(".exe")
                .trim_end_matches(".EXE")
                .to_string(),
        )
    }
}

pub fn list_apps() -> Vec<AppRef> {
    // One AppRef per pid. Without a richer source (e.g. Start menu shortcuts)
    // the running-app list is just the dedup of windows by pid.
    let mut map: HashMap<i32, AppRef> = HashMap::new();
    for w in list_windows() {
        map.entry(w.pid).or_insert_with(|| AppRef {
            pid: w.pid,
            name: w.app_name.clone(),
            bundle_id: None,
            icon_path: None,
        });
    }
    map.into_values().collect()
}

/// Stub for the panel-watch loop: returns an empty set so the watcher
/// never detects a foreign-app activation. Real implementation will land
/// alongside the Recency observer (SetWinEventHook EVENT_SYSTEM_FOREGROUND).
pub fn onscreen_app_window_ids_excluding_pid(_pid: i32) -> std::collections::HashSet<u32> {
    std::collections::HashSet::new()
}
