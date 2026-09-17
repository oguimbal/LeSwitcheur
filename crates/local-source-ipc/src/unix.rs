//! Unix-domain transport. All I/O has an overall deadline and a bounded frame size.
use crate::{invalid, Envelope, MAX_FRAME};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

pub const LIST_TIMEOUT: Duration = Duration::from_millis(750);
pub const FOCUS_TIMEOUT: Duration = Duration::from_secs(3);

pub fn endpoint(source: &str) -> io::Result<PathBuf> {
    if source.is_empty() || !source.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
        return Err(invalid("invalid source identifier"));
    }
    // Short and independent of HOME/TMPDIR overrides; socket paths on macOS are limited to 104 bytes.
    let uid = unsafe { libc::geteuid() };
    Ok(PathBuf::from(format!(
        "/tmp/oliv-local-sources-{uid}/{source}.sock"
    )))
}

fn check_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "source directory is not private",
        ));
    }
    Ok(())
}

fn pause(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "local source timed out",
        ));
    }
    std::thread::sleep(Duration::from_millis(2));
    Ok(())
}

fn connect(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    check_directory(path.parent().ok_or_else(|| invalid("missing parent"))?)?;
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(invalid("socket path too long"));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (dst, src) in address.sun_path.iter_mut().zip(bytes) {
        *dst = *src as _;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as _;
    }
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // Do not leak a connection to a subprocess launched by either application.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    let result = unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
        ) {
            return Err(error);
        }
        loop {
            pause(deadline)?;
            if let Some(error) = stream.take_error()? {
                return Err(error);
            }
            if stream.peer_addr().is_ok() {
                break;
            }
        }
    }
    Ok(stream)
}

fn transfer(
    stream: &mut UnixStream,
    buffer: &mut [u8],
    deadline: Instant,
    write: bool,
) -> io::Result<()> {
    let mut offset = 0;
    while offset < buffer.len() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "local source timed out",
            ));
        }
        let result = if write {
            stream.write(&buffer[offset..])
        } else {
            stream.read(&mut buffer[offset..])
        };
        match result {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "local source disconnected",
                ))
            }
            Ok(n) => offset += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => pause(deadline)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn read_frame(stream: &mut UnixStream, deadline: Instant) -> io::Result<Envelope> {
    let mut size = [0u8; 4];
    transfer(stream, &mut size, deadline, false)?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(invalid("invalid frame length"));
    }
    let mut bytes = vec![0; size];
    transfer(stream, &mut bytes, deadline, false)?;
    Envelope::decode(&bytes)
}

pub fn write_frame(
    stream: &mut UnixStream,
    envelope: &Envelope,
    deadline: Instant,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(envelope).map_err(invalid)?;
    if bytes.len() > MAX_FRAME {
        return Err(invalid("frame too large"));
    }
    transfer(
        stream,
        &mut (bytes.len() as u32).to_be_bytes(),
        deadline,
        true,
    )?;
    transfer(stream, &mut bytes, deadline, true)
}

pub fn request(path: &Path, envelope: &Envelope, timeout: Duration) -> io::Result<Envelope> {
    let deadline = Instant::now() + timeout;
    let mut stream = connect(path, deadline)?;
    write_frame(&mut stream, envelope, deadline)?;
    let response = read_frame(&mut stream, deadline)?;
    response.check(&envelope.source)?;
    if response.method == "error" {
        return Err(invalid(format!(
            "local source: {}",
            response
                .payload
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown_error")
        )));
    }
    if response.method != envelope.method {
        return Err(invalid("unexpected response method"));
    }
    Ok(response)
}

