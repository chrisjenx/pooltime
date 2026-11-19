//! Which project a request works on (usage attribution, "which repos eat quota").
//!
//! Source: Claude Code's system prompt carries an environment block naming the working
//! directory (`Primary working directory: /path` in current versions, `Working directory: /path`
//! inside `<env>` in older ones). [`working_dir`] finds it in the already-parsed request body:
//! the `system` text first, then the first message's text (some clients put it there). Nothing
//! is parsed twice: the inference handler hands over the `Value` it parsed anyway. Lines longer
//! than [`MAX_CWD`] bytes are ignored.
//!
//! Label: the directory is first normalised lexically ([`normalize`]: `.` and `..` resolved, so
//! `/home/user/x/..` cannot dodge the home guard below). The project is the git repository it
//! belongs to ([`project_root`] walks up at most [`MAX_DEPTH`] levels to the nearest `.git`; a
//! linked worktree's `.git` file points back at its main repository, so every worktree of a repo
//! counts as that repo), else the directory itself with a `.claude/worktrees/<name>` or
//! `.worktrees/<name>` tail cut off. The stored label is the root's last path component only
//! ([`Project::name`]); [`Project::path_hash`] (16 hex chars of SHA-256 over the root path) keeps
//! two same-named directories apart. `/` itself has no project (it counts as `unknown`). The
//! full path is never stored or logged.
//!
//! Keying: each request records the hash of the directory it named ([`cwd_hash`]), and the
//! report maps directories to projects, so a client that sends one `metadata.user_id` from many
//! directories is still split correctly. A request that names no directory takes its session's
//! (see [`crate::analytics::projects`]).
//!
//! Cost: the request path only scans the body and hashes one line. Mapping a directory to its
//! project touches the filesystem, so [`ProjectCache::observe`] does it once per directory, on
//! a blocking thread, off the request path and bounded by [`PROBE_TIMEOUT`] (a stalled network
//! mount falls back to the path-only answer, which the lookup corrects if it still finishes; a
//! path-only label never replaces a looked-up one), then writes the `cwd_projects` row. A
//! directory counts as known only once its row is written, so a failed write is retried by the
//! next request naming it. At most [`MAX_PROBES`] lookups run at once (a slot is freed when its
//! lookup ends, also on a panic). The request never waits for any of it.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::logging::hash_session;
use crate::store::Store;

/// The label of requests whose project is not known.
pub const UNKNOWN: &str = "unknown";
/// Longest working-directory line accepted (bytes).
pub const MAX_CWD: usize = 4096;
/// Most directories [`project_root`] looks at, from the working directory up.
pub const MAX_DEPTH: usize = 64;
/// How long a filesystem lookup may take before the path-only answer is used.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Filesystem lookups running at once; beyond that the path-only answer is used.
pub const MAX_PROBES: usize = 4;
/// Longest stored label (characters).
const MAX_NAME: usize = 64;
/// Directories remembered by [`ProjectCache`] before it starts over.
const CACHE_CAP: usize = 4096;
/// A linked worktree's `.git` file is one short line; anything larger is not one.
const MAX_DOTGIT_FILE: u64 = 4096;

/// A project: its name and the hash of its root path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub path_hash: String,
}

/// What a directory's `.git` entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DotGit {
    /// A repository's own `.git` directory.
    Dir,
    /// A `.git` file (linked worktree or submodule) with its contents.
    File(String),
}

/// The working directory named in a Messages request body, if any (absolute paths of at most
/// [`MAX_CWD`] bytes only).
pub fn working_dir(body: &Value) -> Option<&str> {
    texts(&body["system"])
        .find_map(cwd_in)
        .or_else(|| texts(&body["messages"][0]["content"]).find_map(cwd_in))
}

/// The text of a `system` / `content` field: a string, or the `text` of its blocks.
fn texts(v: &Value) -> Box<dyn Iterator<Item = &str> + '_> {
    match v {
        Value::String(s) => Box::new(std::iter::once(s.as_str())),
        Value::Array(blocks) => Box::new(blocks.iter().filter_map(|b| b["text"].as_str())),
        _ => Box::new(std::iter::empty()),
    }
}

