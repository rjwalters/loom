//! The hook scripts of the RUNNING release, on disk, for the sessions this
//! daemon spawns (2am#4501).
//!
//! # The problem
//!
//! `scripts/install/provision-hooks.sh` wires Loom's guard hooks user-scope:
//! every entry runs `${LOOM_HOME:-~/.local/share/loom}/defaults/hooks/<name>`,
//! a git checkout. A host that updates `loom-daemon` from release artifacts
//! never moves that checkout (the artifact path deliberately leaves it alone,
//! see `daemon_update/sync.rs`), so the guards, `roll-pause.sh` and
//! `gh-front-env.sh` ran whatever commit the checkout was last left at, days
//! or weeks behind the daemon that launched the session. Nothing but a manual
//! step could close that gap.
//!
//! # The fix
//!
//! The binary already carries its own `defaults/` tree (`build.rs` packs it,
//! [`crate::init::payload`] embeds it). This module unpacks the parts a hook
//! needs, once per distinct payload, into
//! `~/.local/share/loom-daemon/hooks-home/<version>-<payload sha256[..12]>/`,
//! and every session the daemon spawns gets that directory as
//! [`ENV`] (`LOOM_HOOKS_HOME`). The wired command prefers
//! `$LOOM_HOOKS_HOME/defaults/hooks/<name>` and falls back to the checkout
//! when it is unset or the file is missing, so hook code tracks the binary by
//! construction and nothing regresses where the variable is absent (an
//! attended session, a container that cannot see the host path).
//!
//! The directory is keyed by the payload's own hash, so two binaries with the
//! same files share one directory and a rebuilt binary with different files
//! never reuses a stale one. It is written to a staging sibling and renamed
//! into place with a completion marker, so a reader never sees a half-written
//! tree. Older directories are pruned only once they are both outside the
//! most recent [`KEEP_RECENT`] and older than [`RETAIN`] since last use (the
//! spawn path refreshes the current copy's mtime): a session spawned by the
//! previous daemon still points at its own copy.
//!
//! # Permissions
//!
//! These are security guards, so nobody else may write them. The base is
//! `0700`, the staged tree loses every group/other write bit before the
//! rename whatever the umask, and an existing copy is reused only when it is
//! not a symlink and everything in it is this user's with no group/other
//! write bit ([`is_trusted`]). Otherwise it is unpacked again.
//!
//! # Side effects worth knowing
//!
//! Hooks that log beside their own tree (`guard-loom-workflow.sh` and kin
//! take the "parent is `defaults`" branch) write `hook-errors.log` and the
//! decision logs to `<hooks-home>/<key>/.loom/logs`, which moves with each
//! release and is pruned with its copy. `guard-background-subagents.sh`
//! resolves its root with `git -C` from its own directory, which is `/` here:
//! it stays enabled (stricter), never fail-open.
//!
//! # What is unpacked
//!
//! `defaults/` minus every `tests/` directory: the hooks source
//! `../scripts/lib/*`, `guard-loom-workflow.sh` reaches `../scripts/` and
//! `../roles`, and the tests are most of the bytes and none of the runtime.
//!
//! # Overrides
//!
//! `LOOM_HOOKS_HOME` set in the daemon's own environment is passed through
//! unchanged (an operator pin, never written to). Set to the empty string it
//! disables the export, and sessions run hooks from the checkout as before.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

/// The session variable the wired hook command reads.
pub const ENV: &str = "LOOM_HOOKS_HOME";

/// Home-relative parent of the per-payload directories.
const BASE_REL: &str = ".local/share/loom-daemon/hooks-home";

/// Written last into a fully unpacked directory. A directory without it is
/// a crashed unpack and is replaced, never used.
pub const COMPLETE_MARKER: &str = ".complete";

/// Newest directories kept regardless of age (the current one is always kept).
pub const KEEP_RECENT: usize = 3;

/// Age past which a directory outside [`KEEP_RECENT`] is removed. Far longer
/// than any role session lives.
pub const RETAIN: Duration = Duration::from_secs(3 * 24 * 3600);

/// A staging directory older than this is a crashed unpack.
const STAGING_STALE: Duration = Duration::from_secs(3600);

const STAGING_PREFIX: &str = ".staging-";

/// How often the spawn path refreshes the current copy's mtime.
#[cfg(not(test))]
const TOUCH_EVERY: Duration = Duration::from_secs(600);

/// `(name, value)` for the session environment: this process's hooks home,
/// or `None` when there is none (the spawner blanks or removes the variable,
/// so a value inherited from elsewhere never leaks into a session).
#[must_use]
pub fn agent_env() -> Vec<(&'static str, Option<OsString>)> {
    vec![(ENV, current().map(PathBuf::into_os_string))]
}

