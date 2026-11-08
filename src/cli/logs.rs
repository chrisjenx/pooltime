//! `pooltime logs [--follow] [--request <id>]`: read the daemon's JSON-lines log files
//! (`<logs>/pooltime.log.<date>`, oldest first) without needing the daemon.
//!
//! `--request <id>` prints that request's attempt chain from the daemon's journal
//! (`GET /api/v1/requests/{id}`, one line per attempt; exit 1 for an unknown id). With
//! `--follow`, or when no daemon runs, it keeps the log lines carrying that request id anywhere
//! in the event (its own fields or an enclosing span). Lines are shown as `<time> <LEVEL> <message> k=v…` (`--json`
//! prints them verbatim); `--follow` keeps polling for new lines, across daily rotation.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use serde_json::Value;

use super::client;
use crate::config::Paths;
use crate::logging::LOG_PREFIX;

const FOLLOW_POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Keep printing new lines as they are written.
    #[arg(long, short)]
    pub follow: bool,
    /// Only lines for this request id (the `x-pooltime-request-id` response header).
    #[arg(long, value_name = "ID")]
    pub request: Option<String>,
    /// Print the raw JSON lines.
    #[arg(long)]
    pub json: bool,
    /// Show only the last N matching lines (before following).
    #[arg(long, short = 'n', value_name = "N")]
    pub lines: Option<usize>,
}

/// The log files, oldest first (the date suffix sorts chronologically).
fn log_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(LOG_PREFIX))
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// Whether `v` has a `request_id` equal to `id` at any depth.
fn has_request(v: &Value, id: &str) -> bool {
    match v {
        Value::Object(m) => m
            .iter()
            .any(|(k, v)| (k == "request_id" && v.as_str() == Some(id)) || has_request(v, id)),
        Value::Array(a) => a.iter().any(|v| has_request(v, id)),
        _ => false,
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `<time> <LEVEL> <message> k=v…` (event fields, then the span's); non-JSON lines verbatim.
fn human(line: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return line.to_string();
    };
    let mut out = format!(
        "{} {:<5} {}",
        v["timestamp"].as_str().unwrap_or("-"),
        v["level"].as_str().unwrap_or("-"),
        v["fields"]["message"].as_str().unwrap_or("")
    );
    let pairs = v["fields"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| *k != "message")
        .chain(
            v["span"]
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(k, _)| *k != "name"),
        );
    for (k, val) in pairs {
        out.push_str(&format!(" {k}={}", scalar(val)));
    }
    out
}

/// A line passes the filter; with a request id only JSON lines can.
fn matches(line: &str, request: Option<&str>) -> bool {
    match request {
        None => !line.trim().is_empty(),
        Some(id) => serde_json::from_str::<Value>(line).is_ok_and(|v| has_request(&v, id)),
    }
}

/// Follows one file from an offset, yielding complete lines only.
struct Tail {
    path: PathBuf,
    offset: u64,
    partial: String,
}

impl Tail {
    fn read_new(&mut self) -> std::io::Result<Vec<String>> {
        let mut f = File::open(&self.path)?;
        let len = f.metadata()?.len();
        if len < self.offset {
            // Truncated: start over.
            self.offset = 0;
            self.partial.clear();
        }
        f.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        self.offset += buf.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&buf));
        let mut lines: Vec<String> = self.partial.split('\n').map(str::to_owned).collect();
        self.partial = lines.pop().unwrap_or_default();
        Ok(lines)
    }
}

/// What the daemon's journal says about one request.
pub enum ChainLookup {
    /// `{request, attempts}` from `GET /api/v1/requests/{id}`.
    Found(Value),
    NotFound,
    /// No daemon answers on the configured address.
    NoDaemon,
}

