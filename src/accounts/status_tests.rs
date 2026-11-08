use std::io;
use std::sync::{Arc, Mutex};

use time::OffsetDateTime;

use super::*;
use crate::errors::{FailReason, RefreshErrorKind};
use crate::store::Store;

const BASE: i64 = 1_800_000_000;

fn t(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(BASE + secs).unwrap()
}

fn id(s: &str) -> AccountId {
    s.parse().unwrap()
}

fn win(
    key: &str,
    model: Option<&str>,
    status: WindowStatus,
    utilization: f64,
    reset: Option<OffsetDateTime>,
) -> Window {
    Window {
        key: key.to_string(),
        model: model.map(str::to_string),
        status,
        utilization,
        reset,
    }
}

fn snap(windows: Vec<Window>, at: OffsetDateTime) -> RateLimitSnapshot {
    RateLimitSnapshot {
        unified_status: None,
        windows,
        updated_at: at,
    }
}

fn keys(s: &RateLimitSnapshot) -> Vec<&str> {
    s.windows.iter().map(|w| w.key.as_str()).collect()
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    book: StateBook,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
    let book = StateBook::load_at(store.clone(), t(0)).unwrap();
    Fixture {
        _dir: dir,
        store,
        book,
    }
}

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read: 0,
        cache_write: 0,
    }
}

#[test]
fn status_precedence() {
    let f = fixture();
    let b = &f.book;

    // Ready / NeedsLogin from creds
    let a = id("a");
    assert_eq!(b.status(&a, true, t(0)), AccountStatus::Ready);
    assert_eq!(b.status(&a, false, t(0)), AccountStatus::NeedsLogin);

    // rate limited alone, then it expires on its own
    b.mark_rate_limited(&a, t(600), Some("5h".into()), t(0));
    assert_eq!(
        b.status(&a, true, t(0)),
        AccountStatus::RateLimited {
            until: t(600),
            window: Some("5h".into())
        }
    );
    assert_eq!(b.status(&a, true, t(600)), AccountStatus::Ready);

    // AccessDenied beats RateLimited
    let d = id("denied");
    b.mark_rate_limited(&d, t(5000), None, t(0));
    b.mark_access_denied(&d, t(0));
    assert_eq!(
        b.status(&d, true, t(10)),
        AccountStatus::AccessDenied { until: t(3600) }
    );
    // after the denial cooldown the (longer) rate limit shows again
    assert_eq!(
        b.status(&d, true, t(3600)),
        AccountStatus::RateLimited {
            until: t(5000),
            window: None
        }
    );

    // Billing beats AccessDenied-less RateLimited
    let bl = id("billing");
    b.mark_rate_limited(&bl, t(5000), None, t(0));
    b.mark_billing(&bl, t(0));
    assert_eq!(b.status(&bl, true, t(10)), AccountStatus::Billing);
    // a dead refresh token beats Billing; so do missing creds
    b.mark_refresh_dead(&bl, "rt-secret-1", t(20));
    assert_eq!(b.status(&bl, true, t(30)), AccountStatus::NeedsLogin);
    b.clear_refresh_dead(&bl, t(40));
    assert_eq!(b.status(&bl, true, t(50)), AccountStatus::Billing);
    assert_eq!(b.status(&bl, false, t(50)), AccountStatus::NeedsLogin);
    // billing flag outlives its cooldown but stops counting as a block
    assert_eq!(
        b.status(&bl, true, t(3700)),
        AccountStatus::RateLimited {
            until: t(5000),
            window: None
        }
    );
    // manual recheck clears billing and its cooldown
    b.clear_billing(&bl, t(60));
    assert_eq!(
        b.status(&bl, true, t(60)),
        AccountStatus::RateLimited {
            until: t(5000),
            window: None
        }
    );

    // a non-billing denial does not look like billing, and clears a stale billing flag
    let m = id("mixed");
    b.mark_billing(&m, t(0));
    b.mark_access_denied(&m, t(10));
    assert_eq!(
        b.status(&m, true, t(20)),
        AccountStatus::AccessDenied { until: t(3610) }
    );
    assert_eq!(b.state(&m, t(20)).billing_blocked_at, None);

    // a spent account-wide window with future reset is RateLimited too
    let w = id("windowed");
    b.merge_header_snapshot(
        &w,
        snap(
            vec![win("5h", None, WindowStatus::Rejected, 1.0, Some(t(900)))],
            t(0),
        ),
    );
    assert_eq!(
        b.status(&w, true, t(0)),
        AccountStatus::RateLimited {
            until: t(900),
            window: Some("5h".into())
        }
    );
    assert_eq!(b.status(&w, true, t(900)), AccountStatus::Ready);
}

