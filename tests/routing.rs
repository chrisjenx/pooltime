//! Weighted selection (INV §2), ported from bro `pool/src/accounts/manager.test.ts`.
//!
//! Ported from `describe("weighted strategy")`:
//! - urgency dominates ... (soonest 7d reset wins)            -> `earliest_7d_reset_gets_highest_share`
//! - load flips the winner                                     -> `load_flips_the_winner`
//! - expiry order beats weekly headroom                        -> `expiry_order_beats_weekly_headroom`
//! - manual weight biases selection                            -> `manual_weight_biases_selection`
//! - 5h headroom collapse drops below later-expiring           -> `five_hour_headroom_collapse_drops_below_later_expiring`
//! - below routingMinHeadroom falls to a fallback pool         -> `no_viable_falls_back_best_effort`
//! - priority tiers still gate hard                            -> `only_lowest_priority_tier_used`
//! - weekly-spent with unknown reset ranks last                -> `weekly_spent_unknown_reset_ranks_last`
//!
//! Ported from `describe("routing tuning")`:
//! - expiry ordering: unknown first, known next, spent last    -> `unknown_data_probed_first`
//!
//! Ported from `describe("expiry-share regression")`:
//! - eleven live sessions allocate 5/3/2/1                     -> `eleven_live_sessions_allocate_5_3_2_1`
//! - healthy 5h quota is neutral; only scarcity tapers         -> `low_5h_headroom_tapers_score`
//! - Fable weekly {0.92, 0.999, 1} only excludes true exhaustion -> `fable_weekly_only_excludes_true_exhaustion`
//! - repeated tie preview / round robin (a, b tied)            -> `ties_round_robin`
//!
//! Ported from `describe("expiry shares edge cases")`:
//! - later ranks halve; equal resets share rank; weights multiply -> `later_ranks_halve_equal_resets_share_rank`
//! - weekly-only Codex near exhaustion neutral; allowed kept   -> `weekly_only_allowed_window_stays_neutral`
//! - expired / other-family windows neutral, own family binds  -> `expired_and_model_scoped_windows_are_neutral`
//!   (+ `family_scoped_5h_gate_binds_only_the_requested_family`)
//!
//! New in pooltime:
//! - `timed_weight_override_applies_until_expiry`, `usable_*`, `choose_*`, `tuning_*`.
//!
//! Not ported (not part of `choose`): provider-level ranking, session pins, in-flight caps,
//! the routing preview/`routingSnapshot` shape, and tuning.json persistence.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use pooltime::accounts::{
    AccountId, AccountMeta, AccountStatus, RateLimitSnapshot, TimedOverride, Window, WindowStatus,
};
use pooltime::config::RoutingCfg;
use pooltime::providers::ProviderId;
use pooltime::routing::score::{expiry_rank, expiry_shares, gate_headroom};
use pooltime::routing::{choose, score, usable, Candidate, ExpiryRank, Tuning};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const NOW: OffsetDateTime = datetime!(2026-11-07 12:00 UTC);

fn meta(id: &str) -> AccountMeta {
    AccountMeta::new(id.parse().unwrap(), ProviderId::Anthropic)
}

fn win(key: &str, utilization: f64, reset: Option<Duration>) -> Window {
    Window {
        key: key.to_string(),
        model: None,
        status: WindowStatus::Allowed,
        utilization,
        reset: reset.map(|d| NOW + d),
    }
}

/// A weekly window that is fully used with no explicit `allowed` (i.e. spent).
fn spent_7d(reset: Option<Duration>) -> Window {
    Window {
        status: WindowStatus::AllowedWarning,
        ..win("7d", 1.0, reset)
    }
}

fn model_win(key: &str, model: &str, utilization: f64, reset: Duration) -> Window {
    Window {
        model: Some(model.to_string()),
        ..win(key, utilization, Some(reset))
    }
}

fn snap(windows: Vec<Window>) -> RateLimitSnapshot {
    RateLimitSnapshot {
        unified_status: None,
        windows,
        updated_at: NOW,
    }
}

