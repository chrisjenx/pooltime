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
        let s = Supervised {
            child,
            state: self.pt().join("run/supervisor.json"),
        };
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

/// The supervisor child. On drop it is killed, and so are the workers its state file names (they
/// share the test's process group, so the group cannot be signalled; orphaned workers would
/// drain and exit on their own, but a test should not leave them behind).
struct Supervised {
    child: Child,
    state: PathBuf,
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
        let workers: Vec<u64> = std::fs::read(&self.state)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .map(|st| {
                let mut pids: Vec<u64> = st["draining"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_u64)
                    .collect();
                pids.extend(st["worker_pid"].as_u64());
                pids
            })
            .unwrap_or_default();
        if self.child.try_wait().ok().flatten().is_none() {
            kill("-KILL", self.child.id());
            let _ = self.child.wait();
        }
        for pid in workers {
            if alive(pid) {
                kill("-KILL", pid);
            }
        }
    }
}

fn kill(sig: &str, pid: impl std::fmt::Display) {
    let _ = procs::spawn(
        Command::new("kill")
            .args([sig, &pid.to_string()])
            .stderr(Stdio::null()),
    )
    .wait();
}

fn alive(pid: u64) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// How one exchange went (the texts show in failure messages).
#[allow(dead_code)]
#[derive(Debug)]
enum Answer {
    /// Status and raw body.
    Http(u16, String),
    /// The connection was not accepted at all.
    Refused(String),
    /// Accepted, then reset, cut or timed out.
    Broken(String),
}

/// One HTTP/1.1 exchange on a fresh connection (`Connection: close`).
fn exchange(addr: SocketAddr, method: &str, path: &str, body: Option<&Value>) -> Answer {
    let mut s = match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(s) => s,
        Err(e) => return Answer::Refused(e.to_string()),
    };
    let body = body.map(Value::to_string).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
         content-type: application/json\r\nanthropic-version: 2023-06-01\r\n\
         content-length: {len}\r\n\r\n{body}",
        port = addr.port(),
        len = body.len()
    );
    let mut raw = Vec::new();
    let io = s
        .set_read_timeout(Some(Duration::from_secs(15)))
        .and_then(|()| s.write_all(req.as_bytes()))
        .and_then(|()| s.read_to_end(&mut raw));
    if let Err(e) = io {
        return Answer::Broken(e.to_string());
    }
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let parsed = raw
        .split_whitespace()
        .nth(1)
        .and_then(|st| st.parse().ok())
        .zip(raw.split_once("\r\n\r\n").map(|(_, b)| b.to_string()));
    match parsed {
        Some((status, body)) => Answer::Http(status, body),
        None => Answer::Broken(format!("unparsable answer: {raw:?}")),
    }
}

/// [`exchange`], status and body only.
fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Option<(u16, String)> {
    match exchange(addr, method, path, body) {
        Answer::Http(status, body) => Some((status, body)),
        _ => None,
    }
}

fn stream_body() -> Value {
    json!({"model": "claude-sonnet-5", "max_tokens": 8, "stream": true,
           "messages": [{"role": "user", "content": "hi"}]})
}

/// What the hammer saw.
#[derive(Default)]
struct Seen {
    /// `/health` answers: when the request started, and the pid that answered.
    answers: Vec<(Instant, u64)>,
    /// Every exchange that was not a complete 200.
    failures: Vec<Answer>,
}

/// Fresh connections to `/health` (and every tenth an inference request) until stopped.
struct Hammer {
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Seen>>,
    join: std::thread::JoinHandle<()>,
}