#[test]
fn model_scoped_spent_window_does_not_change_status() {
    let f = fixture();
    let b = &f.book;
    let a = id("a");
    b.merge_header_snapshot(
        &a,
        snap(
            vec![
                win("5h", None, WindowStatus::Allowed, 0.4, Some(t(1000))),
                win(
                    "7d-fable",
                    Some("fable"),
                    WindowStatus::Rejected,
                    1.0,
                    Some(t(5000)),
                ),
            ],
            t(0),
        ),
    );
    assert_eq!(b.status(&a, true, t(1)), AccountStatus::Ready);
    assert_eq!(b.spent_families(&a, t(1)), vec!["fable".to_string()]);
    // once the model window resets it is no longer reported
    assert!(b.spent_families(&a, t(5000)).is_empty());

    // a poll with a spent model window neither sets a cooldown nor changes status
    b.replace_poll_snapshot(
        &a,
        snap(
            vec![
                win("5h", None, WindowStatus::Allowed, 0.4, Some(t(1000))),
                win(
                    "7d-fable",
                    Some("fable"),
                    WindowStatus::Allowed,
                    1.0,
                    Some(t(5000)),
                ),
            ],
            t(2),
        ),
        t(1),
    );
    assert_eq!(b.state(&a, t(2)).rate_limited_until, None);
    assert_eq!(b.status(&a, true, t(2)), AccountStatus::Ready);
    // an unknown account has no spent families
    assert!(b.spent_families(&id("nobody"), t(0)).is_empty());
}

#[test]
fn explicit_allowed_overrides_full_utilization() {
    let f = fixture();
    let b = &f.book;
    let cases = [
        // (status, utilization, expect blocked)
        (WindowStatus::Allowed, 1.0, false),
        (WindowStatus::Allowed, 1.5, false),
        (WindowStatus::AllowedWarning, 1.0, true),
        (WindowStatus::Unknown, 1.0, true),
        (WindowStatus::Unknown, 0.99, false),
        (WindowStatus::Rejected, 0.2, true),
    ];
    for (i, (status, util, blocked)) in cases.into_iter().enumerate() {
        let a = id(&format!("acct{i}"));
        b.merge_header_snapshot(
            &a,
            snap(vec![win("5h", None, status, util, Some(t(900)))], t(0)),
        );
        let s = b.status(&a, true, t(1));
        assert_eq!(
            matches!(s, AccountStatus::RateLimited { .. }),
            blocked,
            "{status:?} @ {util}: got {s:?}"
        );
    }
}

#[test]
fn poll_started_before_billing_block_does_not_clear_it() {
    let f = fixture();
    let b = &f.book;
    let a = id("a");
    b.mark_billing(&a, t(100));
    let clean = |at| {
        snap(
            vec![win("5h", None, WindowStatus::Allowed, 0.1, Some(t(9000)))],
            t(at),
        )
    };

    // probe began (t=90) before the block (t=100) but answered after it
    b.replace_poll_snapshot(&a, clean(101), t(90));
    assert_eq!(b.status(&a, true, t(102)), AccountStatus::Billing);
    let st = b.state(&a, t(102));
    assert_eq!(st.billing_blocked_at, Some(t(100)));
    assert!(st.snapshot.is_some(), "snapshot still recorded");
    assert!(st.usage_check.as_ref().unwrap().ok);

    // a probe that started after the block heals it
    b.replace_poll_snapshot(&a, clean(200), t(150));
    assert_eq!(b.status(&a, true, t(201)), AccountStatus::Ready);
    let st = b.state(&a, t(201));
    assert_eq!(st.billing_blocked_at, None);
    assert_eq!(st.access_denied_until, None);
}