/// Healthy 5h (20% used) + 7d resetting in `d7`.
fn snap_7d(util5: f64, d7: Duration) -> RateLimitSnapshot {
    snap(vec![
        win("5h", util5, Some(Duration::hours(1))),
        win("7d", 0.2, Some(d7)),
    ])
}

fn cand<'a>(m: &'a AccountMeta, s: Option<&'a RateLimitSnapshot>, sessions: u32) -> Candidate<'a> {
    Candidate {
        meta: m,
        snapshot: s,
        active_sessions: sessions,
    }
}

fn pick<'a>(cands: &[Candidate<'a>]) -> Option<&'a AccountId> {
    choose(cands, &Tuning::default(), &AtomicUsize::new(0), NOW, None)
}

fn name(id: Option<&AccountId>) -> &str {
    id.expect("a winner").as_str()
}

#[test]
fn earliest_7d_reset_gets_highest_share() {
    let (soon, later) = (meta("soon"), meta("later"));
    let (s1, s2) = (
        snap_7d(0.2, Duration::days(1)),
        snap_7d(0.2, Duration::days(6)),
    );
    // Order must not matter.
    let a = [cand(&later, Some(&s2), 0), cand(&soon, Some(&s1), 0)];
    assert_eq!(name(pick(&a)), "soon");
    let b = [cand(&soon, Some(&s1), 0), cand(&later, Some(&s2), 0)];
    assert_eq!(name(pick(&b)), "soon");
}

#[test]
fn load_flips_the_winner() {
    let (busy, idle) = (meta("soon-busy"), meta("later-idle"));
    let (s1, s2) = (
        snap_7d(0.2, Duration::days(1)),
        snap_7d(0.2, Duration::days(6)),
    );
    // soon-busy: 5 / 3 sessions = 1.67 vs later-idle: 3 -> later-idle wins.
    let c = [cand(&busy, Some(&s1), 2), cand(&idle, Some(&s2), 0)];
    assert_eq!(name(pick(&c)), "later-idle");
    let c = [cand(&busy, Some(&s1), 0), cand(&idle, Some(&s2), 0)];
    assert_eq!(name(pick(&c)), "soon-busy");
}

#[test]
fn expiry_order_beats_weekly_headroom() {
    let (expiring, fresh) = (meta("expiring-soon"), meta("fresh"));
    let s1 = snap(vec![
        win("5h", 0.0, Some(Duration::hours(1))),
        win("7d", 0.97, Some(Duration::hours(3))),
    ]);
    let s2 = snap(vec![
        win("5h", 0.0, Some(Duration::hours(1))),
        win("7d", 0.49, Some(Duration::hours(65))),
    ]);
    let c = [cand(&fresh, Some(&s2), 0), cand(&expiring, Some(&s1), 0)];
    assert_eq!(name(pick(&c)), "expiring-soon");
}

#[test]
fn manual_weight_biases_selection() {
    let plain = meta("plain");
    let mut boosted = meta("boosted");
    boosted.weight = 2.0;
    let s = snap_7d(0.2, Duration::days(3));
    // Identical snapshots tie on rank; weight decides regardless of order.
    let c = [cand(&plain, Some(&s), 0), cand(&boosted, Some(&s), 0)];
    assert_eq!(name(pick(&c)), "boosted");
}

#[test]
fn five_hour_headroom_collapse_drops_below_later_expiring() {
    let (full, fresh) = (meta("soon-full"), meta("later-fresh"));
    let s1 = snap_7d(0.9, Duration::days(1));
    let s2 = snap_7d(0.1, Duration::days(6));
    // soon-full: 0.1 headroom is still viable, score 5 x (0.1/0.2) = 2.5; later-fresh: 3 x 1.
    let c = [cand(&full, Some(&s1), 0), cand(&fresh, Some(&s2), 0)];
    assert_eq!(name(pick(&c)), "later-fresh");
}

