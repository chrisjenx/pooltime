//! Streaming Server-Sent-Events parser and encoder.
//!
//! The parser is an *observer*: Anthropic streams are forwarded byte-for-byte and this
//! parser only watches them, while Codex streams are parsed for translation. It follows
//! the WHATWG event-stream rules:
//!
//! - lines end in `\n`, `\r\n` or `\r` (a `\r\n` split across chunks is still one ending);
//! - a blank line dispatches the pending event;
//! - `data:` lines are joined with `\n`; one leading space after the colon is stripped;
//! - `event:` sets the event type; `id:` / `retry:` / unknown fields are ignored (but
//!   kept in `raw`); lines starting with `:` are comments.
//!
//! Deviations, deliberately: an event with an `event:` field but no `data:` is still
//! emitted (with empty data), and `finish()` dispatches a final event that lacks its
//! trailing blank line (WHATWG would discard it).
//!
//! `raw` accounting: every input byte ends up in exactly one emitted event's `raw`, in
//! order, so concatenating all `raw`s reproduces the input exactly. Bytes that do not
//! form an event by themselves (a lone comment block, keep-alive `:` lines, `id:`-only
//! blocks) are *not* emitted on their own; they are prepended to the `raw` of the next
//! emitted event. If the stream ends with such orphan bytes, `finish()` emits one last
//! event with `event: None`, `data: ""` carrying them.
//!
//! A line ending in a lone `\r` is only completed when the following byte (or `finish`)
//! arrives, because a `\n` might follow; this keeps results independent of chunking.

use bytes::{BufMut, Bytes, BytesMut};

/// Default per-line cap (8 MiB).
pub const DEFAULT_MAX_LINE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    /// The exact original bytes of the event (including any orphan bytes before it).
    pub raw: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    #[error("SSE line exceeds the {max} byte cap")]
    LineTooLong { max: usize },
}

#[derive(Debug)]
pub struct SseParser {
    max_line: usize,
    /// Every byte consumed since the last emitted event.
    raw: BytesMut,
    /// Offset in `raw` where the current (unfinished) line starts.
    line_start: usize,
    /// `Some(content_end)` when the current line was terminated by `\r` and we are
    /// waiting to see whether a `\n` follows.
    cr_end: Option<usize>,
    event: Option<String>,
    data: String,
    has_data: bool,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_LINE)
    }
}

impl SseParser {
    /// `max_line` is the maximum length of one line's content (terminator excluded).
    pub fn new(max_line: usize) -> Self {
        Self {
            max_line,
            raw: BytesMut::new(),
            line_start: 0,
            cr_end: None,
            event: None,
            data: String::new(),
            has_data: false,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < chunk.len() {
            if let Some(content_end) = self.cr_end.take() {
                // The previous line ended with `\r`; swallow a directly following `\n`.
                if chunk[i] == b'\n' {
                    self.raw.put_u8(b'\n');
                    i += 1;
                }
                self.end_line(content_end, &mut out);
                continue;
            }
            let rest = &chunk[i..];
            match rest.iter().position(|&b| b == b'\n' || b == b'\r') {
                Some(p) => {
                    self.check_cap(p)?;
                    self.raw.extend_from_slice(&rest[..p]);
                    let content_end = self.raw.len();
                    self.raw.put_u8(rest[p]);
                    i += p + 1;
                    if rest[p] == b'\r' {
                        self.cr_end = Some(content_end);
                    } else {
                        self.end_line(content_end, &mut out);
                    }
                }
                None => {
                    self.check_cap(rest.len())?;
                    self.raw.extend_from_slice(rest);
                    i = chunk.len();
                }
            }
        }
        Ok(out)
    }

    /// Flush at end of stream: completes an unterminated last line, dispatches a final
    /// event lacking its blank line, and emits any orphan bytes (see module docs).
    pub fn finish(mut self) -> Result<Vec<SseEvent>, SseError> {
        let mut out = Vec::new();
        if let Some(content_end) = self.cr_end.take() {
            self.end_line(content_end, &mut out);
        }
        if self.raw.len() > self.line_start {
            let content_end = self.raw.len();
            self.end_line(content_end, &mut out);
        }
        self.dispatch(&mut out);
        if !self.raw.is_empty() {
            out.push(SseEvent {
                event: None,
                data: String::new(),
                raw: self.raw.split().freeze(),
            });
        }
        Ok(out)
    }

    /// Would adding `extra` content bytes push the current line over the cap?
    fn check_cap(&self, extra: usize) -> Result<(), SseError> {
        if self.raw.len() - self.line_start + extra > self.max_line {
            Err(SseError::LineTooLong { max: self.max_line })
        } else {
            Ok(())
        }
    }

    /// Process the completed line `raw[line_start..content_end]`; the terminator (if any)
    /// is already in `raw`.
    fn end_line(&mut self, content_end: usize, out: &mut Vec<SseEvent>) {
        let line = &self.raw[self.line_start..content_end];
        self.line_start = self.raw.len();
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line[0] == b':' {
            return;
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(p) => {
                let v = &line[p + 1..];
                (&line[..p], v.strip_prefix(b" ").unwrap_or(v))
            }
            None => (line, &line[line.len()..]),
        };
        match field {
            b"data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(&String::from_utf8_lossy(value));
                self.has_data = true;
            }
            b"event" => self.event = Some(String::from_utf8_lossy(value).into_owned()),
            _ => {}
        }
    }