impl Hammer {
    fn start(addr: SocketAddr) -> Hammer {
        let stop = Arc::new(AtomicBool::new(false));
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let join = std::thread::spawn({
            let (stop, seen) = (stop.clone(), seen.clone());
            move || {
                let mut n = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    n += 1;
                    let at = Instant::now();
                    if n.is_multiple_of(10) {
                        match exchange(addr, "POST", MESSAGES, Some(&stream_body())) {
                            Answer::Http(200, b) if b.contains("message_stop") => {}
                            other => seen.lock().unwrap().failures.push(other),
                        }
                        continue;
                    }
                    match exchange(addr, "GET", "/health", None) {
                        Answer::Http(200, b) => {
                            let pid = serde_json::from_str::<Value>(&b).unwrap()["pid"]
                                .as_u64()
                                .unwrap();
                            seen.lock().unwrap().answers.push((at, pid));
                        }
                        other => seen.lock().unwrap().failures.push(other),
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        });
        Hammer { stop, seen, join }
    }

    fn finish(self) -> Seen {
        self.stop.store(true, Ordering::SeqCst);
        self.join.join().unwrap();
        std::mem::take(&mut *self.seen.lock().unwrap())
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

fn assert_intact(answer: Option<(u16, String)>, text: &str) {
    let (status, body) = answer.expect("the stream was cut");
    assert_eq!(status, 200, "{body}");
    for part in ["message_start", text, "message_delta", "message_stop"] {
        assert!(
            body.contains(part),
            "{part} missing from the stream: {body}"
        );
    }
}

/// A slow first stream (`pause` between its first and second event), quick ones after.
async fn upstream(pause: Duration) -> FakeAnthropic {
    let fake = FakeAnthropic::start().await;
    fake.route(MESSAGES, move |_, i| {
        if i == 0 {
            Reply::sse(stream_frames("from-generation-one", pause))
        } else {
            Reply::sse(stream_frames("quick", Duration::ZERO))
        }
    });
    fake
}

/// Start the slow stream and wait until the upstream has it.
fn slow_stream(h: &Home, fake: &FakeAnthropic) -> std::thread::JoinHandle<Option<(u16, String)>> {
    let addr = h.addr();
    let slow = std::thread::spawn(move || request(addr, "POST", MESSAGES, Some(&stream_body())));
    wait_for("the upstream to see the stream", || {
        !fake.requests_to(MESSAGES).is_empty()
    });
    slow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_hands_over_mid_stream_without_refusing_anyone() {
    let fake = upstream(Duration::from_secs(5)).await;
    let h = Home::new(&fake.url());
    tokio::task::block_in_place(|| {
        let mut sup = h.supervise();
        let gen1 = h.health_pid();
        wait_for("generation 1 to become active", || {
            h.state().is_some_and(|s| s["generation"] == 1)
        });

        // A long stream on generation 1.
        let slow = slow_stream(&h, &fake);
        let hammer = Hammer::start(h.addr());
        std::thread::sleep(Duration::from_millis(200));
        kill("-HUP", sup.pid());

        // From the moment the supervisor records the handoff, generation 1 accepts nothing new.
        wait_for("the handoff", || {
            h.state().is_some_and(|s| s["draining"] == json!([gen1]))
        });
        let handed_off = Instant::now();
        wait_for("the supervisor to record the restart", || {
            h.state().is_some_and(|s| s["restarts"] == 1)
        });
        let st = h.state().unwrap();
        assert_eq!(st["generation"], 2, "{st}");
        assert_eq!(st["last_restart"]["ok"], true, "{st}");
        assert_eq!(st["restarting"], false, "{st}");
        let gen2 = st["worker_pid"].as_u64().unwrap();
        assert_ne!(gen2, gen1);
        assert!(alive(gen1), "generation 1 is still finishing its stream");

        // The stream that started on generation 1 completes intact.
        assert_intact(slow.join().unwrap(), "from-generation-one");
        wait_gone(gen1);

        let seen = hammer.finish();
        assert!(
            seen.failures.is_empty(),
            "requests failed during the handoff: {:?}\n{}",
            seen.failures,
            h.stderr()
        );
        // gen1 answered before the handoff (and possibly once more in the overlap, for a
        // connection it had already accepted), gen2 from then on.
        assert!(seen.answers.iter().all(|(_, p)| *p == gen1 || *p == gen2));
        assert_eq!(seen.answers.first().map(|a| a.1), Some(gen1));
        assert_eq!(seen.answers.last().map(|a| a.1), Some(gen2));
        let late: Vec<_> = seen
            .answers
            .iter()
            .filter(|(at, p)| *p == gen1 && *at > handed_off)
            .collect();
        assert!(
            late.is_empty(),
            "gen1 took new connections after the handoff"
        );

        // New work after the handoff runs on generation 2.
        let (status, body) = request(h.addr(), "POST", MESSAGES, Some(&stream_body())).unwrap();
        assert_eq!(status, 200);
        assert!(body.contains("quick"), "{body}");
        assert_eq!(h.health_pid(), gen2);

        // Stop: the supervisor tells the worker, exits 0 and removes its state file.
        kill("-TERM", sup.pid());
        let code = sup.wait_exit();
        assert_eq!(code.code(), Some(0), "{}", h.stderr());
        assert!(h.state().is_none(), "run/supervisor.json removed at exit");
        wait_gone(gen2);
        assert!(
            matches!(
                exchange(h.addr(), "GET", "/health", None),
                Answer::Refused(_)
            ),
            "port released"
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_to_the_whole_group_drains_instead_of_forcing() {
    // `pkill pooltime`, or a service manager signalling the process group: the worker gets the
    // SIGTERM directly *and* the supervisor's stop. Counted twice, it would force exit and cut
    // the stream.
    let fake = upstream(Duration::from_secs(3)).await;
    let h = Home::new(&fake.url());
    tokio::task::block_in_place(|| {
        let mut sup = h.supervise();
        let gen1 = h.health_pid();
        let slow = slow_stream(&h, &fake);
        kill("-TERM", gen1);
        kill("-TERM", sup.pid());
        assert_intact(slow.join().unwrap(), "from-generation-one");
        assert_eq!(sup.wait_exit().code(), Some(0), "{}", h.stderr());
        wait_gone(gen1);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_orphaned_worker_drains_and_its_second_sigterm_forces() {
    // The supervisor is killed outright: its worker hands off (stops accepting) and drains on
    // its own. SIGTERMs count from then on, including during the 2 s handoff grace: the first
    // changes nothing, the second forces exit.
    let fake = upstream(Duration::from_secs(30)).await;
    let h = Home::new(&fake.url());
    tokio::task::block_in_place(|| {
        let mut sup = h.supervise();
        let gen1 = h.health_pid();
        let slow = slow_stream(&h, &fake);
        kill("-KILL", sup.pid());
        sup.wait_exit();
        std::thread::sleep(Duration::from_millis(500));
        kill("-TERM", gen1);
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            alive(gen1),
            "one SIGTERM does not force: the stream is still running"
        );
        kill("-TERM", gen1);
        let start = Instant::now();
        while alive(gen1) {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the second SIGTERM did not force exit\n{}",
                h.stderr()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // Forced: the 30 s stream was cut, not finished.
        let cut = slow.join().unwrap();
        assert!(
            cut.as_ref()
                .is_none_or(|(_, b)| !b.contains("message_stop")),
            "{cut:?}"
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_restart_keeps_the_old_generation_serving() {
    let fake = upstream(Duration::ZERO).await;
    let h = Home::new(&fake.url());
    tokio::task::block_in_place(|| {
        let sup = h.supervise();
        let gen1 = h.health_pid();
        wait_for("generation 1 to become active", || {
            h.state().is_some_and(|s| s["generation"] == 1)
        });
        // The next generation cannot open the database (generation 1 keeps its open file).
        let junk = h.pt().join("junk.db");
        std::fs::write(&junk, vec![0x5a; 8192]).unwrap();
        std::fs::rename(&junk, h.pt().join("pooltime.db")).unwrap();
        let hammer = Hammer::start(h.addr());
        kill("-HUP", sup.pid());
        wait_for("the failed restart to be recorded", || {
            h.state().is_some_and(|s| s["restarts"] == 1)
        });
        let st = h.state().unwrap();
        assert_eq!(st["last_restart"]["ok"], false, "{st}");
        let why = st["last_restart"]["error"].as_str().unwrap();
        assert!(why.contains("exited before it was ready"), "{why}");
        assert_eq!(st["generation"], 1, "{st}");
        assert_eq!(st["worker_pid"].as_u64(), Some(gen1), "{st}");
        assert!(st["draining"].as_array().unwrap().is_empty(), "{st}");
        std::thread::sleep(Duration::from_millis(200));
        let seen = hammer.finish();
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        assert!(seen.answers.iter().all(|(_, p)| *p == gen1));
        assert_eq!(h.health_pid(), gen1);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crashed_worker_is_replaced_while_clients_wait() {
    let fake = upstream(Duration::ZERO).await;
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
        let seen = hammer.finish();
        // The port never closes, so no connection is refused and nothing answers 5xx: clients
        // wait in the backlog for the replacement. The one exception is unavoidable: an exchange
        // the killed worker had already accepted dies with it (a reset, never a refusal; the
        // hammer has at most one in flight).
        let mut broken = 0;
        for f in &seen.failures {
            match f {
                Answer::Broken(_) => broken += 1,
                other => panic!(
                    "{other:?} while the worker was replaced: {:?}",
                    seen.failures
                ),
            }
        }
        assert!(broken <= 1, "{:?}", seen.failures);
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
        state: pt.join("run/supervisor.json"),
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
