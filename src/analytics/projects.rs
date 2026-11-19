//! Usage by project and by session ("which repos/sessions eat quota") over the last 24 hours,
//! 7 days or 30 days, from the request journal and the sessions' projects
//! ([`super::attribution`]).
//!
//! Per project and per session: requests, tokens (input, output, cache read, cache write; `tokens`
//! is their sum), `share` (of all tokens in the range) and the accounts that served them.
//! Requests without a session, or whose session never named a working directory, count under
//! the `unknown` project. Two projects with the same name are told apart by their `key` (the
//! path hash): their `label` gets the key's first four characters.
//!
//! Quota estimate (`est_subs`, in subscriptions: 1.0 = one account's whole 7-day window): each
//! account's 7-day window consumption inside the range ([`super::burn::consumed_between`], from
//! the sampled usage history) is split over the projects it served in proportion to their
//! **weighted** tokens on that account: input x1, output x5, cache write x1.25, cache read x0.1
//! (the ratios of Anthropic's API prices, which usage limits are assumed to follow). It is an
//! estimate: accounts are sampled, not metered per request, and an account's consumption with no
//! journaled tokens behind it stays unattributed (`quota.unattributed`). Without any sampled
//! consumption in the range the estimate is `null`; nothing is made up.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Serialize, Serializer};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::attribution::UNKNOWN;
use super::burn::consumed_between;
use crate::store::projects::{usage_rows, UsageRow};
use crate::store::usage_history::{self, WINDOW_7D};
use crate::store::Store;

/// Sessions listed (most usage first); `sessions_total` counts them all.
pub const SESSION_LIMIT: usize = 50;

/// Token weights for the quota estimate (see the module docs).
const W_INPUT: f64 = 1.0;
const W_OUTPUT: f64 = 5.0;
const W_CACHE_WRITE: f64 = 1.25;
const W_CACHE_READ: f64 = 0.1;

/// The report's time range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Day,
    Week,
    Month,
}

impl Range {
    pub const ALL: [Range; 3] = [Range::Day, Range::Week, Range::Month];

    pub fn as_str(self) -> &'static str {
        match self {
            Range::Day => "24h",
            Range::Week => "7d",
            Range::Month => "30d",
        }
    }

    pub fn secs(self) -> i64 {
        match self {
            Range::Day => 86_400,
            Range::Week => 7 * 86_400,
            Range::Month => 30 * 86_400,
        }
    }

    pub fn parse(s: &str) -> Option<Range> {
        Range::ALL.into_iter().find(|r| r.as_str() == s)
    }
}

fn round4<S: Serializer>(x: &f64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64((x * 10_000.0).round() / 10_000.0)
}

fn round4_opt<S: Serializer>(x: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match x {
        Some(x) => round4(x, s),
        None => s.serialize_none(),
    }
}

fn rfc3339(unix: i64) -> String {
    OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_default()
}

/// Request and token counts.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Usage {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The sum of the four token counts.
    pub tokens: u64,
}

impl Usage {
    fn add(&mut self, r: &UsageRow) {
        let n = |v: i64| u64::try_from(v).unwrap_or(0);
        self.requests += 1;
        self.input_tokens += n(r.input_tokens);
        self.output_tokens += n(r.output_tokens);
        self.cache_read += n(r.cache_read);
        self.cache_write += n(r.cache_write);
        self.tokens = self.input_tokens + self.output_tokens + self.cache_read + self.cache_write;
    }
}

fn weighted(r: &UsageRow) -> f64 {
    let f = |v: i64| v.max(0) as f64;
    f(r.input_tokens) * W_INPUT
        + f(r.output_tokens) * W_OUTPUT
        + f(r.cache_write) * W_CACHE_WRITE
        + f(r.cache_read) * W_CACHE_READ
}

/// One account's part of a project's or session's usage.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountPart {
    pub account: String,
    pub requests: u64,
    pub tokens: u64,
}

