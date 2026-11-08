//! `harness bench`: resource and latency numbers for bro and pooltime on this machine.
//!
//! `cargo run -p harness -- bench --bro <bro-cli checkout>`
//!
//! Both daemons run in the harness sandbox (see `daemons.rs`: cleared environment, temp
//! HOME/pool/config dirs, never ports 3456/3460, the failing `security` shim, fake upstreams on
//! 127.0.0.1 and fake tokens), one at a time, against their own fresh fakes. Measured per daemon:
//!
//! - **cold start**: spawn until the first `/health` 200 (the harness polls every 5 ms); one
//!   discarded warm-up boot, then the median of `--cold-runs` boots;
//! - **idle RSS**: resident set of the whole process tree (`ps -o rss=`, summed over the daemon
//!   and every descendant) after `--idle-secs` with no traffic;
//! - **added TTFB**: time to the first response body byte through the daemon minus the same
//!   request sent straight to the fake upstream, p50/p95 over `--ttfb-requests` of each,
//!   interleaved (`--ttfb-concurrency` workers each alternate direct / via daemon);
//! - **RSS under load**: peak process-tree RSS while `--streams` concurrent streaming requests
//!   are open, the fake trickling each one out over `--stream-secs`.
//!
//! Not part of `cargo test` (needs bun, a bro checkout and minutes of wall clock).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use clap::Parser;
use serde_json::Value;

use crate::daemons::{self, Daemon, Kind, Tools, FORBIDDEN_PORTS};
use crate::fake::{self, Reply, Upstreams, MESSAGES};
use crate::scenarios::{claude, claude_body, Scenario};

#[derive(Parser, Debug)]
#[command(
    name = "harness bench",
    about = "Measure idle RSS, RSS under concurrent streams, cold start and added TTFB for bro and pooltime"
)]
pub struct BenchArgs {
    /// bro-cli checkout (must contain pool/src/index.ts and pool/node_modules).
    #[arg(long)]
    bro: PathBuf,
    /// pooltime binary (default: `cargo build --release` and use `target/release/pooltime`).
    #[arg(long)]
    pooltime: Option<PathBuf>,
    /// bun binary (default: `bun` on PATH, else /opt/homebrew/bin/bun).
    #[arg(long)]
    bun: Option<PathBuf>,
    /// Seconds of no traffic before the idle RSS sample.
    #[arg(long, default_value_t = 60)]
    idle_secs: u64,
    /// Concurrent streaming requests for the RSS-under-load sample.
    #[arg(long, default_value_t = 50)]
    streams: usize,
    /// How long the fake upstream takes to finish each of those streams.
    #[arg(long, default_value_t = 5)]
    stream_secs: u64,
    /// Timed boots for the cold-start median (after one discarded warm-up boot).
    #[arg(long, default_value_t = 15)]
    cold_runs: usize,
    /// Requests per side (direct and via the daemon) for the TTFB percentiles.
    #[arg(long, default_value_t = 300)]
    ttfb_requests: usize,
    /// Concurrent TTFB workers.
    #[arg(long, default_value_t = 4)]
    ttfb_concurrency: usize,
    /// Only measure one daemon (`bro` or `pooltime`).
    #[arg(long)]
    only: Option<String>,
}

/// Numbers for one daemon.
#[derive(Debug, Default)]
struct Measured {
    cold_ms: Vec<f64>,
    cold_first_ms: f64,
    restart_ms: Vec<f64>,
    idle_rss_kb: u64,
    boot_rss_kb: u64,
    idle_procs: usize,
    ttfb_direct_ms: Vec<f64>,
    ttfb_daemon_ms: Vec<f64>,
    load_peak_rss_kb: u64,
    load_ok: usize,
    load_wall: Duration,
}

