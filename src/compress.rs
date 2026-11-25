//! Request-body compression, both directions of the pool.
//!
//! Upstream: Claude Code resends the whole conversation on every turn, so request bodies are
//! large, repetitive JSON and dominate upload bandwidth. Bodies of at least [`MIN_LEN`] bytes are
//! compressed before they leave the machine: gzip for `api.anthropic.com` (the only coding it
//! accepts; Claude Code gzips its own large bodies the same way), zstd level 3 for the ChatGPT
//! Codex backend (what the official Codex client sends). If an upstream refuses the coding (415,
//! or a 400 that names it) the request is resent uncompressed at once, and when that resend is
//! accepted compression is switched off for that provider until the daemon restarts; so a change
//! upstream costs one extra send, never a failed request.
//!
//! Inbound: a client may itself send `Content-Encoding: gzip` / `zstd` bodies;
//! [`decode_inbound`] inflates them (bounded) before the request is parsed and routed.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode};

use crate::errors::TransportKind;
use crate::upstream::{Limits, Upstream, UpstreamResponse, WatchedBody};

/// Bodies shorter than this are sent as they are (compression would save next to nothing).
pub const MIN_LEN: usize = 1024;

/// zstd level of the official Codex client.
const ZSTD_LEVEL: i32 = 3;

/// A request-body coding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Gzip,
    Zstd,
}

impl Codec {
    /// The `Content-Encoding` token.
    pub fn token(self) -> &'static str {
        match self {
            Codec::Gzip => "gzip",
            Codec::Zstd => "zstd",
        }
    }

    /// Compress `raw` (blocking; call off the async executor for large bodies).
    pub fn encode(self, raw: &[u8]) -> std::io::Result<Vec<u8>> {
        match self {
            Codec::Gzip => {
                let mut e = flate2::write::GzEncoder::new(
                    Vec::with_capacity(raw.len() / 4),
                    flate2::Compression::default(),
                );
                e.write_all(raw)?;
                e.finish()
            }
            Codec::Zstd => zstd::bulk::compress(raw, ZSTD_LEVEL),
        }
    }

    /// Decompress `coded`, failing once the output would exceed `limit` bytes.
    pub fn decode(self, coded: &[u8], limit: usize) -> Result<Vec<u8>, DecodeError> {
        let mut out = Vec::new();
        let cap = limit as u64 + 1;
        let read = match self {
            Codec::Gzip => flate2::read::MultiGzDecoder::new(coded)
                .take(cap)
                .read_to_end(&mut out),
            Codec::Zstd => zstd::stream::read::Decoder::new(coded)
                .and_then(|d| d.take(cap).read_to_end(&mut out)),
        };
        read.map_err(|_| DecodeError::Corrupt)?;
        if out.len() > limit {
            return Err(DecodeError::TooLarge);
        }
        Ok(out)
    }

    fn parse(token: &str) -> Option<Codec> {
        match token.trim().to_ascii_lowercase().as_str() {
            "gzip" | "x-gzip" => Some(Codec::Gzip),
            "zstd" => Some(Codec::Zstd),
            _ => None,
        }
    }
}

/// Why an inbound body could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// A `Content-Encoding` other than identity, gzip or zstd (or a stack of codings).
    Unsupported,
    /// Not valid data of the declared coding.
    Corrupt,
    /// Inflates past the body limit.
    TooLarge,
}

/// Inflate an inbound body per its `Content-Encoding` (absent or `identity`: returned as is),
/// bounded by `limit` decoded bytes.
pub fn decode_inbound(
    headers: &HeaderMap,
    body: Bytes,
    limit: usize,
) -> Result<Bytes, DecodeError> {
    let Some(v) = headers.get(http::header::CONTENT_ENCODING) else {
        return Ok(body);
    };
    let v = v.to_str().map_err(|_| DecodeError::Unsupported)?.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("identity") {
        return Ok(body);
    }
    if v.contains(',') {
        return Err(DecodeError::Unsupported);
    }
    let codec = Codec::parse(v).ok_or(DecodeError::Unsupported)?;
    codec.decode(&body, limit).map(Bytes::from)
}

/// One provider's outbound request compression: its coding and whether it is (still) on.
#[derive(Debug)]
pub struct RequestCompression {
    provider: &'static str,
    codec: Codec,
    enabled: AtomicBool,
}