/// One project (or session) line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Line {
    /// Session: the session hash. Project: the path hash, or `unknown`.
    pub key: String,
    /// The project's display name (with a key suffix when two projects share a name).
    pub project: String,
    #[serde(flatten)]
    pub usage: Usage,
    /// Share of all tokens in the range (0-1).
    #[serde(serialize_with = "round4")]
    pub share: f64,
    /// Estimated quota consumed, in subscriptions; `null` without usage history.
    #[serde(serialize_with = "round4_opt")]
    pub est_subs: Option<f64>,
    /// Serving accounts, most tokens first.
    pub accounts: Vec<AccountPart>,
    /// Sessions (project lines only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<u64>,
    pub first_seen: String,
    pub last_seen: String,
}

/// The quota consumed in the range and how much of it the estimate attributed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Quota {
    /// Subscriptions consumed by all sampled accounts in the range.
    #[serde(serialize_with = "round4")]
    pub consumed: f64,
    /// The part spent on accounts with no journaled tokens in the range.
    #[serde(serialize_with = "round4")]
    pub unattributed: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectReport {
    pub range: &'static str,
    pub since: String,
    pub now: String,
    pub totals: Usage,
    /// `null` when no account's usage was sampled rising in the range.
    pub quota: Option<Quota>,
    /// Most usage first (estimated quota, then tokens).
    pub projects: Vec<Line>,
    /// The [`SESSION_LIMIT`] sessions with the most usage.
    pub sessions: Vec<Line>,
    pub sessions_total: usize,
}

#[derive(Default)]
struct Acc {
    project: String,
    usage: Usage,
    weighted_by_account: HashMap<String, f64>,
    accounts: BTreeMap<String, (u64, u64)>,
    sessions: BTreeSet<String>,
    first: i64,
    last: i64,
}

impl Acc {
    fn add(&mut self, r: &UsageRow) {
        if self.usage.requests == 0 {
            self.first = r.ts;
        }
        self.first = self.first.min(r.ts);
        self.last = self.last.max(r.ts);
        self.usage.add(r);
        if let Some(a) = &r.account {
            let tokens = (r.input_tokens + r.output_tokens + r.cache_read + r.cache_write).max(0);
            let e = self.accounts.entry(a.clone()).or_default();
            e.0 += 1;
            e.1 += tokens as u64;
            *self.weighted_by_account.entry(a.clone()).or_default() += weighted(r);
        }
        if let Some(s) = &r.session_hash {
            self.sessions.insert(s.clone());
        }
    }

    fn line(self, key: String, total_tokens: u64, quota: &QuotaSplit, project_line: bool) -> Line {
        let est = quota.estimate(&self.weighted_by_account);
        let mut accounts: Vec<AccountPart> = self
            .accounts
            .into_iter()
            .map(|(account, (requests, tokens))| AccountPart {
                account,
                requests,
                tokens,
            })
            .collect();
        accounts.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.account.cmp(&b.account)));
        Line {
            key,
            project: self.project,
            share: if total_tokens == 0 {
                0.0
            } else {
                self.usage.tokens as f64 / total_tokens as f64
            },
            usage: self.usage,
            est_subs: est,
            accounts,
            sessions: project_line.then_some(self.sessions.len() as u64),
            first_seen: rfc3339(self.first),
            last_seen: rfc3339(self.last),
        }
    }
}

/// Per account: its consumption in the range and its total weighted tokens.
struct QuotaSplit {
    by_account: BTreeMap<String, (f64, f64)>,
    known: bool,
}

impl QuotaSplit {
    fn estimate(&self, weighted: &HashMap<String, f64>) -> Option<f64> {
        if !self.known {
            return None;
        }
        Some(
            weighted
                .iter()
                .filter_map(|(a, w)| {
                    let (consumed, total) = self.by_account.get(a)?;
                    (*total > 0.0).then(|| consumed * w / total)
                })
                .sum(),
        )
    }
}

fn by_usage(a: &Line, b: &Line) -> std::cmp::Ordering {
    let est = |l: &Line| l.est_subs.unwrap_or(0.0);
    est(b)
        .total_cmp(&est(a))
        .then(b.usage.tokens.cmp(&a.usage.tokens))
        .then(b.usage.requests.cmp(&a.usage.requests))
        .then(a.project.cmp(&b.project))
        .then(a.key.cmp(&b.key))
}

