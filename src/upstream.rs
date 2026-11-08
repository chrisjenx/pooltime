//! Upstream HTTP client with an idle watchdog.
//!
//! Streams to Anthropic/ChatGPT are long-lived SSE, so there is deliberately no total request
//! timeout. Instead three bounds apply: a header-wait bound on [`Upstream::send`], a per-read idle
//! bound on [`WatchedBody::next_chunk`], and a per-attempt deadline that caps both. Automatic
//! decompression is off (no gzip/brotli/deflate features), so bytes are relayed exactly as sent.

use std::error::Error as StdError;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http::{HeaderMap, StatusCode};
use tokio::time::{timeout_at, Instant};

use crate::errors::TransportKind;
use crate::logging::{debug_headers, redact_uri, DebugMode};

type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// Bounds for one upstream attempt.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Max wait for the response headers (covers connect + TLS + time to first byte of headers).
    pub header_wait: Duration,
    /// Max silence between demanded body chunks.
    pub idle: Duration,
    /// Absolute end of this attempt; caps both of the above.
    pub deadline: Instant,
}

/// Shared HTTP client (rustls, no decompression, no total timeout, pool idle 90 s).
#[derive(Clone)]
pub struct Upstream {
    client: reqwest::Client,
    /// While active, request/response headers of every send are logged (redacted).
    debug: DebugMode,
}

impl Default for Upstream {
    fn default() -> Self {
        Self::new()
    }
}

impl Upstream {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            // Never follow redirects: auth headers must not travel cross-origin and the caller
            // (failover logic) decides what a 3xx means.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("static reqwest client configuration is valid");
        Self {
            client,
            debug: DebugMode::default(),
        }
    }

    /// Share `debug` (the daemon hands the same switch to the management API).
    pub fn with_debug(mut self, debug: DebugMode) -> Self {
        self.debug = debug;
        self
    }

    pub fn debug(&self) -> &DebugMode {
        &self.debug
    }

    /// Send `req` and wait for the response headers.
    ///
    /// Header-wait expiry is classified [`TransportKind::IdleTimeout`]; the attempt deadline
    /// expiring first is [`TransportKind::Deadline`].
    pub async fn send(
        &self,
        req: reqwest::Request,
        lim: &Limits,
    ) -> Result<UpstreamResponse, TransportKind> {
        let (until, expiry) = bound(lim.header_wait, lim.deadline);
        if self.debug.active() {
            // Inside the request span, so the line carries the request id.
            tracing::info!(
                direction = "upstream_request",
                method = %req.method(),
                url = %redact_uri(req.url().as_str()),
                headers = %debug_headers(req.headers()),
                "debug headers"
            );
        }
        let resp = match timeout_at(until, self.client.execute(req)).await {
            Err(_) => return Err(expiry),
            Ok(Err(e)) => return Err(classify_reqwest(&e)),
            Ok(Ok(r)) => r,
        };
        let status = resp.status();
        let headers = resp.headers().clone();
        if self.debug.active() {
            tracing::info!(
                direction = "upstream_response",
                status = status.as_u16(),
                headers = %debug_headers(&headers),
                "debug headers"
            );
        }
        Ok(UpstreamResponse {
            status,
            headers,
            body: WatchedBody {
                stream: Box::pin(resp.bytes_stream()),
                idle: lim.idle,
                deadline: lim.deadline,
            },
        })
    }
}

/// `now + wait` capped by `deadline`, and which [`TransportKind`] its expiry means.
fn bound(wait: Duration, deadline: Instant) -> (Instant, TransportKind) {
    let at = Instant::now() + wait;
    if deadline <= at {
        (deadline, TransportKind::Deadline)
    } else {
        (at, TransportKind::IdleTimeout)
    }
}

pub struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: WatchedBody,
}

/// Response body whose reads are bounded by an idle timer and the attempt deadline.
pub struct WatchedBody {
    stream: BodyStream,
    idle: Duration,
    deadline: Instant,
}

impl WatchedBody {
    /// Next non-empty chunk, `None` at end of body.
    ///
    /// The idle timer starts when this is called (idle is per demanded read, so a slow consumer is
    /// not blamed on upstream) and is NOT re-armed by empty frames. Expiry yields
    /// `Some(Err(IdleTimeout))`, or `Some(Err(Deadline))` if the attempt deadline comes first.
    pub async fn next_chunk(&mut self) -> Option<Result<Bytes, TransportKind>> {
        next_nonempty(&mut self.stream, self.idle, self.deadline, classify_reqwest).await
    }

