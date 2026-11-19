//! Which project a request works on (usage attribution, "which repos eat quota").
//!
//! Source: Claude Code's system prompt carries an environment block naming the working
//! directory (`Primary working directory: /path` in current versions, `Working directory: /path`
//! inside `<env>` in older ones). [`working_dir`] finds it in the already-parsed request body:
//! the `system` text first, then the first message's text (some clients put it there). Nothing
//! is parsed twice: the inference handler hands over the `Value` it parsed anyway.
//!
//! Label: the project is the git repository the directory belongs to ([`project_root`] walks up
//! to the nearest `.git`; a linked worktree's `.git` file points back at its main repository, so
//! every worktree of a repo counts as that repo), else the directory itself with a
//! `.claude/worktrees/<name>` or `.worktrees/<name>` tail cut off. The stored label is the
//! root's last path component only ([`Project::name`]); [`Project::path_hash`] (16 hex chars of
//! SHA-256 over the root path) keeps two same-named directories apart. The full path is never
//! stored or logged.
//!
//! Cost: a project is looked up once per client session; [`ProjectCache`] remembers the answer
//! by session hash, so later requests of the session skip the body scan and the filesystem.
//! A session whose requests carry no working directory is labelled `unknown` by the reports.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::logging::hash_session;

/// The label of requests whose project is not known.
pub const UNKNOWN: &str = "unknown";
/// Longest stored label (characters).
const MAX_NAME: usize = 64;
/// Sessions remembered by [`ProjectCache`] before it starts over.
const CACHE_CAP: usize = 4096;
/// A linked worktree's `.git` file is one short line; anything larger is not one.
const MAX_DOTGIT_FILE: u64 = 4096;

/// A project as stored per session: its name and the hash of its root path.
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

/// The working directory named in a Messages request body, if any (absolute paths only).
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
            let line = rest.split(['\n', '\r']).next().unwrap_or_default().trim();
            line.starts_with('/').then_some(line)
        })
}

/// The project root for working directory `cwd`: the nearest enclosing git repository (its main
/// repository for a linked worktree), never `home` itself unless `cwd` is `home` (a dotfiles
/// repository in the home directory would claim everything below it); without one, `cwd` minus
/// a `.claude/worktrees/<name>` or `.worktrees/<name>` tail. `probe` reports a directory's `.git`
/// entry ([`fs_probe`] in production).
pub fn project_root(
    cwd: &str,
    home: Option<&Path>,
    probe: impl Fn(&Path) -> Option<DotGit>,
) -> PathBuf {
    let cwd = Path::new(cwd.trim_end_matches('/'));
    for dir in cwd.ancestors() {
        if dir.as_os_str().is_empty() || dir == Path::new("/") {
            break;
        }
        if home.is_some_and(|h| dir == h) && dir != cwd {
            break;
        }
        match probe(dir) {
            Some(DotGit::Dir) => return dir.to_path_buf(),
            Some(DotGit::File(contents)) => return main_repo(dir, &contents),
            None => {}
        }
    }
    strip_worktree_tail(cwd)
}

/// For a `.git` file `gitdir: <main>/.git/worktrees/<name>`, `<main>`; otherwise (a submodule,
/// anything unexpected) the directory holding the file.
fn main_repo(dir: &Path, contents: &str) -> PathBuf {
    contents
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .and_then(|g| g.find("/.git/worktrees/").map(|i| PathBuf::from(&g[..i])))
        .filter(|p| p.is_absolute())
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
/// at most [`MAX_NAME`] characters; [`UNKNOWN`] when nothing is left.
pub fn label(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    if name.trim().is_empty() {
        UNKNOWN.to_string()
    } else {
        name
    }
}

/// The project of working directory `cwd`.
pub fn project_of(
    cwd: &str,
    home: Option<&Path>,
    probe: impl Fn(&Path) -> Option<DotGit>,
) -> Project {
    let root = project_root(cwd, home, probe);
    Project {
        name: label(&root),
        path_hash: hash_session(&root.to_string_lossy()),
    }
}

/// Projects already found, by session hash (bounded; cleared when full).
#[derive(Debug, Default)]
pub struct ProjectCache {
    map: Mutex<HashMap<String, Project>>,
}

impl ProjectCache {
    /// The project of session `session_hash`, from the cache or else from `body` (which then
    /// fills the cache). `None` when the body names no working directory; that is not cached, so
    /// a later request of the session that carries one still labels it.
    pub fn resolve(&self, session_hash: &str, body: &Value) -> Option<Project> {
        if let Some(p) = self.lock().get(session_hash) {
            return Some(p.clone());
        }
        let cwd = working_dir(body)?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let project = project_of(cwd, home.as_deref(), fs_probe);
        tracing::debug!(project = %project.name, "session project found");
        let mut map = self.lock();
        if map.len() >= CACHE_CAP {
            map.clear();
        }
        map.insert(session_hash.to_string(), project.clone());
        Some(project)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Project>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
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
    fn absent_or_relative_working_dir_is_none() {
        assert_eq!(working_dir(&json!({})), None);
        assert_eq!(
            working_dir(&json!({"system": "no environment here", "messages": [{"content": 3}]})),
            None
        );
        assert_eq!(
            working_dir(&json!({"system": "Working directory: relative/path"})),
            None
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
    }

    #[test]
    fn labels_are_the_last_component_only() {
        let p = project_of("/home/user/src/acme", None, no_git);
        assert_eq!(p.name, "acme");
        assert_eq!(p.path_hash, hash_session("/home/user/src/acme"));
        // Same name, different path: same label, different hash.
        let q = project_of("/home/user/work/acme", None, no_git);
        assert_eq!(q.name, "acme");
        assert_ne!(q.path_hash, p.path_hash);
        assert_eq!(label(Path::new("/")), UNKNOWN);
        assert_eq!(
            label(Path::new(&format!("/x/{}", "a".repeat(100)))).len(),
            64
        );
        assert_eq!(label(Path::new("/x/we\u{7}ird")), "weird");
    }

    #[test]
    fn cache_remembers_found_projects_only() {
        let cache = ProjectCache::default();
        let with = json!({"system": "Working directory: /nonexistent-pooltime-test/src/acme"});
        let without = json!({"system": "nothing"});
        assert_eq!(cache.resolve("s1", &without), None);
        let p = cache.resolve("s1", &with).unwrap();
        assert_eq!(p.name, "acme");
        // Cached: a later body without the block still resolves.
        assert_eq!(cache.resolve("s1", &without), Some(p));
        assert_eq!(cache.resolve("s2", &without), None);
    }
}