    /// Emit the pending event if it has data or an event type; otherwise leave the bytes
    /// in `raw` to be attached to the next emitted event.
    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if !self.has_data && self.event.is_none() {
            return;
        }
        out.push(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
            raw: self.raw.split().freeze(),
        });
        self.line_start = 0;
        self.has_data = false;
    }
}

/// Encode one event: `event: <e>\ndata: <line>\n` per data line, then a blank line.
/// `event` must not contain line breaks; any `\r\n`/`\r` in `data` is treated as `\n`.
pub fn encode(event: &str, data: &str) -> Bytes {
    let mut out = BytesMut::with_capacity(event.len() + data.len() + 24);
    out.put_slice(b"event: ");
    out.put_slice(event.as_bytes());
    out.put_u8(b'\n');
    for line in data.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        out.put_slice(b"data: ");
        out.put_slice(line.as_bytes());
        out.put_u8(b'\n');
    }
    out.put_u8(b'\n');
    out.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(input: &[u8]) -> Vec<SseEvent> {
        let mut p = SseParser::default();
        let mut out = p.push(input).unwrap();
        out.extend(p.finish().unwrap());
        out
    }

    fn parse_chunks(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut p = SseParser::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.push(c).unwrap());
        }
        out.extend(p.finish().unwrap());
        out
    }

    fn ev(event: Option<&str>, data: &str) -> (Option<String>, String) {
        (event.map(str::to_string), data.to_string())
    }

    fn pairs(events: &[SseEvent]) -> Vec<(Option<String>, String)> {
        events
            .iter()
            .map(|e| (e.event.clone(), e.data.clone()))
            .collect()
    }

    /// CRLF, CR-only, LF, comments, multi-line data, id/retry, a comment-only block, and a
    /// final event with no trailing blank line.
    const FIXTURE: &[u8] = b"event: a\r\ndata: 1\r\n\r\n\
: keep-alive\n\n\
event: b\rdata: x\rdata: y\r\r\
data:z\n\n\
id: 7\nretry: 100\ndata: w\n\n\
event: last\ndata: tail";

    fn fixture_expected() -> Vec<(Option<String>, String)> {
        vec![
            ev(Some("a"), "1"),
            ev(Some("b"), "x\ny"),
            ev(None, "z"),
            ev(None, "w"),
            ev(Some("last"), "tail"),
        ]
    }

    #[test]
    fn parses_lf_crlf_and_cr() {
        for (name, input) in [
            ("lf", &b"event: m\ndata: hi\n\n"[..]),
            ("crlf", b"event: m\r\ndata: hi\r\n\r\n"),
            ("cr", b"event: m\rdata: hi\r\r"),
        ] {
            let evs = parse_all(input);
            assert_eq!(pairs(&evs), vec![ev(Some("m"), "hi")], "{name}");
            assert_eq!(&evs[0].raw[..], input, "{name}");
        }
        assert_eq!(pairs(&parse_all(FIXTURE)), fixture_expected());
    }

    #[test]
    fn multiline_data_joined_with_newline() {
        let evs = parse_all(b"data: a\ndata:  b\ndata:\ndata: c\n\n");
        // Only one leading space is stripped; an empty data line contributes "".
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "a\n b\n\nc");
        assert_eq!(evs[0].event, None);
    }

    #[test]
    fn comments_ignored_but_kept_in_raw() {
        let input = b": hello\nevent: m\n: mid\ndata: d\n\n";
        let evs = parse_all(input);
        assert_eq!(pairs(&evs), vec![ev(Some("m"), "d")]);
        assert_eq!(&evs[0].raw[..], &input[..]);

        // A comment-only block is attached to the next event's raw.
        let evs = parse_all(b": ping\n\ndata: 1\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(&evs[0].raw[..], b": ping\n\ndata: 1\n\n");

        // Trailing orphan bytes come out as a final empty event.
        let evs = parse_all(b"data: 1\n\n: bye\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].event, None);
        assert_eq!(evs[1].data, "");
        assert_eq!(&evs[1].raw[..], b": bye\n\n");
    }

    #[test]
    fn crlf_split_across_chunks_is_one_line_ending() {
        let evs = parse_chunks(&[b"data: x\r", b"\n\r", b"\n"]);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "x");
        assert_eq!(&evs[0].raw[..], b"data: x\r\n\r\n");
        let evs = parse_chunks(&[b"data: x\r\n\r\ndata: y\r\n\r\n"]);
        assert_eq!(evs.len(), 2);
        assert_eq!(&evs[1].raw[..], b"data: y\r\n\r\n");
    }

    #[test]
    fn split_across_chunks_anywhere() {
        let whole = parse_all(FIXTURE);
        assert_eq!(pairs(&whole), fixture_expected());
        for i in 0..=FIXTURE.len() {
            let (a, b) = FIXTURE.split_at(i);
            assert_eq!(parse_chunks(&[a, b]), whole, "split at {i}");
        }
        for i in 0..=FIXTURE.len() {
            for j in i..=FIXTURE.len() {
                let chunks = [&FIXTURE[..i], &FIXTURE[i..j], &FIXTURE[j..]];
                assert_eq!(parse_chunks(&chunks), whole, "split at {i},{j}");
            }
        }
        let bytes: Vec<&[u8]> = FIXTURE.chunks(1).collect();
        assert_eq!(parse_chunks(&bytes), whole);
    }

    #[test]
    fn raw_bytes_concatenate_to_input() {
        let evs = parse_all(FIXTURE);
        let joined: Vec<u8> = evs.iter().flat_map(|e| e.raw.to_vec()).collect();
        assert_eq!(joined, FIXTURE);

        // Pseudo-random streams of SSE-ish tokens, split at pseudo-random points.
        const TOKENS: &[&str] = &[
            "data: ", "data:", "event: ", "id: ", "retry: ", ": c", ":", "x", "yz", " ", "\n",
            "\r", "\r\n", "\n\n", "\r\n\r\n", "\u{e9}",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..500 {
            let mut input = Vec::new();
            for _ in 0..(next() % 30) {
                input.extend_from_slice(TOKENS[(next() % TOKENS.len() as u64) as usize].as_bytes());
            }
            let whole = parse_all(&input);
            let joined: Vec<u8> = whole.iter().flat_map(|e| e.raw.to_vec()).collect();
            assert_eq!(joined, input);

            let mut chunks: Vec<&[u8]> = Vec::new();
            let mut rest = &input[..];
            while !rest.is_empty() {
                let n = 1 + (next() as usize % rest.len());
                let (a, b) = rest.split_at(n);
                chunks.push(a);
                rest = b;
            }
            assert_eq!(parse_chunks(&chunks), whole, "input {input:?}");
        }
    }

    #[test]
    fn line_cap_enforced_at_8mib() {
        let cap = 8 * 1024 * 1024;
        assert_eq!(DEFAULT_MAX_LINE, cap);

        // Exactly the cap, unterminated: still fine; one more byte errors.
        let mut p = SseParser::default();
        assert!(p.push(&vec![b'a'; cap]).unwrap().is_empty());
        assert_eq!(p.push(b"a"), Err(SseError::LineTooLong { max: cap }));

        // Exactly the cap in one terminated line parses; cap+1 errors in a single chunk.
        let mut line = b"data: ".to_vec();
        line.resize(cap, b'a');
        let mut ok = line.clone();
        ok.extend_from_slice(b"\n\n");
        let mut p = SseParser::default();
        let evs = p.push(&ok).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data.len(), cap - 6);

        let mut too_long = line;
        too_long.extend_from_slice(b"a\n\n");
        let mut p = SseParser::default();
        assert_eq!(p.push(&too_long), Err(SseError::LineTooLong { max: cap }));
    }

    #[test]
    fn line_cap_is_configurable_and_does_not_buffer_unboundedly() {
        let mut p = SseParser::new(10);
        assert!(p.push(b"data: 1234").unwrap().is_empty());
        // Error fires as soon as the partial line exceeds the cap, without a terminator.
        assert_eq!(p.push(b"5"), Err(SseError::LineTooLong { max: 10 }));
        // Cap is per line, not per event.
        let mut p = SseParser::new(10);
        assert_eq!(p.push(b"data: 1234\ndata: 1234\n\n").unwrap().len(), 1);
    }

    #[test]
    fn encode_formats_events() {
        assert_eq!(
            &encode("message_start", "{\"a\":1}")[..],
            b"event: message_start\ndata: {\"a\":1}\n\n"
        );
        assert_eq!(
            &encode("e", "l1\nl2")[..],
            b"event: e\ndata: l1\ndata: l2\n\n"
        );
        assert_eq!(&encode("e", "")[..], b"event: e\ndata: \n\n");
        let evs = parse_all(&encode("e", "l1\nl2"));
        assert_eq!(pairs(&evs), vec![ev(Some("e"), "l1\nl2")]);
    }
}
