//! Free ports and child processes. All integration tests share one binary (`tests/it`), so some
//! test is always spawning children while another wants a free port.
//!
//! Finding a free port means binding a listener on `127.0.0.1:0` and closing it. A child spawned
//! by another test thread meanwhile gets a copy of that listening socket (its fds are only closed
//! at exec), so the port keeps accepting connections and then resets them: a probe sees an
//! `Unresponsive` listener instead of a free port, and a daemon cannot bind it. The gate
//! makes the two exclusive: [`free_port`] waits until no spawn is in progress, and every spawn
//! goes through [`spawn`] (which returns only after the child has exec'd).
//!
//! Use these instead of `Command::spawn`/`output`/`status`, `assert_cmd`'s `assert()`, or a bare
//! `TcpListener::bind("127.0.0.1:0")` that is closed.

use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command, Output, Stdio};
use std::sync::RwLock;

static SPAWN_GATE: RwLock<()> = RwLock::new(());

/// A loopback port nothing listens on (and no child of this process holds open).
pub fn free_port() -> u16 {
    let _g = SPAWN_GATE.write().unwrap_or_else(|e| e.into_inner());
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// [`free_port`] as a loopback address (a connect to it is refused).
pub fn free_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], free_port()))
}

/// `c.spawn()`, never concurrently with [`free_port`].
pub fn spawn(c: &mut Command) -> Child {
    let _g = SPAWN_GATE.read().unwrap_or_else(|e| e.into_inner());
    c.spawn().unwrap()
}

/// `c.output()` through [`spawn`]: stdout and stderr captured (set stdin yourself).
pub fn output(c: &mut Command) -> Output {
    spawn(c.stdout(Stdio::piped()).stderr(Stdio::piped()))
        .wait_with_output()
        .unwrap()
}

/// [`output`] with stdin closed, as an `assert_cmd` assertion.
pub fn assert(c: &mut Command) -> assert_cmd::assert::Assert {
    use assert_cmd::assert::OutputAssertExt;
    output(c.stdin(Stdio::null())).assert()
}
