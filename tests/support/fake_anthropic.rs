//! Scriptable fake upstream on 127.0.0.1:0 (hyper 1, HTTP/1.1).
//!
//! Each path has a handler `Fn(&Recorded, hit_index) -> Reply`; [`FakeAnthropic::script`] is the
//! common case (replies in order, the last one repeating). A [`Reply`] can be a plain body, SSE
//! frames with per-frame delays, delayed headers, a connection dropped after N body bytes, or a
//! body held silent (optionally forever). Every request is recorded (method, path, query, headers,
//! body bytes) before its reply is computed. Unscripted paths get a 404.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

pub const MESSAGES: &str = "/v1/messages";
pub const TOKEN: &str = "/v1/oauth/token";

/// One request the fake received.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("recorded body is JSON")
    }
}

/// How the body ends after its frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// Close normally.
    None,
    /// Stay open and silent this long, then close normally.
    For(Duration),
    /// Stay open and silent until the client goes away.
    Forever,
}

/// A scripted response.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Body frames, each sent after its delay.
    pub frames: Vec<(Duration, Bytes)>,
    /// Wait this long before sending the response headers.
    pub header_delay: Duration,
    /// Abort the connection once this many body bytes have been sent (frames are cut to fit).
    pub drop_after: Option<usize>,
    pub hold: Hold,
}

impl Reply {
    pub fn new(status: u16) -> Reply {
        Reply {
            status,
            headers: Vec::new(),
            frames: Vec::new(),
            header_delay: Duration::ZERO,
            drop_after: None,
            hold: Hold::None,
        }
    }

    /// Raw body bytes (no content-type).
    pub fn bytes(status: u16, body: impl Into<Bytes>) -> Reply {
        let mut r = Reply::new(status);
        r.frames.push((Duration::ZERO, body.into()));
        r
    }

    /// JSON body with `content-type: application/json`.
    pub fn json(status: u16, body: &serde_json::Value) -> Reply {
        Reply::bytes(status, body.to_string()).header("content-type", "application/json")
    }

    /// Anthropic error envelope `{"type":"error","error":{"type":..,"message":..}}`.
    pub fn error(status: u16, kind: &str, message: &str) -> Reply {
        Reply::json(
            status,
            &serde_json::json!({"type": "error", "error": {"type": kind, "message": message}}),
        )
    }

    /// `text/event-stream` body of `(delay, frame)` pairs.
    pub fn sse(frames: Vec<(Duration, String)>) -> Reply {
        let mut r = Reply::new(200).header("content-type", "text/event-stream");
        r.frames = frames
            .into_iter()
            .map(|(d, f)| (d, Bytes::from(f)))
            .collect();
        r
    }

    pub fn header(mut self, name: &str, value: &str) -> Reply {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn delay_headers(mut self, d: Duration) -> Reply {
        self.header_delay = d;
        self
    }

    pub fn drop_after_bytes(mut self, n: usize) -> Reply {
        self.drop_after = Some(n);
        self
    }

    pub fn hold(mut self, hold: Hold) -> Reply {
        self.hold = hold;
        self
    }
}

type Handler = Arc<dyn Fn(&Recorded, usize) -> Reply + Send + Sync>;

#[derive(Default)]
struct Inner {
    handlers: HashMap<String, Handler>,
    hits: HashMap<String, usize>,
    requests: Vec<Recorded>,
}

#[derive(Clone)]
pub struct FakeAnthropic {
    pub addr: SocketAddr,
    inner: Arc<Mutex<Inner>>,
}

impl FakeAnthropic {
    pub async fn start() -> FakeAnthropic {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let inner: Arc<Mutex<Inner>> = Arc::default();
        let shared = inner.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let inner = shared.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        let inner = inner.clone();
                        async move { Ok::<_, Infallible>(serve(inner, req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        FakeAnthropic { addr, inner }
    }

    /// Base URL, e.g. `http://127.0.0.1:1234` (no trailing slash).
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Answer `path` with `f(request, hit_index)`.
    pub fn route(&self, path: &str, f: impl Fn(&Recorded, usize) -> Reply + Send + Sync + 'static) {
        self.lock().handlers.insert(path.into(), Arc::new(f));
    }

    /// Answer `path` with `replies` in order; the last one repeats.
    pub fn script(&self, path: &str, replies: Vec<Reply>) {
        assert!(!replies.is_empty(), "script needs at least one reply");
        self.route(path, move |_, i| replies[i.min(replies.len() - 1)].clone());
    }

    /// Every request received so far (all paths), in arrival order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.lock().requests.clone()
    }

    /// Requests received on `path`.
    pub fn requests_to(&self, path: &str) -> Vec<Recorded> {
        self.lock()
            .requests
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

type Body = BoxBody<Bytes, std::io::Error>;

async fn serve(inner: Arc<Mutex<Inner>>, req: Request<hyper::body::Incoming>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    let rec = Recorded {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().map(str::to_owned),
        headers: parts.headers,
        body,
    };
    let reply = {
        let mut g = inner.lock().unwrap_or_else(|e| e.into_inner());
        g.requests.push(rec.clone());
        let handler = g.handlers.get(&rec.path).cloned();
        let hit = g.hits.entry(rec.path.clone()).or_insert(0);
        let i = *hit;
        *hit += 1;
        drop(g);
        match handler {
            Some(h) => h(&rec, i),
            None => Reply::error(404, "not_found_error", "unscripted path"),
        }
    };
    tokio::time::sleep(reply.header_delay).await;
    let mut resp = Response::new(body_of(&reply));
    *resp.status_mut() = http::StatusCode::from_u16(reply.status).unwrap();
    for (k, v) in &reply.headers {
        resp.headers_mut().append(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    resp
}

/// Frames (cut at `drop_after`), then an abort or the hold.
fn body_of(reply: &Reply) -> Body {
    let mut frames = Vec::new();
    let mut left = reply.drop_after.unwrap_or(usize::MAX);
    for (d, b) in &reply.frames {
        if b.len() >= left {
            frames.push((*d, b.slice(..left)));
            break;
        }
        frames.push((*d, b.clone()));
        left -= b.len();
    }
    let abort = reply.drop_after.is_some();
    let hold = reply.hold;
    let stream = futures_util::stream::unfold(
        (frames.into_iter(), false),
        move |(mut it, done)| async move {
            if done {
                return None;
            }
            if let Some((d, b)) = it.next() {
                tokio::time::sleep(d).await;
                return Some((Ok(Frame::data(b)), (it, false)));
            }
            if abort {
                let e = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "fake drop");
                return Some((Err(e), (it, true)));
            }
            match hold {
                Hold::None => {}
                Hold::For(d) => tokio::time::sleep(d).await,
                Hold::Forever => std::future::pending::<()>().await,
            }
            None
        },
    );
    BodyExt::boxed(StreamBody::new(stream))
}