/// The path after the first working-directory marker in `text`.
fn cwd_in(text: &str) -> Option<&str> {
    ["Primary working directory:", "Working directory:"]
        .iter()
        .find_map(|marker| {
            let at = text.find(marker)? + marker.len();
            let rest = &text[at..];
            // Look no further than a line may be long.
            let window = &rest.as_bytes()[..rest.len().min(MAX_CWD + 2)];
            let end = window
                .iter()
                .position(|b| *b == b'\n' || *b == b'\r')
                .unwrap_or(rest.len());
            let line = rest.get(..end)?.trim();
            (line.starts_with('/') && line.len() <= MAX_CWD).then_some(line)
        })
}

/// `cwd` with `.` and `..` resolved lexically (no filesystem access; `..` at `/` stays at `/`)
/// and trailing slashes dropped. `None` unless it is absolute.
pub fn normalize(cwd: &str) -> Option<PathBuf> {
    let p = Path::new(cwd);
    if !p.is_absolute() {
        return None;
    }
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    Some(out)
}

/// The hash a request records for working directory `cwd` (of its normalised form); `None` when
/// it is not an absolute path.
pub fn cwd_hash(cwd: &str) -> Option<String> {
    normalize(cwd).map(|p| hash_session(&p.to_string_lossy()))
}

/// The project root for working directory `cwd`: the nearest enclosing git repository (its main
/// repository for a linked worktree) within [`MAX_DEPTH`] levels, never `home` itself unless
/// `cwd` is `home` (a dotfiles repository in the home directory would claim everything below
/// it); without one, `cwd` minus a `.claude/worktrees/<name>` or `.worktrees/<name>` tail. Both
/// paths are normalised first. `probe` reports a directory's `.git` entry ([`fs_probe`] in
/// production).
pub fn project_root(
    cwd: &str,
    home: Option<&Path>,
    probe: impl Fn(&Path) -> Option<DotGit>,
) -> PathBuf {
    let Some(cwd) = normalize(cwd) else {
        return PathBuf::from("/");
    };
    let home = home.and_then(|h| normalize(&h.to_string_lossy()));
    for dir in cwd.ancestors().take(MAX_DEPTH) {
        if dir == Path::new("/") {
            break;
        }
        if home.as_deref().is_some_and(|h| dir == h) && dir != cwd {
            break;
        }
        match probe(dir) {
            Some(DotGit::Dir) => return dir.to_path_buf(),
            Some(DotGit::File(contents)) => return main_repo(dir, &contents),
            None => {}
        }
    }
    strip_worktree_tail(&cwd)
}

/// For a `.git` file `gitdir: <main>/.git/worktrees/<name>`, `<main>`; otherwise (a submodule,
/// anything unexpected) the directory holding the file.
fn main_repo(dir: &Path, contents: &str) -> PathBuf {
    contents
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .and_then(|g| g.find("/.git/worktrees/").map(|i| &g[..i]))
        .and_then(normalize)
        .filter(|p| p != Path::new("/"))
        .unwrap_or_else(|| dir.to_path_buf())
}

fn strip_worktree_tail(cwd: &Path) -> PathBuf {
    let s = cwd.to_string_lossy();
    for marker in ["/.claude/worktrees/", "/.worktrees/"] {
        if let Some(i) = s.find(marker) {
            if i > 0 {
                return PathBuf::from(&s[..i]);
            }
        }
    }
    cwd.to_path_buf()
}

/// The `.git` entry of `dir` on this machine.
pub fn fs_probe(dir: &Path) -> Option<DotGit> {
    let p = dir.join(".git");
    let meta = std::fs::symlink_metadata(&p).ok()?;
    if meta.is_dir() {
        Some(DotGit::Dir)
    } else if meta.is_file() && meta.len() <= MAX_DOTGIT_FILE {
        std::fs::read_to_string(&p).ok().map(DotGit::File)
    } else {
        None
    }
}

/// The stored label for project root `root`: its last component, control characters dropped and
/// at most [`MAX_NAME`] characters; `None` when nothing is left (e.g. `/`).
pub fn label(root: &Path) -> Option<String> {
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    (!name.trim().is_empty()).then_some(name)
}

/// The project of working directory `cwd`; `None` for `/` and anything without a name (those
/// count as `unknown`).
pub fn project_of(
    cwd: &str,
    home: Option<&Path>,
    probe: impl Fn(&Path) -> Option<DotGit>,
) -> Option<Project> {
    let root = project_root(cwd, home, probe);
    Some(Project {
        name: label(&root)?,
        path_hash: hash_session(&root.to_string_lossy()),
    })
}

/// Where a working directory stands in [`ProjectCache`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// Its lookup is running; no row written yet.
    Pending,
    /// A row was written (or it has no project, like `/`).
    Done,
}

type Known = Arc<Mutex<HashMap<String, Seen>>>;

