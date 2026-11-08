//! Atomic writes, guarded read-modify-write patching and advisory file locks.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

const MAX_SYMLINK_HOPS: usize = 40;

/// Follow symlinks on the final path component so writes go to the real file
/// instead of replacing the link (dotfile-managed `settings.json`). Relative
/// link targets are joined onto the link's parent directory. A dangling link
/// resolves to its (absent) target, which then gets created. Errors on a loop
/// or after more than 40 hops. Symlinked parent directories need no handling:
/// the OS follows them.
fn resolve_final(path: &Path) -> io::Result<PathBuf> {
    let mut cur = path.to_path_buf();
    for _ in 0..=MAX_SYMLINK_HOPS {
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                let target = fs::read_link(&cur)?;
                cur = if target.is_absolute() {
                    target
                } else {
                    parent_of(&cur).join(target)
                };
            }
            Ok(_) => return Ok(cur),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(cur),
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(format!(
        "too many levels of symbolic links resolving {}",
        path.display()
    )))
}

fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// How hard a write is flushed before it counts as done.
#[derive(Clone, Copy)]
enum Durability {
    /// `File::sync_all` (on macOS `F_FULLFSYNC`: the drive's cache is flushed too).
    Full,
    /// Plain `fsync(2)`: in the OS's hands, but on macOS the drive may still hold it in cache.
    Relaxed,
}

fn sync(file: &File, d: Durability) -> io::Result<()> {
    match d {
        Durability::Full => file.sync_all(),
        Durability::Relaxed => {
            use std::os::fd::AsRawFd;
            // SAFETY: `fsync` only reads the descriptor, which `file` keeps open for the call.
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
}

/// Write `bytes` to a temp file in the same directory (created 0600, then set
/// to `mode`), sync it, and return it ready to `persist`.
fn staged_temp(path: &Path, bytes: &[u8], mode: u32, d: Durability) -> io::Result<NamedTempFile> {
    let mut tmp = NamedTempFile::new_in(parent_of(path))?;
    tmp.write_all(bytes)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    sync(tmp.as_file(), d)?;
    Ok(tmp)
}

fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn write_atomically(path: &Path, bytes: &[u8], mode: u32, d: Durability) -> io::Result<()> {
    let path = resolve_final(path)?;
    let path = path.as_path();
    let tmp = staged_temp(path, bytes, mode, d)?;
    tmp.persist(path).map_err(|e| e.error)?;
    sync(&File::open(parent_of(path))?, d)
}

/// Atomically replace `path` with `bytes`: temp file in the same directory,
/// fsync, rename, fsync of the parent directory. The result has exactly `mode`
/// (permission bits, regardless of umask). Readers see the old or the new
/// content, never a partial file; the temp file is removed on failure. If
/// `path` is a symlink the file it points to is replaced and the link is kept.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    write_atomically(path, bytes, mode, Durability::Full)
}

/// [`atomic_write`] with plain `fsync(2)` instead of `sync_all` for the file and its directory.
/// Still atomic (temp + rename: readers see the old or the new content, never a partial file)
/// and still exactly `mode`, but on macOS a power loss right after the write may lose it, where
/// `sync_all` (`F_FULLFSYNC`, ~10 ms per write there) would not.
///
/// **Only for regenerable files** whose loss costs nothing but a regeneration — today only
/// `run/mgmt.key` (a lost key just changes the dashboard URL/cookie; `docs/perf.md` "First boot").
/// Credentials, the database, config, snapshots and Claude Code files use [`atomic_write`].
pub fn atomic_write_relaxed(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    write_atomically(path, bytes, mode, Durability::Relaxed)
}

/// What a [`guarded_patch`] closure wants done with the file.
pub enum Patch {
    /// Leave the file alone (not even its mtime is touched).
    Unchanged,
    /// Replace the file with these complete bytes.
    Write(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The closure asked for no change, or the new bytes equal the current ones.
    Unchanged,
    Written,
    /// The file kept changing under us for every attempt; nothing was written.
    GaveUp,
}

/// Identity of a file version: inode/device, size and nanosecond mtime.
/// Comparing all of them makes a same-instant rewrite very unlikely to look
/// like "unchanged".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
}

impl Stamp {
    fn of(m: &fs::Metadata) -> Stamp {
        Stamp {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime: (m.mtime(), m.mtime_nsec()),
        }
    }
}

/// Read the file and its stamp from one handle. The stamp is taken before the
/// read, so a concurrent write can only make us retry, never miss a change.
/// `None` when the file is absent.
fn read_with_stamp(path: &Path) -> io::Result<Option<(Vec<u8>, Stamp, u32)>> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = f.metadata()?;
    let mut buf = Vec::with_capacity(meta.len() as usize);
    f.read_to_end(&mut buf)?;
    Ok(Some((buf, Stamp::of(&meta), meta.mode() & 0o7777)))
}