#[test]
fn no_viable_falls_back_best_effort() {
    let (a, b) = (meta("nearly-out"), meta("also-out"));
    let s1 = snap_7d(0.95, Duration::days(1));
    let s2 = snap_7d(0.99, Duration::days(2));
    // Neither clears min_headroom (0.1); best-effort still serves the higher score.
    let c = [cand(&b, Some(&s2), 0), cand(&a, Some(&s1), 0)];
    assert_eq!(name(pick(&c)), "nearly-out");
}

#[test]
fn viable_account_beats_higher_scoring_unviable_one() {
    let (soon, ok) = (meta("soon-unviable"), meta("later-viable"));
    let s1 = snap_7d(0.95, Duration::days(1));
    let s2 = snap_7d(0.5, Duration::days(6));
    let mut heavy = soon.clone();
    heavy.weight = 10.0;
    // Unviable score: 10 x 5 x 0.25 = 12.5 > 3, but the gate removes it.
    let c = [cand(&heavy, Some(&s1), 0), cand(&ok, Some(&s2), 0)];
    assert_eq!(name(pick(&c)), "later-viable");
}

#[test]
fn only_lowest_priority_tier_used() {
    let mut t1 = meta("tier1");
    let mut t2 = meta("tier2-heavy");
    t1.priority = 1;
    t2.priority = 2;
    t2.weight = 10.0;
    let c = [cand(&t2, None, 0), cand(&t1, None, 5)];
    assert_eq!(name(pick(&c)), "tier1");
}

#[test]
fn weekly_spent_unknown_reset_ranks_last() {
    let (spent, known) = (meta("spent"), meta("known-later"));
    let s1 = snap(vec![
        win("5h", 0.0, Some(Duration::hours(1))),
        spent_7d(None),
    ]);
    let s2 = snap_7d(0.0, Duration::days(5));
    assert_eq!(expiry_rank(Some(&s1), None, NOW), ExpiryRank::Spent);
    let c = [cand(&spent, Some(&s1), 0), cand(&known, Some(&s2), 0)];
    assert_eq!(name(pick(&c)), "known-later");
}

#[test]
fn unknown_data_probed_first() {
    let (unprobed, known, spent) = (meta("unprobed"), meta("known"), meta("spent"));
    let s_known = snap_7d(0.0, Duration::days(5));
    let s_spent = snap(vec![
        win("5h", 0.0, Some(Duration::hours(1))),
        spent_7d(None),
    ]);
    let all = [
        cand(&spent, Some(&s_spent), 0),
        cand(&known, Some(&s_known), 0),
        cand(&unprobed, None, 0),
    ];
    assert_eq!(name(pick(&all)), "unprobed");
    // Failover: the caller drops tried accounts.
    assert_eq!(name(pick(&all[..2])), "known");
    assert_eq!(name(pick(&all[..1])), "spent");
}

#[test]
fn eleven_live_sessions_allocate_5_3_2_1() {
    let metas: Vec<AccountMeta> = ["a", "b", "c", "d"].map(meta).into();
    let snaps: Vec<RateLimitSnapshot> = (1..=4)
        .map(|h| snap(vec![win("7d", 0.1, Some(Duration::hours(h)))]))
        .collect();
    let mut sessions = [0u32; 4];
    let cursor = AtomicUsize::new(0);
    for _ in 0..11 {
        let cands: Vec<Candidate> = (0..4)
            .map(|i| cand(&metas[i], Some(&snaps[i]), sessions[i]))
            .collect();
        let w = choose(&cands, &Tuning::default(), &cursor, NOW, None).unwrap();
        let i = metas.iter().position(|m| &m.id == w).unwrap();
        sessions[i] += 1;
    }
    assert_eq!(sessions, [5, 3, 2, 1]);
}

#[test]
fn low_5h_headroom_tapers_score() {
    let t = Tuning::default();
    for (headroom, factor) in [
        (1.0, 1.0),
        (0.5, 1.0),
        (0.2, 1.0),
        (0.15, 0.75),
        (0.05, 0.25),
    ] {
        let s = snap(vec![win("5h", 1.0 - headroom, Some(Duration::hours(1)))]);
        let h = gate_headroom(Some(&s), None, NOW);
        assert!((h - headroom).abs() < 1e-12);
        // Single account -> expiry rank 0 -> share 5.
        let got = score(1.0, 5.0, h, 0, &t);
        assert!((got - 5.0 * factor).abs() < 1e-9, "{headroom}: {got}");
    }
}