#[test]
fn poll_replace_keeps_overage_window() {
    let f = fixture();
    let b = &f.book;
    let a = id("a");
    b.merge_header_snapshot(
        &a,
        snap(
            vec![
                win("5h", None, WindowStatus::Allowed, 0.2, Some(t(100))),
                win("7d", None, WindowStatus::Allowed, 0.3, Some(t(200))),
                win("7d-opus", Some("opus"), WindowStatus::Allowed, 0.1, None),
                win("overage", None, WindowStatus::Allowed, 0.0, None),
            ],
            t(0),
        ),
    );
    b.replace_poll_snapshot(
        &a,
        snap(
            vec![win("5h", None, WindowStatus::Allowed, 0.5, Some(t(300)))],
            t(10),
        ),
        t(9),
    );
    let s = b.state(&a, t(10)).snapshot.unwrap();
    // 7d and 7d-opus are duration-keyed and gone; overage survives; 5h is the fresh value
    assert_eq!(keys(&s), vec!["5h", "overage"]);
    assert_eq!(s.windows[0].utilization, 0.5);
    assert_eq!(s.updated_at, t(10));
}

#[test]
fn poll_extends_cooldown_clears_stale_one_and_synthesizes_reset() {
    let f = fixture();
    let b = &f.book;
    let a = id("a");

    // a 429 set a short cooldown; the poll shows the 5h window spent until later: extend only
    b.mark_rate_limited(&a, t(100), None, t(0));
    b.replace_poll_snapshot(
        &a,
        snap(
            vec![win("5h", None, WindowStatus::Rejected, 1.0, Some(t(800)))],
            t(10),
        ),
        t(9),
    );
    let st = b.state(&a, t(10));
    assert_eq!(st.rate_limited_until, Some(t(800)));
    assert_eq!(st.rate_limited_window.as_deref(), Some("5h"));

    // a later cooldown is never shortened by a poll window that resets sooner
    b.mark_rate_limited(&a, t(5000), None, t(20));
    b.replace_poll_snapshot(
        &a,
        snap(
            vec![win("5h", None, WindowStatus::Rejected, 1.0, Some(t(800)))],
            t(30),
        ),
        t(29),
    );
    assert_eq!(b.state(&a, t(30)).rate_limited_until, Some(t(5000)));

    // nothing blocking: the stale cooldown (and its rate-limit last_error) are dropped
    b.record_error(
        &a,
        ErrorRecord {
            at: t(31),
            reason: FailReason::Quota {
                reset: Some(t(5000)),
            },
            upstream_request_id: Some("req_1".into()),
        },
    );
    b.replace_poll_snapshot(
        &a,
        snap(
            vec![win("5h", None, WindowStatus::Allowed, 0.1, Some(t(9000)))],
            t(40),
        ),
        t(39),
    );
    let st = b.state(&a, t(40));
    assert_eq!(st.rate_limited_until, None);
    assert_eq!(st.rate_limited_window, None);
    assert_eq!(st.last_error, None);
    assert_eq!(b.status(&a, true, t(40)), AccountStatus::Ready);

    // an unrelated last_error is kept
    b.record_error(
        &a,
        ErrorRecord {
            at: t(41),
            reason: FailReason::Overloaded,
            upstream_request_id: None,
        },
    );
    b.replace_poll_snapshot(&a, snap(vec![], t(50)), t(49));
    assert!(b.state(&a, t(50)).last_error.is_some());

    // spent window without a reset gets now + duration
    let c = id("c");
    b.replace_poll_snapshot(
        &c,
        snap(
            vec![
                win("5h", None, WindowStatus::Rejected, 1.0, None),
                win("overage", None, WindowStatus::Rejected, 1.0, None),
            ],
            t(100),
        ),
        t(99),
    );
    let s = b.state(&c, t(100)).snapshot.unwrap();
    assert_eq!(s.windows[0].reset, Some(t(100 + 5 * 3600)));
    assert_eq!(
        s.windows[1].reset, None,
        "no duration token, nothing to synthesize"
    );
    assert_eq!(
        b.status(&c, true, t(101)),
        AccountStatus::RateLimited {
            until: t(100 + 5 * 3600),
            window: Some("5h".into())
        }
    );
}

