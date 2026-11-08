//! The localhost OAuth redirect listener: `127.0.0.1:<ephemeral>`, path `/callback` only (Claude);
//! or, via [`bind_first`], the first free port of a fixed list with a provider path (Codex:
//! `/auth/callback`).
//!
//! The `state` parameter is validated in constant time before anything else; a request with a
//! wrong or missing state is answered 400 and does NOT consume the listener (a stray request must
//! not be able to kill a sign-in). The first request with a valid state is delivered to the
//! handler, every later one is refused. Closing the listener is the owner's job (`shutdown`).

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use futures_util::future::BoxFuture;
use tokio::net::TcpListener;

use super::pkce::state_eq;

/// What the browser redirect carried (the code is a secret: never logged, `Debug` aside).
#[derive(Clone, PartialEq, Eq)]
pub enum CallbackEvent {
    Code(String),
    /// The provider redirected with `error=...`. `on_hold` is the `account_on_hold` case.
    Denied {
        error: String,
        on_hold: bool,
    },
}

impl std::fmt::Debug for CallbackEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallbackEvent::Code(_) => f.write_str("Code(<redacted>)"),
            CallbackEvent::Denied { error, on_hold } => f
                .debug_struct("Denied")
                .field("error", error)
                .field("on_hold", on_hold)
                .finish(),
        }
    }
}

/// Receives the one valid event and returns the page to show the user (status, HTML).
pub type Handler =
    Arc<dyn Fn(CallbackEvent) -> BoxFuture<'static, (StatusCode, String)> + Send + Sync>;

/// A bound, not yet serving listener (so the port is known before the authorize URL is built).
pub struct BoundCallback {
    listener: TcpListener,
    port: u16,
    path: &'static str,
}

/// Bind `127.0.0.1:0`, serving `/callback`. Must be called inside a tokio runtime.
pub fn bind() -> io::Result<BoundCallback> {
    bind_port(0, "/callback")
}