#[test]
fn five_hour_exp_shapes_the_taper() {
    let t = Tuning::new(2.0, 0.1, 0.2).unwrap();
    assert!((score(1.0, 1.0, 0.1, 0, &t) - 0.25).abs() < 1e-12);
    let t0 = Tuning::new(0.0, 0.1, 0.2).unwrap();
    assert_eq!(score(1.0, 1.0, 0.1, 0, &t0), 1.0);
    // A wider taper start penalizes mid headroom.
    let wide = Tuning::new(1.0, 0.1, 1.0).unwrap();
    assert!((score(1.0, 1.0, 0.5, 0, &wide) - 0.5).abs() < 1e-12);
}

#[test]
fn later_ranks_halve_equal_resets_share_rank() {
    let ids = ["a", "b", "c", "d", "e", "f", "g"];
    let metas: Vec<AccountMeta> = ids.map(meta).into();
    let snaps: Vec<RateLimitSnapshot> = (0..7usize)
        .map(|i| {
            snap(vec![win(
                "7d",
                0.99,
                Some(Duration::hours(i.min(5) as i64 + 1)),
            )])
        })
        .collect();
    let ranks: Vec<ExpiryRank> = snaps
        .iter()
        .map(|s| expiry_rank(Some(s), None, NOW))
        .collect();
    assert_eq!(
        expiry_shares(&ranks),
        vec![5.0, 3.0, 2.0, 1.0, 0.5, 0.25, 0.25]
    );
    // Weight multiplies: failover drops a, b, c; ranks are recomputed over d..g.
    let mut metas = metas;
    metas[6].weight = 4.0;
    let rest: Vec<Candidate> = (3..7)
        .map(|i| cand(&metas[i], Some(&snaps[i]), 0))
        .collect();
    // d:5, e:3, f:2, g:2x4=8 -> g.
    assert_eq!(name(pick(&rest)), "g");
}

#[test]
fn weekly_only_allowed_window_stays_neutral() {
    let c = meta("codex");
    for util in [0.92, 0.999, 1.0] {
        let s = snap(vec![win("7d", util, Some(Duration::hours(1)))]);
        assert_eq!(gate_headroom(Some(&s), None, NOW), 1.0);
        assert_eq!(name(pick(&[cand(&c, Some(&s), 0)])), "codex");
    }
    // `allowed` at 100% 5h is unenforced: neutral. An enforced one is not.
    let s = snap(vec![win("5h", 1.0, Some(Duration::hours(1)))]);
    assert_eq!(gate_headroom(Some(&s), None, NOW), 1.0);
    let mut w = win("5h", 1.0, Some(Duration::hours(1)));
    w.status = WindowStatus::AllowedWarning;
    assert_eq!(gate_headroom(Some(&snap(vec![w])), None, NOW), 0.0);
}

#[test]
fn expired_and_model_scoped_windows_are_neutral() {
    let s = snap(vec![
        // 5h window whose reset already passed: refreshed.
        win("5h", 0.99, Some(Duration::seconds(-1))),
        model_win("5h-fable", "fable", 0.95, Duration::hours(1)),
        model_win("7d-fable", "fable", 0.999, Duration::hours(2)),
    ]);
    // Windows of OTHER families (or of no family) are neutral...
    for fam in [None, Some("sonnet")] {
        assert_eq!(gate_headroom(Some(&s), fam, NOW), 1.0);
        assert_eq!(expiry_rank(Some(&s), fam, NOW), ExpiryRank::Unknown);
    }
    // ...while the requested family's own windows bind (0.95 used -> 0.05 left; the
    // 7d-fable reset is the soonest future expiry).
    let h = gate_headroom(Some(&s), Some("fable"), NOW);
    assert!((h - 0.05).abs() < 1e-12, "{h}");
    assert_eq!(
        expiry_rank(Some(&s), Some("fable"), NOW),
        ExpiryRank::At(NOW + Duration::hours(2))
    );
    // A 7d whose reset has passed is treated as unknown/refreshing.
    let s = snap(vec![win("7d", 1.0, Some(Duration::seconds(-5)))]);
    assert_eq!(expiry_rank(Some(&s), None, NOW), ExpiryRank::Unknown);
}