/// Body of an SSE stream that the fake upstream trickles out over `total`.
fn slow_stream(total: Duration) -> Reply {
    const DELTAS: u32 = 10;
    let step = total / (DELTAS + 1);
    let mut frames = vec![
        (Duration::ZERO, fake::message_start()),
        (Duration::ZERO, fake::text_block_start(0)),
    ];
    for i in 0..DELTAS {
        frames.push((step, fake::text_delta(0, &format!("chunk {i} "))));
    }
    frames.push((step, fake::block_stop(0)));
    for f in fake::message_end() {
        frames.push((Duration::ZERO, f));
    }
    fake::allowed_headers(Reply::sse(frames))
}

/// Fresh fakes whose `/v1/messages` answers immediately, or slowly while `slow` is set.
async fn bench_upstreams(slow: Arc<AtomicBool>, stream_for: Duration) -> Upstreams {
    let up = Upstreams::start().await;
    let fast = fake::anthropic_ok("benchmark reply");
    let slow_reply = slow_stream(stream_for);
    up.anthropic.route(MESSAGES, move |_, _| {
        if slow.load(Ordering::Relaxed) {
            slow_reply.clone()
        } else {
            fast.clone()
        }
    });
    up
}

fn bench_scenario() -> Scenario {
    Scenario::new("bench", "benchmark: one healthy Claude account", |_| {})
        .accounts(vec![claude("alpha", 1)])
}

async fn start(
    kind: Kind,
    tools: &Tools,
    sc: &Scenario,
    up: &Upstreams,
    sandbox: &Path,
) -> anyhow::Result<Daemon> {
    let d = match kind {
        Kind::Bro => daemons::start_bro(tools, sc, up, sandbox).await,
        Kind::Pooltime => daemons::start_pooltime(tools, sc, up, sandbox).await,
    };
    d.with_context(|| {
        format!(
            "{} failed to boot\n{}",
            kind.as_str(),
            daemons::log_tail(sandbox, 15)
        )
    })
}

/// Parse `ps -A -o pid=,ppid=,rss=` output.
fn parse_ps(text: &str) -> Vec<(u32, u32, u64)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let rss = it.next()?.parse().ok()?;
            Some((pid, ppid, rss))
        })
        .collect()
}

/// `root` and all its descendants with their RSS (KiB).
fn tree(rows: &[(u32, u32, u64)], root: u32) -> Vec<(u32, u64)> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let rss: HashMap<u32, u64> = rows.iter().map(|(p, _, r)| (*p, *r)).collect();
    for (pid, ppid, _) in rows {
        children.entry(*ppid).or_default().push(*pid);
    }
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if let Some(r) = rss.get(&p) {
            out.push((p, *r));
            stack.extend(children.get(&p).into_iter().flatten().copied());
        }
    }
    out
}

/// The daemon's process tree right now: `(pids, total RSS KiB)`.
async fn tree_rss(root: u32) -> anyhow::Result<(Vec<u32>, u64)> {
    let out = tokio::process::Command::new("/bin/ps")
        .args(["-A", "-o", "pid=,ppid=,rss="])
        .output()
        .await
        .context("running ps")?;
    let rows = parse_ps(&String::from_utf8_lossy(&out.stdout));
    let t = tree(&rows, root);
    ensure!(!t.is_empty(), "daemon process {root} is gone");
    Ok((
        t.iter().map(|(p, _)| *p).collect(),
        t.iter().map(|(_, r)| r).sum(),
    ))
}