    /// Read the remaining body, bounded by the same limits.
    pub async fn read_all(mut self) -> Result<Bytes, TransportKind> {
        let mut buf = Vec::new();
        while let Some(chunk) = self.next_chunk().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(Bytes::from(buf))
    }

    /// Read the remaining body as (lossy UTF-8) text, bounded by the same limits.
    pub async fn text(self) -> Result<String, TransportKind> {
        let buf = self.read_all().await?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

async fn next_nonempty<S, E>(
    stream: &mut S,
    idle: Duration,
    deadline: Instant,
    classify: impl Fn(&E) -> TransportKind,
) -> Option<Result<Bytes, TransportKind>>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    let (until, expiry) = bound(idle, deadline);
    let read = async {
        loop {
            match stream.next().await {
                Some(Ok(b)) if b.is_empty() => continue,
                other => return other,
            }
        }
    };
    match timeout_at(until, read).await {
        Err(_) => Some(Err(expiry)),
        Ok(None) => None,
        Ok(Some(Ok(b))) => Some(Ok(b)),
        Ok(Some(Err(e))) => Some(Err(classify(&e))),
    }
}

/// Join an upstream base URL and a request path.
///
/// Tolerates a base that already ends in `/v1` (the path's leading `/v1` is not duplicated), a
/// trailing slash on the base and a missing leading slash on the path; never drops other path
/// prefixes on the base.
pub fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let mut path = path.trim_start_matches('/');
    if base.ends_with("/v1") {
        if let Some(rest) = path.strip_prefix("v1") {
            if rest.is_empty() || rest.starts_with('/') {
                path = rest.trim_start_matches('/');
            }
        }
    }
    if path.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{path}")
    }
}

/// Classify a reqwest error into a [`TransportKind`] (never carries the message onward).
pub fn classify_reqwest(e: &reqwest::Error) -> TransportKind {
    classify_chain(e, e.is_timeout(), e.is_connect())
}