/// bro: `expiry-share regression` / "Fable weekly {0.92, 0.999, 1} only excludes true
/// exhaustion".
#[test]
fn fable_weekly_only_excludes_true_exhaustion() {
    let (soon, later) = (meta("soon"), meta("later"));
    for used in [0.92, 0.999, 1.0] {
        let mut w = model_win("7d-fable", "fable", used, Duration::hours(3));
        w.status = WindowStatus::AllowedWarning;
        let s1 = snap(vec![win("5h", 0.0, Some(Duration::hours(1))), w]);
        let s2 = snap(vec![
            win("5h", 0.0, Some(Duration::hours(1))),
            model_win("7d-fable", "fable", 0.2, Duration::hours(70)),
        ]);
        let c = [cand(&soon, Some(&s1), 0), cand(&later, Some(&s2), 0)];
        let got = choose(
            &c,
            &Tuning::default(),
            &AtomicUsize::new(0),
            NOW,
            Some("fable"),
        );
        // Fully used -> ranked last; anything less still drains the soonest reset first.
        let want = if used >= 1.0 { "later" } else { "soon" };
        assert_eq!(name(got), want, "used {used}");
    }
}

#[test]
fn family_scoped_5h_gate_binds_only_the_requested_family() {
    let (a, b) = (meta("a"), meta("b"));
    let s1 = snap(vec![
        win("5h", 0.0, Some(Duration::hours(1))),
        win("7d", 0.2, Some(Duration::days(1))),
        model_win("5h-fable", "fable", 0.95, Duration::hours(1)),
    ]);
    let s2 = snap_7d(0.0, Duration::days(6));
    let c = [cand(&a, Some(&s1), 0), cand(&b, Some(&s2), 0)];
    let t = Tuning::default();
    let at = |fam| name(choose(&c, &t, &AtomicUsize::new(0), NOW, fam));
    // Sonnet is unaffected: a resets sooner and wins.
    assert_eq!(at(Some("sonnet")), "a");
    assert_eq!(at(None), "a");
    // Fable: a's 5h-fable headroom 0.05 < min_headroom 0.1 -> not viable.
    assert_eq!(at(Some("fable")), "b");
}

#[test]
fn ties_round_robin() {
    let (a, b) = (meta("a"), meta("b"));
    let cands = [cand(&a, None, 0), cand(&b, None, 0)];
    let cursor = AtomicUsize::new(0);
    let t = Tuning::default();
    let seq: Vec<&str> = (0..6)
        .map(|_| name(choose(&cands, &t, &cursor, NOW, None)))
        .collect();
    assert_eq!(seq, ["a", "b", "a", "b", "a", "b"]);
}

#[test]
fn cursor_advances_only_on_ties() {
    let (a, b) = (meta("a"), meta("b"));
    let (s1, s2) = (
        snap_7d(0.2, Duration::days(1)),
        snap_7d(0.2, Duration::days(2)),
    );
    let cands = [cand(&a, Some(&s1), 0), cand(&b, Some(&s2), 0)];
    let cursor = AtomicUsize::new(0);
    for _ in 0..5 {
        assert_eq!(
            name(choose(&cands, &Tuning::default(), &cursor, NOW, None)),
            "a"
        );
    }
    assert_eq!(cursor.load(Ordering::SeqCst), 0);
}

