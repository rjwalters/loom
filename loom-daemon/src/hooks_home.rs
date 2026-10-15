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
//! most recent [`KEEP_RECENT`] and older than [`RETAIN`]: a session spawned by
//! the previous daemon still points at its own copy, and a hook whose file has
//! vanished fails open.
//!
//! # What is unpacked
//!
//! `defaults/` minus every `tests/` directory: the hooks source
//! `../scripts/lib/*`, `guard-loom-workflow.sh` reaches `../scripts/` and
//! `../roles`, and the tests are most of the bytes and none of the runtime.
//!
//! # Permissions
//!
//! These files are executed by every spawned session, so the tree is private
//! to the daemon's user whatever the umask: the base is `0700`, group/other
//! write is stripped from the staged tree before the rename, and a directory
//! found already in place is reused only if it is a real directory owned by
//! the current euid with no group/other write bit; otherwise it is replaced.
//!
//! # Overrides
//!
//! `LOOM_HOOKS_HOME` set in the daemon's own environment is passed through
//! unchanged (an operator pin, never written to). Set to the empty string it
//! disables the export, and sessions run hooks from the checkout as before.

use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
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
        static CURRENT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        CURRENT
            .get_or_init(|| {
                resolve(
                    std::env::var(ENV).ok(),
                    dirs::home_dir(),
                    crate::init::payload::embedded_archive(),
                    env!("CARGO_PKG_VERSION"),
                )
            })
            .clone()
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
/// complete copy is already there, then prune older copies. Returns the
/// directory that holds `defaults/`.
///
/// # Errors
/// The base or staging directory cannot be created, the archive does not
/// unpack, it has no `defaults/hooks/`, or the rename fails with no complete
/// copy in place.
pub fn materialize(base: &Path, archive: &[u8], version: &str) -> Result<PathBuf> {
    let dest = base.join(key(archive, version));
    if is_usable(&dest) {
        prune(base, &dest, SystemTime::now());
        return Ok(dest);
    }
    std::fs::create_dir_all(base).with_context(|| format!("create {}", base.display()))?;
    std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restrict {}", base.display()))?;
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
    restrict_tree(staging.path()).context("restrict the staged tree")?;
    if dest.symlink_metadata().is_ok() {
        // A crashed unpack, or a copy that is not ours / is writable by
        // others, under this key: replace it.
        remove_any(&dest);
    }
    let staged = staging.keep();
    if let Err(e) = std::fs::rename(&staged, &dest) {
        let _ = std::fs::remove_dir_all(&staged);
        // Another process won the race with the same bytes.
        if !is_usable(&dest) {
            return Err(anyhow!(e)).with_context(|| format!("rename into {}", dest.display()));
        }
    }
    prune(base, &dest, SystemTime::now());
    Ok(dest)
}

fn is_complete(dir: &Path) -> bool {
    dir.join(COMPLETE_MARKER).is_file() && dir.join("defaults/hooks").is_dir()
}

/// A complete copy that is safe to execute hooks from: a real directory (not
/// a symlink) owned by the current euid with no group/other write bit.
fn is_usable(dir: &Path) -> bool {
    let Ok(meta) = dir.symlink_metadata() else {
        return false;
    };
    // SAFETY: geteuid only reads the caller's effective user id.
    let owned = meta.uid() == unsafe { libc::geteuid() };
    meta.is_dir() && owned && meta.permissions().mode() & 0o022 == 0 && is_complete(dir)
}

fn remove_any(path: &Path) {
    let is_dir = path.symlink_metadata().is_ok_and(|m| m.is_dir());
    let _ = if is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
}

/// Clear group/other write on `root` and everything under it (symlinks are
/// left alone: their mode is not consulted), and make `root` itself `0700`.
fn restrict_tree(root: &Path) -> Result<()> {
    fn walk(dir: &Path) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let meta = path.symlink_metadata()?;
            if meta.file_type().is_symlink() {
                continue;
            }
            let mode = meta.permissions().mode();
            if mode & 0o022 != 0 {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & !0o022))?;
            }
            if meta.is_dir() {
                walk(&path)?;
            }
        }
        Ok(())
    }
    walk(root)?;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
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
