//! `pooltime status`: the one-screen summary, rendered from `GET /api/v1/status`.
//!
//! Format (stable; scripts should use `--json`):
//!
//! ```text
//! pooltime 1.3.1 on 127.0.0.1:3460
//! pool 2/3 ready · 7d 77% used
//! this week: 1.4 subs used · on pace for ~2.6 of 3
//! ⚠ runway: At the current rate the pool runs out ~14h before the next reset (Thu 09:00)
//! top projects 24h: acme 62% · widgets 30% · unknown 8%
//!
//! ACCOUNT  PROVIDER   STATUS       7D USED  PRIO  WEIGHT  EMAIL
//! gamma    anthropic  needs_login  -        0     1       -
//! alpha    anthropic  ready        60%      0     1       a@example.com
//!
//! alerts: gamma needs login (never_logged_in)
//! ```
//!
//! Accounts are listed most severe state first (spec §4: `needs_login > billing >
//! access_denied > erroring > rate_limited > expiring > ready`), then by id.
//!
//! "7d N% used" for the pool is the summed 7-day usage (`pool.seven_day_used_total`, one
//! account = 1.0) over the number of accounts, so a pool of fresh accounts reads 0% and an
//! account without a known 7-day window (including one that needs login) counts as fully used
//! (it has no headroom).
//! Per account it is that account's own `seven_day_used`, `-` when unknown. Percentages are
//! rounded to whole numbers. (The JSON keeps the `seven_day_left*` fields too.)
//!
//! The "this week" and "runway" lines come from `GET /api/v1/analytics/burn` (needs the
//! management key); they are left out when that call fails. The runway line
//! ([`crate::analytics::runway`]) says whether the pool's 7-day quota lasts at the current burn
//! rate, or when it runs out against the next reset (local time; `⚠` when at least
//! `alerts.runway_warn_hours` short); it is left out while there is too little usage history.
//! The "top projects" line (`GET /api/v1/analytics/projects?range=24h`, share of tokens over the
//! last 24 hours, at most three projects) is left out when that call fails or nothing was
//! requested in that time.

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use super::client::Client;
use crate::analytics::{local_offset, runway};

/// `0.6` -> `60%`; `None` -> `-`.
pub fn pct(fraction: Option<f64>) -> String {
    fraction.map_or("-".into(), |f| format!("{:.0}%", f * 100.0))
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("-")
}

fn num(v: &Value) -> String {
    match v.as_f64() {
        Some(f) if f.fract() == 0.0 => format!("{f:.0}"),
        Some(f) => format!("{f}"),
        None => "-".into(),
    }
}