/// This process's hooks home, resolved (and unpacked) once.
#[must_use]
pub fn current() -> Option<PathBuf> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        static CURRENT: std::sync::OnceLock<Option<(PathBuf, bool)>> = std::sync::OnceLock::new();
        let resolved = CURRENT
            .get_or_init(|| {
                let pinned = std::env::var(ENV).ok();
                let managed = pinned.is_none();
                resolve(
                    pinned,
                    dirs::home_dir(),
                    crate::init::payload::embedded_archive(),
                    env!("CARGO_PKG_VERSION"),
                )
                .map(|dir| (dir, managed))
            })
            .clone();
        if let Some((dir, true)) = &resolved {
            touch_on_use(dir);
        }
        resolved.map(|(dir, _)| dir)
    }
}

/// [`touch`] the managed copy at most every [`TOUCH_EVERY`], from the spawn
/// path: a daemon that keeps spawning sessions keeps its copy young.
#[cfg(not(test))]
fn touch_on_use(dir: &Path) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let last = LAST.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= TOUCH_EVERY.as_secs()
        && LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        touch(dir);
    }
}

/// The hooks home for an `env` value (`LOOM_HOOKS_HOME` as this process sees
/// it) and a home directory: the pin when set, nothing when set empty, else
/// `archive` unpacked under `home`. An unpack failure is logged and yields
/// `None`, which leaves sessions on the checkout exactly as before.
#[must_use]
pub fn resolve(
    env: Option<String>,
    home: Option<PathBuf>,
    archive: &[u8],
    version: &str,
) -> Option<PathBuf> {
    match env {
        Some(pin) if pin.is_empty() => return None,
        Some(pin) => return Some(PathBuf::from(pin)),
        None => {}
    }
    let base = home?.join(BASE_REL);
    match materialize(&base, archive, version) {
        Ok(dir) => Some(dir),
        Err(e) => {
            log::warn!(
                "hooks-home: could not unpack this release's hooks under {}: {e:#}; sessions run hooks from the machine checkout",
                base.display()
            );
            None
        }
    }
}

/// The directory name for `archive` at `version`.
#[must_use]
pub fn key(archive: &[u8], version: &str) -> String {
    let digest = hex::encode(Sha256::digest(archive));
    format!("{version}-{}", &digest[..12])
}

/// Unpack `archive` (a `.tar.zst` of `defaults/…`) under `base`, unless a
/// complete, trusted copy is already there, then prune older copies. Returns
/// the directory that holds `defaults/`.
///
/// The tree holds security guards, so it must not be writable by anyone but
/// this user: `base` is kept `0700`, the staged tree loses every group and
/// other write bit before it is renamed into place (whatever the umask), and
/// an existing copy is reused only if [`is_trusted`] holds. Anything else is
/// unpacked again.
///
/// # Errors
/// `base` cannot be created or is not a private directory of this user, the
/// staging directory cannot be created, the archive does not unpack, it has
/// no `defaults/hooks/`, an untrusted copy cannot be removed, or the rename
/// fails with no trusted copy in place.
pub fn materialize(base: &Path, archive: &[u8], version: &str) -> Result<PathBuf> {
    ensure_private_dir(base)?;
    let dest = base.join(key(archive, version));
    if is_complete(&dest) {
        if is_trusted(&dest) {
            touch(&dest);
            prune(base, &dest, SystemTime::now());
            return Ok(dest);
        }
        log::warn!(
            "hooks-home: {} is not owned by this user or is group/world-writable; unpacking it again",
            dest.display()
        );
    }
    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(base)
        .context("create the staging directory")?;
    unpack(archive, staging.path())?;
    anyhow::ensure!(
        staging.path().join("defaults/hooks").is_dir(),
        "the payload has no defaults/hooks/"
    );
    std::fs::write(staging.path().join(COMPLETE_MARKER), format!("{version}\n"))
        .context("write the completion marker")?;
    harden(staging.path()).context("restrict the staged tree's permissions")?;
    if dest.symlink_metadata().is_ok() {
        // A crashed unpack, or an untrusted copy, under this key.
        remove_path(&dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    let staged = staging.keep();
    if let Err(e) = std::fs::rename(&staged, &dest) {
        let _ = std::fs::remove_dir_all(&staged);
        // Another process won the race with the same bytes.
        if !(is_complete(&dest) && is_trusted(&dest)) {
            return Err(anyhow!(e)).with_context(|| format!("rename into {}", dest.display()));
        }
    }
    touch(&dest);
    prune(base, &dest, SystemTime::now());
    Ok(dest)
}

fn is_complete(dir: &Path) -> bool {
    dir.join(COMPLETE_MARKER).is_file() && dir.join("defaults/hooks").is_dir()
}

/// This process's effective uid.
fn euid() -> u32 {
    // SAFETY: geteuid only reads the caller's effective user id.
    unsafe { libc::geteuid() }
}

/// Group and other write.
const SHARED_WRITE: u32 = 0o022;

/// Create `base` (mode `0700`) if it is missing, and require it to be a real
/// directory owned by this user; tighten it to `0700` if it is looser.
fn ensure_private_dir(base: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    if let Some(parent) = base.parent() {
        // Missing ancestors are created 0700 too (an existing one is left
        // alone): a umask-default parent would be group-writable.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    if base.symlink_metadata().is_err() {
        let made = std::fs::DirBuilder::new().mode(0o700).create(base);
        if let Err(e) = made {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e).with_context(|| format!("create {}", base.display()));
            }
        }
    }
    let meta = base
        .symlink_metadata()
        .with_context(|| format!("stat {}", base.display()))?;
    anyhow::ensure!(
        meta.is_dir() && !meta.file_type().is_symlink() && meta.uid() == euid(),
        "{} is not a directory owned by this user",
        base.display()
    );
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 700 {}", base.display()))?;
    }
    Ok(())
}