fn current_stamp(path: &Path) -> io::Result<Option<Stamp>> {
    match fs::metadata(path) {
        Ok(m) => Ok(Some(Stamp::of(&m))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read-modify-write for files another process (Claude Code) rewrites
/// concurrently. Reads the file and its stamp, calls `f` (with `None` when the
/// file is absent), and on `Patch::Write` stages the full bytes in a temp file
/// and fsyncs it. Immediately before the rename the target is re-stat'ed; if it
/// changed since the read, the temp file is discarded and the whole cycle
/// (including `f`) is retried, up to `retries` extra times (so at most
/// `retries + 1` calls to `f`), then `GaveUp`. An existing file keeps its mode;
/// a new one is created with `mode`.
///
/// A write landing in the few microseconds between the re-stat and the rename
/// cannot be detected without a lock Claude Code does not take; that window is
/// inherent to the approach. Symlinks are followed (see [`resolve_final`]) so
/// the link itself is never replaced.
pub fn guarded_patch(
    path: &Path,
    mode: u32,
    retries: u8,
    mut f: impl FnMut(Option<&[u8]>) -> anyhow::Result<Patch>,
) -> anyhow::Result<WriteOutcome> {
    for _ in 0..=retries {
        let path = resolve_final(path)?;
        let path = path.as_path();
        let read = read_with_stamp(path)?;
        let (current, stamp, file_mode) = match &read {
            Some((bytes, stamp, m)) => (Some(bytes.as_slice()), Some(*stamp), *m),
            None => (None, None, mode),
        };
        let new = match f(current)? {
            Patch::Unchanged => return Ok(WriteOutcome::Unchanged),
            Patch::Write(b) => b,
        };
        if current == Some(new.as_slice()) {
            return Ok(WriteOutcome::Unchanged);
        }
        let tmp = staged_temp(path, &new, file_mode, Durability::Full)?;
        if current_stamp(path)? != stamp {
            continue; // `tmp` is dropped (deleted) here.
        }
        tmp.persist(path).map_err(|e| e.error)?;
        fsync_dir(parent_of(path))?;
        return Ok(WriteOutcome::Written);
    }
    Ok(WriteOutcome::GaveUp)
}

/// Advisory exclusive `flock` held for the lifetime of the value. The lock file
/// is created 0600 and never deleted (deleting it would let two holders lock
/// different inodes). The lock is released on drop or process exit.
#[derive(Debug)]
pub struct FileLock(#[allow(dead_code)] File);

impl FileLock {
    fn open(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
    }

    /// Block until the lock is held.
    pub fn acquire(path: &Path) -> io::Result<FileLock> {
        let f = Self::open(path)?;
        f.lock()?;
        Ok(FileLock(f))
    }

    /// `Ok(None)` if another open file description (process or handle) holds it.
    pub fn try_acquire(path: &Path) -> io::Result<Option<FileLock>> {
        let f = Self::open(path)?;
        match f.try_lock() {
            Ok(()) => Ok(Some(FileLock(f))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}

/// Set the permission bits to `mode` if they differ.
pub fn ensure_mode(path: &Path, mode: u32) -> io::Result<()> {
    let cur = fs::metadata(path)?.permissions().mode() & 0o7777;
    if cur != mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn dir_names(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn atomic_write_creates_with_mode_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.json");
        atomic_write(&p, b"one", 0o600).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"one");
        assert_eq!(mode_of(&p), 0o600);
        // Replace, with a different mode requested.
        atomic_write(&p, b"two-longer", 0o640).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two-longer");
        assert_eq!(mode_of(&p), 0o640);
        // No temp leftovers.
        assert_eq!(dir_names(dir.path()), vec!["f.json"]);
    }

    #[test]
    fn atomic_write_relaxed_is_atomic_with_mode_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mgmt.key");
        atomic_write_relaxed(&p, b"one\n", 0o600).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"one\n");
        assert_eq!(mode_of(&p), 0o600);
        // Replacing goes through a rename: a reader holding the old file keeps the old bytes.
        let old = File::open(&p).unwrap();
        atomic_write_relaxed(&p, b"two-longer\n", 0o600).unwrap();
        let mut held = String::new();
        (&old).read_to_string(&mut held).unwrap();
        assert_eq!(held, "one\n");
        assert_eq!(fs::read(&p).unwrap(), b"two-longer\n");
        assert_eq!(mode_of(&p), 0o600);
        assert_eq!(dir_names(dir.path()), vec!["mgmt.key"]);
    }

    #[test]
    fn atomic_write_relaxed_failure_keeps_the_old_file_whole() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("run");
        fs::create_dir(&sub).unwrap();
        let p = sub.join("mgmt.key");
        atomic_write_relaxed(&p, b"old\n", 0o600).unwrap();
        // No temp file can be created in a read-only directory: the write fails, the old file
        // stays whole and nothing partial is left behind.
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o500)).unwrap();
        let r = atomic_write_relaxed(&p, b"new\n", 0o600);
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(r.is_err());
        assert_eq!(fs::read(&p).unwrap(), b"old\n");
        assert_eq!(dir_names(&sub), vec!["mgmt.key"]);
    }

    #[test]
    fn guarded_patch_unchanged_does_not_touch_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        fs::write(&p, b"{}").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let before = fs::metadata(&p).unwrap();

        let out = guarded_patch(&p, 0o600, 3, |cur| {
            assert_eq!(cur, Some(&b"{}"[..]));
            Ok(Patch::Unchanged)
        })
        .unwrap();
        assert_eq!(out, WriteOutcome::Unchanged);
        let after = fs::metadata(&p).unwrap();
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(dir_names(dir.path()), vec!["s.json"]);

        // Absent file: closure sees None; Unchanged creates nothing.
        let q = dir.path().join("absent.json");
        let out = guarded_patch(&q, 0o600, 3, |cur| {
            assert!(cur.is_none());
            Ok(Patch::Unchanged)
        })
        .unwrap();
        assert_eq!(out, WriteOutcome::Unchanged);
        assert!(!q.exists());

        // Absent + Write creates with the requested mode; existing keeps its mode.
        let out = guarded_patch(&q, 0o600, 3, |_| Ok(Patch::Write(b"new".to_vec()))).unwrap();
        assert_eq!(out, WriteOutcome::Written);
        assert_eq!(fs::read(&q).unwrap(), b"new");
        assert_eq!(mode_of(&q), 0o600);
        let out = guarded_patch(&p, 0o600, 3, |_| Ok(Patch::Write(b"{\"a\":1}".to_vec()))).unwrap();
        assert_eq!(out, WriteOutcome::Written);
        assert_eq!(fs::read(&p).unwrap(), b"{\"a\":1}");
        assert_eq!(mode_of(&p), 0o644);
    }

    #[test]
    fn guarded_write_retries_when_file_changes_between_read_and_rename() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(&p, b"a=1\n").unwrap();
        let mut calls = 0;
        let out = guarded_patch(&p, 0o600, 3, |cur| {
            calls += 1;
            if calls == 1 {
                // Simulate Claude Code writing between our read and our rename.
                fs::write(&p, b"a=1\nb=2\n").unwrap();
            }
            let mut v = cur.unwrap().to_vec();
            v.extend_from_slice(b"patched\n");
            Ok(Patch::Write(v))
        })
        .unwrap();
        assert_eq!(out, WriteOutcome::Written);
        assert_eq!(calls, 2);
        // Patch applied to the second read; the concurrent change is preserved.
        assert_eq!(fs::read(&p).unwrap(), b"a=1\nb=2\npatched\n");
        assert_eq!(dir_names(dir.path()), vec!["settings.json"]);
    }

    #[test]
    fn guarded_patch_gives_up_after_retries() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(&p, b"x").unwrap();
        let mut calls = 0u32;
        let out = guarded_patch(&p, 0o600, 2, |_| {
            calls += 1;
            // Always changes under us (size grows, so the stamp differs).
            fs::write(&p, "x".repeat(calls as usize + 1)).unwrap();
            Ok(Patch::Write(b"mine".to_vec()))
        })
        .unwrap();
        assert_eq!(out, WriteOutcome::GaveUp);
        assert_eq!(calls, 3); // first attempt + 2 retries
                              // The concurrent writer's content wins; ours was never written.
        assert_eq!(fs::read(&p).unwrap(), "x".repeat(4).as_bytes());
        assert_eq!(dir_names(dir.path()), vec!["settings.json"]);
    }

    #[test]
    fn file_lock_excludes_second_process() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pool.lock");
        let first = FileLock::acquire(&p).unwrap();
        // flock is per open file description, so an independent handle in the
        // same process excludes exactly like a second process would.
        assert!(FileLock::try_acquire(&p).unwrap().is_none());
        drop(first);
        let again = FileLock::try_acquire(&p).unwrap();
        assert!(again.is_some());
        assert!(FileLock::try_acquire(&p).unwrap().is_none());
        assert_eq!(mode_of(&p), 0o600);
    }

    #[test]
    fn ensure_mode_sets_only_when_different() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        fs::write(&p, b"").unwrap();
        ensure_mode(&p, 0o600).unwrap();
        assert_eq!(mode_of(&p), 0o600);
        ensure_mode(&p, 0o600).unwrap();
        assert_eq!(mode_of(&p), 0o600);
    }

    #[test]
    fn atomic_write_through_absolute_symlink_keeps_link() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("dotfiles");
        let home = dir.path().join("home");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&home).unwrap();
        let target = repo.join("settings.json");
        fs::write(&target, b"old").unwrap();
        let link = home.join("settings.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        atomic_write(&link, b"new", 0o600).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), target);
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(mode_of(&target), 0o600);
        assert_eq!(dir_names(&repo), vec!["settings.json"]);
        assert_eq!(dir_names(&home), vec!["settings.json"]);
    }

    #[test]
    fn guarded_patch_through_absolute_symlink_keeps_link_and_target_mode() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("dotfiles");
        let home = dir.path().join("home");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&home).unwrap();
        let target = repo.join("settings.json");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let link = home.join("settings.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let out = guarded_patch(&link, 0o600, 3, |cur| {
            assert_eq!(cur, Some(&b"old"[..]));
            Ok(Patch::Write(b"new".to_vec()))
        })
        .unwrap();
        assert_eq!(out, WriteOutcome::Written);
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), target);
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(mode_of(&target), 0o644);
        assert_eq!(dir_names(&repo), vec!["settings.json"]);
        assert_eq!(dir_names(&home), vec!["settings.json"]);
    }

    #[test]
    fn relative_and_chained_symlinks_are_followed() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a")).unwrap();
        fs::create_dir_all(dir.path().join("b")).unwrap();
        let real = dir.path().join("a/real.json");
        fs::write(&real, b"old").unwrap();
        // b/mid -> ../a/real.json ; link -> b/mid (relative, chained)
        std::os::unix::fs::symlink("../a/real.json", dir.path().join("b/mid")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink("b/mid", &link).unwrap();
        atomic_write(&link, b"w1", 0o600).unwrap();
        assert_eq!(fs::read(&real).unwrap(), b"w1");
        guarded_patch(&link, 0o600, 3, |_| Ok(Patch::Write(b"w2".to_vec()))).unwrap();
        assert_eq!(fs::read(&real).unwrap(), b"w2");
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("b/mid"));
        assert_eq!(
            fs::read_link(dir.path().join("b/mid")).unwrap(),
            Path::new("../a/real.json")
        );
    }

    #[test]
    fn dangling_symlink_creates_target_and_keeps_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("later.json");
        let link = dir.path().join("l1");
        std::os::unix::fs::symlink("later.json", &link).unwrap();
        atomic_write(&link, b"made", 0o600).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"made");
        assert_eq!(mode_of(&target), 0o600);

        fs::remove_file(&target).unwrap();
        let out = guarded_patch(&link, 0o640, 3, |cur| {
            assert!(cur.is_none());
            Ok(Patch::Write(b"patched".to_vec()))
        })
        .unwrap();
        assert_eq!(out, WriteOutcome::Written);
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"patched");
        assert_eq!(mode_of(&target), 0o640);
    }

    #[test]
    fn symlink_loop_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::os::unix::fs::symlink("b", &a).unwrap();
        std::os::unix::fs::symlink("a", &b).unwrap();
        assert!(atomic_write(&a, b"x", 0o600).is_err());
        assert!(guarded_patch(&a, 0o600, 3, |_| Ok(Patch::Write(b"x".to_vec()))).is_err());
        assert!(fs::symlink_metadata(&a).unwrap().file_type().is_symlink());
    }
}