#[test]
fn header_merge_is_per_window_key() {
    let f = fixture();
    let b = &f.book;
    let a = id("a");
    let mut first = snap(
        vec![
            win("7d", None, WindowStatus::Allowed, 0.3, Some(t(500))),
            win("5h", None, WindowStatus::Allowed, 0.2, Some(t(100))),
        ],
        t(0),
    );
    first.unified_status = Some("allowed".into());
    b.merge_header_snapshot(&a, first);

    // second response only reports 5h and a model-scoped window; unified_status absent
    b.merge_header_snapshot(
        &a,
        snap(
            vec![
                win("5h", None, WindowStatus::AllowedWarning, 0.9, Some(t(150))),
                win("7d-fable", Some("fable"), WindowStatus::Allowed, 0.1, None),
            ],
            t(5),
        ),
    );
    let s = b.state(&a, t(5)).snapshot.unwrap();
    // account-wide first, then model-scoped, each sorted by key
    assert_eq!(keys(&s), vec!["5h", "7d", "7d-fable"]);
    assert_eq!(s.windows[0].utilization, 0.9, "fresh data wins per key");
    assert_eq!(s.windows[0].status, WindowStatus::AllowedWarning);
    assert_eq!(s.windows[1].utilization, 0.3, "absent window is kept");
    assert_eq!(s.unified_status.as_deref(), Some("allowed"));
    assert_eq!(s.updated_at, t(5));
}

/// Writer that appends into a shared buffer so a test can read emitted log lines.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn timeline_rows(store: &Store) -> Vec<(i64, String, String, String, String)> {
    store
        .read(|c| {
            let mut st = c.prepare(
                "SELECT ts, account, from_state, to_state, reason FROM timeline ORDER BY rowid",
            )?;
            let rows = st.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
        .unwrap()
}

#[tokio::test]
async fn every_transition_emits_event_and_timeline_row() {
    let f = fixture();
    let b = &f.book;
    let a = id("a");
    let mut rx = b.transitions();

    crate::test_support::ensure_callsites_stay_enabled();
    let buf = SharedBuf::default();
    let sub = {
        let buf = buf.clone();
        tracing_subscriber::fmt()
            .json()
            .with_writer(move || buf.clone())
            .finish()
    };
    tracing::subscriber::with_default(sub, || {
        b.mark_rate_limited(&a, t(600), Some("5h".into()), t(0)); // ready -> rate_limited
        b.mark_rate_limited(&a, t(900), Some("5h".into()), t(1)); // same status: no event
        b.mark_billing(&a, t(2)); // rate_limited -> billing
        b.clear_billing(&a, t(3)); // billing -> rate_limited
        b.reevaluate(&a, true, t(1000)); // rate_limited -> ready (cooldown over)
        b.reevaluate(&a, true, t(1001)); // no change: no event
        b.reevaluate(&a, false, t(1002)); // ready -> needs_login
        b.reevaluate(&a, true, t(1003)); // needs_login -> ready
    });

    let mut got = Vec::new();
    while let Ok(tr) = rx.try_recv() {
        got.push(tr);
    }
    let labels: Vec<(&str, &str)> = got
        .iter()
        .map(|tr| (tr.from.label(), tr.to.label()))
        .collect();
    assert_eq!(
        labels,
        vec![
            ("ready", "rate_limited"),
            ("rate_limited", "billing"),
            ("billing", "rate_limited"),
            ("rate_limited", "ready"),
            ("ready", "needs_login"),
            ("needs_login", "ready"),
        ]
    );
    assert!(got
        .iter()
        .all(|tr| tr.account == a && !tr.reason.is_empty()));
    assert_eq!(got[0].at, t(0));
    assert_eq!(got[3].reason, "cooldown_expired");

    // timeline rows mirror the events, in order
    f.book.flush().await.unwrap();
    let rows = timeline_rows(&f.store);
    assert_eq!(rows.len(), got.len());
    for (row, tr) in rows.iter().zip(&got) {
        assert_eq!(row.0, tr.at.unix_timestamp());
        assert_eq!(row.1, "a");
        assert_eq!(row.2, tr.from.label());
        assert_eq!(row.3, tr.to.label());
        assert_eq!(row.4, tr.reason);
    }

    // one log line per transition: warn when leaving Ready, info on return
    let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), got.len(), "{text}");
    for (line, tr) in lines.iter().zip(&got) {
        let want = if matches!(tr.to, AccountStatus::Ready) {
            "INFO"
        } else {
            "WARN"
        };
        assert_eq!(line["level"], want);
        assert_eq!(line["fields"]["account"], "a");
        assert_eq!(line["fields"]["from"], tr.from.label());
        assert_eq!(line["fields"]["to"], tr.to.label());
        assert_eq!(line["fields"]["reason"], tr.reason.as_str());
    }
}