#[test]
fn timed_weight_override_applies_until_expiry() {
    let (plain, mut boosted) = (meta("plain"), meta("boosted"));
    boosted.weight_override = Some(TimedOverride {
        weight: 5.0,
        until: NOW + Duration::minutes(10),
    });
    let s = snap_7d(0.2, Duration::days(3));
    let c = [cand(&plain, Some(&s), 0), cand(&boosted, Some(&s), 0)];
    let t = Tuning::default();
    let at = |now| name(choose(&c, &t, &AtomicUsize::new(0), now, None));
    assert_eq!(at(NOW), "boosted");
    assert_eq!(at(NOW + Duration::minutes(9)), "boosted");
    // Expired: both weigh 1 and tie, so the fresh cursor yields the first candidate.
    assert_eq!(at(NOW + Duration::minutes(10)), "plain");
    assert_eq!(at(NOW + Duration::hours(1)), "plain");
    // Override can also lower the weight below the base.
    let mut base_heavy = meta("heavy");
    base_heavy.weight = 3.0;
    base_heavy.weight_override = Some(TimedOverride {
        weight: 0.1,
        until: NOW + Duration::hours(1),
    });
    let c = [cand(&base_heavy, Some(&s), 0), cand(&plain, Some(&s), 0)];
    assert_eq!(name(pick(&c)), "plain");
}

#[test]
fn choose_empty_is_none() {
    assert!(pick(&[]).is_none());
}

#[test]
fn choose_ignores_nothing_when_snapshot_missing() {
    let a = meta("a");
    assert_eq!(name(pick(&[cand(&a, None, 0)])), "a");
}

// ---- usable ------------------------------------------------------------------

fn tried(ids: &[&str]) -> HashSet<AccountId> {
    ids.iter().map(|s| s.parse().unwrap()).collect()
}

#[test]
fn usable_requires_ready_status() {
    let m = meta("a");
    let none = tried(&[]);
    let p = ProviderId::Anthropic;
    assert!(usable(&m, &AccountStatus::Ready, p, None, &[], &none));
    let until = NOW + Duration::hours(1);
    for st in [
        AccountStatus::NeedsLogin,
        AccountStatus::Billing,
        AccountStatus::RateLimited {
            until,
            window: Some("5h".into()),
        },
        AccountStatus::AccessDenied { until },
    ] {
        assert!(!usable(&m, &st, p, None, &[], &none), "{st:?}");
    }
}

#[test]
fn usable_checks_provider_tried_and_spent_family() {
    let m = meta("a");
    let ready = AccountStatus::Ready;
    let spent = vec!["fable".to_string()];
    assert!(!usable(
        &m,
        &ready,
        ProviderId::OpenAi,
        None,
        &[],
        &tried(&[])
    ));
    assert!(!usable(
        &m,
        &ready,
        ProviderId::Anthropic,
        None,
        &[],
        &tried(&["a"])
    ));
    assert!(usable(
        &m,
        &ready,
        ProviderId::Anthropic,
        None,
        &[],
        &tried(&["b"])
    ));
    // Spent fable blocks fable requests only.
    let a = ProviderId::Anthropic;
    assert!(!usable(&m, &ready, a, Some("fable"), &spent, &tried(&[])));
    assert!(usable(&m, &ready, a, Some("opus"), &spent, &tried(&[])));
    assert!(usable(&m, &ready, a, None, &spent, &tried(&[])));
}

// ---- tuning ------------------------------------------------------------------

#[test]
fn tuning_from_routing_cfg() {
    let cfg = RoutingCfg {
        five_hour_exp: 2.0,
        min_headroom: 0.3,
        headroom_taper_start: 0.5,
        ..RoutingCfg::default()
    };
    let t = Tuning::from(&cfg);
    assert_eq!(
        (t.five_hour_exp, t.min_headroom, t.headroom_taper_start),
        (2.0, 0.3, 0.5)
    );
    assert_eq!(Tuning::default(), Tuning::from(&RoutingCfg::default()));
}

#[test]
fn tuning_new_rejects_out_of_range() {
    assert!(Tuning::new(5.5, 0.1, 0.2).is_err());
    assert!(Tuning::new(1.0, 2.0, 0.2).is_err());
    assert!(Tuning::new(1.0, 0.1, 0.0).is_err());
}