/// Ask the running daemon for request `id`'s journal entry.
pub async fn lookup(paths: &Paths, id: &str) -> anyhow::Result<ChainLookup> {
    let Some(client) = client::find(paths).await? else {
        return Ok(ChainLookup::NoDaemon);
    };
    Ok(
        match client
            .get_authed_opt(&format!("/api/v1/requests/{}", client::seg(id)))
            .await?
        {
            Some(v) => ChainLookup::Found(v),
            None => ChainLookup::NotFound,
        },
    )
}

fn field(v: &Value, k: &str) -> Option<String> {
    match &v[k] {
        Value::Null => None,
        other => Some(scalar(other)),
    }
}

/// The request line, then one line per attempt:
/// `#n <account> <provider> <outcome> [reason=..] [upstream=<status>] [upstream_id=..] [<ms>ms]`.
pub fn chain_lines(doc: &Value) -> Vec<String> {
    let r = &doc["request"];
    let mut head = format!(
        "{} {} {} {}",
        r["id"].as_str().unwrap_or("-"),
        r["time"].as_str().unwrap_or("-"),
        field(r, "model")
            .or_else(|| field(r, "path"))
            .unwrap_or_else(|| "-".into()),
        r["outcome"].as_str().unwrap_or("-"),
    );
    for (label, key) in [
        ("status", "status"),
        ("reason", "reason"),
        ("provider", "provider"),
    ] {
        if let Some(v) = field(r, key) {
            head.push_str(&format!(" {label}={v}"));
        }
    }
    if let Some(v) = field(r, "ttfb_ms") {
        head.push_str(&format!(" ttfb={v}ms"));
    }
    if let Some(v) = field(r, "total_ms") {
        head.push_str(&format!(" total={v}ms"));
    }
    if let (Some(i), Some(o)) = (field(r, "input_tokens"), field(r, "output_tokens")) {
        head.push_str(&format!(" tokens={i}/{o}"));
    }
    let mut out = vec![head];
    let attempts = doc["attempts"].as_array().cloned().unwrap_or_default();
    if attempts.is_empty() {
        out.push("  (no upstream attempt)".into());
    }
    for a in &attempts {
        let mut l = format!(
            "  #{} {} {} {}",
            scalar(&a["n"]),
            a["account"].as_str().unwrap_or("-"),
            a["provider"].as_str().unwrap_or("-"),
            a["outcome"].as_str().unwrap_or("-"),
        );
        if let Some(v) = field(a, "reason") {
            l.push_str(&format!(" reason={v}"));
        }
        if let Some(v) = field(a, "upstream_status") {
            l.push_str(&format!(" upstream={v}"));
        }
        if let Some(v) = field(a, "upstream_request_id") {
            l.push_str(&format!(" upstream_id={v}"));
        }
        if let Some(v) = field(a, "ms") {
            l.push_str(&format!(" {v}ms"));
        }
        out.push(l);
    }
    out
}

/// `pooltime logs`. With `--request <id>` (and no `--follow`) the attempt chain comes from the
/// daemon's journal (exit 1 when the id is unknown); without a running daemon it falls back to
/// the log lines carrying that id.
pub async fn run(paths: &Paths, args: LogsArgs) -> anyhow::Result<i32> {
    if let (Some(id), false) = (args.request.as_deref(), args.follow) {
        match lookup(paths, id).await? {
            ChainLookup::Found(v) => {
                if args.json {
                    println!("{v}");
                } else {
                    for l in chain_lines(&v) {
                        println!("{l}");
                    }
                }
                return Ok(0);
            }
            ChainLookup::NotFound => {
                eprintln!("no journal entry for request {id}");
                return Ok(1);
            }
            ChainLookup::NoDaemon => {
                eprintln!("pooltime is not running; showing log lines for {id}");
            }
        }
    }
    log_lines(paths, args)
}

