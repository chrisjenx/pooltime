//! Blocking work started by the daemon's background tasks (profile sync, status-cache writes,
//! usage sampling, notifications). Aborting a task does not stop a `spawn_blocking` closure it
//! started, so a daemon generation that hands off waits here ([`idle`]) until all of them have
//! finished before it tells the supervisor its background work is released: the next
//! generation is then the only writer.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

struct Tracked;

impl Drop for Tracked {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `tokio::task::spawn_blocking(f)`, counted until `f` returns (even if the awaiting task is
/// aborted meanwhile).
pub async fn blocking<F, T>(f: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
    let guard = Tracked;
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        f()
    })
    .await
}

/// Blocking background work running now.
pub fn in_flight() -> usize {
    IN_FLIGHT.load(Ordering::SeqCst)
}

/// Wait until no tracked blocking work runs.
pub async fn idle() {
    while in_flight() > 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[tokio::test]
    async fn idle_waits_for_work_whose_task_was_aborted() {
        let (started, done) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (s, d) = (started.clone(), done.clone());
        let task = tokio::spawn(blocking(move || {
            s.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            d.store(true, Ordering::SeqCst);
        }));
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        task.abort();
        tokio::time::timeout(Duration::from_secs(5), idle())
            .await
            .unwrap();
        assert!(
            done.load(Ordering::SeqCst),
            "idle returned before the work finished"
        );
    }
}