#[test]
fn min_headroom_gate_follows_tuning() {
    let (mid, fresh) = (meta("mid"), meta("fresh"));
    let s1 = snap_7d(0.4, Duration::days(1));
    let s2 = snap_7d(0.1, Duration::days(5));
    let c = [cand(&mid, Some(&s1), 0), cand(&fresh, Some(&s2), 0)];
    let cursor = AtomicUsize::new(0);
    let loose = Tuning::new(1.0, 0.1, 0.2).unwrap();
    assert_eq!(name(choose(&c, &loose, &cursor, NOW, None)), "mid");
    // 0.7 gate is above mid's 0.6 headroom -> benched.
    let strict = Tuning::new(1.0, 0.7, 0.2).unwrap();
    assert_eq!(name(choose(&c, &strict, &cursor, NOW, None)), "fresh");
}

// ---- Task 22: sticky pins and provider choice ----

use pooltime::server::fallback::{pick_provider, ProviderPool};
use pooltime::server::inference::pick_account;

fn pick_pinned<'a>(cands: &[Candidate<'a>], pin: &str) -> Option<&'a AccountId> {
    let pin: AccountId = pin.parse().unwrap();
    pick_account(
        cands,
        Some(&pin),
        &Tuning::default(),
        &AtomicUsize::new(0),
        NOW,
        None,
    )
}

#[test]
fn sticky_pin_kept_only_if_usable_and_active_tier() {
    let a = meta("a");
    let b = meta("b");
    let mut low = meta("low");
    low.priority = 200;
    // b would win on score (sooner weekly reset); low has the best data but a lower tier.
    let (sa, sb, sl) = (
        snap_7d(0.2, Duration::days(6)),
        snap_7d(0.2, Duration::days(1)),
        snap_7d(0.0, Duration::hours(2)),
    );
    let all = [
        cand(&a, Some(&sa), 0),
        cand(&b, Some(&sb), 0),
        cand(&low, Some(&sl), 0),
    ];
    assert_eq!(name(pick(&all)), "b");
    // Usable pin in the active tier is kept even though it scores lower.
    assert_eq!(name(pick_pinned(&all, "a")), "a");
    // Pin to a lower-tier account is not honored while the top tier has room.
    assert_eq!(name(pick_pinned(&all, "low")), "b");

    // Unusable pin (rate limited -> filtered by `usable`) falls back to `choose`.
    let limited = AccountStatus::RateLimited {
        until: NOW + Duration::hours(1),
        window: None,
    };
    let none = HashSet::new();
    let usable_now: Vec<Candidate<'_>> = all
        .iter()
        .filter(|c| {
            let st = if c.meta.id.as_str() == "a" {
                limited.clone()
            } else {
                AccountStatus::Ready
            };
            usable(c.meta, &st, ProviderId::Anthropic, None, &[], &none)
        })
        .copied()
        .collect();
    assert_eq!(name(pick_pinned(&usable_now, "a")), "b");
    // A pin to an account that is gone entirely is ignored too.
    assert_eq!(name(pick_pinned(&all, "deleted")), "b");

    // Once the top tier is exhausted (tried), the lower tier is active and its pin holds.
    let mut extra = meta("low2");
    extra.priority = 200;
    let rest = [cand(&low, Some(&sb), 0), cand(&extra, Some(&sl), 0)];
    assert_eq!(name(pick(&rest)), "low2");
    assert_eq!(name(pick_pinned(&rest, "low")), "low");
}

fn openai_meta(id: &str) -> AccountMeta {
    AccountMeta::new(id.parse().unwrap(), ProviderId::OpenAi)
}

fn pools<'a>(anth: Vec<Candidate<'a>>, open: Vec<Candidate<'a>>) -> Vec<ProviderPool<'a>> {
    vec![
        ProviderPool {
            provider: ProviderId::Anthropic,
            family: Some("sonnet"),
            cands: anth,
        },
        ProviderPool {
            provider: ProviderId::OpenAi,
            family: None,
            cands: open,
        },
    ]
}