struct Binding {
    _listener: UnixListener,
    _lock: File,
    path: PathBuf,
}
impl Drop for Binding {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub struct Server {
    stop: Arc<AtomicBool>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Server {
    pub fn start(
        path: PathBuf,
        source: &'static str,
        handler: impl Fn(Envelope, Instant) -> Envelope + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| invalid("missing socket parent"))?;
        match fs::DirBuilder::new().mode(0o700).create(parent) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        check_directory(parent)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path.with_extension("lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "local source already running",
            ));
        }
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() } => {
                fs::remove_file(&path)?
            }
            Ok(_) => return Err(invalid("source endpoint is not an owned socket")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        let binding = Arc::new(Binding {
            _listener: listener,
            _lock: lock,
            path,
        });
        fs::set_permissions(&binding.path, fs::Permissions::from_mode(0o600))?;
        binding._listener.set_nonblocking(true)?;
        let server = Self {
            stop: Arc::new(AtomicBool::new(false)),
        };
        let handler = Arc::new(handler);
        // Fixed workers bound resources even when clients disconnect or stall.
        for index in 0..2 {
            let binding = binding.clone();
            let stop = server.stop.clone();
            let handler = handler.clone();
            std::thread::Builder::new()
                .name(format!("local-source-{index}"))
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match binding._listener.accept() {
                            Ok((mut stream, _)) => {
                                if stream.set_nonblocking(true).is_err() {
                                    continue;
                                }
                                let deadline = Instant::now() + LIST_TIMEOUT;
                                if let Ok(request) = read_frame(&mut stream, deadline) {
                                    let compatible = request.check(source).is_ok();
                                    let deadline = Instant::now()
                                        + if compatible && request.method == "focus" {
                                            FOCUS_TIMEOUT
                                        } else {
                                            LIST_TIMEOUT
                                        };
                                    let response = if compatible {
                                        handler(request, deadline)
                                    } else {
                                        Envelope::error(source, "incompatible_version")
                                    };
                                    let _ = write_frame(&mut stream, &response, deadline);
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(10))
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(50)),
                        }
                    }
                })?;
        }
        Ok(server)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AGENTSMON;
    use serde_json::{json, Value};

    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn exchanges_rejects_old_versions_and_recovers_after_bad_client() {
        let dir = private_dir();
        let path = dir.path().join("source.sock");
        let _server = Server::start(path.clone(), AGENTSMON, |r, _| {
            Envelope::new(AGENTSMON, &r.method, json!({"ok":true}))
        })
        .unwrap();
        assert!(Server::start(path.clone(), AGENTSMON, |r, _| r).is_err());
        let query = Envelope::new(AGENTSMON, "list", Value::Null);
        assert!(request(&path, &query, LIST_TIMEOUT).unwrap().payload["ok"]
            .as_bool()
            .unwrap());
        let mut old = Envelope::new(AGENTSMON, "focus", Value::Null);
        old.version = 999;
        assert!(request(&path, &old, LIST_TIMEOUT)
            .unwrap_err()
            .to_string()
            .contains("incompatible_version"));
        let mut bad = UnixStream::connect(&path).unwrap();
        bad.write_all(&u32::MAX.to_be_bytes()).unwrap();
        drop(bad);
        assert!(request(&path, &query, LIST_TIMEOUT).is_ok());
    }

    #[test]
    fn deadline_bounds_partial_frames_and_missing_source() {
        let dir = private_dir();
        let path = dir.path().join("missing.sock");
        assert!(request(
            &path,
            &Envelope::new(AGENTSMON, "list", Value::Null),
            LIST_TIMEOUT
        )
        .is_err());
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer.write_all(&[0, 0]).unwrap();
        let start = Instant::now();
        assert_eq!(
            read_frame(&mut reader, start + Duration::from_millis(30))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn stale_socket_is_recovered_but_insecure_directory_is_rejected() {
        let dir = private_dir();
        let path = dir.path().join("source.sock");
        drop(UnixListener::bind(&path).unwrap());
        let _server = Server::start(path, AGENTSMON, |r, _| r).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Server::start(dir.path().join("other.sock"), AGENTSMON, |r, _| r).is_err());
    }
}
