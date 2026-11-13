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

/// What the supervisor has told this worker so far. Every flag only ever turns on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Orders {
    /// Run the background tasks.
    pub active: bool,
    /// A newer generation serves: stop accepting, release the background tasks, drain.
    pub handoff: bool,
    /// The service is stopping: drain (503 for new requests meanwhile).
    pub stop: bool,
    /// Stop without waiting for in-flight requests.
    pub force: bool,
    /// The control socket closed: the supervisor is gone. Implies `handoff` (unless already
    /// stopping); from here on the worker's own OS signals count again.
    pub orphaned: bool,
}

impl Orders {
    /// Serving should end: how.
    pub fn ending(&self) -> Option<crate::server::StopKind> {
        if self.stop {
            Some(crate::server::StopKind::Stop)
        } else if self.handoff {
            Some(crate::server::StopKind::Handoff)
        } else {
            None
        }
    }
}

/// The worker's end of the control socket.
pub struct Control {
    writer: UnixStream,
    rx: watch::Receiver<Orders>,
}

impl Control {
    /// Start reading orders from `stream` (a reader task; must run inside a runtime).
    pub fn start(stream: UnixStream) -> anyhow::Result<Control> {
        let writer = stream.try_clone()?;
        stream.set_nonblocking(true)?;
        let reader = tokio::net::UnixStream::from_std(stream)?;
        let (tx, rx) = watch::channel(Orders::default());
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(l)) => {
                        let Some(msg) = ToWorker::parse(&l) else {
                            tracing::debug!("ignoring an unknown supervisor message");
                            continue;
                        };
                        tracing::info!(order = ?msg, "supervisor order");
                        tx.send_modify(|o| match msg {
                            ToWorker::Active => o.active = true,
                            ToWorker::Handoff => o.handoff = true,
                            ToWorker::Stop => o.stop = true,
                            ToWorker::Force => o.force = true,
                        });
                    }
                    _ => {
                        if tx.borrow().ending().is_none() {
                            tracing::warn!(
                                "the supervisor is gone: no longer accepting, draining and exiting"
                            );
                        }
                        tx.send_modify(|o| {
                            o.orphaned = true;
                            o.handoff = true;
                        });
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

    pub fn orders(&self) -> watch::Receiver<Orders> {
        self.rx.clone()
    }
}

/// Wait until `f` holds for the orders; the orders then.
pub async fn until(rx: &mut watch::Receiver<Orders>, f: impl Fn(&Orders) -> bool) -> Orders {
    let got = rx.wait_for(|o| f(o)).await.map(|o| *o).ok();
    got.unwrap_or_else(|| *rx.borrow())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn orders_accumulate_and_eof_means_orphaned_handoff() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let ctl = Control::start(theirs).unwrap();
        let mut rx = ctl.orders();
        assert_eq!(*rx.borrow(), Orders::default());

        ctl.send(&FromWorker::Ready {
            version: "9.9.9".into(),
        });
        let mut got = [0u8; 10];
        std::io::Read::read_exact(&mut &ours, &mut got).unwrap();
        assert_eq!(&got, b"ready 9.9.");

        let wait = |rx: &mut watch::Receiver<Orders>, f: fn(&Orders) -> bool| {
            let mut rx = rx.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(5), until(&mut rx, f))
                    .await
                    .unwrap()
            }
        };
        (&ours).write_all(b"active\n").unwrap();
        assert!(wait(&mut rx, |o| o.active).await.ending().is_none());
        // Unknown lines change nothing.
        (&ours).write_all(b"something new\nstop\n").unwrap();
        let o = wait(&mut rx, |o| o.stop).await;
        assert_eq!(o.ending(), Some(crate::server::StopKind::Stop));
        (&ours).write_all(b"force\n").unwrap();
        wait(&mut rx, |o| o.force).await;
        drop(ours);
        let o = wait(&mut rx, |o| o.orphaned).await;
        assert_eq!(
            o.ending(),
            Some(crate::server::StopKind::Stop),
            "a stop stays a stop"
        );
    }

    #[tokio::test]
    async fn a_vanished_supervisor_means_handoff() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let ctl = Control::start(theirs).unwrap();
        let mut rx = ctl.orders();
        drop(ours);
        let o = tokio::time::timeout(Duration::from_secs(5), until(&mut rx, |o| o.orphaned))
            .await
            .unwrap();
        assert_eq!(o.ending(), Some(crate::server::StopKind::Handoff));
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