/// Stop the daemon and anything it spawned.
async fn stop(d: Daemon) -> anyhow::Result<()> {
    // Descendants first, while their pids are still theirs (the root is killed and reaped by
    // `Daemon::stop`, so its pid is never signalled after it could be reused).
    if let Some(root) = d.pid() {
        let tree = tree_rss(root).await.map(|(p, _)| p).unwrap_or_default();
        for pid in tree.into_iter().filter(|p| *p != root) {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-9", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    let touched = d.keychain_touched();
    d.stop().await;
    ensure!(
        !touched,
        "a daemon invoked `security` (macOS Keychain); the sandbox must prevent that"
    );
    Ok(())
}

/// Nearest-rank percentile of `v` (`q` in 0..=1).
fn percentile(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let rank = ((q * s.len() as f64).ceil() as usize).clamp(1, s.len());
    s[rank - 1]
}

fn median(v: &[f64]) -> f64 {
    percentile(v, 0.5)
}

/// Time to the first body byte of a streaming Messages request (`None` on any failure).
async fn ttfb(client: &reqwest::Client, url: &str, body: &Value) -> Option<f64> {
    let t = Instant::now();
    let mut r = client.post(url).json(body).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    let first = r.chunk().await.ok()??;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    drop(first);
    while let Ok(Some(_)) = r.chunk().await {}
    Some(ms)
}

/// `(direct, via daemon)` TTFB samples, interleaved across `workers`.
async fn measure_ttfb(
    base: &str,
    upstream: &str,
    requests: usize,
    workers: usize,
) -> anyhow::Result<(Vec<f64>, Vec<f64>)> {
    let client = reqwest::Client::builder().no_proxy().build()?;
    let body = claude_body("claude-sonnet-5", true, None);
    let direct_url = format!("{upstream}{MESSAGES}");
    let daemon_url = format!("{base}{MESSAGES}");
    // Warm both paths (connection pools, lazy init) before timing.
    for _ in 0..10 {
        ttfb(&client, &direct_url, &body).await;
        ttfb(&client, &daemon_url, &body).await;
    }
    let per_worker = requests.div_ceil(workers);
    let mut tasks = Vec::new();
    for w in 0..workers {
        let (client, body) = (client.clone(), body.clone());
        let (direct_url, daemon_url) = (direct_url.clone(), daemon_url.clone());
        tasks.push(tokio::spawn(async move {
            let (mut d, mut p) = (Vec::new(), Vec::new());
            for i in 0..per_worker {
                // Alternate which side goes first so neither always runs on a warmer path.
                let order = (i + w) % 2 == 0;
                for side in [order, !order] {
                    if side {
                        d.push(ttfb(&client, &direct_url, &body).await);
                    } else {
                        p.push(ttfb(&client, &daemon_url, &body).await);
                    }
                }
            }
            (d, p)
        }));
    }
    let (mut direct, mut daemon) = (Vec::new(), Vec::new());
    for t in tasks {
        let (d, p) = t.await?;
        direct.extend(d);
        daemon.extend(p);
    }
    let failed = direct.iter().chain(&daemon).filter(|x| x.is_none()).count();
    ensure!(failed == 0, "{failed} TTFB requests failed");
    Ok((
        direct.into_iter().flatten().collect(),
        daemon.into_iter().flatten().collect(),
    ))
}

/// Open `n` concurrent slow streams; sample tree RSS until they all finish. Returns
/// `(peak RSS KiB, streams that completed with message_stop, wall time)`.
async fn measure_load(base: &str, pid: u32, n: usize) -> anyhow::Result<(u64, usize, Duration)> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .build()?;
    let body = claude_body("claude-sonnet-5", true, None);
    let url = format!("{base}{MESSAGES}");
    let started = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..n {
        let (client, body, url) = (client.clone(), body.clone(), url.clone());
        tasks.push(tokio::spawn(async move {
            let Ok(mut r) = client.post(&url).json(&body).send().await else {
                return false;
            };
            if !r.status().is_success() {
                return false;
            }
            let mut all = Vec::new();
            while let Ok(Some(c)) = r.chunk().await {
                all.extend_from_slice(&c);
            }
            String::from_utf8_lossy(&all).contains("message_stop")
        }));
    }
    let mut peak = 0;
    while !tasks.iter().all(|t| t.is_finished()) {
        peak = peak.max(tree_rss(pid).await?.1);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let wall = started.elapsed();
    let mut ok = 0;
    for t in tasks {
        ok += usize::from(t.await?);
    }
    Ok((peak, ok, wall))
}

/// Spawn the daemon in `sandbox` and time spawn → first `/health` 200, then stop it.
async fn timed_boot(
    kind: Kind,
    tools: &Tools,
    sc: &Scenario,
    sandbox: &Path,
    stream_for: Duration,
) -> anyhow::Result<f64> {
    let up = bench_upstreams(Arc::default(), stream_for).await;
    let t = Instant::now();
    let d = start(kind, tools, sc, &up, sandbox).await?;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    stop(d).await?;
    Ok(ms)
}

/// Cold starts for every daemon, interleaved run by run so drift in machine load hits all of
/// them alike. First boot: an empty state dir every time (the daemon creates its state; one
/// discarded warm-up round for the file cache). Restart: one state dir per daemon, reused (what
/// a service restart sees; the untimed first round creates the state).
async fn measure_cold(
    kinds: &[Kind],
    tools: &Tools,
    args: &BenchArgs,
    out: &mut [Measured],
) -> anyhow::Result<()> {
    let sc = bench_scenario();
    let stream_for = Duration::from_secs(args.stream_secs);
    for run in 0..=args.cold_runs {
        for (i, kind) in kinds.iter().enumerate() {
            let dir = tempfile::Builder::new()
                .prefix("pooltime-bench-")
                .tempdir()?;
            let ms = timed_boot(*kind, tools, &sc, dir.path(), stream_for).await?;
            if run == 0 {
                out[i].cold_first_ms = ms;
            } else {
                out[i].cold_ms.push(ms);
            }
            println!("  {} first boot {run}: {ms:.1} ms", kind.as_str());
        }
    }
    let dirs = kinds
        .iter()
        .map(|_| tempfile::Builder::new().prefix("pooltime-bench-").tempdir())
        .collect::<Result<Vec<_>, _>>()?;
    for run in 0..=args.cold_runs {
        for (i, kind) in kinds.iter().enumerate() {
            let ms = timed_boot(*kind, tools, &sc, dirs[i].path(), stream_for).await?;
            if run > 0 {
                out[i].restart_ms.push(ms);
            }
            println!("  {} restart {run}: {ms:.1} ms", kind.as_str());
        }
    }
    Ok(())
}

/// Idle RSS, TTFB and RSS under load for one long-lived daemon.
async fn measure(
    kind: Kind,
    tools: &Tools,
    args: &BenchArgs,
    m: &mut Measured,
) -> anyhow::Result<()> {
    let sc = bench_scenario();
    let stream_for = Duration::from_secs(args.stream_secs);

    // One long-lived daemon: idle, then TTFB, then concurrent streams.
    let dir = tempfile::Builder::new()
        .prefix("pooltime-bench-")
        .tempdir()?;
    let slow = Arc::new(AtomicBool::new(false));
    let up = bench_upstreams(slow.clone(), stream_for).await;
    let d = start(kind, tools, &sc, &up, dir.path()).await?;
    let result = async {
        let pid = d.pid().context("daemon has no pid")?;
        m.boot_rss_kb = tree_rss(pid).await?.1;
        println!("  {} idle for {} s ...", kind.as_str(), args.idle_secs);
        tokio::time::sleep(Duration::from_secs(args.idle_secs)).await;
        let (pids, rss) = tree_rss(pid).await?;
        m.idle_rss_kb = rss;
        m.idle_procs = pids.len();

        let (direct, daemon) = measure_ttfb(
            &d.base,
            &up.anthropic.url(),
            args.ttfb_requests,
            args.ttfb_concurrency,
        )
        .await?;
        m.ttfb_direct_ms = direct;
        m.ttfb_daemon_ms = daemon;

        slow.store(true, Ordering::Relaxed);
        let (peak, ok, wall) = measure_load(&d.base, pid, args.streams).await?;
        m.load_peak_rss_kb = peak;
        m.load_ok = ok;
        m.load_wall = wall;
        anyhow::Ok(())
    }
    .await;
    stop(d).await?;
    result
}

fn command_line(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn build_release(root: &Path) -> anyhow::Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = std::process::Command::new(cargo)
        .args([
            "build",
            "--release",
            "--quiet",
            "-p",
            "pooltime",
            "--bin",
            "pooltime",
        ])
        .current_dir(root)
        .status()
        .context("running cargo build --release for pooltime")?;
    if !status.success() {
        bail!("building pooltime (release) failed");
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    Ok(target.join("release").join("pooltime"))
}

fn mib(kb: u64) -> String {
    format!("{:.1} MiB", kb as f64 / 1024.0)
}

fn row(m: &Measured, streams: usize) -> [String; 9] {
    let p = |v: &[f64], q| percentile(v, q);
    [
        mib(m.idle_rss_kb),
        format!(
            "{} (peak, {}/{} streams ok, {:.1} s)",
            mib(m.load_peak_rss_kb),
            m.load_ok,
            streams,
            m.load_wall.as_secs_f64()
        ),
        format!(
            "{:.1} ms (min {:.1}, max {:.1}; first {:.1})",
            median(&m.cold_ms),
            p(&m.cold_ms, 0.0),
            p(&m.cold_ms, 1.0),
            m.cold_first_ms
        ),
        format!(
            "{:.1} ms (min {:.1}, max {:.1})",
            median(&m.restart_ms),
            p(&m.restart_ms, 0.0),
            p(&m.restart_ms, 1.0)
        ),
        format!(
            "{:.2} ms",
            p(&m.ttfb_daemon_ms, 0.5) - p(&m.ttfb_direct_ms, 0.5)
        ),
        format!(
            "{:.2} ms",
            p(&m.ttfb_daemon_ms, 0.95) - p(&m.ttfb_direct_ms, 0.95)
        ),
        format!(
            "{:.2} / {:.2} ms",
            p(&m.ttfb_direct_ms, 0.5),
            p(&m.ttfb_direct_ms, 0.95)
        ),
        format!(
            "{:.2} / {:.2} ms",
            p(&m.ttfb_daemon_ms, 0.5),
            p(&m.ttfb_daemon_ms, 0.95)
        ),
        format!(
            "{} after boot, {} process(es)",
            mib(m.boot_rss_kb),
            m.idle_procs
        ),
    ]
}

pub async fn run(args: BenchArgs) -> anyhow::Result<()> {
    let root = crate::workspace_root();
    let bro = args
        .bro
        .canonicalize()
        .with_context(|| format!("bro checkout {}", args.bro.display()))?;
    ensure!(
        bro.join("pool/src/index.ts").is_file() && bro.join("pool/node_modules").is_dir(),
        "{} is not a bro-cli checkout with pool/node_modules",
        bro.display()
    );
    let bun = crate::find_bun(args.bun.clone())?;
    let pooltime = match &args.pooltime {
        Some(p) => p
            .canonicalize()
            .with_context(|| format!("pooltime binary {}", p.display()))?,
        None => build_release(&root)?,
    };
    ensure!(
        FORBIDDEN_PORTS.contains(&3456),
        "the live bro port must stay forbidden"
    );
    let tools = Tools {
        bun: bun.clone(),
        bro: bro.clone(),
        pooltime: pooltime.clone(),
        real_home: std::env::var_os("HOME").map(PathBuf::from),
    };
    let kinds: Vec<Kind> = match args.only.as_deref() {
        None => vec![Kind::Pooltime, Kind::Bro],
        Some("bro") => vec![Kind::Bro],
        Some("pooltime") => vec![Kind::Pooltime],
        Some(o) => bail!("--only must be bro or pooltime, not {o:?}"),
    };

    let mut measured: Vec<Measured> = kinds.iter().map(|_| Measured::default()).collect();
    println!("cold starts ...");
    measure_cold(&kinds, &tools, &args, &mut measured).await?;
    for (k, m) in kinds.iter().zip(measured.iter_mut()) {
        println!("measuring {} ...", k.as_str());
        measure(*k, &tools, &args, m).await?;
    }
    let results: Vec<(Kind, Measured)> = kinds.iter().copied().zip(measured).collect();

    let model = command_line("/usr/sbin/sysctl", &["-n", "hw.model"]);
    let cpu = command_line("/usr/sbin/sysctl", &["-n", "machdep.cpu.brand_string"]);
    let ncpu = command_line("/usr/sbin/sysctl", &["-n", "hw.ncpu"]);
    let mem: u64 = command_line("/usr/sbin/sysctl", &["-n", "hw.memsize"])
        .parse()
        .unwrap_or(0);
    let macos = command_line("/usr/bin/sw_vers", &["-productVersion"]);
    let bun_version = command_line(&bun.display().to_string(), &["--version"]);
    let rustc = command_line("rustc", &["--version"]);
    println!("\n## Results\n");
    println!(
        "- Machine: {model}, {cpu}, {ncpu} cores, {} GiB RAM, macOS {macos}",
        mem / (1 << 30)
    );
    println!(
        "- bro: `{}` on `{}`, bun {bun_version}",
        crate::git(&bro, &["rev-parse", "--short", "HEAD"]),
        crate::git(&bro, &["rev-parse", "--abbrev-ref", "HEAD"])
    );
    println!(
        "- pooltime: `{}` ({}), binary `{}`, {rustc}",
        crate::git(&root, &["rev-parse", "--short", "HEAD"]),
        if pooltime.components().any(|c| c.as_os_str() == "release") {
            "release profile"
        } else {
            "profile: see binary path"
        },
        pooltime.display()
    );
    println!(
        "- Parameters: idle {} s, {} streams x {} s, {} timed boots (+1 warm-up), {} TTFB requests per side at concurrency {}\n",
        args.idle_secs,
        args.streams,
        args.stream_secs,
        args.cold_runs,
        args.ttfb_requests,
        args.ttfb_concurrency
    );
    let names: Vec<&str> = results.iter().map(|(k, _)| k.as_str()).collect();
    println!("| Metric | {} |", names.join(" | "));
    println!("|---|{}", "---|".repeat(names.len()));
    let labels = [
        format!("Idle RSS after {} s", args.idle_secs),
        format!("RSS under {} concurrent streams", args.streams),
        "Cold start to /health 200, first boot (median)".to_string(),
        "Cold start to /health 200, restart (median)".to_string(),
        "Added TTFB p50".to_string(),
        "Added TTFB p95".to_string(),
        "TTFB direct to fake p50 / p95".to_string(),
        "TTFB via daemon p50 / p95".to_string(),
        "RSS right after boot".to_string(),
    ];
    let rows: Vec<[String; 9]> = results.iter().map(|(_, m)| row(m, args.streams)).collect();
    for (i, label) in labels.iter().enumerate() {
        let cells: Vec<&str> = rows.iter().map(|r| r[i].as_str()).collect();
        println!("| {label} | {} |", cells.join(" | "));
    }
    for (k, m) in &results {
        ensure!(
            m.load_ok == args.streams,
            "{}: only {}/{} streams completed",
            k.as_str(),
            m.load_ok,
            args.streams
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_tree_sums_descendants_only() {
        let ps = "  1     0  100\n 10     1  2000\n 11    10  300\n 12    11  40\n 20     1  5000\ngarbage\n";
        let rows = parse_ps(ps);
        assert_eq!(rows.len(), 5);
        let t = tree(&rows, 10);
        let mut pids: Vec<u32> = t.iter().map(|(p, _)| *p).collect();
        pids.sort_unstable();
        assert_eq!(pids, vec![10, 11, 12]);
        assert_eq!(t.iter().map(|(_, r)| r).sum::<u64>(), 2340);
        assert!(tree(&rows, 99).is_empty());
    }

    #[test]
    fn nearest_rank_percentiles() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 0.5), 50.0);
        assert_eq!(percentile(&v, 0.95), 95.0);
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 1.0), 100.0);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert!(percentile(&[], 0.5).is_nan());
    }
}
