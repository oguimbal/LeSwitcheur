#![cfg(unix)]

use local_source_ipc::{
    unix::{request, Server, LIST_TIMEOUT},
    Envelope, AGENTSMON,
};
use serde_json::Value;
use std::fs;
use std::io::{self, Write};
use std::os::unix::{fs::PermissionsExt, net::UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Process(Child);
impl Process {
    fn start(test: &str, path: &Path) -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", test, "--nocapture"])
                .env("LOCAL_SOURCE_TEST_SOCKET", path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn kill(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "subprocess did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn list(path: &Path) -> io::Result<Envelope> {
    request(
        path,
        &Envelope::new(AGENTSMON, "list", Value::Null),
        LIST_TIMEOUT,
    )
}

#[test]
#[ignore = "subprocess fixture, invoked by the restart tests"]
fn server_process() {
    let Some(path) = std::env::var_os("LOCAL_SOURCE_TEST_SOCKET") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let marker = path.with_extension("holding");
    let _server = Server::start(path, AGENTSMON, move |query, _| {
        if query.method == "hold" {
            fs::write(&marker, b"ready").unwrap();
            std::thread::park();
        }
        Envelope::new(
            AGENTSMON,
            &query.method,
            serde_json::json!({"pid":std::process::id()}),
        )
    })
    .unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
#[ignore = "subprocess fixture, invoked by the restart tests"]
fn client_process() {
    let Some(path) = std::env::var_os("LOCAL_SOURCE_TEST_SOCKET") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let mut stream = UnixStream::connect(&path).unwrap();
    // Die halfway through a frame, before the server can decode a request.
    stream.write_all(&[0, 0]).unwrap();
    fs::write(
        path.with_extension(format!("client-{}", std::process::id())),
        b"ready",
    )
    .unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn source_killed_during_request_recovers_at_same_endpoint() {
    let dir = private_dir();
    let path = dir.path().join("source.sock");
    let mut server = Process::start("server_process", &path);
    wait_until(|| list(&path).is_ok());
    let first_pid = list(&path).unwrap().payload["pid"].clone();
    let client_path = path.clone();
    let client = std::thread::spawn(move || {
        request(
            &client_path,
            &Envelope::new(AGENTSMON, "hold", Value::Null),
            Duration::from_secs(3),
        )
    });
    wait_until(|| path.with_extension("holding").exists());
    server.kill();
    let start = Instant::now();
    assert!(client.join().unwrap().is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(list(&path).is_err());
    assert!(
        path.exists(),
        "SIGKILL should leave a stale socket to reclaim"
    );

    let _replacement = Process::start("server_process", &path);
    wait_until(|| list(&path).is_ok());
    let replacement_pid = list(&path).unwrap().payload["pid"].clone();
    assert_ne!(first_pid, replacement_pid);
}

#[test]
fn killed_clients_release_workers_and_new_clients_can_connect() {
    let dir = private_dir();
    let path = dir.path().join("source.sock");
    let _server = Process::start("server_process", &path);
    wait_until(|| list(&path).is_ok());
    let server_pid = list(&path).unwrap().payload["pid"].clone();
    let mut clients: Vec<_> = (0..2)
        .map(|_| Process::start("client_process", &path))
        .collect();
    for client in &clients {
        wait_until(|| {
            path.with_extension(format!("client-{}", client.0.id()))
                .exists()
        });
    }
    for client in &mut clients {
        client.kill();
    }
    // Each call opens a new connection, as a relaunched switcher would.
    for _ in 0..3 {
        assert_eq!(list(&path).unwrap().payload["pid"], server_pid);
    }
}