fn lock(known: &Known) -> std::sync::MutexGuard<'_, HashMap<String, Seen>> {
    known.lock().unwrap_or_else(|e| e.into_inner())
}

/// One running filesystem lookup: holds a slot of the [`MAX_PROBES`] and frees it when dropped,
/// also when the lookup panics.
struct ProbeSlot(Arc<AtomicUsize>);

impl ProbeSlot {
    /// A slot, or `None` when [`MAX_PROBES`] lookups are already running.
    fn take(probes: &Arc<AtomicUsize>) -> Option<ProbeSlot> {
        // Check and increment in one atomic step (compare-and-swap loop).
        let mut n = probes.load(Ordering::SeqCst);
        while n < MAX_PROBES {
            match probes.compare_exchange_weak(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(ProbeSlot(probes.clone())),
                Err(current) => n = current,
            }
        }
        None
    }
}

impl Drop for ProbeSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Working directories already mapped (or being mapped) to a project, by hash (bounded; cleared
/// when full). A directory counts as done only once its row was written; a failed write is
/// forgotten, so the next request naming it tries again.
#[derive(Debug, Default)]
pub struct ProjectCache {
    known: Known,
    probes: Arc<AtomicUsize>,
}

/// Write `project` for `key` (`resolved`: from a filesystem lookup) and mark the directory done,
/// or forget it when the write fails. `None` (no project) is done at once.
async fn record(
    store: &Arc<Store>,
    known: &Known,
    key: &str,
    ts: i64,
    project: Option<Project>,
    resolved: bool,
) {
    let ok = match project {
        None => true,
        Some(p) => {
            let k = key.to_string();
            let res = store
                .exec(move |c| Ok(crate::store::projects::upsert(c, &k, &p, ts, resolved)?))
                .await;
            if let Err(e) = &res {
                tracing::debug!(error = %format!("{e:#}"), "could not record a project label");
            }
            res.is_ok()
        }
    };
    let mut k = lock(known);
    if ok {
        k.insert(key.to_string(), Seen::Done);
    } else if k.get(key) == Some(&Seen::Pending) {
        k.remove(key);
    }
}

impl ProjectCache {
    /// Note that a request named working directory `cwd` at `ts` (unix seconds) and return the
    /// hash to record on it (`None`: not an absolute path). The first time a directory is seen
    /// its project is resolved in the background and written to `cwd_projects`; this call never
    /// blocks on the filesystem.
    pub fn observe(&self, store: &Arc<Store>, cwd: &str, ts: i64) -> Option<String> {
        self.observe_with(store, cwd, ts, fs_probe)
    }

    /// [`ProjectCache::observe`] with the filesystem lookup `probe`.
    fn observe_with(
        &self,
        store: &Arc<Store>,
        cwd: &str,
        ts: i64,
        probe: fn(&Path) -> Option<DotGit>,
    ) -> Option<String> {
        let path = normalize(cwd)?;
        let hash = hash_session(&path.to_string_lossy());
        {
            let mut known = lock(&self.known);
            if known.contains_key(&hash) {
                return Some(hash);
            }
            if known.len() >= CACHE_CAP {
                known.clear();
            }
            known.insert(hash.clone(), Seen::Pending);
        }
        let path = path.to_string_lossy().into_owned();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let (lookup_path, lookup_home) = (path.clone(), home.clone());
        let text_only = move || project_of(&path, home.as_deref(), |_| None);
        let (store, known, key) = (store.clone(), self.known.clone(), hash.clone());
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            // No runtime (not the daemon): the path alone, written in order on the store writer.
            let project = text_only();
            let queued = match project {
                None => Ok(()),
                Some(p) => store.submit(move |c| {
                    crate::store::projects::upsert(c, &key, &p, ts, false)?;
                    Ok(())
                }),
            };
            let mut k = lock(&known);
            if queued.is_ok() {
                k.insert(hash.clone(), Seen::Done);
            } else {
                k.remove(&hash);
            }
            return Some(hash);
        };
        let slot = ProbeSlot::take(&self.probes);
        rt.spawn(async move {
            let Some(slot) = slot else {
                // Too many lookups stuck: the path alone, no filesystem.
                record(&store, &known, &key, ts, text_only(), false).await;
                return;
            };
            let mut job = tokio::task::spawn_blocking(move || {
                let _slot = slot; // freed when the lookup ends, also on a panic
                project_of(&lookup_path, lookup_home.as_deref(), probe)
            });
            match tokio::time::timeout(PROBE_TIMEOUT, &mut job).await {
                Ok(Ok(p)) => record(&store, &known, &key, ts, p, true).await,
                Ok(Err(_)) => record(&store, &known, &key, ts, text_only(), false).await,
                Err(_) => {
                    // Slow filesystem: label by the path alone now, and correct it if the
                    // lookup still finishes.
                    record(&store, &known, &key, ts, text_only(), false).await;
                    if let Ok(p) = job.await {
                        record(&store, &known, &key, ts, p, true).await;
                    }
                }
            }
        });
        Some(hash)
    }

    #[cfg(test)]
    fn seen(&self, hash: &str) -> Option<Seen> {
        lock(&self.known).get(hash).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_git(_: &Path) -> Option<DotGit> {
        None
    }

    /// A fake filesystem: `dirs` hold a `.git` directory, `files` a `.git` file.
    fn fake<'a>(
        dirs: &'a [&str],
        files: &'a [(&str, &str)],
    ) -> impl Fn(&Path) -> Option<DotGit> + 'a {
        move |p: &Path| {
            let p = p.to_str().unwrap();
            if dirs.contains(&p) {
                Some(DotGit::Dir)
            } else {
                files
                    .iter()
                    .find(|(d, _)| *d == p)
                    .map(|(_, c)| DotGit::File(c.to_string()))
            }
        }
    }

    #[test]
    fn finds_the_current_claude_code_environment_block() {
        let body = json!({
            "system": [
                {"type": "text", "text": "You are Claude Code."},
                {"type": "text", "text": "# Environment\nYou have been invoked in the following environment: \n - Primary working directory: /home/user/src/acme\n - Is a git repository: true\n - Platform: linux"}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert_eq!(working_dir(&body), Some("/home/user/src/acme"));
    }

    #[test]
    fn finds_the_older_env_block_and_a_string_system() {
        let body = json!({
            "system": "Here is useful information about the environment you are running in:\n<env>\nWorking directory: /home/user/src/widgets\nIs directory a git repo: Yes\n</env>",
            "messages": []
        });
        assert_eq!(working_dir(&body), Some("/home/user/src/widgets"));
    }

    #[test]
    fn falls_back_to_the_first_message() {
        let body = json!({
            "system": "short",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "<system-reminder>\nWorking directory: /home/user/src/acme\n</system-reminder>"},
            ]}]
        });
        assert_eq!(working_dir(&body), Some("/home/user/src/acme"));
    }

    #[test]
    fn absent_relative_or_oversized_working_dir_is_none() {
        assert_eq!(working_dir(&json!({})), None);
        assert_eq!(
            working_dir(&json!({"system": "no environment here", "messages": [{"content": 3}]})),
            None
        );
        assert_eq!(
            working_dir(&json!({"system": "Working directory: relative/path"})),
            None
        );
        let long = format!("/{}", "a".repeat(MAX_CWD));
        assert_eq!(
            working_dir(&json!({ "system": format!("Working directory: {long}\nmore") })),
            None
        );
        let ok = format!("/{}", "a".repeat(MAX_CWD - 1));
        assert_eq!(
            working_dir(&json!({ "system": format!("Working directory: {ok}") })),
            Some(ok.as_str())
        );
    }

    #[test]
    fn normalizes_dot_segments_lexically() {
        assert_eq!(
            normalize("/home/user/src/acme/../widgets/./x/"),
            Some(PathBuf::from("/home/user/src/widgets/x"))
        );
        assert_eq!(normalize("/../.."), Some(PathBuf::from("/")));
        assert_eq!(normalize("relative"), None);
        assert_eq!(
            cwd_hash("/home/user/src/acme/"),
            cwd_hash("/home/user/src/./acme")
        );
    }

    #[test]
    fn root_is_the_enclosing_git_repository() {
        let probe = fake(&["/home/user/src/acme"], &[]);
        assert_eq!(
            project_root("/home/user/src/acme/crates/core/", None, &probe),
            PathBuf::from("/home/user/src/acme")
        );
    }

    #[test]
    fn the_walk_up_is_bounded() {
        let deep = format!("/repo{}", "/d".repeat(MAX_DEPTH + 10));
        let probe = fake(&["/repo"], &[]);
        // `/repo` is more than MAX_DEPTH levels up: not found, the directory itself is the root.
        assert_eq!(project_root(&deep, None, &probe), PathBuf::from(&deep));
        let near = format!("/repo{}", "/d".repeat(MAX_DEPTH - 2));
        assert_eq!(project_root(&near, None, &probe), PathBuf::from("/repo"));
    }

    #[test]
    fn linked_worktrees_count_as_their_main_repository() {
        let probe = fake(
            &["/home/user/src/acme"],
            &[(
                "/home/user/src/acme/.claude/worktrees/agent-1",
                "gitdir: /home/user/src/acme/.git/worktrees/agent-1\n",
            )],
        );
        let root = project_root("/home/user/src/acme/.claude/worktrees/agent-1", None, probe);
        assert_eq!(root, PathBuf::from("/home/user/src/acme"));
        // A worktree elsewhere on disk still names its main repository.
        let probe = fake(
            &[],
            &[(
                "/tmp/wt/feature",
                "gitdir: /home/user/src/acme/.git/worktrees/feature",
            )],
        );
        assert_eq!(
            project_root("/tmp/wt/feature", None, probe),
            PathBuf::from("/home/user/src/acme")
        );
        // A submodule's `.git` file points into the parent's modules: the submodule is the root.
        let probe = fake(
            &[],
            &[(
                "/home/user/src/acme/vendor/lib",
                "gitdir: ../../.git/modules/lib",
            )],
        );
        assert_eq!(
            project_root("/home/user/src/acme/vendor/lib", None, probe),
            PathBuf::from("/home/user/src/acme/vendor/lib")
        );
    }

    #[test]
    fn without_git_the_directory_minus_a_worktree_tail() {
        assert_eq!(
            project_root(
                "/home/user/src/acme/.claude/worktrees/agent-7",
                None,
                no_git
            ),
            PathBuf::from("/home/user/src/acme")
        );
        assert_eq!(
            project_root("/home/user/src/acme/.worktrees/fix", None, no_git),
            PathBuf::from("/home/user/src/acme")
        );
        assert_eq!(
            project_root("/home/user/scratch", None, no_git),
            PathBuf::from("/home/user/scratch")
        );
    }

    #[test]
    fn a_home_dotfiles_repository_does_not_claim_everything() {
        let probe = fake(&["/home/user"], &[]);
        let home = Some(Path::new("/home/user"));
        assert_eq!(
            project_root("/home/user/scratch", home, &probe),
            PathBuf::from("/home/user/scratch")
        );
        assert_eq!(
            project_root("/home/user", home, &probe),
            PathBuf::from("/home/user")
        );
        // Dot segments do not get around the guard.
        assert_eq!(
            project_root("/home/user/scratch/../scratch/./", home, &probe),
            PathBuf::from("/home/user/scratch")
        );
        assert_eq!(
            project_root(
                "/home/other/../user/scratch",
                Some(Path::new("/home/user/")),
                &probe
            ),
            PathBuf::from("/home/user/scratch")
        );
    }

    #[test]
    fn labels_are_the_last_component_only() {
        let p = project_of("/home/user/src/acme", None, no_git).unwrap();
        assert_eq!(p.name, "acme");
        assert_eq!(p.path_hash, hash_session("/home/user/src/acme"));
        // Same name, different path: same label, different hash.
        let q = project_of("/home/user/work/acme", None, no_git).unwrap();
        assert_eq!(q.name, "acme");
        assert_ne!(q.path_hash, p.path_hash);
        assert_eq!(
            label(Path::new(&format!("/x/{}", "a".repeat(100))))
                .unwrap()
                .len(),
            64
        );
        assert_eq!(label(Path::new("/x/we\u{7}ird")).as_deref(), Some("weird"));
    }

    #[test]
    fn the_root_directory_has_no_project() {
        assert_eq!(label(Path::new("/")), None);
        assert_eq!(project_of("/", None, no_git), None);
        assert_eq!(project_of("/home/..", None, no_git), None);
    }

    #[tokio::test]
    async fn a_stalled_filesystem_never_delays_the_caller() {
        // A probe that hangs far longer than the timeout (a stuck network mount).
        fn stuck(_: &Path) -> Option<DotGit> {
            std::thread::sleep(PROBE_TIMEOUT + Duration::from_millis(500));
            Some(DotGit::Dir)
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let cache = ProjectCache::default();
        let started = std::time::Instant::now();
        let h = cache
            .observe_with(&store, "/mnt/slow/src/acme/.claude/worktrees/w1", 1, stuck)
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "observe blocked"
        );
        // After the timeout the path-only answer is recorded (worktree tail cut off)...
        assert_eq!(
            wait_label(&store, &h, |_| true).await.as_deref(),
            Some("acme")
        );
        assert!(
            started.elapsed() < PROBE_TIMEOUT * 2,
            "{:?}",
            started.elapsed()
        );
        // (marked done right after its write lands)
        for _ in 0..100 {
            if cache.seen(&h) == Some(Seen::Done) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(cache.seen(&h), Some(Seen::Done));
        // ...and corrected once the slow lookup finishes (this fake finds a repo right there).
        let fixed = wait_label(&store, &h, |l| l == "w1").await;
        assert_eq!(fixed.as_deref(), Some("w1"));
        assert_eq!(cache.probes.load(Ordering::SeqCst), 0, "slot freed");
    }

    /// The label stored for `h` once `ok` accepts it (about 5 s at most).
    async fn wait_label(store: &Store, h: &str, ok: impl Fn(&str) -> bool) -> Option<String> {
        for _ in 0..200 {
            store.exec(|_| Ok(())).await.unwrap();
            let got = store
                .read(crate::store::projects::projects)
                .unwrap()
                .get(h)
                .map(|(p, _)| p.clone());
            if got.as_deref().is_some_and(&ok) {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        None
    }

    #[test]
    fn probe_slots_are_capped_and_freed() {
        let probes = Arc::new(AtomicUsize::new(0));
        let slots: Vec<ProbeSlot> = (0..MAX_PROBES)
            .map(|_| ProbeSlot::take(&probes).unwrap())
            .collect();
        assert!(ProbeSlot::take(&probes).is_none());
        drop(slots);
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        // Racing threads never take more than the cap.
        let taken: Vec<Option<ProbeSlot>> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..32)
                .map(|_| s.spawn(|| ProbeSlot::take(&probes)))
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(taken.iter().flatten().count(), MAX_PROBES);
        drop(taken);
        assert_eq!(probes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_panicking_lookup_frees_its_slot_and_falls_back() {
        fn boom(_: &Path) -> Option<DotGit> {
            panic!("probe failed");
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let cache = ProjectCache::default();
        let h = cache
            .observe_with(&store, "/home/user/src/acme", 1, boom)
            .unwrap();
        assert_eq!(
            wait_label(&store, &h, |_| true).await.as_deref(),
            Some("acme")
        );
        assert_eq!(cache.probes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_failed_write_is_forgotten_and_retried() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        store
            .exec(|c| Ok(c.execute_batch("ALTER TABLE cwd_projects RENAME TO gone")?))
            .await
            .unwrap();
        let cache = ProjectCache::default();
        let cwd = "/nonexistent-pooltime-test/src/acme";
        let h = cache.observe_with(&store, cwd, 1, |_| None).unwrap();
        for _ in 0..200 {
            if cache.seen(&h).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            cache.seen(&h),
            None,
            "not marked known after a failed write"
        );
        // The table is back: the next request naming the directory labels it.
        store
            .exec(|c| Ok(c.execute_batch("ALTER TABLE gone RENAME TO cwd_projects")?))
            .await
            .unwrap();
        cache.observe_with(&store, cwd, 2, |_| None).unwrap();
        assert_eq!(
            wait_label(&store, &h, |_| true).await.as_deref(),
            Some("acme")
        );
        for _ in 0..100 {
            if cache.seen(&h) == Some(Seen::Done) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(cache.seen(&h), Some(Seen::Done));
    }

    #[tokio::test]
    async fn observe_returns_the_hash_at_once_and_labels_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let cache = ProjectCache::default();
        let cwd = "/nonexistent-pooltime-test/src/acme";
        let h = cache.observe(&store, cwd, 100).unwrap();
        assert_eq!(Some(h.clone()), cwd_hash(cwd));
        assert!(cache.seen(&h).is_some());
        // Seen again: same hash, no second lookup.
        assert_eq!(
            cache.observe(&store, "/nonexistent-pooltime-test/src/acme/", 101),
            Some(h.clone())
        );
        assert_eq!(cache.observe(&store, "relative", 1), None);
        // `/` is recorded on the request but gets no project row.
        let root = cache.observe(&store, "/", 1).unwrap();
        let mut found = None;
        for _ in 0..100 {
            store.exec(|_| Ok(())).await.unwrap();
            found = store
                .read(crate::store::projects::projects)
                .unwrap()
                .get(&h)
                .cloned();
            if found.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(found.unwrap().0, "acme");
        let all = store.read(crate::store::projects::projects).unwrap();
        assert!(!all.contains_key(&root), "{all:?}");
    }
}