impl RequestCompression {
    pub fn new(provider: &'static str, codec: Codec, enabled: bool) -> Self {
        RequestCompression {
            provider,
            codec,
            enabled: AtomicBool::new(enabled),
        }
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// `body` in this provider's coding, or `None` to send it as is: compression off, a body under
    /// [`MIN_LEN`], an encoder failure, or no saving.
    pub async fn encode(&self, body: &Bytes) -> Option<Bytes> {
        if !self.enabled() || body.len() < MIN_LEN {
            return None;
        }
        let (codec, raw) = (self.codec, body.clone());
        let coded = tokio::task::spawn_blocking(move || codec.encode(&raw))
            .await
            .ok()?
            .ok()?;
        if coded.len() >= body.len() {
            return None;
        }
        tracing::debug!(
            provider = self.provider,
            encoding = codec.token(),
            raw_bytes = body.len(),
            sent_bytes = coded.len(),
            "request body compressed"
        );
        Some(Bytes::from(coded))
    }

    /// Switch compression off for good (this process); logged once.
    fn disable(&self) {
        if self.enabled.swap(false, Ordering::Relaxed) {
            tracing::warn!(
                provider = self.provider,
                encoding = self.codec.token(),
                "upstream refused compressed request bodies; sending them uncompressed from now on"
            );
        }
    }

    /// Send `body` (compressed when it pays) and wait for the response headers.
    ///
    /// `build(body, content_encoding)` makes the request. A compressed send answered 415, or 400
    /// with an error naming the coding, is resent uncompressed; when that resend is not refused
    /// the same way, compression is switched off. Any other answer to the compressed send is
    /// returned as is (a 400 whose body had to be read is handed back from memory).
    pub async fn send<F>(
        &self,
        up: &Upstream,
        lim: &Limits,
        body: &Bytes,
        build: F,
    ) -> Result<UpstreamResponse, TransportKind>
    where
        F: Fn(Bytes, Option<HeaderValue>) -> Result<reqwest::Request, TransportKind>,
    {
        let Some(coded) = self.encode(body).await else {
            return up.send(build(body.clone(), None)?, lim).await;
        };
        let token = HeaderValue::from_static(self.codec.token());
        let resp = up.send(build(coded, Some(token))?, lim).await?;
        if !matches!(resp.status.as_u16(), 400 | 415) {
            return Ok(resp);
        }
        let UpstreamResponse {
            status,
            headers,
            body: rbody,
        } = resp;
        let text = rbody.read_all().await.unwrap_or_default();
        if !refuses_coding(status, &text) {
            return Ok(UpstreamResponse {
                status,
                headers,
                body: WatchedBody::from_bytes(text, lim),
            });
        }
        tracing::info!(
            provider = self.provider,
            status = status.as_u16(),
            "compressed request body refused; resending uncompressed"
        );
        let plain = up.send(build(body.clone(), None)?, lim).await?;
        if !matches!(plain.status.as_u16(), 400 | 415) {
            self.disable();
        }
        Ok(plain)
    }
}

/// A 415, or a 400 whose error text is about the body's coding.
fn refuses_coding(status: StatusCode, body: &[u8]) -> bool {
    if status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
        return true;
    }
    if status != StatusCode::BAD_REQUEST {
        return false;
    }
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    ["encoding", "gzip", "zstd", "compress"]
        .iter()
        .any(|w| text.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize) -> Vec<u8> {
        let turn = br#"{"role":"user","content":[{"type":"text","text":"read src/main.rs and fix the bug"}]},"#;
        turn.iter().copied().cycle().take(n).collect()
    }

    #[test]
    fn both_codecs_round_trip_and_shrink_json() {
        let raw = sample(64 * 1024);
        for c in [Codec::Gzip, Codec::Zstd] {
            let coded = c.encode(&raw).unwrap();
            assert!(coded.len() * 10 < raw.len(), "{c:?}: {}", coded.len());
            assert_eq!(c.decode(&coded, raw.len()).unwrap(), raw);
        }
    }

    #[test]
    fn decode_refuses_output_past_the_limit() {
        let raw = sample(10_000);
        for c in [Codec::Gzip, Codec::Zstd] {
            let coded = c.encode(&raw).unwrap();
            assert_eq!(c.decode(&coded, 9_999), Err(DecodeError::TooLarge));
            assert_eq!(c.decode(b"not compressed", 100), Err(DecodeError::Corrupt));
        }
    }

    #[test]
    fn decode_inbound_follows_content_encoding() {
        let raw = Bytes::from(sample(5000));
        let mut h = HeaderMap::new();
        assert_eq!(decode_inbound(&h, raw.clone(), 10).unwrap(), raw);
        h.insert("content-encoding", HeaderValue::from_static("identity"));
        assert_eq!(decode_inbound(&h, raw.clone(), 10).unwrap(), raw);
        for (tok, c) in [("gzip", Codec::Gzip), ("ZSTD", Codec::Zstd)] {
            h.insert("content-encoding", HeaderValue::from_static(tok));
            let coded = Bytes::from(c.encode(&raw).unwrap());
            assert_eq!(decode_inbound(&h, coded, 5000).unwrap(), raw);
        }
        for tok in ["br", "deflate", "gzip, zstd"] {
            h.insert("content-encoding", HeaderValue::from_static(tok));
            assert_eq!(
                decode_inbound(&h, raw.clone(), 10_000),
                Err(DecodeError::Unsupported)
            );
        }
    }

    #[tokio::test]
    async fn encode_skips_small_bodies_and_disabled_providers() {
        let on = RequestCompression::new("test", Codec::Gzip, true);
        assert!(on.encode(&Bytes::from(sample(MIN_LEN - 1))).await.is_none());
        assert!(on.encode(&Bytes::from(sample(MIN_LEN * 4))).await.is_some());
        let off = RequestCompression::new("test", Codec::Gzip, false);
        assert!(off
            .encode(&Bytes::from(sample(MIN_LEN * 4)))
            .await
            .is_none());
        on.disable();
        assert!(!on.enabled());
    }

    #[test]
    fn refusal_is_415_or_a_400_about_the_coding() {
        let s400 = StatusCode::BAD_REQUEST;
        assert!(refuses_coding(StatusCode::UNSUPPORTED_MEDIA_TYPE, b""));
        assert!(refuses_coding(
            s400,
            br#"{"error":{"message":"Unsupported Content-Encoding: zstd"}}"#
        ));
        assert!(refuses_coding(s400, b"could not decompress body"));
        assert!(!refuses_coding(
            s400,
            br#"{"error":{"message":"max_tokens: too big"}}"#
        ));
        assert!(!refuses_coding(StatusCode::TOO_MANY_REQUESTS, b"gzip"));
    }
}
