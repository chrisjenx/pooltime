//! The worker side of the supervisor protocol ([`crate::supervisor`]): take over the inherited
//! listening and control sockets (`serve --listen-fd N --control-fd M`), report `ready`, and
//! follow the supervisor's `active` / `handoff` directives.

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

use anyhow::{anyhow, bail, Context};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

use crate::supervisor::proto::{FromWorker, ToWorker};

/// The sockets a supervisor passed down.
pub struct Inherited {
    pub listener: std::net::TcpListener,
    pub control: UnixStream,
}

fn fd_is_socket(fd: RawFd) -> bool {
    // SAFETY: fstat writes into a zeroed stat buffer we own.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFSOCK
    }
}

/// A stream socket (`SO_ACCEPTCONN` is not readable on macOS; `local_addr` then tells TCP).
fn is_stream(fd: RawFd) -> bool {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt into a c_int we own, with its size.
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut v as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    r == 0 && v == libc::SOCK_STREAM
}

fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl on an fd this process owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

impl Inherited {
    /// Take ownership of the inherited descriptors. Call before anything else could know their
    /// numbers. They are made close-on-exec again so nothing this worker spawns inherits them.
    pub fn take(listen_fd: RawFd, control_fd: RawFd) -> anyhow::Result<Inherited> {
        if listen_fd < 3 || control_fd < 3 || listen_fd == control_fd {
            bail!(
                "--listen-fd/--control-fd must be two distinct inherited descriptors (3 or above)"
            );
        }
        if !fd_is_socket(listen_fd) || !is_stream(listen_fd) {
            bail!("--listen-fd {listen_fd} is not a listening socket");
        }
        if !fd_is_socket(control_fd) {
            bail!("--control-fd {control_fd} is not a socket");
        }
        // SAFETY: both are open sockets this process inherited from the supervisor (checked
        // above); nothing else in this process refers to them, so each gets exactly one owner.
        let (listener, control) = unsafe {
            (
                std::net::TcpListener::from_raw_fd(listen_fd),
                UnixStream::from_raw_fd(control_fd),
            )
        };
        for fd in [listener.as_raw_fd(), control.as_raw_fd()] {
            set_cloexec(fd).context("marking an inherited socket close-on-exec")?;
        }
        listener
            .local_addr()
            .map_err(|e| anyhow!("--listen-fd {listen_fd} is not a TCP listener: {e}"))?;
        listener.set_nonblocking(true)?;
        Ok(Inherited { listener, control })
    }
}

/// What the supervisor has told this worker so far (only ever moves forward).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Directive {
    /// Serving, but another generation still runs the background tasks.
    Standby,
    /// Run the background tasks.
    Active,
    /// Stop accepting, stop the background tasks, drain. Also when the supervisor is gone.
    HandOff,
}

/// The worker's end of the control socket.
pub struct Control {
    writer: UnixStream,
    rx: watch::Receiver<Directive>,
}

impl Control {
    /// Start reading directives from `stream` (a reader task; must run inside a runtime).
    pub fn start(stream: UnixStream) -> anyhow::Result<Control> {
        let writer = stream.try_clone()?;
        stream.set_nonblocking(true)?;
        let reader = tokio::net::UnixStream::from_std(stream)?;
        let (tx, rx) = watch::channel(Directive::Standby);
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            let advance = |d: Directive| {
                tx.send_if_modified(|cur| {
                    let moved = d > *cur;
                    if moved {
                        *cur = d;
                    }
                    moved
                });
            };
            loop {
                match lines.next_line().await {
                    Ok(Some(l)) => match ToWorker::parse(&l) {
                        Some(ToWorker::Active) => advance(Directive::Active),
                        Some(ToWorker::Handoff) => advance(Directive::HandOff),
                        None => tracing::debug!("ignoring an unknown supervisor message"),
                    },
                    _ => {
                        if *tx.borrow() != Directive::HandOff {
                            tracing::warn!(
                                "the supervisor is gone: no longer accepting, draining and exiting"
                            );
                        }
                        advance(Directive::HandOff);
                        // Keep the sender alive so waiters see the final value.
                        std::future::pending::<()>().await;
                    }
                }
            }
        });
        Ok(Control { writer, rx })
    }

    pub fn send(&self, msg: &FromWorker) {
        if let Err(e) = (&self.writer).write_all(msg.line().as_bytes()) {
            tracing::warn!(error = %e, "could not reach the supervisor");
        }
    }

    pub fn directives(&self) -> watch::Receiver<Directive> {
        self.rx.clone()
    }
}

/// Wait until the directive is at least `d`.
pub async fn reached(rx: &mut watch::Receiver<Directive>, d: Directive) {
    let _ = rx.wait_for(|cur| *cur >= d).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn directives_move_forward_and_eof_means_handoff() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let ctl = Control::start(theirs).unwrap();
        let mut rx = ctl.directives();
        assert_eq!(*rx.borrow(), Directive::Standby);

        ctl.send(&FromWorker::Ready {
            version: "9.9.9".into(),
        });
        let mut got = [0u8; 10];
        std::io::Read::read_exact(&mut &ours, &mut got).unwrap();
        assert_eq!(&got, b"ready 9.9.");

        (&ours).write_all(b"active\n").unwrap();
        tokio::time::timeout(Duration::from_secs(5), reached(&mut rx, Directive::Active))
            .await
            .unwrap();
        assert_eq!(*rx.borrow(), Directive::Active);
        // Unknown lines change nothing.
        (&ours).write_all(b"something new\n").unwrap();
        drop(ours);
        tokio::time::timeout(Duration::from_secs(5), reached(&mut rx, Directive::HandOff))
            .await
            .unwrap();
    }

    #[test]
    fn take_refuses_what_is_not_an_inherited_listener() {
        let (a, b) = UnixStream::pair().unwrap();
        let err = Inherited::take(1, 2).err().unwrap();
        assert!(err.to_string().contains("3 or above"), "{err}");
        let f = std::fs::File::open("Cargo.toml").unwrap();
        let err = Inherited::take(f.as_raw_fd(), b.as_raw_fd()).err().unwrap();
        assert!(err.to_string().contains("not a listening socket"), "{err}");
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(is_stream(l.as_raw_fd()) && is_stream(a.as_raw_fd()));
        let u = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(!is_stream(u.as_raw_fd()));
    }
}
