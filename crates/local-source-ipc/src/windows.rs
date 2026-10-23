//! Named-pipe transport (client side). Same framing, bounds and deadlines as
//! the Unix transport. The server owns the pipe; its DACL must restrict access
//! to the current user and it must reject remote clients.
use crate::{invalid, Envelope, MAX_FRAME};
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    OpenThread, PROCESS_QUERY_LIMITED_INFORMATION, THREAD_TERMINATE,
};
use windows_sys::Win32::System::IO::CancelSynchronousIo;

pub const LIST_TIMEOUT: Duration = Duration::from_millis(750);
pub const FOCUS_TIMEOUT: Duration = Duration::from_secs(3);

/// `\\.\pipe\oliv-local-sources-<session>-<source>`: one pipe per source and
/// logon session (RDS hosts run several sessions of the same user).
pub fn endpoint(source: &str) -> io::Result<String> {
    if source.is_empty() || !source.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
        return Err(invalid("invalid source identifier"));
    }
    let mut session = 0u32;
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(format!(r"\\.\pipe\oliv-local-sources-{session}-{source}"))
}

fn wide(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "local source timed out")
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(timed_out)
}

/// User SID bytes of a process token.
fn user_sid(process: HANDLE) -> io::Result<Vec<u8>> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = owned(token)?;
    let mut size = 0u32;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut size,
        )
    };
    // u64 storage keeps TOKEN_USER suitably aligned.
    let mut buffer = vec![0u64; (size as usize).div_ceil(8).max(1)];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    // A SID is at most 68 bytes; copy it out so the buffer can be dropped.
    let sid = user.User.Sid as *const u8;
    let count = unsafe { *sid.add(1) } as usize;
    Ok(unsafe { std::slice::from_raw_parts(sid, 8 + 4 * count) }.to_vec())
}

/// Refuse a pipe served by another user's process (name squatting).
fn check_server_owner(pipe: &OwnedHandle) -> io::Result<()> {
    let mut pid = 0u32;
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let server = owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
    let mut theirs = user_sid(server.as_raw_handle())?;
    let mut ours = user_sid(unsafe { GetCurrentProcess() })?;
    if unsafe { EqualSid(theirs.as_mut_ptr().cast(), ours.as_mut_ptr().cast()) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local source pipe is owned by another user",
        ));
    }
    Ok(())
}

fn connect(name: &str, deadline: Instant) -> io::Result<File> {
    let name = wide(name);
    loop {
        // Identification level: the server cannot impersonate us.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            let pipe = owned(handle)?;
            check_server_owner(&pipe)?;
            return Ok(File::from(pipe));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(error);
        }
        let wait = remaining(deadline)?.as_millis().clamp(1, u32::MAX as u128) as u32;
        if unsafe { WaitNamedPipeW(name.as_ptr(), wait) } == 0 && Instant::now() >= deadline {
            return Err(timed_out());
        }
    }
}

/// Run blocking pipe I/O on this thread; a watchdog cancels it at the deadline.
/// The watchdog is joined before returning so it can never cancel later I/O.
fn with_deadline<T>(deadline: Instant, io: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let thread = owned(unsafe { OpenThread(THREAD_TERMINATE, 0, GetCurrentThreadId()) })?;
    let raw = thread.as_raw_handle() as usize;
    let (done, wait) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if wait.recv_timeout(timeout) == Err(mpsc::RecvTimeoutError::Timeout) {
            unsafe { CancelSynchronousIo(raw as *mut c_void) };
            true
        } else {
            false
        }
    });
    let result = io();
    let _ = done.send(());
    let fired = watchdog.join().unwrap_or(true);
    drop(thread);
    match result {
        Err(e) if fired || e.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) => {
            Err(timed_out())
        }
        other => other,
    }
}

pub fn read_frame(stream: &mut impl Read) -> io::Result<Envelope> {
    let mut size = [0u8; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(invalid("invalid frame length"));
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes)?;
    Envelope::decode(&bytes)
}

pub fn write_frame(stream: &mut impl Write, envelope: &Envelope) -> io::Result<()> {
    let bytes = serde_json::to_vec(envelope).map_err(invalid)?;
    if bytes.len() > MAX_FRAME {
        return Err(invalid("frame too large"));
    }
    let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&bytes);
    stream.write_all(&frame)?;
    stream.flush()
}

pub fn request(name: &str, envelope: &Envelope, timeout: Duration) -> io::Result<Envelope> {
    let deadline = Instant::now() + timeout;
    let mut pipe = connect(name, deadline)?;
    let response = with_deadline(deadline, || {
        write_frame(&mut pipe, envelope)?;
        read_frame(&mut pipe)
    })?;
    crate::check_response(envelope, response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AGENTSMON;
    use serde_json::json;
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    fn serve_once(
        name: &str,
        reply: impl FnOnce(&mut File) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let wide_name = wide(name);
        let pipe = owned(unsafe {
            CreateNamedPipeW(
                wide_name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                65536,
                65536,
                0,
                std::ptr::null(),
            )
        })
        .unwrap();
        std::thread::spawn(move || {
            unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) };
            let mut file = File::from(pipe);
            reply(&mut file);
        })
    }

    fn test_name(tag: &str) -> String {
        format!(
            r"\\.\pipe\local-source-ipc-test-{}-{tag}",
            std::process::id()
        )
    }

    #[test]
    fn endpoint_is_session_scoped_and_validated() {
        let name = endpoint("lena").unwrap();
        assert!(name.starts_with(r"\\.\pipe\oliv-local-sources-"));
        assert!(name.ends_with("-lena"));
        assert!(endpoint("../x").is_err());
    }

    #[test]
    fn exchanges_and_surfaces_remote_codes() {
        let name = test_name("exchange");
        let server = serve_once(&name, |pipe| {
            let request = read_frame(pipe).unwrap();
            write_frame(
                pipe,
                &Envelope::new(AGENTSMON, &request.method, json!({"ok": true})),
            )
            .unwrap();
        });
        let response = request(
            &name,
            &Envelope::new(AGENTSMON, "list", json!(null)),
            LIST_TIMEOUT,
        )
        .unwrap();
        assert_eq!(response.payload["ok"], true);
        server.join().unwrap();

        let server = serve_once(&name, |pipe| {
            read_frame(pipe).unwrap();
            write_frame(pipe, &Envelope::error(AGENTSMON, "stale")).unwrap();
        });
        let error = request(
            &name,
            &Envelope::new(AGENTSMON, "focus", json!(null)),
            LIST_TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(crate::remote_code(&error), Some("stale"));
        server.join().unwrap();
    }

    #[test]
    fn missing_pipe_is_not_found_and_silent_server_times_out() {
        let error = request(
            &test_name("absent"),
            &Envelope::new(AGENTSMON, "list", json!(null)),
            LIST_TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);

        let name = test_name("silent");
        let server = serve_once(&name, |pipe| {
            read_frame(pipe).unwrap();
            std::thread::sleep(Duration::from_millis(600));
        });
        let started = Instant::now();
        let error = request(
            &name,
            &Envelope::new(AGENTSMON, "list", json!(null)),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(500));
        server.join().unwrap();
    }
}