fn classify_chain(
    e: &(dyn StdError + 'static),
    is_timeout: bool,
    is_connect: bool,
) -> TransportKind {
    if is_timeout {
        return TransportKind::IdleTimeout;
    }
    let (mut dns, mut tls, mut reset, mut refused) = (false, false, false, false);
    let mut cur: Option<&(dyn StdError + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            match io.kind() {
                ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof => reset = true,
                ConnectionRefused => refused = true,
                _ => {}
            }
        }
        let msg = err.to_string().to_ascii_lowercase();
        dns |= msg.contains("dns error")
            || msg.contains("failed to lookup")
            || msg.contains("name resolution")
            || msg.contains("nodename nor servname");
        tls |= msg.contains("tls handshake")
            || msg.contains("rustls")
            || msg.contains("certificate")
            || msg.contains("invalid peer")
            || msg.contains("received fatal alert");
        reset |= msg.contains("connection closed before message completed");
        // HTTP/2 RST_STREAM / GOAWAY surface as h2::Error with no io source.
        if let Some(h2e) = err.downcast_ref::<h2::Error>() {
            reset |= h2e.reason().is_some(); // RST_STREAM, GOAWAY or bare reason
        }
        reset |= msg.contains("stream error received")
            || msg.contains("connection error received")
            || msg.contains("connection error detected")
            || msg.contains("go away");
        cur = err.source();
    }
    if dns {
        TransportKind::Dns
    } else if tls {
        TransportKind::Tls
    } else if reset {
        TransportKind::Reset
    } else if refused || is_connect {
        TransportKind::Connect
    } else {
        TransportKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::fmt;
    use std::io::ErrorKind;

    use http_body_util::{BodyExt, StreamBody};
    use hyper::body::Frame;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const IDLE: Duration = Duration::from_millis(200);

    fn limits(header_wait: Duration, idle: Duration, deadline_in: Duration) -> Limits {
        Limits {
            header_wait,
            idle,
            deadline: Instant::now() + deadline_in,
        }
    }

    fn get(url: &str) -> reqwest::Request {
        reqwest::Request::new(reqwest::Method::GET, url.parse().unwrap())
    }

    type TestBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

    /// Serve every connection with `handler` (hyper 1, HTTP/1) on 127.0.0.1:0.
    async fn serve<F, Fut>(handler: F) -> String
    where
        F: Fn(Request<hyper::body::Incoming>) -> Fut + Clone + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response<TestBody>> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        let handler = handler.clone();
                        async move { Ok::<_, Infallible>(handler(req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// Body that yields `frames` (each after its delay) then stays open for `hold`.
    fn timed_body(frames: Vec<(Duration, Bytes)>, hold: Duration) -> TestBody {
        let s =
            futures_util::stream::unfold((frames.into_iter(), hold), |(mut it, hold)| async move {
                if let Some((d, b)) = it.next() {
                    tokio::time::sleep(d).await;
                    return Some((Ok::<_, Infallible>(Frame::data(b)), (it, hold)));
                }
                tokio::time::sleep(hold).await;
                None
            });
        BodyExt::boxed(StreamBody::new(s))
    }

    #[tokio::test]
    async fn idle_watchdog_fires_and_empty_chunks_do_not_rearm() {
        // One data frame, then empty frames every 50ms for well over the idle window.
        let frames: Vec<_> = std::iter::once((Duration::ZERO, Bytes::from_static(b"data: 1\n\n")))
            .chain((0..40).map(|_| (Duration::from_millis(50), Bytes::new())))
            .collect();
        let url = serve(move |_| {
            let frames = frames.clone();
            async move { Response::new(timed_body(frames, Duration::ZERO)) }
        })
        .await;

        let up = Upstream::new();
        let lim = limits(Duration::from_secs(2), IDLE, Duration::from_secs(10));
        let mut resp = up.send(get(&url), &lim).await.unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        let first = resp.body.next_chunk().await.unwrap().unwrap();
        assert_eq!(&first[..], b"data: 1\n\n");
        let t0 = Instant::now();
        let err = resp.body.next_chunk().await.unwrap().unwrap_err();
        let waited = t0.elapsed();
        assert_eq!(err, TransportKind::IdleTimeout);
        // Lower bound only: an upper bound would flake on a loaded CI box. That empty frames do
        // not re-arm the timer is proven deterministically by the paused-time test below.
        assert!(
            waited >= IDLE - Duration::from_millis(20),
            "fired at {waited:?}, expected >= ~{IDLE:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn empty_frames_are_skipped_without_rearming_the_timer() {
        // Pure-logic check of the skip loop (HTTP/1 never delivers empty frames to the client).
        let s = futures_util::stream::unfold(0u32, |n| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Some((Ok::<_, String>(Bytes::new()), n + 1))
        });
        let mut s = Box::pin(s);
        let t0 = Instant::now();
        // Outer timeout (virtual time) so a re-arming regression fails instead of hanging.
        let r = tokio::time::timeout(
            Duration::from_secs(10),
            next_nonempty(
                &mut s,
                Duration::from_millis(200),
                t0 + Duration::from_secs(60),
                |_: &String| TransportKind::Other,
            ),
        )
        .await
        .expect("idle timer was re-armed by empty frames");
        assert_eq!(r.unwrap().unwrap_err(), TransportKind::IdleTimeout);
        assert_eq!(t0.elapsed(), Duration::from_millis(200));

        // A real chunk after an empty one is returned.
        let mut s = Box::pin(futures_util::stream::iter(vec![
            Ok::<_, String>(Bytes::new()),
            Ok(Bytes::from_static(b"x")),
        ]));
        let r = next_nonempty(&mut s, IDLE, t0 + Duration::from_secs(60), |_: &String| {
            TransportKind::Other
        })
        .await;
        assert_eq!(&r.unwrap().unwrap()[..], b"x");
    }

    #[tokio::test]
    async fn header_wait_timeout_classified() {
        let url = serve(|_| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Response::new(timed_body(vec![], Duration::ZERO))
        })
        .await;
        let up = Upstream::new();
        let lim = limits(Duration::from_millis(150), IDLE, Duration::from_secs(10));
        let t0 = Instant::now();
        let err = up.send(get(&url), &lim).await.err().unwrap();
        assert_eq!(err, TransportKind::IdleTimeout);
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn deadline_classified() {
        let url = serve(|_| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Response::new(timed_body(vec![], Duration::ZERO))
        })
        .await;
        let up = Upstream::new();

        // Deadline before header-wait while waiting for headers.
        let lim = limits(Duration::from_secs(5), IDLE, Duration::from_millis(150));
        let err = up.send(get(&url), &lim).await.err().unwrap();
        assert_eq!(err, TransportKind::Deadline);

        // Deadline hits mid-body even though chunks keep arriving inside the idle window.
        let url = serve(|_| async {
            let frames = (0..100)
                .map(|_| (Duration::from_millis(40), Bytes::from_static(b"x")))
                .collect();
            Response::new(timed_body(frames, Duration::ZERO))
        })
        .await;
        let lim = limits(Duration::from_secs(2), IDLE, Duration::from_millis(300));
        let mut resp = up.send(get(&url), &lim).await.unwrap();
        let err = loop {
            match resp.body.next_chunk().await.expect("deadline before eof") {
                Ok(_) => continue,
                Err(e) => break e,
            }
        };
        assert_eq!(err, TransportKind::Deadline);
    }

    #[tokio::test]
    async fn reset_classified() {
        // Promise 100 bytes, send 7, then drop the socket mid-body.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let mut seen = Vec::new();
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = s.read(&mut buf).await.unwrap();
                seen.extend_from_slice(&buf[..n]);
            }
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\npartial")
                .await
                .unwrap();
            s.flush().await.unwrap();
            drop(s);
        });
        let up = Upstream::new();
        let lim = limits(
            Duration::from_secs(2),
            Duration::from_secs(2),
            Duration::from_secs(5),
        );
        let mut resp = up.send(get(&format!("http://{addr}")), &lim).await.unwrap();
        let mut last = None;
        while let Some(c) = resp.body.next_chunk().await {
            last = Some(c);
        }
        assert_eq!(last.unwrap().unwrap_err(), TransportKind::Reset);
    }

    #[tokio::test]
    async fn connect_refused_classified() {
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        let up = Upstream::new();
        let lim = limits(Duration::from_secs(2), IDLE, Duration::from_secs(5));
        let err = up
            .send(get(&format!("http://{addr}")), &lim)
            .await
            .err()
            .unwrap();
        assert_eq!(err, TransportKind::Connect);
    }

    #[tokio::test]
    async fn no_auto_decompression() {
        // Not valid gzip on purpose: it must come back byte-for-byte, header intact.
        let payload: &'static [u8] = &[0x1f, 0x8b, 0x08, 0x00, 0xde, 0xad, 0xbe, 0xef];
        let url = serve(move |_| async move {
            let mut r = Response::new(timed_body(
                vec![(Duration::ZERO, Bytes::from_static(payload))],
                Duration::ZERO,
            ));
            r.headers_mut()
                .insert("content-encoding", "gzip".parse().unwrap());
            r
        })
        .await;
        let up = Upstream::new();
        let mut req = get(&url);
        req.headers_mut()
            .insert("accept-encoding", "gzip".parse().unwrap());
        let lim = limits(
            Duration::from_secs(2),
            Duration::from_secs(2),
            Duration::from_secs(5),
        );
        let mut resp = up.send(req, &lim).await.unwrap();
        assert_eq!(resp.headers.get("content-encoding").unwrap(), "gzip");
        let mut got = Vec::new();
        while let Some(c) = resp.body.next_chunk().await {
            got.extend_from_slice(&c.unwrap());
        }
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn text_reads_whole_body() {
        let url = serve(|_| async {
            Response::new(timed_body(
                vec![
                    (Duration::ZERO, Bytes::from_static(b"hel")),
                    (Duration::from_millis(10), Bytes::from_static(b"lo")),
                ],
                Duration::ZERO,
            ))
        })
        .await;
        let up = Upstream::new();
        let lim = limits(
            Duration::from_secs(2),
            Duration::from_secs(2),
            Duration::from_secs(5),
        );
        let resp = up.send(get(&url), &lim).await.unwrap();
        assert_eq!(resp.body.text().await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let url = serve(|req| async move {
            if req.uri().path() == "/moved" {
                let mut r = Response::new(timed_body(vec![], Duration::ZERO));
                *r.status_mut() = StatusCode::FOUND;
                r.headers_mut()
                    .insert("location", "http://127.0.0.1:9/elsewhere".parse().unwrap());
                r
            } else {
                panic!("redirect target must not be requested");
            }
        })
        .await;
        let up = Upstream::new();
        let lim = limits(
            Duration::from_secs(2),
            Duration::from_secs(2),
            Duration::from_secs(5),
        );
        let mut req = get(&format!("{url}/moved"));
        *req.method_mut() = reqwest::Method::POST;
        req.headers_mut()
            .insert("x-api-key", "not-a-real-key".parse().unwrap());
        let resp = up.send(req, &lim).await.unwrap();
        assert_eq!(resp.status, StatusCode::FOUND);
        assert_eq!(
            resp.headers.get("location").unwrap(),
            "http://127.0.0.1:9/elsewhere"
        );
    }

    #[test]
    fn join_url_cases() {
        let cases = [
            ("https://x/v1", "/v1/messages", "https://x/v1/messages"),
            ("https://x/v1/", "/v1/messages", "https://x/v1/messages"),
            ("https://x", "/v1/messages", "https://x/v1/messages"),
            ("https://x/", "/v1/messages", "https://x/v1/messages"),
            ("https://x", "v1/messages", "https://x/v1/messages"),
            ("https://x/v1", "v1/messages", "https://x/v1/messages"),
            (
                "https://gw/prefix",
                "/v1/messages",
                "https://gw/prefix/v1/messages",
            ),
            (
                "https://gw/prefix/",
                "/v1/messages",
                "https://gw/prefix/v1/messages",
            ),
            ("https://x/v1", "/v1", "https://x/v1"),
            (
                "https://x/v1",
                "/v1beta/models",
                "https://x/v1/v1beta/models",
            ),
            ("https://x/v1", "/other", "https://x/v1/other"),
        ];
        for (base, path, want) in cases {
            assert_eq!(join_url(base, path), want, "{base} + {path}");
        }
    }

    #[derive(Debug)]
    struct Msg(&'static str, Option<Box<dyn StdError + 'static>>);
    impl fmt::Display for Msg {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }
    impl StdError for Msg {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.1.as_deref()
        }
    }

    #[test]
    fn classify_chain_cases() {
        let io = |k| Box::new(std::io::Error::from(k)) as Box<dyn StdError>;
        for k in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
        ] {
            let e = Msg("error reading a body from connection", Some(io(k)));
            assert_eq!(
                classify_chain(&e, false, false),
                TransportKind::Reset,
                "{k:?}"
            );
        }
        let e = Msg("connection closed before message completed", None);
        assert_eq!(classify_chain(&e, false, false), TransportKind::Reset);

        let e = Msg(
            "client error (Connect)",
            Some(Box::new(Msg("dns error", Some(io(ErrorKind::Other))))),
        );
        assert_eq!(classify_chain(&e, false, true), TransportKind::Dns);

        // HTTP/2 RST_STREAM / GOAWAY (no io source in the chain).
        let rst = h2::Error::from(h2::Reason::REFUSED_STREAM);
        assert!(rst.reason().is_some());
        assert_eq!(classify_chain(&rst, false, false), TransportKind::Reset);
        let wrapped = Msg("error reading a body from connection", Some(Box::new(rst)));
        assert_eq!(classify_chain(&wrapped, false, false), TransportKind::Reset);
        for m in [
            "stream error received: refused stream before processing any application logic",
            "connection error received: not a result of an error",
            "connection error detected: unspecific protocol error detected",
        ] {
            assert_eq!(
                classify_chain(&Msg(m, None), false, false),
                TransportKind::Reset,
                "{m}"
            );
        }

        let e = Msg(
            "client error (Connect)",
            Some(Box::new(Msg(
                "invalid peer certificate: UnknownIssuer",
                None,
            ))),
        );
        assert_eq!(classify_chain(&e, false, true), TransportKind::Tls);

        let e = Msg(
            "client error (Connect)",
            Some(io(ErrorKind::ConnectionRefused)),
        );
        assert_eq!(classify_chain(&e, false, true), TransportKind::Connect);

        assert_eq!(
            classify_chain(&Msg("whatever", None), true, true),
            TransportKind::IdleTimeout
        );
        assert_eq!(
            classify_chain(&Msg("whatever", None), false, false),
            TransportKind::Other
        );
    }
}