/// Build the report from journal `rows` (oldest first) and each account's 7-day consumption in
/// the range (`consumed`, from [`consumed_between`]).
pub fn compute(
    rows: &[UsageRow],
    consumed: &BTreeMap<String, f64>,
    range: Range,
    now: i64,
) -> ProjectReport {
    let mut totals = Usage::default();
    let mut account_weight: BTreeMap<String, f64> = BTreeMap::new();
    let mut projects: BTreeMap<String, Acc> = BTreeMap::new();
    let mut sessions: BTreeMap<String, Acc> = BTreeMap::new();
    for r in rows {
        totals.add(r);
        if let Some(a) = &r.account {
            *account_weight.entry(a.clone()).or_default() += weighted(r);
        }
        let name = r.project.clone().unwrap_or_else(|| UNKNOWN.to_string());
        let key = match (&r.project, &r.path_hash) {
            (Some(_), Some(h)) => h.clone(),
            (Some(p), None) => format!("name:{p}"),
            (None, _) => UNKNOWN.to_string(),
        };
        let p = projects.entry(key).or_default();
        p.project.clone_from(&name);
        p.add(r);
        if let Some(s) = &r.session_hash {
            let e = sessions.entry(s.clone()).or_default();
            e.project.clone_from(&name);
            e.add(r);
        }
    }

    let unattributed: f64 = consumed
        .iter()
        .filter(|(a, _)| account_weight.get(*a).copied().unwrap_or(0.0) <= 0.0)
        .map(|(_, c)| c)
        .sum();
    let quota_split = QuotaSplit {
        by_account: consumed
            .iter()
            .map(|(a, c)| {
                (
                    a.clone(),
                    (*c, account_weight.get(a).copied().unwrap_or(0.0)),
                )
            })
            .collect(),
        known: !consumed.is_empty(),
    };
    let quota = quota_split.known.then(|| Quota {
        consumed: consumed.values().sum(),
        unattributed,
    });

    // Same-named projects: tell them apart by the key.
    let mut names: HashMap<String, u32> = HashMap::new();
    for p in projects.values() {
        *names.entry(p.project.clone()).or_default() += 1;
    }
    let display = |name: &str, key: &str| -> String {
        if names.get(name).copied().unwrap_or(0) > 1 && key != UNKNOWN {
            let short: String = key.trim_start_matches("name:").chars().take(4).collect();
            format!("{name} ({short})")
        } else {
            name.to_string()
        }
    };
    // Session lines show their project's display name.
    let mut session_project: HashMap<String, String> = HashMap::new();
    for r in rows {
        if let (Some(s), Some(p), Some(h)) = (&r.session_hash, &r.project, &r.path_hash) {
            session_project.insert(s.clone(), display(p, h));
        }
    }

    let total_tokens = totals.tokens;
    let mut project_lines: Vec<Line> = projects
        .into_iter()
        .map(|(key, mut acc)| {
            acc.project = display(&acc.project, &key);
            acc.line(key, total_tokens, &quota_split, true)
        })
        .collect();
    project_lines.sort_by(by_usage);
    let mut session_lines: Vec<Line> = sessions
        .into_iter()
        .map(|(key, mut acc)| {
            if let Some(p) = session_project.get(&key) {
                acc.project.clone_from(p);
            }
            acc.line(key, total_tokens, &quota_split, false)
        })
        .collect();
    session_lines.sort_by(by_usage);
    let sessions_total = session_lines.len();
    session_lines.truncate(SESSION_LIMIT);

    ProjectReport {
        range: range.as_str(),
        since: rfc3339(now - range.secs()),
        now: rfc3339(now),
        totals,
        quota,
        projects: project_lines,
        sessions: session_lines,
        sessions_total,
    }
}

