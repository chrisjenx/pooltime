//! Differential harness: plays scenarios against bro-cli and pooltime, each wired to its own
//! identically scripted fake upstreams, and compares what clients and upstreams saw.
//!
//! `cargo run -p harness -- --bro <bro-cli checkout> --scenarios all`
//!
//! Observation fields (per daemon):
//! - `step<i>.status`, `step<i>.header.<name>`, `step<i>.sse` (semantic event sequence),
//!   `step<i>.end` (how the stream ended), `step<i>.json` (projection of a JSON body);
//! - `account.<name>.status` (`ready | needs_login | rate_limited | billing | access_denied`) and
//!   `account.<name>.cooldown` (remaining cooldown, whole minutes);
//! - `upstream.inference` (`messages@<account>` / `responses@<account>` in arrival order),
//!   `upstream.refresh` (refresh grants per account), `upstream.detail<n>` (scenario projection).
//!
//! Exit status 0 iff every scenario ran and every difference is allowlisted.

mod bench;
mod compare;
mod daemons;
mod fake;
mod observe;
#[cfg(test)]
mod parity;
mod scenarios;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use clap::Parser;

use compare::{diff, Allowlist, Diff, Observation};
use daemons::{Daemon, Tools};
use fake::{account_of, refresh_account_of, Upstreams, MESSAGES, OAUTH_TOKEN, RESPONSES, TOKEN};
use scenarios::Scenario;

#[derive(Parser)]
#[command(about = "Run bro-cli and pooltime side by side against fake upstreams and compare")]
struct Args {
    /// bro-cli checkout (must contain pool/src/index.ts and pool/node_modules).
    #[arg(long)]
    bro: PathBuf,
    /// `all` or a comma-separated list of scenario names.
    #[arg(long, default_value = "all")]
    scenarios: String,
    /// pooltime binary (default: build `target/debug/pooltime` with cargo first).
    #[arg(long)]
    pooltime: Option<PathBuf>,
    /// bun binary (default: `bun` on PATH, else /opt/homebrew/bin/bun).
    #[arg(long)]
    bun: Option<PathBuf>,
    /// Allowlist of intended differences.
    #[arg(long)]
    allowlist: Option<PathBuf>,
    /// Report path (default `docs/harness-report.md` in the workspace).
    #[arg(long)]
    report: Option<PathBuf>,
    /// Print every field, not only differences.
    #[arg(long)]
    verbose: bool,
}

struct Outcome {
    name: &'static str,
    about: &'static str,
    diffs: Vec<Diff>,
    error: Option<String>,
    elapsed: Duration,
}

impl Outcome {
    fn failed(&self) -> bool {
        self.error.is_some() || self.diffs.iter().any(|d| d.allowed.is_none())
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("harness lives in the workspace")
        .to_path_buf()
}

fn find_bun(explicit: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(b) = explicit {
        return Ok(b);
    }
    let from_path = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join("bun"))
            .find(|c| c.is_file())
    });
    from_path
        .or_else(|| {
            let c = PathBuf::from("/opt/homebrew/bin/bun");
            c.is_file().then_some(c)
        })
        .context("bun not found; pass --bun")
}

fn build_pooltime(root: &Path) -> anyhow::Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = std::process::Command::new(cargo)
        .args(["build", "--quiet", "-p", "pooltime", "--bin", "pooltime"])
        .current_dir(root)
        .status()
        .context("running cargo build for pooltime")?;
    if !status.success() {
        bail!("building pooltime failed");
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    Ok(target.join("debug").join("pooltime"))
}

