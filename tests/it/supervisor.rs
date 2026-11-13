//! `pooltime serve --supervise`: worker generations on one listening socket. A restart (`SIGHUP`)
//! must let the new generation take new connections while the old one finishes its streams,
//! with no refused connection and no 503 in between; a crashed worker is replaced while
//! clients wait in the backlog. Real child processes, a loopback fake upstream, temp homes.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::support::procs::{self, free_port};
use crate::support::{self, FakeAnthropic, Reply, MESSAGES};

const BUDGET: Duration = Duration::from_secs(20);

fn ev(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn stream_frames(text: &str, pause: Duration) -> Vec<(Duration, String)> {
    vec![
        (
            Duration::ZERO,
            ev(
                "message_start",
                json!({"type": "message_start", "message": {
                    "id": "msg_s", "type": "message", "role": "assistant", "content": [],
                    "usage": {"input_tokens": 3, "output_tokens": 1}}}),
            ),
        ),
        (
            pause,
            ev(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "text_delta", "text": text}}),
            ),
        ),
        (
            Duration::ZERO,
            ev(
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                       "usage": {"output_tokens": 2}}),
            ),
        ),
        (
            Duration::ZERO,
            ev("message_stop", json!({"type": "message_stop"})),
        ),
    ]
}

/// A temp home whose config points the pool at `upstream`, with one signed-in account.
struct Home {
    dir: tempfile::TempDir,
    port: u16,
}

impl Home {
    fn new(upstream: &str) -> Home {
        let h = Home {
            dir: tempfile::tempdir().unwrap(),
            port: free_port(),
        };
        let paths = pooltime::config::Paths::under(&h.pt());
        paths.ensure().unwrap();
        std::fs::write(
            &paths.config,
            format!(
                "port = {port}\n\n[anthropic]\napi_base = \"{upstream}\"\n\
                 token_url = \"{upstream}/v1/oauth/token\"\nusage_refresh = false\n",
                port = h.port
            ),
        )
        .unwrap();
        let reg = pooltime::accounts::Registry::load(&paths).unwrap();
        let meta = pooltime::accounts::AccountMeta::new(
            "work".parse().unwrap(),
            pooltime::providers::ProviderId::Anthropic,
        );
        reg.upsert(meta.clone()).unwrap();
        reg.write_creds(&meta.id, &support::creds("sup-access-SECRET", 3600))
            .unwrap();
        h
    }

    fn pt(&self) -> PathBuf {
        self.dir.path().join("pt")
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    fn supervise(&self) -> Supervised {
        let log = std::fs::File::create(self.dir.path().join("daemon.err")).unwrap();
        let child = procs::spawn(
            Command::new(env!("CARGO_BIN_EXE_pooltime"))
                .args(["serve", "--supervise", "--i-stopped-bro"])
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("TMPDIR", std::env::temp_dir())
                .env("HOME", self.dir.path())
                .env("POOLTIME_HOME", self.pt())
                .env("POOLTIME_ALERTS_NOTIFY", "false")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log),
        );
        let s = Supervised { child };
        let start = Instant::now();
        loop {
            if let Some((200, _)) = request(self.addr(), "GET", "/health", None) {
                return s;
            }
            assert!(
                start.elapsed() < BUDGET,
                "supervisor did not come up: {}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn state(&self) -> Option<Value> {
        let raw = std::fs::read(self.pt().join("run/supervisor.json")).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("daemon.err")).unwrap_or_default()
    }

    fn health_pid(&self) -> u64 {
        let (status, body) = request(self.addr(), "GET", "/health", None).expect("health");
        assert_eq!(status, 200, "{body}");
        serde_json::from_str::<Value>(&body).unwrap()["pid"]
            .as_u64()
            .unwrap()
    }
}

/// The supervisor child; killed (with its process group's workers) on drop.
struct Supervised {
    child: Child,
}

impl Supervised {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            assert!(start.elapsed() < BUDGET, "supervisor did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            kill("-KILL", self.child.id());
            let _ = self.child.wait();
        }
    }
}

fn kill(sig: &str, pid: impl std::fmt::Display) {
    let _ = procs::spawn(Command::new("kill").args([sig, &pid.to_string()])).wait();
}