/// Print (and with `--follow`, keep printing) the log lines that pass the filter.
fn log_lines(paths: &Paths, args: LogsArgs) -> anyhow::Result<i32> {
    let request = args.request.as_deref();
    let show = |line: &str| {
        if args.json {
            println!("{line}");
        } else {
            println!("{}", human(line));
        }
    };
    let files = log_files(&paths.logs);
    if files.is_empty() && !args.follow {
        eprintln!("no log files yet in {}", paths.logs.display());
        return Ok(0);
    }
    let mut tail: Option<Tail> = None;
    let mut shown = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let mut t = Tail {
            path: f.clone(),
            offset: 0,
            partial: String::new(),
        };
        shown.extend(t.read_new()?.into_iter().filter(|l| matches(l, request)));
        // A last line without its newline is shown, except in the file being followed (the
        // writer may be mid-line; the rest arrives with the next poll).
        let followed = args.follow && i + 1 == files.len();
        if !followed {
            let rest = std::mem::take(&mut t.partial);
            if matches(&rest, request) {
                shown.push(rest);
            }
        }
        tail = Some(t);
    }
    let skip = args.lines.map_or(0, |n| shown.len().saturating_sub(n));
    for l in &shown[skip..] {
        show(l);
    }
    if !args.follow {
        return Ok(0);
    }
    loop {
        std::thread::sleep(FOLLOW_POLL);
        if let Some(t) = &mut tail {
            for l in t.read_new()?.into_iter().filter(|l| matches(l, request)) {
                show(&l);
            }
        }
        // Daily rotation: move to a newer file once one appears (the old one is drained above).
        let newest = log_files(&paths.logs).pop();
        if let Some(n) = newest {
            if tail.as_ref().is_none_or(|t| t.path < n) {
                tail = Some(Tail {
                    path: n,
                    offset: 0,
                    partial: String::new(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_id_found_at_any_depth() {
        let v = json!({"fields": {"message": "m"}, "spans": [{"request_id": "r1"}]});
        assert!(has_request(&v, "r1"));
        assert!(!has_request(&v, "r2"));
        assert!(!matches("plain text", Some("r1")));
        assert!(matches("plain text", None));
    }

    #[test]
    fn chain_is_one_line_per_attempt() {
        let doc = json!({
            "request": {"id": "req_1", "time": "2026-11-08T10:00:00Z", "model": "claude-sonnet-5",
                        "outcome": "completed", "status": 200, "reason": null, "provider": "anthropic",
                        "ttfb_ms": 120, "total_ms": 900, "input_tokens": 3, "output_tokens": 4},
            "attempts": [
                {"n": 1, "account": "a1", "provider": "anthropic", "outcome": "failover",
                 "reason": "quota", "upstream_status": 429, "upstream_request_id": "req_u1", "ms": 40},
                {"n": 2, "account": "a2", "provider": "anthropic", "outcome": "completed",
                 "reason": null, "upstream_status": 200, "upstream_request_id": null, "ms": 80}
            ]
        });
        assert_eq!(
            chain_lines(&doc),
            [
                "req_1 2026-11-08T10:00:00Z claude-sonnet-5 completed status=200 provider=anthropic ttfb=120ms total=900ms tokens=3/4",
                "  #1 a1 anthropic failover reason=quota upstream=429 upstream_id=req_u1 40ms",
                "  #2 a2 anthropic completed upstream=200 80ms",
            ]
        );
    }

    #[test]
    fn human_format() {
        let l = r#"{"timestamp":"t","level":"INFO","fields":{"message":"hi","account":"a"},"span":{"request_id":"r","name":"req"}}"#;
        assert_eq!(human(l), "t INFO  hi account=a request_id=r");
        assert_eq!(human("raw"), "raw");
    }

    #[test]
    fn tail_yields_complete_lines_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pooltime.log.x");
        std::fs::write(&p, "a\nb").unwrap();
        let mut t = Tail {
            path: p.clone(),
            offset: 0,
            partial: String::new(),
        };
        assert_eq!(t.read_new().unwrap(), ["a"]);
        std::fs::write(&p, "a\nbc\nd\n").unwrap();
        assert_eq!(t.read_new().unwrap(), ["bc", "d"]);
    }
}