fn choose_provider(
    p: &[ProviderPool<'_>],
    pins: &[(ProviderId, &str)],
    recent: Option<ProviderId>,
) -> Option<ProviderId> {
    let pins: Vec<(ProviderId, AccountId)> =
        pins.iter().map(|(p, a)| (*p, a.parse().unwrap())).collect();
    pick_provider(
        p,
        &pins,
        recent,
        &Tuning::default(),
        &AtomicUsize::new(0),
        NOW,
    )
}

#[test]
fn pick_provider_compares_on_one_expiry_scale() {
    let a = meta("a");
    let o = openai_meta("o");
    let (sa, so) = (
        snap_7d(0.2, Duration::days(6)),
        snap_7d(0.2, Duration::days(1)),
    );
    // Provider-local ranks would both be 5 (tie -> Anthropic); one global scale ranks the
    // sooner-expiring Codex weekly first.
    let p = pools(vec![cand(&a, Some(&sa), 0)], vec![cand(&o, Some(&so), 0)]);
    assert_eq!(choose_provider(&p, &[], None), Some(ProviderId::OpenAi));
    // Reversed data: Anthropic wins.
    let p = pools(vec![cand(&a, Some(&so), 0)], vec![cand(&o, Some(&sa), 0)]);
    assert_eq!(choose_provider(&p, &[], None), Some(ProviderId::Anthropic));
    // Exact tie keeps Anthropic first.
    let p = pools(vec![cand(&a, None, 0)], vec![cand(&o, None, 0)]);
    assert_eq!(choose_provider(&p, &[], None), Some(ProviderId::Anthropic));
    // A provider is compared on its active tier only: Codex's better-placed backup tier
    // does not count.
    let mut backup = openai_meta("o-backup");
    backup.priority = 200;
    let p = pools(
        vec![cand(&a, Some(&sa), 0)],
        vec![cand(&o, Some(&sa), 3), cand(&backup, Some(&so), 0)],
    );
    assert_eq!(choose_provider(&p, &[], None), Some(ProviderId::Anthropic));
    // Pools with no usable account drop out.
    let p = pools(vec![], vec![cand(&o, None, 0)]);
    assert_eq!(choose_provider(&p, &[], None), Some(ProviderId::OpenAi));
    let p = pools(vec![], vec![]);
    assert_eq!(choose_provider(&p, &[], None), None);
}

#[test]
fn pick_provider_sticks_to_pinned_provider() {
    let a = meta("a");
    let o = openai_meta("o");
    let mut o_low = openai_meta("o-low");
    o_low.priority = 200;
    let (sa, so) = (
        snap_7d(0.2, Duration::days(1)),
        snap_7d(0.2, Duration::days(6)),
    );
    // Anthropic would win on score...
    let p = pools(
        vec![cand(&a, Some(&sa), 0)],
        vec![cand(&o, Some(&so), 0), cand(&o_low, Some(&so), 0)],
    );
    assert_eq!(choose_provider(&p, &[], None), Some(ProviderId::Anthropic));
    // ...but a live pin to an active-tier Codex account keeps the session there.
    assert_eq!(
        choose_provider(&p, &[(ProviderId::OpenAi, "o")], None),
        Some(ProviderId::OpenAi)
    );
    // A pin to a non-active-tier account does not count.
    assert_eq!(
        choose_provider(&p, &[(ProviderId::OpenAi, "o-low")], None),
        Some(ProviderId::Anthropic)
    );
    // Both pinned: the most recently served provider wins; unknown -> list order.
    let both = [(ProviderId::Anthropic, "a"), (ProviderId::OpenAi, "o")];
    assert_eq!(
        choose_provider(&p, &both, Some(ProviderId::OpenAi)),
        Some(ProviderId::OpenAi)
    );
    assert_eq!(
        choose_provider(&p, &both, Some(ProviderId::Anthropic)),
        Some(ProviderId::Anthropic)
    );
    assert_eq!(
        choose_provider(&p, &both, None),
        Some(ProviderId::Anthropic)
    );
}
