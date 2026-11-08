use predicates::prelude::*;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[test]
fn version_flag_prints_name_and_version() {
    assert_cmd::Command::cargo_bin("pooltime")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("pooltime "));
}

const BOOT_BUDGET: Duration = Duration::from_secs(10);

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// A `pooltime serve` command over a temp home (never the real `$HOME`).
fn serve_cmd(home: &TempDir, port: u16) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pooltime"));
    c.args(["serve", "--i-stopped-bro", "--port", &port.to_string()])
        .env("POOLTIME_HOME", home.path().join("pt"))
        .env("HOME", home.path())
        .env_remove("POOLTIME_PORT")
        .env_remove("POOLTIME_HOST")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    c
}

/// A running daemon, killed on drop.
struct Daemon {
    child: Child,
    port: u16,
    _home: TempDir,
}

impl Daemon {
    fn spawn(home: TempDir, port: u16) -> Daemon {
        let child = serve_cmd(&home, port)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Daemon {
            child,
            port,
            _home: home,
        }
    }

    fn wait_healthy(&mut self) {
        let start = Instant::now();
        while start.elapsed() < BOOT_BUDGET {
            if let Some(st) = self.child.try_wait().unwrap() {
                panic!("daemon exited early: {st}");
            }
            if let Some((200, body)) = get_health(self.port) {
                assert!(body.contains(r#""service":"pooltime""#), "{body}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not answer /health within {BOOT_BUDGET:?}");
    }

    fn sigterm(&self) {
        let st = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(st.success());
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let start = Instant::now();
        while start.elapsed() < BOOT_BUDGET {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not exit after SIGTERM");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn get_health(port: u16) -> Option<(u16, String)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).ok()?;
    let status = raw.split_whitespace().nth(1)?.parse().ok()?;
    let body = raw.split("\r\n\r\n").nth(1)?.to_string();
    Some((status, body))
}

#[test]
fn serve_boots_and_answers_health() {
    let mut d = Daemon::spawn(tempfile::tempdir().unwrap(), free_port());
    d.wait_healthy();
    d.sigterm();
    let st = d.wait_exit();
    assert_eq!(st.code(), Some(0), "graceful SIGTERM exits 0, got {st}");
}

#[test]
fn second_serve_on_same_port_says_already_running() {
    let mut first = Daemon::spawn(tempfile::tempdir().unwrap(), free_port());
    first.wait_healthy();
    let second_home = tempfile::tempdir().unwrap();
    let out = serve_cmd(&second_home, first.port).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("already running"), "{err}");
    // The first daemon is unharmed.
    assert!(matches!(get_health(first.port), Some((200, _))));
}
