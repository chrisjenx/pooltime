//! Real-socket helpers: a minimal HTTP/1.1 server that hands each request to an async handler
//! returning an axum `Response` (so streamed bodies go over a real TCP connection), and a raw
//! TCP client that can read part of a response and then abort the connection.
//!
//! Task 21/22 add the real server; these stay as the harness for client-disconnect tests.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Serve HTTP/1.1 on 127.0.0.1:0; every request (body fully read) goes to `handler`.
pub async fn serve<F, Fut>(handler: F) -> SocketAddr
where
    F: Fn(http::Request<Bytes>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = axum::response::Response> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let handler = handler.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: http::Request<hyper::body::Incoming>| {
                    let handler = handler.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        let body = body
                            .collect()
                            .await
                            .map(|b| b.to_bytes())
                            .unwrap_or_default();
                        Ok::<_, Infallible>(handler(http::Request::from_parts(parts, body)).await)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

/// `POST {path}` HTTP/1.1 request bytes with a JSON body and `connection: close`.
pub fn post_json(path: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "POST {path} HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// A client connection over a real `TcpStream`.
pub struct RawStream {
    stream: TcpStream,
    /// Everything read so far.
    pub received: Vec<u8>,
}

/// Connect to `addr` and write `request` (raw HTTP bytes).
pub async fn raw_client(addr: SocketAddr, request: &[u8]) -> RawStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
    RawStream {
        stream,
        received: Vec::new(),
    }
}

impl RawStream {
    /// Read until `needle` appears in the received bytes (panics after `within` or on EOF).
    pub async fn read_until(&mut self, needle: &[u8], within: Duration) {
        let deadline = tokio::time::Instant::now() + within;
        while !contains(&self.received, needle) {
            let mut buf = [0u8; 4096];
            let n = tokio::time::timeout_at(deadline, self.stream.read(&mut buf))
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "timed out waiting for {:?}; got {:?}",
                        String::from_utf8_lossy(needle),
                        String::from_utf8_lossy(&self.received)
                    )
                })
                .unwrap();
            assert!(n > 0, "EOF before {:?}", String::from_utf8_lossy(needle));
            self.received.extend_from_slice(&buf[..n]);
        }
    }

    /// Read until the server closes the connection (panics after `within`).
    pub async fn read_to_end(&mut self, within: Duration) -> Vec<u8> {
        let mut rest = Vec::new();
        tokio::time::timeout(within, self.stream.read_to_end(&mut rest))
            .await
            .expect("server did not close the connection in time")
            .unwrap();
        self.received.extend_from_slice(&rest);
        self.received.clone()
    }

    /// Abort the connection (zero linger -> RST), like a client killed mid-response.
    pub fn abort(self) {
        self.stream.set_zero_linger().unwrap();
        drop(self.stream);
    }

    /// Close our write half and drop the connection (orderly FIN).
    pub async fn shutdown(mut self) {
        let _ = self.stream.shutdown().await;
    }
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}