fn git(dir: &Path, args: &[&str]) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Upstream-side fields: which account each inference/refresh request was for, plus the
/// scenario's request projection.
fn upstream_fields(up: &Upstreams, sc: &Scenario) -> Observation {
    let mut out = Observation::new();
    let mut inference = Vec::new();
    let mut refresh = Vec::new();
    let mut details = Vec::new();
    // Arrival order across both fakes (shared log), so a cross-provider hop's order is compared.
    for (side, r) in &up.arrivals() {
        match r.path.as_str() {
            MESSAGES | RESPONSES => {
                let kind = if r.path == MESSAGES {
                    "messages"
                } else {
                    "responses"
                };
                inference.push(format!("{kind}@{}", account_of(r)));
                if let Some(d) = sc.detail.and_then(|f| f(r)) {
                    details.push(format!("{kind}: {d}"));
                }
            }
            TOKEN | OAUTH_TOKEN => refresh.push(format!("{side}@{}", refresh_account_of(r))),
            _ => {}
        }
    }
    out.insert("upstream.inference".into(), inference.join(", "));
    out.insert("upstream.refresh".into(), refresh.join(", "));
    for (i, d) in details.into_iter().enumerate() {
        out.insert(format!("upstream.detail{i}"), d);
    }
    out
}

/// Rename accounts in first-use order (`A`, `B`, ...) for scenarios where the identity of
/// interchangeable accounts is not part of the behavior.
fn canonicalize(obs: Observation) -> Observation {
    let order: Vec<String> = obs
        .get("upstream.inference")
        .map(|v| {
            let mut seen: Vec<String> = Vec::new();
            for hit in v.split(", ") {
                if let Some((_, a)) = hit.split_once('@') {
                    if !seen.iter().any(|s| s == a) {
                        seen.push(a.to_string());
                    }
                }
            }
            seen
        })
        .unwrap_or_default();
    let canon = |name: &str| -> String {
        order
            .iter()
            .position(|a| a == name)
            .map(|i| ((b'A' + i as u8) as char).to_string())
            .unwrap_or_else(|| name.to_string())
    };
    obs.into_iter()
        .map(|(k, v)| {
            let k = match k.strip_prefix("account.").and_then(|r| r.split_once('.')) {
                Some((name, rest)) => format!("account.{}.{rest}", canon(name)),
                None => k,
            };
            let v = if k == "upstream.inference" || k.ends_with("x-pool-account") {
                v.split(", ")
                    .map(|hit| match hit.split_once('@') {
                        Some((kind, a)) => format!("{kind}@{}", canon(a)),
                        None => canon(hit),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                v
            };
            (k, v)
        })
        .collect()
}

async fn play(d: &Daemon, sc: &Scenario, up: &Upstreams) -> anyhow::Result<Observation> {
    let client = reqwest::Client::builder().no_proxy().build()?;
    let mut obs = Observation::new();
    for (i, step) in sc.steps.iter().enumerate() {
        let t = Instant::now();
        for (f, v) in observe::run_step(&client, &d.base, step).await {
            obs.insert(format!("step{i}.{f}"), v);
        }
        obs.insert(
            format!("{}step{i}.ms", compare::INFO),
            t.elapsed().as_millis().to_string(),
        );
        tokio::time::sleep(step.wait_after).await;
    }
    for (name, status, cooldown) in d.accounts(&client).await? {
        obs.insert(format!("account.{name}.status"), status);
        obs.insert(format!("account.{name}.cooldown"), cooldown);
    }
    obs.extend(upstream_fields(up, sc));
    if sc.canonical_accounts {
        obs = canonicalize(obs);
    }
    Ok(obs)
}

async fn run_scenario(
    tools: &Tools,
    sc: &Scenario,
    allow: &Allowlist,
) -> anyhow::Result<(Observation, Observation, Vec<Diff>)> {
    let dir = tempfile::Builder::new()
        .prefix("pooltime-harness-")
        .tempdir()
        .context("creating the scenario sandbox")?;
    let bro_box = dir.path().join("bro");
    let pt_box = dir.path().join("pooltime");
    let bro_up = Upstreams::start().await;
    (sc.script)(&bro_up);
    let pt_up = Upstreams::start().await;
    (sc.script)(&pt_up);

    let bro = daemons::start_bro(tools, sc, &bro_up, &bro_box).await;
    let pt = daemons::start_pooltime(tools, sc, &pt_up, &pt_box).await;
    let (bro, pt) = match (bro, pt) {
        (Ok(b), Ok(p)) => (b, p),
        (b, p) => {
            let mut msg = String::new();
            if let Err(e) = &b {
                let _ = write!(msg, "bro: {e:#}\n{}\n", daemons::log_tail(&bro_box, 15));
            }
            if let Err(e) = &p {
                let _ = write!(msg, "pooltime: {e:#}\n{}\n", daemons::log_tail(&pt_box, 15));
            }
            for d in [b.ok(), p.ok()].into_iter().flatten() {
                d.stop().await;
            }
            bail!("boot failed\n{msg}");
        }
    };
    let (b, p) = tokio::join!(play(&bro, sc, &bro_up), play(&pt, sc, &pt_up));
    let touched = bro.keychain_touched() || pt.keychain_touched();
    bro.stop().await;
    pt.stop().await;
    if touched {
        bail!("a daemon invoked `security` (macOS Keychain); the sandbox must prevent that");
    }
    let b = b.context("playing against bro")?;
    let p = p.context("playing against pooltime")?;
    let diffs = diff(sc.name, &b, &p, allow);
    Ok((b, p, diffs))
}

fn md_cell(s: &str) -> String {
    let s = s.replace('|', "\\|").replace('\n', " ");
    if s.chars().count() > 400 {
        let cut: String = s.chars().take(400).collect();
        format!("`{cut}…`")
    } else {
        format!("`{s}`")
    }
}

#[derive(serde::Deserialize, Default)]
struct FixedFile {
    #[serde(default)]
    fixed: Vec<Fixed>,
}

#[derive(serde::Deserialize)]
struct Fixed {
    scenario: String,
    summary: String,
    commit: String,
    test: String,
}

fn report(
    outcomes: &[Outcome],
    allow: &Allowlist,
    fixed: &FixedFile,
    bro_dir: &Path,
    root: &Path,
    total: Duration,
) -> String {
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let mut r = String::new();
    let _ = writeln!(r, "# Differential harness report\n");
    let _ = writeln!(
        r,
        "Generated by `cargo run -p harness -- --bro <bro-cli> --scenarios all` (see `harness/README.md`).\n"
    );
    let _ = writeln!(r, "- Run: {now} ({:.0} s)", total.as_secs_f64());
    let _ = writeln!(
        r,
        "- bro: `{}` on `{}`",
        git(bro_dir, &["rev-parse", "--short", "HEAD"]),
        git(bro_dir, &["rev-parse", "--abbrev-ref", "HEAD"])
    );
    // The code under test is pooltime's `src/`; harness-only edits do not change what was tested.
    let dirty = git(
        root,
        &["status", "--porcelain", "--untracked-files=no", "--", "src"],
    );
    let _ = writeln!(
        r,
        "- pooltime: `{}` (`src/` last changed in `{}`){}",
        git(root, &["rev-parse", "--short", "HEAD"]),
        git(root, &["log", "-1", "--format=%h", "--", "src"]),
        if dirty.is_empty() || dirty == "unknown" {
            ""
        } else {
            ", with uncommitted changes under `src/`"
        }
    );
    let passed = outcomes.iter().filter(|o| !o.failed()).count();
    let allowed: usize = outcomes
        .iter()
        .map(|o| o.diffs.iter().filter(|d| d.allowed.is_some()).count())
        .sum();
    let _ = writeln!(
        r,
        "- Scenarios: {} run, {passed} passed, {} failed; {allowed} allowlisted differences\n",
        outcomes.len(),
        outcomes.len() - passed
    );

    let _ = writeln!(r, "## Scenarios\n");
    let _ = writeln!(r, "| Scenario | Result | Allowlisted | What it checks |");
    let _ = writeln!(r, "|---|---|---|---|");
    for o in outcomes {
        let n_allowed = o.diffs.iter().filter(|d| d.allowed.is_some()).count();
        let result = if o.error.is_some() {
            "ERROR"
        } else if o.failed() {
            "FAIL"
        } else {
            "pass"
        };
        let _ = writeln!(r, "| `{}` | {result} | {n_allowed} | {} |", o.name, o.about);
    }

    let failing: Vec<&Outcome> = outcomes.iter().filter(|o| o.failed()).collect();
    if !failing.is_empty() {
        let _ = writeln!(r, "\n## Unexplained differences\n");
        for o in failing {
            if let Some(e) = &o.error {
                let _ = writeln!(r, "- `{}`: error: {}", o.name, md_cell(e));
            }
            for d in o.diffs.iter().filter(|d| d.allowed.is_none()) {
                let _ = writeln!(
                    r,
                    "- `{}` `{}`: bro {} / pooltime {}",
                    o.name,
                    d.field,
                    md_cell(&d.bro),
                    md_cell(&d.pooltime)
                );
            }
        }
    }

    let _ = writeln!(r, "\n## Allowlisted differences\n");
    let _ = writeln!(r, "| Scenario | Field | bro | pooltime | Reason | Source |");
    let _ = writeln!(r, "|---|---|---|---|---|---|");
    let mut used = vec![false; allow.entries.len()];
    for o in outcomes {
        for d in &o.diffs {
            if let Some(i) = d.allowed {
                used[i] = true;
                let e = &allow.entries[i];
                let _ = writeln!(
                    r,
                    "| `{}` | `{}` | {} | {} | {} | {} |",
                    o.name,
                    d.field,
                    md_cell(&d.bro),
                    md_cell(&d.pooltime),
                    e.reason,
                    e.source
                );
            }
        }
    }
    let unused: Vec<usize> = (0..used.len()).filter(|i| !used[*i]).collect();
    if !unused.is_empty() {
        let _ = writeln!(
            r,
            "\nAllowlist entries that matched nothing in this run (scenario not selected, or stale):\n"
        );
        for i in unused {
            let e = &allow.entries[i];
            let _ = writeln!(r, "- `{}` `{}` ({})", e.scenario, e.field, e.source);
        }
    }

    let _ = writeln!(r, "\n## pooltime bugs found and fixed\n");
    if fixed.fixed.is_empty() {
        let _ = writeln!(r, "None.");
    } else {
        let _ = writeln!(r, "| Scenario | Bug | Fix commit | Regression test |");
        let _ = writeln!(r, "|---|---|---|---|");
        for f in &fixed.fixed {
            let _ = writeln!(
                r,
                "| `{}` | {} | {} | `{}` |",
                f.scenario, f.summary, f.commit, f.test
            );
        }
    }
    r
}

async fn real_main() -> anyhow::Result<bool> {
    let args = Args::parse();
    let root = workspace_root();
    let bro = args
        .bro
        .canonicalize()
        .with_context(|| format!("bro checkout {}", args.bro.display()))?;
    if !bro.join("pool/src/index.ts").is_file() {
        bail!(
            "{} is not a bro-cli checkout (no pool/src/index.ts)",
            bro.display()
        );
    }
    if !bro.join("pool/node_modules").is_dir() {
        bail!(
            "{} has no pool/node_modules; run `bun install` in {}/pool once",
            bro.display(),
            bro.display()
        );
    }
    let bun = find_bun(args.bun)?;
    let pooltime = match args.pooltime {
        Some(p) => p,
        None => build_pooltime(&root)?,
    };
    let allow_path = args
        .allowlist
        .unwrap_or_else(|| root.join("harness/allowlist.toml"));
    let allow = Allowlist::load(&allow_path)?;
    let fixed: FixedFile = std::fs::read_to_string(root.join("harness/fixed.toml"))
        .ok()
        .map(|t| toml::from_str(&t))
        .transpose()
        .context("parsing harness/fixed.toml")?
        .unwrap_or_default();
    let tools = Tools {
        bun,
        bro: bro.clone(),
        pooltime,
        real_home: std::env::var_os("HOME").map(PathBuf::from),
    };

    let all = scenarios::all();
    let selected: Vec<&Scenario> = if args.scenarios == "all" {
        all.iter().collect()
    } else {
        let names: Vec<&str> = args.scenarios.split(',').map(str::trim).collect();
        for n in &names {
            if !all.iter().any(|s| s.name == *n) {
                bail!("unknown scenario {n:?}");
            }
        }
        all.iter().filter(|s| names.contains(&s.name)).collect()
    };

    let started = Instant::now();
    let mut outcomes = Vec::new();
    for sc in selected {
        let t = Instant::now();
        let outcome = match run_scenario(&tools, sc, &allow).await {
            Ok((b, p, diffs)) => {
                if args.verbose {
                    print_fields(sc.name, &b, &p);
                }
                Outcome {
                    name: sc.name,
                    about: sc.about,
                    diffs,
                    error: None,
                    elapsed: t.elapsed(),
                }
            }
            Err(e) => Outcome {
                name: sc.name,
                about: sc.about,
                diffs: Vec::new(),
                error: Some(format!("{e:#}")),
                elapsed: t.elapsed(),
            },
        };
        print_outcome(&outcome);
        outcomes.push(outcome);
    }

    let report_path = args
        .report
        .unwrap_or_else(|| root.join("docs/harness-report.md"));
    std::fs::write(
        &report_path,
        report(&outcomes, &allow, &fixed, &bro, &root, started.elapsed()),
    )
    .with_context(|| format!("writing {}", report_path.display()))?;
    let failed = outcomes.iter().filter(|o| o.failed()).count();
    println!(
        "\n{} scenarios, {} passed, {failed} failed; report: {}",
        outcomes.len(),
        outcomes.len() - failed,
        report_path.display()
    );
    Ok(failed == 0)
}

fn print_fields(name: &str, b: &Observation, p: &Observation) {
    let keys: std::collections::BTreeSet<&String> = b.keys().chain(p.keys()).collect();
    println!("--- {name}");
    for k in keys {
        let bv = b.get(k).map_or(compare::ABSENT, String::as_str);
        let pv = p.get(k).map_or(compare::ABSENT, String::as_str);
        let mark = if bv == pv { " " } else { "*" };
        println!("{mark} {k}\n    bro:      {bv}\n    pooltime: {pv}");
    }
}

fn print_outcome(o: &Outcome) {
    let status = if o.error.is_some() {
        "ERROR"
    } else if o.failed() {
        "FAIL"
    } else {
        "pass"
    };
    let allowed = o.diffs.iter().filter(|d| d.allowed.is_some()).count();
    println!(
        "{status:5} {:28} {:5.1}s  allowlisted={allowed}",
        o.name,
        o.elapsed.as_secs_f64()
    );
    if let Some(e) = &o.error {
        println!("      {e}");
    }
    for d in o.diffs.iter().filter(|d| d.allowed.is_none()) {
        println!(
            "      {}\n        bro:      {}\n        pooltime: {}",
            d.field, d.bro, d.pooltime
        );
    }
}

fn main() -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("harness: {e}");
            return ExitCode::from(2);
        }
    };
    // `harness bench ...` is the resource/latency benchmark (see bench.rs); everything else is
    // the differential run.
    if std::env::args().nth(1).as_deref() == Some("bench") {
        let args = bench::BenchArgs::parse_from(std::env::args().skip(1));
        return match rt.block_on(bench::run(args)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("harness bench: {e:#}");
                ExitCode::from(2)
            }
        };
    }
    match rt.block_on(real_main()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("harness: {e:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_names_follow_first_use() {
        let mut o = Observation::new();
        o.insert(
            "upstream.inference".into(),
            "messages@beta, messages@beta, messages@alpha".into(),
        );
        o.insert("account.alpha.status".into(), "ready".into());
        o.insert("account.beta.status".into(), "ready".into());
        let c = canonicalize(o);
        assert_eq!(
            c["upstream.inference"],
            "messages@A, messages@A, messages@B"
        );
        assert!(c.contains_key("account.A.status"));
        assert!(c.contains_key("account.B.status"));
    }

    #[test]
    fn codex_5xx_allowance_is_one_attempt_plus_the_retry_budget() {
        let allow = Allowlist::load(&workspace_root().join("harness/allowlist.toml")).unwrap();
        let e = allow
            .entries
            .iter()
            .find(|e| e.scenario == "codex_server_error")
            .expect("codex_server_error entry");
        let hits = vec!["responses@cx"; 1 + daemons::OVERLOAD_RETRY_MAX as usize].join(", ");
        assert_eq!(e.pooltime, hits);
    }

    #[test]
    fn every_scenario_name_is_unique() {
        let all = scenarios::all();
        let mut names: Vec<&str> = all.iter().map(|s| s.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len());
    }
}
