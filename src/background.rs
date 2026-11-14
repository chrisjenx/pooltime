//! Blocking work started by the daemon's background tasks (profile sync, status-cache writes,
//! usage sampling, notifications). Aborting a task does not stop a `spawn_blocking` closure it
//! started, so a daemon generation that hands off waits ([`Work::idle`]) until all of them have
//! finished before it tells the supervisor its background work is released: the next
//! generation is then the only writer.
//!
//! Only the background tasks' own work is counted. The tasks run in a [`Work`] scope
//! ([`Work::enter`] around their [`spawn`]s, which carry it into the tasks), and [`blocking`]
//! counts against the scope of the task that calls it. A request handler has no scope, so the
//! same code on the request path (a profile sync started by `connect`, a model refresh) runs
//! uncounted and never holds up a handoff.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

tokio::task_local! {
    static SCOPE: Work;
}

/// The blocking work of one set of background tasks.
#[derive(Debug, Clone, Default)]
pub struct Work(Arc<AtomicUsize>);

impl Work {
    pub fn new() -> Work {
        Work::default()
    }

    /// Run `f` with this as the current scope: tasks it starts with [`spawn`] belong to it.
    pub fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        SCOPE.sync_scope(self.clone(), f)
    }

    /// Blocking work of this scope running now.
    pub fn in_flight(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    /// Wait until none of this scope's blocking work runs.
    pub async fn idle(&self) {
        while self.in_flight() > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

struct Tracked(Work);

impl Drop for Tracked {
    fn drop(&mut self) {
        self.0 .0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `tokio::spawn`, carrying the caller's [`Work`] scope (if any) into the new task.
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    match SCOPE.try_with(Work::clone) {
        Ok(work) => tokio::spawn(SCOPE.scope(work, fut)),
        Err(_) => tokio::spawn(fut),
    }
}

/// `tokio::task::spawn_blocking(f)`; inside a [`Work`] scope, counted there until `f` returns
/// (even if the awaiting task is aborted meanwhile).
pub async fn blocking<F, T>(f: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let guard = SCOPE
        .try_with(|work| {
            work.0.fetch_add(1, Ordering::SeqCst);
            Tracked(work.clone())
        })
        .ok();
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        f()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    async fn until_set(flag: &AtomicBool) {
        while !flag.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn idle_waits_for_work_whose_task_was_aborted() {
        let work = Work::new();
        let (started, done) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (s, d) = (started.clone(), done.clone());
        // A nested spawn keeps the scope.
        let task = work.enter(|| {
            spawn(async move {
                spawn(blocking(move || {
                    s.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(200));
                    d.store(true, Ordering::SeqCst);
                }))
                .await
            })
        });
        until_set(&started).await;
        task.abort();
        tokio::time::timeout(Duration::from_secs(5), work.idle())
            .await
            .unwrap();
        assert!(
            done.load(Ordering::SeqCst),
            "idle returned before the work finished"
        );
    }

    /// Request-path blocking work (no scope) and another scope's work never hold up `idle`.
    #[tokio::test]
    async fn idle_ignores_work_outside_its_scope() {
        let (work, other) = (Work::new(), Work::new());
        let (started, release) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let stuck = |started: Arc<AtomicBool>, release: Arc<AtomicBool>| {
            blocking(move || {
                started.store(true, Ordering::SeqCst);
                while !release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };
        // A request handler: a plain task, no scope.
        let request = tokio::spawn(stuck(started.clone(), release.clone()));
        until_set(&started).await;
        // Another generation's (or set's) background work.
        let started2 = Arc::new(AtomicBool::new(false));
        let theirs = other.enter(|| spawn(stuck(started2.clone(), release.clone())));
        until_set(&started2).await;
        assert_eq!(work.in_flight(), 0);
        assert_eq!(other.in_flight(), 1);
        tokio::time::timeout(Duration::from_millis(500), work.idle())
            .await
            .expect("idle waited for work that is not its own");
        release.store(true, Ordering::SeqCst);
        request.await.unwrap().unwrap();
        theirs.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(5), other.idle())
            .await
            .unwrap();
    }
}