/// The report for the `range` ending at `now`, from the store. A blocking read: call off the
/// async threads.
pub fn report(store: &Store, now: OffsetDateTime, range: Range) -> anyhow::Result<ProjectReport> {
    let now = now.unix_timestamp();
    let since = now - range.secs();
    let rows = store.read(|c| usage_rows(c, since))?;
    let history = store.read(|c| usage_history::rows_since(c, WINDOW_7D, since))?;
    let consumed = consumed_between(&history, since, now);
    Ok(compute(&rows, &consumed, range, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_182_400;

    fn row(
        ts: i64,
        session: Option<&str>,
        project: Option<(&str, &str)>,
        account: &str,
        tokens: (i64, i64, i64, i64),
    ) -> UsageRow {
        UsageRow {
            ts,
            session_hash: session.map(str::to_string),
            project: project.map(|p| p.0.to_string()),
            path_hash: project.map(|p| p.1.to_string()),
            account: (!account.is_empty()).then(|| account.to_string()),
            input_tokens: tokens.0,
            output_tokens: tokens.1,
            cache_read: tokens.2,
            cache_write: tokens.3,
        }
    }

    fn fixture() -> Vec<UsageRow> {
        let acme = Some(("acme", "1111aaaa22223333"));
        let widgets = Some(("widgets", "4444bbbb55556666"));
        vec![
            row(NOW - 300, Some("s1"), acme, "acct-a", (100, 50, 1000, 0)),
            row(NOW - 200, Some("s1"), acme, "acct-a", (100, 50, 1000, 0)),
            row(NOW - 150, Some("s2"), widgets, "acct-a", (100, 0, 0, 0)),
            row(NOW - 100, Some("s3"), acme, "acct-b", (10, 10, 0, 0)),
            row(NOW - 50, None, None, "acct-b", (5, 5, 0, 0)),
            // Failed before any attempt: a request, no tokens, no account.
            row(NOW - 10, Some("s4"), None, "", (0, 0, 0, 0)),
        ]
    }

    #[test]
    fn totals_shares_and_accounts_per_project() {
        let r = compute(&fixture(), &BTreeMap::new(), Range::Day, NOW);
        assert_eq!(r.range, "24h");
        assert_eq!(r.totals.requests, 6);
        assert_eq!(r.totals.tokens, 2300 + 100 + 20 + 10);
        assert_eq!(r.quota, None);
        let names: Vec<&str> = r.projects.iter().map(|p| p.project.as_str()).collect();
        assert_eq!(names, ["acme", "widgets", "unknown"], "most tokens first");
        let acme = &r.projects[0];
        assert_eq!(acme.key, "1111aaaa22223333");
        assert_eq!(acme.usage.requests, 3);
        assert_eq!(acme.usage.tokens, 2320);
        assert_eq!(acme.sessions, Some(2));
        assert_eq!(acme.est_subs, None);
        assert_eq!(
            acme.accounts,
            vec![
                AccountPart {
                    account: "acct-a".into(),
                    requests: 2,
                    tokens: 2300
                },
                AccountPart {
                    account: "acct-b".into(),
                    requests: 1,
                    tokens: 20
                },
            ]
        );
        let share_sum: f64 = r.projects.iter().map(|p| p.share).sum();
        assert!((share_sum - 1.0).abs() < 1e-9);
        let unknown = &r.projects[2];
        assert_eq!(
            (unknown.key.as_str(), unknown.usage.requests),
            ("unknown", 2)
        );
        assert_eq!(unknown.sessions, Some(1));
        // Sessions: only requests with a session; the session line carries its project.
        assert_eq!(r.sessions_total, 4);
        assert_eq!(r.sessions[0].key, "s1");
        assert_eq!(r.sessions[0].project, "acme");
        assert_eq!(r.sessions[0].sessions, None);
        assert_eq!(r.sessions[3].project, "unknown");
    }

    #[test]
    fn quota_is_split_by_weighted_tokens_per_account() {
        let consumed = BTreeMap::from([
            ("acct-a".to_string(), 0.3),
            ("acct-b".to_string(), 0.1),
            ("idle".to_string(), 0.05),
        ]);
        let r = compute(&fixture(), &consumed, Range::Week, NOW);
        let q = r.quota.clone().unwrap();
        assert!((q.consumed - 0.45).abs() < 1e-9);
        assert!((q.unattributed - 0.05).abs() < 1e-9, "idle served nothing");
        let est = |name: &str| {
            r.projects
                .iter()
                .find(|p| p.project == name)
                .unwrap()
                .est_subs
                .unwrap()
        };
        // acct-a weights: acme 2 x (100 + 250 + 100) = 900, widgets 100.
        // acct-b weights: acme 60, unknown 30.
        assert!((est("acme") - (0.3 * 0.9 + 0.1 * 60.0 / 90.0)).abs() < 1e-9);
        assert!((est("widgets") - 0.3 * 0.1).abs() < 1e-9);
        assert!((est("unknown") - 0.1 * 30.0 / 90.0).abs() < 1e-9);
        let attributed: f64 = r.projects.iter().filter_map(|p| p.est_subs).sum();
        assert!((attributed + q.unattributed - q.consumed).abs() < 1e-9);
    }

    #[test]
    fn same_named_projects_stay_apart() {
        let rows = vec![
            row(
                NOW - 5,
                Some("s1"),
                Some(("acme", "aaaa0000")),
                "a",
                (10, 0, 0, 0),
            ),
            row(
                NOW - 4,
                Some("s2"),
                Some(("acme", "bbbb0000")),
                "a",
                (5, 0, 0, 0),
            ),
        ];
        let r = compute(&rows, &BTreeMap::new(), Range::Day, NOW);
        let labels: Vec<&str> = r.projects.iter().map(|p| p.project.as_str()).collect();
        assert_eq!(labels, ["acme (aaaa)", "acme (bbbb)"]);
        assert_eq!(r.sessions[1].project, "acme (bbbb)");
    }

    #[test]
    fn empty_journal() {
        let r = compute(&[], &BTreeMap::new(), Range::Month, NOW);
        assert_eq!(r.totals, Usage::default());
        assert!(r.projects.is_empty() && r.sessions.is_empty());
        assert_eq!(r.since, rfc3339(NOW - 30 * 86_400));
    }

    #[test]
    fn report_reads_journal_and_usage_history() {
        use crate::analytics::attribution::Project;
        use crate::store::journal::{self, AttemptRow, RequestRow};
        use crate::store::usage_history::{insert_sample, Point};

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("pooltime.db")).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(store.exec(|c| {
            let req = RequestRow {
                id: "r1".into(),
                ts: NOW - 600,
                path: Some("/v1/messages".into()),
                outcome: "completed".into(),
                input_tokens: Some(100),
                output_tokens: Some(10),
                session_hash: Some("s1".into()),
                ..RequestRow::default()
            };
            let att = AttemptRow {
                n: 1,
                account: "acct-a".into(),
                provider: "anthropic".into(),
                outcome: "completed".into(),
                ..AttemptRow::default()
            };
            journal::insert(c, &req, &[att])?;
            let acme = Project {
                name: "acme".into(),
                path_hash: "1111aaaa22223333".into(),
            };
            crate::store::projects::upsert(c, "s1", &acme, NOW - 600)?;
            let reset = Some(NOW + 86_400);
            for (ts, util) in [(NOW - 3600, 0.2), (NOW - 60, 0.25)] {
                insert_sample(c, "acct-a", WINDOW_7D, &Point { ts, util, reset })?;
            }
            Ok(())
        }))
        .unwrap();
        let now = OffsetDateTime::from_unix_timestamp(NOW).unwrap();
        let r = report(&store, now, Range::Day).unwrap();
        assert_eq!(r.projects.len(), 1);
        assert_eq!(r.projects[0].project, "acme");
        let est = r.projects[0].est_subs.unwrap();
        assert!((est - 0.05).abs() < 1e-9, "{est}");
        assert!((r.quota.unwrap().consumed - 0.05).abs() < 1e-9);
    }

    #[test]
    fn ranges_parse() {
        assert_eq!(Range::parse("24h"), Some(Range::Day));
        assert_eq!(Range::parse("7d"), Some(Range::Week));
        assert_eq!(Range::parse("30d"), Some(Range::Month));
        assert_eq!(Range::parse("1y"), None);
    }
}