/// Left-aligned columns separated by two spaces.
pub fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for r in rows {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{cell:<w$}", w = widths[i]))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// Sort rank of a status word, most severe first (spec §4; unknown words last).
fn severity(word: &str) -> u8 {
    match word {
        "needs_login" => 0,
        "billing" => 1,
        "access_denied" => 2,
        "erroring" => 3,
        "rate_limited" => 4,
        "expiring" => 5,
        "ready" => 6,
        _ => 7,
    }
}

/// The accounts, most severe state first, then by id.
fn by_severity(status: &Value) -> Vec<Value> {
    let mut accounts = status["accounts"].as_array().cloned().unwrap_or_default();
    accounts.sort_by(|a, b| {
        severity(str_of(a, "status"))
            .cmp(&severity(str_of(b, "status")))
            .then_with(|| str_of(a, "id").cmp(str_of(b, "id")))
    });
    accounts
}

/// The account table (`accounts list` and `status`), most severe state first.
pub fn accounts_table(status: &Value) -> String {
    let accounts = by_severity(status);
    if accounts.is_empty() {
        return "no accounts (add one with `pooltime accounts login <name>`)\n".into();
    }
    let mut rows = vec![[
        "ACCOUNT", "PROVIDER", "STATUS", "7D USED", "PRIO", "WEIGHT", "EMAIL",
    ]
    .map(String::from)
    .to_vec()];
    for a in &accounts {
        let weight = if a["weight_override"].is_object() {
            format!(
                "{} (until {})",
                num(&a["effective_weight"]),
                str_of(&a["weight_override"], "until")
            )
        } else {
            num(&a["weight"])
        };
        rows.push(vec![
            str_of(a, "id").into(),
            str_of(a, "provider").into(),
            str_of(a, "status").into(),
            pct(a["seven_day_used"].as_f64()),
            num(&a["priority"]),
            weight,
            a["identity"]["email"].as_str().unwrap_or("-").into(),
        ]);
    }
    table(&rows)
}

/// Lines for every account that is not ready: `<id> <status words> (<reason>)`.
fn alerts(status: &Value) -> Vec<String> {
    let rfc = |v: &Value| {
        v.as_str()
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
    };
    let now = rfc(&status["now"]).unwrap_or_else(OffsetDateTime::now_utc);
    let mut out = Vec::new();
    for a in &by_severity(status) {
        let words = match str_of(a, "status") {
            "ready" => continue,
            "needs_login" => "needs login".to_string(),
            "rate_limited" => format!("is rate-limited until {}", str_of(a, "until")),
            "billing" => "has a billing problem".to_string(),
            "access_denied" => format!("is access-denied until {}", str_of(a, "until")),
            // Passed but no refresh refused it yet: only a probe can confirm needs_login.
            "expiring" if rfc(&a["until"]).is_some_and(|u| u <= now) => {
                format!("login expired {} (unconfirmed)", str_of(a, "until"))
            }
            "expiring" => format!("login expires {}", str_of(a, "until")),
            "erroring" => format!("is erroring ({} failures)", num(&a["count"])),
            other => format!("is {other}"),
        };
        let reason = a["reason"]
            .as_str()
            .filter(|r| *r != "refresh_token_expiring")
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        out.push(format!("{} {words}{reason}", str_of(a, "id")));
    }
    out
}

/// The pool's "7d N% used" fraction: the summed 7-day usage over the number of accounts (see
/// the module docs); `None` with no accounts. The status line shows the same figure.
pub fn pool_seven_day_used(status: &Value) -> Option<f64> {
    let pool = &status["pool"];
    let n = pool["accounts"].as_u64().unwrap_or(0);
    pool["seven_day_used_total"]
        .as_f64()
        .filter(|_| n > 0)
        .map(|t| t / n as f64)
}

/// `this week: 4.9 subs used · on pace for ~6.2 of 7` from `GET /api/v1/analytics/burn`
/// (without the pace in the week's first hour); `None` when the document lacks the numbers.
pub fn burn_line(burn: &Value) -> Option<String> {
    let f = &burn["forecast"];
    let used = f["subs_used_so_far"].as_f64()?;
    let n = f["accounts"].as_u64()?;
    Some(match f["projected"].as_f64() {
        Some(p) => format!("this week: {used:.1} subs used · on pace for ~{p:.1} of {n}"),
        None => format!("this week: {used:.1} subs used of {n}"),
    })
}

/// The runway line from the burn document's `runway` at `now` (times in `offset`): `⚠ runway: At
/// the current rate the pool runs out ~14h before the next reset (Thu 09:00)` (the `⚠` only when
/// at risk), or `runway: lasts at the current rate (0.05 subs/h)`; ` (low confidence: 5h of
/// history)` while the rate covers less than a day. `None` without a forecast.
pub fn runway_line(burn: &Value, now: OffsetDateTime, offset: UtcOffset) -> Option<String> {
    let r = &burn["runway"];
    let t = |k: &str| {
        r[k].as_str()
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
    };
    let state = r["state"].as_str()?;
    let low = match (
        r["low_confidence"].as_bool(),
        r["rate_window_hours"].as_f64(),
    ) {
        (Some(true), Some(h)) => format!(" (low confidence: {h:.0}h of history)"),
        (Some(true), None) => " (low confidence)".to_string(),
        _ => String::new(),
    };
    if state == "lasts" {
        let rate = r["rate_per_hour"].as_f64()?;
        return Some(format!(
            "runway: lasts at the current rate ({rate:.2} subs/h){low}"
        ));
    }
    let text = runway::describe(
        state,
        t("exhausts_at"),
        t("next_reset"),
        r["short_by_secs"].as_i64(),
        now,
        offset,
    )?;
    let warn = if r["at_risk"].as_bool() == Some(true) {
        "⚠ "
    } else {
        ""
    };
    Some(format!("{warn}runway: {text}{low}"))
}

/// Projects named in [`projects_line`].
const TOP_PROJECTS: usize = 3;

/// `top projects 24h: acme 62% · widgets 30% · unknown 8%` from
/// `GET /api/v1/analytics/projects?range=24h` (share of tokens, the report's order: most usage
/// first); `None` without any project.
pub fn projects_line(report: &Value) -> Option<String> {
    let projects = report["projects"].as_array()?;
    let parts: Vec<String> = projects
        .iter()
        .take(TOP_PROJECTS)
        .filter_map(|p| {
            let name = p["project"].as_str()?;
            let share = p["share"].as_f64()?;
            Some(format!("{name} {}", pct(Some(share))))
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    let more = projects.len().saturating_sub(TOP_PROJECTS);
    let tail = if more > 0 {
        format!(" (+{more} more)")
    } else {
        String::new()
    };
    Some(format!(
        "top projects {}: {}{tail}",
        report["range"].as_str().unwrap_or("24h"),
        parts.join(" · ")
    ))
}

/// The whole summary.
pub fn render(status: &Value, addr: &str) -> String {
    render_with_burn(status, None, addr, UtcOffset::UTC)
}

/// [`render`] plus the weekly-burn and runway lines when `burn` (the burn API answer) is
/// available; runway times in `offset`.
pub fn render_with_burn(
    status: &Value,
    burn: Option<&Value>,
    addr: &str,
    offset: UtcOffset,
) -> String {
    render_full(status, burn, None, addr, offset)
}

/// [`render_with_burn`] plus the top-projects line when `projects` (the usage-by-project answer)
/// has any.
pub fn render_full(
    status: &Value,
    burn: Option<&Value>,
    projects: Option<&Value>,
    addr: &str,
    offset: UtcOffset,
) -> String {
    let pool = &status["pool"];
    let n = pool["accounts"].as_u64().unwrap_or(0);
    let used = pool_seven_day_used(status);
    let mut out = format!(
        "pooltime {} on {addr}\npool {}/{n} ready · 7d {} used\n\n",
        str_of(status, "version"),
        pool["available"].as_u64().unwrap_or(0),
        pct(used)
    );
    // Right under the pool line (before its blank separator).
    let now = status["now"]
        .as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
    let lines = [
        burn.and_then(burn_line),
        burn.and_then(|b| runway_line(b, now, offset)),
    ];
    for line in lines.into_iter().flatten() {
        out.insert_str(out.len() - 1, &format!("{line}\n"));
    }
    if let Some(line) = projects.and_then(projects_line) {
        out.insert_str(out.len() - 1, &format!("{line}\n"));
    }
    out.push_str(&accounts_table(status));
    let alerts = alerts(status);
    if !alerts.is_empty() {
        out.push_str(&format!("\nalerts: {}\n", alerts.join("; ")));
    }
    out
}

pub async fn run(client: &Client, json: bool) -> anyhow::Result<i32> {
    let status = client.get("/api/v1/status").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        // Optional: an older daemon or a missing key just leaves the line out.
        let burn = client
            .get_authed_opt("/api/v1/analytics/burn?weeks=1")
            .await
            .ok()
            .flatten();
        let projects = client
            .get_authed_opt("/api/v1/analytics/projects?range=24h")
            .await
            .ok()
            .flatten();
        print!(
            "{}",
            render_full(
                &status,
                burn.as_ref(),
                projects.as_ref(),
                &client.addr.to_string(),
                local_offset(OffsetDateTime::now_utc()),
            )
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_pool_and_rows() {
        let s = json!({
            "version": "0.1.0",
            "pool": {"accounts": 2, "available": 1, "seven_day_used_total": 0.4},
            "accounts": [
                {"id": "alpha", "provider": "anthropic", "status": "ready", "seven_day_used": 0.4,
                 "priority": 0, "weight": 1.0, "identity": {"email": "a@x"}},
                {"id": "beta", "provider": "anthropic", "status": "needs_login",
                 "reason": "never_logged_in", "seven_day_used": null, "priority": 2,
                 "weight": 2.5, "identity": {"email": null}}
            ]
        });
        let out = render(&s, "127.0.0.1:3460");
        assert!(out.contains("pool 1/2 ready · 7d 20% used"), "{out}");
        assert!(
            out.contains("alpha    anthropic  ready        40%"),
            "{out}"
        );
        assert!(
            out.contains("beta     anthropic  needs_login  -        2     2.5"),
            "{out}"
        );
        assert!(
            out.contains("alerts: beta needs login (never_logged_in)"),
            "{out}"
        );
        assert!(
            out.find("beta ").unwrap() < out.find("alpha ").unwrap(),
            "{out}"
        );
    }

    #[test]
    fn health_states_sort_and_alert() {
        let row =
            |id: &str, status: &str| json!({"id": id, "provider": "anthropic", "status": status});
        let mut flaky = row("flaky", "erroring");
        flaky["count"] = json!(3);
        flaky["reason"] = json!("refresh:server:503");
        let mut soon = row("soon", "expiring");
        soon["until"] = json!("2026-11-09T12:00:00Z");
        soon["reason"] = json!("refresh_token_expiring");
        let mut lapsed = row("lapsed", "expiring");
        lapsed["until"] = json!("2026-11-07T11:00:00Z");
        let s = json!({"version": "0.1.0", "now": "2026-11-07T12:00:00Z",
            "pool": {"accounts": 5, "available": 4, "seven_day_used_total": 0.0},
            "accounts": [row("a-ready", "ready"), soon, row("limited", "rate_limited"), flaky,
                lapsed]});
        let out = render(&s, "x");
        let at = |id: &str| {
            out.find(&format!("\n{id} "))
                .unwrap_or_else(|| panic!("{id}: {out}"))
        };
        assert!(
            at("flaky") < at("limited") && at("limited") < at("soon") && at("soon") < at("a-ready"),
            "{out}"
        );
        assert!(
            out.contains(
                "alerts: flaky is erroring (3 failures) (refresh:server:503); limited is \
                 rate-limited until -; lapsed login expired 2026-11-07T11:00:00Z (unconfirmed); \
                 soon login expires 2026-11-09T12:00:00Z"
            ),
            "{out}"
        );
    }

    #[test]
    fn status_shows_weekly_burn_line() {
        let s = json!({"version": "0.1.0", "pool": {"accounts": 3, "available": 3,
            "seven_day_used_total": 1.8}, "accounts": []});
        let burn = json!({"weeks": [], "idle_accounts": [], "forecast": {"week": "2026-W45",
            "subs_used_so_far": 1.44, "elapsed_fraction": 0.55, "projected": 2.618, "accounts": 3}});
        let out = render_with_burn(&s, Some(&burn), "x", UtcOffset::UTC);
        assert!(
            out.contains("pool 3/3 ready · 7d 60% used\nthis week: 1.4 subs used · on pace for ~2.6 of 3\n\n"),
            "{out}"
        );
        let mut early = burn.clone();
        early["forecast"]["projected"] = Value::Null;
        let out = render_with_burn(&s, Some(&early), "x", UtcOffset::UTC);
        assert!(out.contains("this week: 1.4 subs used of 3\n"), "{out}");
        // No burn document (endpoint failed): the line is simply absent.
        assert!(!render_with_burn(&s, None, "x", UtcOffset::UTC).contains("this week"));
        assert!(
            !render_with_burn(&s, Some(&json!({"error": {}})), "x", UtcOffset::UTC)
                .contains("this week")
        );
    }

    #[test]
    fn status_shows_the_runway() {
        let s = json!({"version": "0.1.0", "now": "2026-11-11T09:00:00Z", "pool": {"accounts": 2,
            "available": 2, "seven_day_used_total": 1.2}, "accounts": []});
        let now = OffsetDateTime::parse("2026-11-11T09:00:00Z", &Rfc3339).unwrap();
        let mut burn = json!({"weeks": [], "idle_accounts": [], "forecast": {"week": "2026-W46",
            "subs_used_so_far": 1.2, "elapsed_fraction": 0.4, "projected": 3.0, "accounts": 2},
            "runway": {"state": "short", "rate_per_hour": 0.02, "rate_window_hours": 24.0,
            "headroom": 0.8, "exhausts_at": "2026-11-11T19:00:00Z",
            "next_reset": "2026-11-12T09:00:00Z", "short_by_secs": 50400, "at_risk": true,
            "warn_hours": 6}});
        let out = render_with_burn(&s, Some(&burn), "x", UtcOffset::UTC);
        assert!(
            out.contains(
                "on pace for ~3.0 of 2\n⚠ runway: At the current rate the pool runs out ~14h \
                 before the next reset (Thu 09:00)\n\nno accounts"
            ),
            "{out}"
        );
        // Short but under the threshold: no warning sign.
        burn["runway"]["at_risk"] = json!(false);
        burn["runway"]["short_by_secs"] = json!(3600);
        assert!(runway_line(&burn, now, UtcOffset::UTC)
            .unwrap()
            .starts_with("runway: At the current rate the pool runs out ~1h before"));
        let out_now = json!({"runway": {"state": "out", "exhausts_at": "2026-11-11T19:00:00Z",
            "next_reset": "2026-11-12T09:00:00Z", "short_by_secs": 50400, "at_risk": true}});
        assert_eq!(
            runway_line(&out_now, now, UtcOffset::from_hms(1, 0, 0).unwrap()).unwrap(),
            "⚠ runway: The pool is out of 7-day quota until the next reset (Thu 10:00)"
        );
        let lasts = json!({"runway": {"state": "lasts", "rate_per_hour": 0.0123}});
        assert_eq!(
            runway_line(&lasts, now, UtcOffset::UTC).unwrap(),
            "runway: lasts at the current rate (0.01 subs/h)"
        );
        // Under a day of history: marked; a reset more than 6 days out carries its date.
        let young = json!({"runway": {"state": "short", "exhausts_at": "2026-11-17T19:00:00Z",
            "next_reset": "2026-11-18T09:00:00Z", "short_by_secs": 50400, "at_risk": false,
            "low_confidence": true, "rate_window_hours": 5.0}});
        assert_eq!(
            runway_line(&young, now, UtcOffset::UTC).unwrap(),
            "runway: At the current rate the pool runs out ~14h before the next reset \
             (Wed 18 Nov 09:00) (low confidence: 5h of history)"
        );
        // No forecast (too little history, an older daemon): no line.
        for b in [
            json!({"runway": {"state": "unknown"}}),
            json!({"runway": null}),
            json!({}),
        ] {
            assert_eq!(runway_line(&b, now, UtcOffset::UTC), None, "{b}");
        }
    }

    #[test]
    fn status_shows_top_projects_line() {
        let s = json!({"version": "x", "pool": {"accounts": 1, "available": 1}, "accounts": []});
        let p = |n: usize| {
            let projects: Vec<Value> = ["acme", "widgets", "unknown", "misc"][..n]
                .iter()
                .zip([0.62, 0.3, 0.06, 0.02])
                .map(|(name, share)| json!({"project": name, "share": share}))
                .collect();
            json!({"range": "24h", "projects": projects})
        };
        let out = render_full(&s, None, Some(&p(4)), "x", UtcOffset::UTC);
        assert!(
            out.contains("top projects 24h: acme 62% · widgets 30% · unknown 6% (+1 more)\n\n"),
            "{out}"
        );
        assert_eq!(
            projects_line(&p(1)).as_deref(),
            Some("top projects 24h: acme 62%")
        );
        assert_eq!(projects_line(&p(0)), None);
        assert!(!render_full(&s, None, Some(&p(0)), "x", UtcOffset::UTC).contains("top projects"));
    }

    #[test]
    fn empty_pool() {
        let s = json!({"version": "0.1.0", "pool": {"accounts": 0, "available": 0,
            "seven_day_used_total": 0.0}, "accounts": []});
        let out = render(&s, "x");
        assert!(out.contains("7d - used"), "{out}");
        assert!(out.contains("no accounts"), "{out}");
    }
}