/// Bind the first of `ports` that is free (`0` = any free port), serving `path`. Errors with the
/// last bind error when none is. Must be called inside a tokio runtime.
pub fn bind_first(ports: &[u16], path: &'static str) -> io::Result<BoundCallback> {
    let mut last = io::Error::new(io::ErrorKind::InvalidInput, "no callback port configured");
    for &port in ports {
        match bind_port(port, path) {
            Ok(b) => return Ok(b),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn bind_port(port: u16, path: &'static str) -> io::Result<BoundCallback> {
    let std = std::net::TcpListener::bind(("127.0.0.1", port))?;
    std.set_nonblocking(true)?;
    let port = std.local_addr()?.port();
    Ok(BoundCallback {
        listener: TcpListener::from_std(std)?,
        port,
        path,
    })
}

struct Shared {
    state: String,
    used: AtomicBool,
    handler: Handler,
}

impl BoundCallback {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Serve until `shutdown` resolves (then stop accepting; in-flight requests finish). The
    /// returned task ends once the listener is closed and open connections are drained.
    pub fn serve(
        self,
        expected_state: String,
        handler: Handler,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let shared = Arc::new(Shared {
            state: expected_state,
            used: AtomicBool::new(false),
            handler,
        });
        let app = Router::new()
            .route(self.path, get(callback))
            .with_state(shared);
        tokio::spawn(async move {
            let _ = axum::serve(self.listener, app)
                .with_graceful_shutdown(shutdown)
                .await;
        })
    }
}

/// The minimal HTML page shown in the browser. `msg` must be a fixed string, never user input.
pub fn page_html(msg: &str) -> String {
    format!("<!doctype html><meta charset=utf-8><title>pooltime sign-in</title><p>{msg}</p>")
}

fn page(status: StatusCode, msg: &str) -> (StatusCode, Html<String>) {
    (status, Html(page_html(msg)))
}

async fn callback(
    State(s): State<Arc<Shared>>,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, Html<String>) {
    let state_ok = q.get("state").is_some_and(|got| state_eq(got, &s.state));
    if !state_ok {
        return page(
            StatusCode::BAD_REQUEST,
            "Invalid sign-in link (state mismatch). Start the sign-in again.",
        );
    }
    let event = if let Some(error) = q.get("error").filter(|e| !e.is_empty()) {
        CallbackEvent::Denied {
            error: error
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                .take(64)
                .collect(),
            on_hold: q.get("error_description").map(String::as_str) == Some("account_on_hold"),
        }
    } else if let Some(code) = q.get("code").filter(|c| !c.is_empty()) {
        CallbackEvent::Code(code.clone())
    } else {
        return page(
            StatusCode::BAD_REQUEST,
            "The sign-in redirect carried no code.",
        );
    };
    if s.used.swap(true, Ordering::SeqCst) {
        return page(
            StatusCode::BAD_REQUEST,
            "This sign-in link was already used. Start the sign-in again if needed.",
        );
    }
    // Hyper drops this future when the browser tab closes. The exchange and save must not go
    // with it (a half-run flow stays `Exchanging` forever and keeps its port), so they run on
    // their own task and the page only waits for them (spec §16).
    match tokio::spawn((s.handler)(event)).await {
        Ok((status, body)) => (status, Html(body)),
        Err(e) => {
            tracing::error!(panic = e.is_panic(), "sign-in callback handler failed");
            page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Sign-in failed. Return to pooltime for details.",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::sync::oneshot;

    struct Server {
        port: u16,
        events: Arc<Mutex<Vec<CallbackEvent>>>,
        stop: oneshot::Sender<()>,
    }

    fn start(state: &str) -> Server {
        let bound = bind().unwrap();
        let port = bound.port();
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        let handler: Handler = Arc::new(move |ev| {
            seen.lock().unwrap().push(ev);
            Box::pin(async { (StatusCode::OK, "handled".to_string()) })
        });
        let (stop, rx) = oneshot::channel::<()>();
        bound.serve(state.to_string(), handler, async move {
            let _ = rx.await;
        });
        Server { port, events, stop }
    }

    async fn hit(port: u16, path_and_query: &str) -> (u16, String) {
        let r = reqwest::get(format!("http://127.0.0.1:{port}{path_and_query}"))
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    #[tokio::test]
    async fn callback_rejects_bad_state_and_is_one_shot() {
        let s = start("good-state");

        // Wrong, missing or empty state: 400, handler never called, flow stays armed.
        for q in [
            "/callback?code=c1&state=evil",
            "/callback?code=c1",
            "/callback?code=c1&state=",
            "/callback?state=good-state",
        ] {
            let (status, _) = hit(s.port, q).await;
            assert_eq!(status, 400, "{q}");
        }
        assert!(s.events.lock().unwrap().is_empty());

        // Only /callback is served.
        assert_eq!(hit(s.port, "/other?code=c1&state=good-state").await.0, 404);

        // The first valid hit is delivered...
        let (status, body) = hit(s.port, "/callback?code=c1&state=good-state").await;
        assert_eq!((status, body.as_str()), (200, "handled"));
        // ...and the second is refused with an error page, even with a valid state.
        let (status, body) = hit(s.port, "/callback?code=c2&state=good-state").await;
        assert_eq!(status, 400);
        assert!(body.contains("already"), "{body}");
        assert_eq!(
            *s.events.lock().unwrap(),
            vec![CallbackEvent::Code("c1".into())]
        );
    }

    #[tokio::test]
    async fn handler_survives_dropped_connection() {
        use tokio::io::AsyncWriteExt;
        let bound = bind().unwrap();
        let port = bound.port();
        let (started_tx, started) = oneshot::channel::<()>();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let (finished_tx, finished) = oneshot::channel::<()>();
        let finished_tx = Arc::new(Mutex::new(Some(finished_tx)));
        let handler: Handler = Arc::new(move |_| {
            let (s, f) = (started_tx.clone(), finished_tx.clone());
            Box::pin(async move {
                let _ = s.lock().unwrap().take().unwrap().send(());
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = f.lock().unwrap().take().unwrap().send(());
                (StatusCode::OK, "handled".to_string())
            })
        });
        let (_stop, rx) = oneshot::channel::<()>();
        bound.serve("st".into(), handler, async move {
            let _ = rx.await;
        });
        let mut tab = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        tab.write_all(b"GET /callback?code=c&state=st HTTP/1.1\r\nhost: localhost\r\n\r\n")
            .await
            .unwrap();
        started.await.unwrap();
        drop(tab);
        tokio::time::timeout(Duration::from_secs(5), finished)
            .await
            .expect("handler cancelled with the connection")
            .unwrap();
    }

    #[tokio::test]
    async fn provider_error_is_delivered_once() {
        let s = start("st");
        let (status, _) = hit(
            s.port,
            "/callback?error=access_denied&error_description=account_on_hold&state=st",
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            *s.events.lock().unwrap(),
            vec![CallbackEvent::Denied {
                error: "access_denied".into(),
                on_hold: true
            }]
        );
    }

    #[tokio::test]
    async fn shutdown_closes_the_listener() {
        let s = start("st");
        assert_eq!(hit(s.port, "/callback?code=c&state=st").await.0, 200);
        let port = s.port;
        s.stop.send(()).unwrap();
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("listener still accepting after shutdown");
    }
}
