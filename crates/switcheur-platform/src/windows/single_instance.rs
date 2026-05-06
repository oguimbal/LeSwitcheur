//! Single-instance enforcement on Windows + URL forwarding.
//!
//! macOS gets URL events through `NSAppleEventManager`/`on_open_urls` which
//! the OS funnels into the running process. Windows has no equivalent — every
//! invocation of `LeSwitcheur.exe leswitcheur://activate?...` spawns a fresh
//! process. We bridge that gap with the standard pattern:
//!
//! 1. Try to claim a named mutex. Failure means a primary is already running.
//! 2. As primary, run a tiny named-pipe server that drains URLs forwarded by
//!    secondary instances and pushes them onto the main `url_tx` channel.
//! 3. As secondary, write any URL on argv to that pipe and exit.
//!
//! Both names are scoped to `Local\` (per-session) so two users on the same
//! machine each get their own primary instance.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::time::Duration;

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY,
    GENERIC_READ, GENERIC_WRITE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

/// Lets the primary process steal foreground from us when it surfaces a window
/// in response to the URL we're about to forward. Secondaries are short-lived
/// and have just received fresh user input via the shell launch, so the call
/// succeeds; passing `ASFW_ANY` (0xFFFFFFFF) avoids needing the primary's PID.
const ASFW_ANY: u32 = 0xFFFFFFFF;

const MUTEX_NAME: &str = "Local\\fr.gmbl.LeSwitcheur.SingleInstance";
const PIPE_NAME: &str = "\\\\.\\pipe\\fr.gmbl.LeSwitcheur.URL";

#[derive(Debug)]
pub enum SingleInstanceOutcome {
    /// We hold the mutex. The caller should continue with normal init and
    /// later call [`start_pipe_server`] once its `url_tx` channel exists.
    Primary,
    /// A primary is already running. We forwarded the URL (if any was on the
    /// command line) and the caller should exit.
    ForwardedExit,
}

/// Probe for an existing primary. If one is found, forward the optional URL
/// to it via the named pipe and report `ForwardedExit` so the caller can exit
/// immediately. Otherwise claim the mutex and report `Primary` — the caller
/// must subsequently call [`start_pipe_server`] to receive forwarded URLs
/// from future secondaries.
///
/// Splitting this from the pipe-server start lets the caller bail out *before*
/// expensive init (hotkey registration, GPU device, font discovery) when the
/// process is just a URL-forwarding secondary. Otherwise an early failure in
/// any of those steps would crash the secondary before the URL is delivered.
pub fn check_or_forward(cmdline_url: Option<String>) -> Result<SingleInstanceOutcome> {
    let mutex_name_w = wide(MUTEX_NAME);
    // Hold the mutex handle for the lifetime of the process. `windows-rs`
    // wraps it in a struct with no Drop, so the underlying kernel object stays
    // referenced until the process exits — exactly what we want.
    let _handle = unsafe {
        CreateMutexW(None, false, PCWSTR(mutex_name_w.as_ptr())).context("CreateMutexW")?
    };
    let last = unsafe { GetLastError() };
    if last == ERROR_ALREADY_EXISTS {
        tracing::info!(
            url = ?cmdline_url,
            "secondary instance: forwarding to existing primary"
        );
        if let Some(url) = cmdline_url {
            if let Err(e) = forward_to_pipe(&url) {
                tracing::warn!("forward URL via pipe: {e:#}");
            } else {
                tracing::info!("secondary: URL forwarded via pipe");
            }
        }
        return Ok(SingleInstanceOutcome::ForwardedExit);
    }
    Ok(SingleInstanceOutcome::Primary)
}

/// Start the named-pipe server that drains URLs forwarded by future secondary
/// instances and pushes them onto `url_tx`. Must only be called by a primary
/// (i.e. after [`check_or_forward`] returned `Primary`).
pub fn start_pipe_server(url_tx: async_channel::Sender<String>) {
    tracing::info!("primary instance: starting pipe server");
    spawn_pipe_server(url_tx);
}

fn forward_to_pipe(url: &str) -> Result<()> {
    let name_w = wide(PIPE_NAME);
    // Hand foreground rights to the primary up front: when it pops a window
    // in response to our URL, Windows otherwise blocks `SetForegroundWindow`
    // and the user only sees a taskbar flash.
    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };

    // The primary's pipe server runs `loop { CreateNamedPipeW; ConnectNamedPipe; … }`.
    // There's a brief window between `CloseHandle` of the previous instance and
    // the next `CreateNamedPipeW` where the pipe doesn't exist yet. Retry a few
    // times so a URL forwarded right after another doesn't get dropped.
    let mut last_err: Option<windows::core::Error> = None;
    for attempt in 0..5 {
        let h = unsafe {
            CreateFileW(
                PCWSTR(name_w.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };
        match h {
            Ok(handle) => {
                let buf = url.as_bytes();
                let mut written = 0u32;
                let res = unsafe { WriteFile(handle, Some(buf), Some(&mut written), None) };
                unsafe {
                    let _ = CloseHandle(handle);
                }
                return res.context("write pipe").map(|_| ());
            }
            Err(e) => {
                let code = e.code().0 as u32 & 0xFFFF;
                let busy = code == ERROR_PIPE_BUSY.0 || code == ERROR_FILE_NOT_FOUND.0;
                last_err = Some(e);
                if !busy {
                    break;
                }
                tracing::debug!(attempt, "pipe busy, retrying");
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name_w.as_ptr()), 200) };
            }
        }
    }
    Err(last_err
        .map(|e| anyhow::anyhow!("connect to pipe: {e}"))
        .unwrap_or_else(|| anyhow::anyhow!("connect to pipe: unknown")))
}

fn spawn_pipe_server(url_tx: async_channel::Sender<String>) {
    std::thread::Builder::new()
        .name("leswitcheur-pipe-server".into())
        .spawn(move || pipe_server_loop(url_tx))
        .expect("spawn pipe server");
}

fn pipe_server_loop(url_tx: async_channel::Sender<String>) {
    let name_w = wide(PIPE_NAME);
    loop {
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(name_w.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX.0),
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                None,
            )
        };
        if h.is_invalid() {
            tracing::warn!("CreateNamedPipeW returned invalid handle");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        if let Err(e) = unsafe { ConnectNamedPipe(h, None) } {
            tracing::warn!("ConnectNamedPipe: {e:?}");
            unsafe {
                let _ = CloseHandle(h);
            }
            continue;
        }
        let mut buf = [0u8; 4096];
        let mut read = 0u32;
        let read_ok =
            unsafe { ReadFile(h, Some(&mut buf), Some(&mut read), None).is_ok() };
        unsafe {
            let _ = CloseHandle(h);
        }
        if !read_ok || read == 0 {
            continue;
        }
        let url = String::from_utf8_lossy(&buf[..read as usize])
            .trim()
            .to_string();
        if !url.is_empty() {
            tracing::info!(%url, "forwarded URL from secondary instance");
            let _ = url_tx.send_blocking(url);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