#[tokio::test]
async fn state_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pooltime.db");
    let (a, d) = (id("a"), id("dead"));

    let (state_a, state_d) = {
        let store = Arc::new(Store::open(&path).unwrap());
        let book = StateBook::load_at(store.clone(), t(0)).unwrap();
        book.mark_rate_limited(&a, t(7200), Some("7d".into()), t(0));
        book.merge_header_snapshot(
            &a,
            snap(
                vec![
                    win("5h", None, WindowStatus::Allowed, 0.25, Some(t(300))),
                    win("7d-fable", Some("fable"), WindowStatus::Rejected, 1.0, None),
                ],
                t(1),
            ),
        );
        book.record_success(&a, usage(10, 20), t(2));
        book.record_error(
            &a,
            ErrorRecord {
                at: t(3),
                reason: FailReason::Refresh(RefreshErrorKind::Server(503)),
                upstream_request_id: Some("req_x".into()),
            },
        );
        book.mark_refresh_dead(&d, "rt-very-secret", t(4));
        book.flush().await.unwrap();
        (book.state(&a, t(5)), book.state(&d, t(5)))
        // book and store dropped here
    };

    let store = Arc::new(Store::open(&path).unwrap());
    let book = StateBook::load_at(store.clone(), t(10)).unwrap();
    let mut rx = book.transitions();
    assert_eq!(book.state(&a, t(5)), state_a);
    assert_eq!(book.state(&d, t(5)), state_d);
    assert_eq!(book.status(&d, true, t(10)), AccountStatus::NeedsLogin);
    assert_eq!(
        book.status(&a, true, t(10)),
        AccountStatus::RateLimited {
            until: t(7200),
            window: Some("7d".into())
        }
    );
    // loading is not a transition, and a re-check of the same state emits nothing
    book.reevaluate(&d, true, t(11));
    book.reevaluate(&a, true, t(11));
    assert!(rx.try_recv().is_err());

    // the dead-token marker is a fingerprint, never the token itself
    let fp = state_d.dead_refresh_fp.unwrap();
    assert_eq!(fp.len(), 16);
    assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()));
    let json: String = store
        .read(|c| {
            Ok(c.query_row(
                "SELECT json FROM account_state WHERE account='dead'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(!json.contains("rt-very-secret"));
}

fn quarantine_rows(store: &Store) -> Vec<(String, Option<String>)> {
    store
        .read(|c| {
            let mut st =
                c.prepare("SELECT account, json FROM account_state_quarantine ORDER BY rowid")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
        .unwrap()
}

/// INV §10.9: a corrupt row may have held the dead-token marker, so it must never read as
/// Ready. It is quarantined (raw bytes kept), the account fails closed as `needs_login`, and
/// the next proof of health (a successful poll or refresh, or a re-login) restores it.
#[tokio::test]
async fn corrupt_row_fails_closed_and_is_quarantined() {
    let f = fixture();
    let (good, bad, bad2) = (id("good"), id("bad"), id("bad2"));
    f.book
        .mark_rate_limited(&good, t(7200), Some("7d".into()), t(0));
    f.book.flush().await.unwrap();
    f.store
        .exec(|c| {
            c.execute(
                "INSERT INTO account_state(account, json) VALUES ('bad', '{not json dead_refresh_fp')",
                [],
            )?;
            c.execute(
                "INSERT INTO account_state(account, json) VALUES ('bad2', '[1,2')",
                [],
            )?;
            c.execute(
                "INSERT INTO account_state(account, json) VALUES ('../evil', '{}')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    crate::test_support::ensure_callsites_stay_enabled();
    let buf = SharedBuf::default();
    let sub = {
        let buf = buf.clone();
        tracing_subscriber::fmt()
            .json()
            .with_writer(move || buf.clone())
            .finish()
    };
    let book = tracing::subscriber::with_default(sub, || {
        StateBook::load_at(f.store.clone(), t(100)).unwrap()
    });
    book.flush().await.unwrap();

    // Load succeeded, the healthy row is intact, the corrupt accounts are not routable.
    assert!(matches!(
        book.status(&good, true, t(100)),
        AccountStatus::RateLimited { .. }
    ));
    for a in [&bad, &bad2] {
        assert_eq!(book.status(a, true, t(100)), AccountStatus::NeedsLogin);
        assert!(book.is_unverified(a));
    }
    assert!(!book.is_unverified(&good));
    // Loading is not a transition.
    assert!(book.transitions().try_recv().is_err());

    // Raw bytes preserved, one row per corrupt account (the invalid id has no account).
    assert_eq!(
        quarantine_rows(&f.store),
        vec![
            (
                "bad".to_string(),
                Some("{not json dead_refresh_fp".to_string())
            ),
            ("bad2".to_string(), Some("[1,2".to_string())),
        ]
    );

    // A warn per corrupt account names only the account id, never row content.
    let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let mut warned: Vec<String> = text
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|l| l["level"] == "WARN")
        .filter_map(|l| l["fields"]["account"].as_str().map(str::to_string))
        .collect();
    warned.sort();
    assert_eq!(warned, ["bad", "bad2"], "{text}");
    assert!(
        !text.contains("dead_refresh_fp") && !text.contains("[1,2"),
        "{text}"
    );

    // The unverified flag is persisted, so a restart keeps failing closed without
    // quarantining the replacement row again.
    let book2 = StateBook::load_at(f.store.clone(), t(200)).unwrap();
    book2.flush().await.unwrap();
    assert_eq!(book2.status(&bad, true, t(200)), AccountStatus::NeedsLogin);
    assert_eq!(quarantine_rows(&f.store).len(), 2);

    // Recovery 1: a successful usage poll proves the account healthy.
    book.replace_poll_snapshot(
        &bad,
        snap(
            vec![win("5h", None, WindowStatus::Allowed, 0.1, Some(t(900)))],
            t(300),
        ),
        t(299),
    );
    assert_eq!(book.status(&bad, true, t(300)), AccountStatus::Ready);
    assert!(!book.is_unverified(&bad));
    // Recovery 2: a successful token refresh.
    book.mark_verified(&bad2, t(301));
    assert_eq!(book.status(&bad2, true, t(301)), AccountStatus::Ready);
    book.flush().await.unwrap();
    let book3 = StateBook::load_at(f.store.clone(), t(400)).unwrap();
    assert_eq!(book3.status(&bad, true, t(400)), AccountStatus::Ready);
    assert_eq!(book3.status(&bad2, true, t(400)), AccountStatus::Ready);
    assert_eq!(quarantine_rows(&f.store).len(), 2);
}

/// A rejected refresh token supersedes the unreadable-state marker (the sweeper then stops
/// re-trying it); a re-login also clears it.
#[tokio::test]
async fn unverified_is_superseded_by_dead_marker_and_relogin() {
    let f = fixture();
    f.store
        .exec(|c| {
            c.execute(
                "INSERT INTO account_state(account, json) VALUES ('a', 'x')",
                [],
            )?;
            c.execute(
                "INSERT INTO account_state(account, json) VALUES ('b', 'x')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let book = StateBook::load_at(f.store.clone(), t(0)).unwrap();
    let (a, b) = (id("a"), id("b"));
    book.mark_refresh_dead(&a, "rt", t(1));
    assert!(!book.is_unverified(&a));
    assert!(book.is_refresh_dead(&a, "rt"));
    assert_eq!(book.status(&a, true, t(1)), AccountStatus::NeedsLogin);
    book.clear_refresh_dead(&b, t(1));
    assert!(!book.is_unverified(&b));
    assert_eq!(book.status(&b, true, t(1)), AccountStatus::Ready);
}

#[test]
fn last_used_only_on_success() {
    let f = fixture();
    let b = &f.book;
    let a = id("a");

    b.record_error(
        &a,
        ErrorRecord {
            at: t(10),
            reason: FailReason::Status(500),
            upstream_request_id: Some("req_1".into()),
        },
    );
    let st = b.state(&a, t(10));
    assert_eq!(st.last_used_at, None);
    assert_eq!(st.last_success_at, None);
    assert_eq!(
        st.last_error.as_ref().unwrap().reason,
        FailReason::Status(500)
    );
    assert_eq!(st.window_counters.errors, 1);
    assert_eq!(st.lifetime.errors, 1);

    b.mark_access_denied(&a, t(15));
    b.record_success(&a, usage(100, 50), t(20));
    let st = b.state(&a, t(20));
    assert_eq!(st.last_used_at, Some(t(20)));
    assert_eq!(st.last_success_at, Some(t(20)));
    assert_eq!(st.last_error, None, "success clears last_error");
    assert_eq!(st.access_denied_until, None, "success clears access denied");
    assert_eq!(st.billing_blocked_at, None);
    assert_eq!(b.status(&a, true, t(21)), AccountStatus::Ready);
    assert_eq!(st.lifetime.successes, 1);
    assert_eq!(st.lifetime.input_tokens, 100);
    assert_eq!(st.lifetime.output_tokens, 50);

    // a later error leaves last_used_at alone
    b.record_error(
        &a,
        ErrorRecord {
            at: t(30),
            reason: FailReason::Overloaded,
            upstream_request_id: None,
        },
    );
    let st = b.state(&a, t(30));
    assert_eq!(st.last_used_at, Some(t(20)));
    assert_eq!(st.last_error.unwrap().reason, FailReason::Overloaded);
}

#[test]
fn success_clears_billing_block() {
    let f = fixture();
    let b = &f.book;
    let a = id("a");
    b.mark_billing(&a, t(0));
    assert_eq!(b.status(&a, true, t(1)), AccountStatus::Billing);
    b.record_success(&a, usage(1, 1), t(2));
    assert_eq!(b.status(&a, true, t(3)), AccountStatus::Ready);
}

#[test]
fn counters_roll_after_usage_window_but_lifetime_keeps_counting() {
    let f = fixture();
    let b = &f.book;
    let a = id("a");
    b.record_success(&a, usage(10, 1), t(0));
    b.record_success(&a, usage(10, 1), t(3600));
    let st = b.state(&a, t(3600));
    assert_eq!(st.window_counters.requests, 2);
    assert_eq!(st.window_counters.input_tokens, 20);

    // 5h after the window opened: the next request starts a fresh window
    b.record_success(&a, usage(7, 1), t(5 * 3600));
    let st = b.state(&a, t(5 * 3600));
    assert_eq!(st.window_counters.requests, 1);
    assert_eq!(st.window_counters.input_tokens, 7);
    assert_eq!(st.lifetime.requests, 3);
    assert_eq!(st.lifetime.input_tokens, 27);

    // reading after the window lapsed shows an empty window without mutating
    let st = b.state(&a, t(11 * 3600));
    assert_eq!(st.window_counters, Counters::default());
    assert_eq!(st.lifetime.requests, 3);
}

#[test]
fn usage_check_error_is_recorded_as_label_only() {
    let f = fixture();
    let b = &f.book;
    let a = id("a");
    b.record_usage_check_error(&a, t(5), "timeout");
    let uc = b.state(&a, t(5)).usage_check.unwrap();
    assert_eq!(
        uc,
        UsageCheck {
            at: t(5),
            ok: false,
            error: Some("timeout".into())
        }
    );
}

#[test]
fn window_duration_parses_keys_like_bro() {
    use time::Duration;
    assert_eq!(window_duration("5h"), Some(Duration::hours(5)));
    assert_eq!(window_duration("7d"), Some(Duration::days(7)));
    assert_eq!(window_duration("7d-fable"), Some(Duration::days(7)));
    assert_eq!(window_duration("fable_7d"), Some(Duration::days(7)));
    assert_eq!(window_duration("30min"), Some(Duration::minutes(30)));
    assert_eq!(window_duration("2wk"), Some(Duration::days(14)));
    assert_eq!(window_duration("1mo"), Some(Duration::days(30)));
    assert_eq!(window_duration("overage"), None);
    assert_eq!(window_duration("h"), None);
    assert_eq!(window_duration(""), None);
}

#[test]
fn status_and_state_serde_round_trip() {
    let st = AccountState {
        snapshot: Some(snap(
            vec![win("5h", None, WindowStatus::Allowed, 0.5, Some(t(9)))],
            t(1),
        )),
        rate_limited_until: Some(t(5)),
        last_error: Some(ErrorRecord {
            at: t(2),
            reason: FailReason::Quota { reset: None },
            upstream_request_id: None,
        }),
        ..AccountState::default()
    };
    let json = serde_json::to_string(&st).unwrap();
    assert_eq!(serde_json::from_str::<AccountState>(&json).unwrap(), st);
    // unknown/missing fields default, so older rows keep loading
    assert_eq!(
        serde_json::from_str::<AccountState>("{}").unwrap(),
        AccountState::default()
    );
    assert_eq!(
        serde_json::to_string(&AccountStatus::RateLimited {
            until: t(0),
            window: None
        })
        .unwrap(),
        format!(
            r#"{{"status":"rate_limited","until":"{}","window":null}}"#,
            t(0).format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        )
    );
    assert_eq!(
        serde_json::to_string(&AccountStatus::NeedsLogin).unwrap(),
        r#"{"status":"needs_login"}"#
    );
    assert_eq!(
        serde_json::to_string(&AccountStatus::Erroring {
            class: "status:401".into(),
            count: 3
        })
        .unwrap(),
        r#"{"status":"erroring","class":"status:401","count":3}"#
    );
    assert_eq!(
        serde_json::to_string(&AccountStatus::Expiring { expires_at: t(0) }).unwrap(),
        format!(
            r#"{{"status":"expiring","expires_at":"{}"}}"#,
            t(0).format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        )
    );
}

fn failure(reason: FailReason, at: OffsetDateTime) -> ErrorRecord {
    ErrorRecord {
        at,
        reason,
        upstream_request_id: None,
    }
}

#[test]
fn erroring_threshold_and_clear() {
    use crate::health::states::Erroring;
    let f = fixture();
    let b = &f.book;
    let a = id("a");
    let server = || FailReason::Refresh(RefreshErrorKind::Server(503));

    // Two counted failures: not erroring yet. Quota failures never count.
    b.record_error(&a, failure(server(), t(0)));
    b.record_error(&a, failure(FailReason::Quota { reset: None }, t(10)));
    b.record_error(&a, failure(server(), t(20)));
    assert_eq!(b.erroring(&a, t(30)), None);
    // The third within 15 minutes: erroring, with the last class and the count.
    b.record_error(&a, failure(FailReason::Status(401), t(30)));
    assert_eq!(
        b.erroring(&a, t(40)),
        Some(Erroring {
            class: "status:401".into(),
            count: 3,
            since: t(0),
        })
    );
    // Display only: routing availability is untouched.
    assert_eq!(b.status(&a, true, t(40)), AccountStatus::Ready);
    // Failures age out of the 15-minute window.
    assert_eq!(b.erroring(&a, t(15 * 60 + 5)), None);

    // A served request clears the count.
    let c = id("c");
    for s in [0, 10, 20] {
        b.record_error(&c, failure(FailReason::AccessDenied, t(s)));
    }
    assert!(b.erroring(&c, t(30)).is_some());
    b.record_success(&c, usage(1, 1), t(40));
    assert_eq!(b.erroring(&c, t(41)), None);
    b.record_error(&c, failure(FailReason::AccessDenied, t(50)));
    b.record_error(&c, failure(FailReason::AccessDenied, t(60)));
    assert_eq!(b.erroring(&c, t(61)), None, "a success restarts the count");

    // A successful poll clears too.
    let p = id("p");
    for s in [0, 10, 20] {
        b.record_error(&p, failure(server(), t(s)));
    }
    b.replace_poll_snapshot(&p, snap(vec![], t(30)), t(25));
    assert_eq!(b.erroring(&p, t(31)), None);

    // A successful refresh clears refresh failures, not a persistent 401.
    let r = id("r");
    for s in [0, 10, 20] {
        b.record_error(&r, failure(server(), t(s)));
    }
    b.mark_verified(&r, t(30));
    assert_eq!(b.erroring(&r, t(31)), None);
    for s in [40, 50, 60] {
        b.record_error(&r, failure(FailReason::Status(401), t(s)));
        b.mark_verified(&r, t(s + 1));
    }
    assert_eq!(b.erroring(&r, t(70)).map(|e| e.count), Some(3));

    // A failed expiry probe is erroring on its own until a success, for at most its TTL (the
    // probe's 15-minute retry renews it).
    let q = id("q");
    b.record_probe_failure(&q, "refresh:network:connect", t(0));
    assert_eq!(
        b.erroring(&q, t(16 * 60)),
        Some(Erroring {
            class: "refresh:network:connect".into(),
            count: 1,
            since: t(0),
        })
    );
    assert_eq!(b.erroring(&q, t(17 * 60)), None, "lapses without a retry");
    b.record_probe_failure(&q, "refresh:network:connect", t(17 * 60));
    assert!(b.erroring(&q, t(17 * 60 + 1)).is_some());
    b.mark_verified(&q, t(18 * 60));
    assert_eq!(b.erroring(&q, t(18 * 60 + 1)), None);
}