fn alive(pid: u64) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// One HTTP/1.1 exchange on a fresh connection (`Connection: close`): status and raw body.
/// `None` when the connection is refused, reset or times out.
fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Option<(u16, String)> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(15))).ok()?;
    let body = body.map(Value::to_string).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
         content-type: application/json\r\nanthropic-version: 2023-06-01\r\n\
         content-length: {len}\r\n\r\n{body}",
        port = addr.port(),
        len = body.len()
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok()?;
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let status = raw.split_whitespace().nth(1)?.parse().ok()?;
    let body = raw.split_once("\r\n\r\n")?.1.to_string();
    Some((status, body))
}

fn stream_body() -> Value {
    json!({"model": "claude-sonnet-5", "max_tokens": 8, "stream": true,
           "messages": [{"role": "user", "content": "hi"}]})
}

/// Fresh connections to `/health` (and the occasional inference request) until stopped; every
/// failure (refused, reset, timeout, non-200) is recorded. Returns the pids that answered.
struct Hammer {
    stop: Arc<AtomicBool>,
    failures: Arc<Mutex<Vec<String>>>,
    join: std::thread::JoinHandle<Vec<u64>>,
}

impl Hammer {
    fn start(addr: SocketAddr) -> Hammer {
        let stop = Arc::new(AtomicBool::new(false));
        let failures: Arc<Mutex<Vec<String>>> = Arc::default();
        let join = std::thread::spawn({
            let (stop, failures) = (stop.clone(), failures.clone());
            move || {
                let mut pids = Vec::new();
                let mut n = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    n += 1;
                    if n.is_multiple_of(10) {
                        match request(addr, "POST", MESSAGES, Some(&stream_body())) {
                            Some((200, b)) if b.contains("message_stop") => {}
                            other => failures.lock().unwrap().push(format!("POST: {other:?}")),
                        }
                        continue;
                    }
                    match request(addr, "GET", "/health", None) {
                        Some((200, b)) => {
                            let pid = serde_json::from_str::<Value>(&b).unwrap()["pid"]
                                .as_u64()
                                .unwrap();
                            if pids.last() != Some(&pid) {
                                pids.push(pid);
                            }
                        }
                        other => failures.lock().unwrap().push(format!("health: {other:?}")),
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                pids
            }
        });
        Hammer {
            stop,
            failures,
            join,
        }
    }

    fn finish(self) -> (Vec<u64>, Vec<String>) {
        self.stop.store(true, Ordering::SeqCst);
        let pids = self.join.join().unwrap();
        let failures = self.failures.lock().unwrap().clone();
        (pids, failures)
    }
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < BUDGET, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_gone(pid: u64) {
    wait_for("the old generation to exit", || !alive(pid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_hands_over_mid_stream_without_refusing_anyone() {
    let fake = FakeAnthropic::start().await;
    // The first request streams slowly: generation 1 must finish it after the handoff.
    fake.route(MESSAGES, |_, i| {
        if i == 0 {
            Reply::sse(stream_frames("from-generation-one", Duration::from_secs(5)))
        } else {
            Reply::sse(stream_frames("quick", Duration::ZERO))
        }
    });
    let h = Home::new(&fake.url());
    tokio::task::block_in_place(|| {
        let mut sup = h.supervise();
        let gen1 = h.health_pid();
        wait_for("generation 1 to become active", || {
            h.state().is_some_and(|s| s["generation"] == 1)
        });

        // A long stream on generation 1.
        let addr = h.addr();
        let slow =
            std::thread::spawn(move || request(addr, "POST", MESSAGES, Some(&stream_body())));
        wait_for("the upstream to see the stream", || {
            !fake.requests_to(MESSAGES).is_empty()
        });

        let hammer = Hammer::start(h.addr());
        std::thread::sleep(Duration::from_millis(200));
        kill("-HUP", sup.pid());

        wait_for("generation 2 to answer", || h.health_pid() != gen1);
        wait_for("the supervisor to record the restart", || {
            h.state().is_some_and(|s| s["restarts"] == 1)
        });
        let st = h.state().unwrap();
        assert_eq!(st["generation"], 2, "{st}");
        assert_eq!(st["last_restart"]["ok"], true, "{st}");
        let gen2 = st["worker_pid"].as_u64().unwrap();
        assert_ne!(gen2, gen1);
        assert!(alive(gen1), "generation 1 is still finishing its stream");
        assert_eq!(st["draining"], json!([gen1]), "{st}");

        // The stream that started on generation 1 completes intact.
        let (status, body) = slow.join().unwrap().expect("the gen-1 stream was cut");
        assert_eq!(status, 200);
        for part in [
            "message_start",
            "from-generation-one",
            "message_delta",
            "message_stop",
        ] {
            assert!(
                body.contains(part),
                "{part} missing from the gen-1 stream: {body}"
            );
        }
        wait_gone(gen1);

        let (pids, failures) = hammer.finish();
        assert!(
            failures.is_empty(),
            "requests failed during the handoff: {failures:?}\n{}",
            h.stderr()
        );
        // Generation 1 answered until the handoff, generation 2 from then on.
        assert_eq!(pids, [gen1, gen2]);

        // New work after the handoff runs on generation 2.
        let (status, body) = request(h.addr(), "POST", MESSAGES, Some(&stream_body())).unwrap();
        assert_eq!(status, 200);
        assert!(body.contains("quick"), "{body}");
        assert_eq!(h.health_pid(), gen2);

        // Stop: forwarded to the worker, the supervisor exits 0 and removes its state file.
        kill("-TERM", sup.pid());
        let code = sup.wait_exit();
        assert_eq!(code.code(), Some(0), "{}", h.stderr());
        assert!(h.state().is_none(), "run/supervisor.json removed at exit");
        wait_gone(gen2);
        assert!(
            request(h.addr(), "GET", "/health", None).is_none(),
            "port released"
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crashed_worker_is_replaced_while_clients_wait() {
    let fake = FakeAnthropic::start().await;
    fake.route(MESSAGES, |_, _| {
        Reply::sse(stream_frames("ok", Duration::ZERO))
    });
    let h = Home::new(&fake.url());
    tokio::task::block_in_place(|| {
        let _sup = h.supervise();
        let gen1 = h.health_pid();
        let hammer = Hammer::start(h.addr());
        std::thread::sleep(Duration::from_millis(100));
        kill("-KILL", gen1);
        wait_for("a replacement worker", || {
            h.state()
                .is_some_and(|s| s["generation"] == 2 && s["worker_pid"].as_u64() != Some(gen1))
        });
        wait_for("the replacement to answer", || h.health_pid() != gen1);
        std::thread::sleep(Duration::from_millis(200));
        let (_, failures) = hammer.finish();
        // Requests in flight on the killed worker die with it; nobody is refused or sees a 503.
        for f in &failures {
            assert!(
                !f.contains("Some((503") && !f.contains("Some((502"),
                "a 5xx while the worker was replaced: {failures:?}"
            );
        }
        assert!(
            failures.len() <= 2,
            "only requests in flight on the killed worker may fail: {failures:?}"
        );
    });
}

#[test]
fn a_worker_that_cannot_start_stops_the_supervisor() {
    // The supervisor binds fine, but its first worker refuses to boot: exit 1, so the service
    // manager's restart policy (and its throttle) takes over and the port is released.
    let dir = tempfile::tempdir().unwrap();
    let pt = dir.path().join("pt");
    std::fs::create_dir_all(&pt).unwrap();
    let port = free_port();
    std::fs::write(pt.join("config.toml"), format!("port = {port}\n")).unwrap();
    // A database that is not one: the supervisor never opens it, the worker cannot.
    std::fs::write(pt.join("pooltime.db"), vec![0x5a; 8192]).unwrap();
    let mut sup = Supervised {
        child: procs::spawn(
            Command::new(env!("CARGO_BIN_EXE_pooltime"))
                .args(["serve", "--supervise", "--i-stopped-bro"])
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", dir.path())
                .env("POOLTIME_HOME", &pt)
                .env("POOLTIME_ALERTS_NOTIFY", "false")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        ),
    };
    assert_eq!(sup.wait_exit().code(), Some(1));
    assert!(!Path::new(&pt.join("run/supervisor.json")).exists());
}