/// Visit `root` and everything under it without following symlinks. Stops
/// and returns `false` as soon as `f` does, or a directory cannot be read.
fn walk(root: &Path, f: &mut dyn FnMut(&Path, &std::fs::Metadata) -> bool) -> bool {
    let Ok(meta) = root.symlink_metadata() else {
        return false;
    };
    if !f(root, &meta) {
        return false;
    }
    if meta.is_dir() && !meta.file_type().is_symlink() {
        let Ok(read) = std::fs::read_dir(root) else {
            return false;
        };
        for entry in read {
            let Ok(entry) = entry else {
                return false;
            };
            if !walk(&entry.path(), f) {
                return false;
            }
        }
    }
    true
}

/// Strip group and other write from every file and directory under `root`
/// (symlinks have no mode of their own), and make `root` itself `0700`.
fn harden(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut failed = None;
    walk(root, &mut |path, meta| {
        if meta.file_type().is_symlink() {
            return true;
        }
        let mode = meta.permissions().mode();
        let want = if path == root {
            0o700
        } else {
            mode & !SHARED_WRITE & 0o7777
        };
        if mode & 0o7777 != want {
            if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(want)) {
                failed = Some(anyhow!(e).context(format!("chmod {}", path.display())));
                return false;
            }
        }
        true
    });
    failed.map_or(Ok(()), Err)
}

/// Whether an existing copy may be used: not a symlink, and every file and
/// directory in it owned by this user with no group or other write bit. A
/// tree anyone else could have written to is never handed to a guard.
#[must_use]
pub fn is_trusted(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let uid = euid();
    match dir.symlink_metadata() {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        _ => return false,
    }
    walk(dir, &mut |_, meta| {
        meta.file_type().is_symlink()
            || (meta.uid() == uid && meta.permissions().mode() & SHARED_WRITE == 0)
    })
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            std::fs::remove_dir_all(path)
        }
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Mark `dir` as used now: [`prune`] ages a copy by its mtime, so a copy a
/// long-lived daemon keeps handing to sessions is never pruned under them.
/// Best-effort.
pub fn touch(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.set_modified(SystemTime::now());
    }
}

/// Whether `path` (archive-relative) is unpacked: under `defaults/`, and in
/// no `tests` directory.
fn wanted(path: &Path) -> bool {
    let mut parts = path.components();
    if parts.next() != Some(Component::Normal("defaults".as_ref())) {
        return false;
    }
    !path
        .components()
        .any(|c| c == Component::Normal("tests".as_ref()))
}

fn unpack(archive: &[u8], into: &Path) -> Result<()> {
    let tar = zstd::decode_all(archive).context("decompress the payload")?;
    let mut archive = tar::Archive::new(tar.as_slice());
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(false);
    for entry in archive.entries().context("read the payload")? {
        let mut entry = entry.context("read a payload entry")?;
        let path = entry.path().context("a payload entry path")?.into_owned();
        if !wanted(&path) {
            continue;
        }
        // `unpack_in` refuses any entry that would land outside `into`.
        entry
            .unpack_in(into)
            .with_context(|| format!("unpack {}", path.display()))?;
    }
    Ok(())
}

/// Remove copies other than `keep` that are outside the [`KEEP_RECENT`]
/// newest and older than [`RETAIN`], and staging directories older than
/// [`STAGING_STALE`]. Best-effort: a failure leaves the directory for the
/// next run.
pub fn prune(base: &Path, keep: &Path, now: SystemTime) {
    let Ok(read) = std::fs::read_dir(base) else {
        return;
    };
    let age = |p: &Path| {
        p.symlink_metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .unwrap_or_default()
    };
    let mut copies = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path == keep || !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(STAGING_PREFIX) {
            if age(&path) > STAGING_STALE {
                let _ = std::fs::remove_dir_all(&path);
            }
            continue;
        }
        if name.starts_with('.') {
            continue;
        }
        copies.push((age(&path), path));
    }
    // Youngest first; the current copy is one of the KEEP_RECENT.
    copies.sort();
    for (age, path) in copies.into_iter().skip(KEEP_RECENT.saturating_sub(1)) {
        if age > RETAIN {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
#[path = "hooks_home_tests.rs"]
mod tests;
