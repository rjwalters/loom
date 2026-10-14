//! Signing and provisioning — the two steps that still run as **shell**,
//! deliberately.
//!
//! `scripts/install/provision-daemon.sh` is a separate, separately-allowlisted
//! script with its own retained suite (`tests/install/test-provision-daemon.sh`),
//! and it is `source`d by the installer as well as by this update flow. It is
//! not in this port's scope, so `sign_daemon_binary` and
//! `provision_machine_daemon` are invoked exactly as the shell invoked them:
//! by sourcing that file and calling the function.
//!
//! Two details of that invocation are contract:
//!
//! * `provision_machine_daemon` reports its destination by EXPORTING
//!   `PROVISIONED_DAEMON_BIN` into the sourcing shell — including on its
//!   version-equality short-circuit, which is the case the post-provision
//!   verification exists to catch. A subprocess cannot export into its parent,
//!   so the value is written to a scratch file and read back. Printing it on
//!   stdout would not do: the function's own human-facing output goes there.
//! * Both functions are OPTIONAL. `declare -F` guarded each call, and the
//!   retained suite relies on that — scenario 11's fake `provision-daemon.sh`
//!   defines `provision_machine_daemon` and nothing else.
//!
//! # The binary write itself is Rust (#10983)
//!
//! Neither path writes the destination in place, and both keep the binary
//! they replace:
//!
//! * [`stage`] copies the candidate to a temp file BESIDE the destination,
//!   and [`publish`] retains the live binary as `<dest>.previous`, records
//!   the transaction ([`txn`]) and `rename(2)`s the candidate over the path.
//! * [`install_to`] (the `LOOM_DAEMON_BIN` override) runs the two back to
//!   back. `provision_machine_daemon` (the shell) calls them through
//!   `loom-daemon install-binary stage|publish`, with its loadability check
//!   and signing step between the two, so both run on the staged file and the
//!   live path is never touched before the rename.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::out;
use super::selfrepl;
use super::util;

pub mod txn;

pub use txn::{InstallError, Published};

/// Names the `loom-daemon` that performs the shell path's `install-binary`
/// calls. Set here to the running binary, which is known to execute on this
/// host; the script falls back to the candidate itself, then to the installed
/// binary, when it is unset or cannot stage.
pub const INSTALL_HELPER_ENV: &str = "LOOM_DAEMON_INSTALL_HELPER";

/// `$REPO_ROOT/scripts/install/provision-daemon.sh`, when it is readable.
#[must_use]
pub fn provision_script(repo_root: &Path) -> Option<PathBuf> {
    let p = repo_root.join("scripts/install/provision-daemon.sh");
    // `[[ -r ... ]]`
    if std::fs::File::open(&p).is_ok() {
        Some(p)
    } else {
        None
    }
}

/// `sign_daemon_binary "$NEW_BIN"` — best-effort, non-fatal (#4016).
///
/// Ad-hoc-signs the freshly built binary with a stable identifier BEFORE
/// provisioning, so both provisioning branches copy an already-signed binary
/// (the Mach-O signature is embedded, so it survives a byte-for-byte copy,
/// including the one kept as `<dest>.previous`). Signing does NOT make a TCC
/// grant survive a rebuild; it only pins a human-legible identifier in place of
/// the rustc metadata hash.
///
/// Never called in artifact-fetch mode: a fetched macOS artifact may already
/// carry a REAL Developer ID signature, and force-resigning it would silently
/// downgrade that to ad-hoc. `sign_daemon_binary` itself guards against that
/// now, but skipping the call entirely is the shell's behaviour and there is
/// nothing useful for it to do to an already-signed or genuinely-unsigned
/// fetched artifact.
pub fn sign_daemon_binary(script: &Path, new_bin: &Path) {
    // `declare -F sign_daemon_binary` + the call, in one shell so the source
    // and the guard see the same definitions.
    let _ = Command::new("bash")
        .arg("-c")
        .arg(
            r#"set -uo pipefail
# shellcheck source=/dev/null
source "$1" || exit 0
declare -F sign_daemon_binary >/dev/null 2>&1 || exit 0
sign_daemon_binary "$2"
"#,
        )
        .arg("loom-daemon-update")
        .arg(script)
        .arg(new_bin)
        .status();
}

/// What `provision_machine_daemon` did.
pub enum ProvisionOutcome {
    /// The function ran and succeeded; the payload is `PROVISIONED_DAEMON_BIN`
    /// (possibly empty, which the caller passes straight to the verifier — an
    /// empty destination is itself a verification failure).
    Provisioned(String),
    /// The function ran and FAILED. A soft warn here (the pre-#4053 behaviour)
    /// left the exit code at 0, which is exactly the "reports success while
    /// shipping nothing" defect.
    Failed,
    /// `provision-daemon.sh` did not define the function.
    NotDefined,
}

/// `provision_machine_daemon "$NEW_BIN" "" "$REPO_ROOT/defaults"`.
///
/// The third argument is passed so an already-installed standalone daemon
/// picks up (or refreshes) its `loom-daemon init` recovery payload on every
/// update, not only on first install (#5389). `$REPO_ROOT` is always a genuine
/// Loom SOURCE checkout here — this script rebuilds from source — so
/// `$REPO_ROOT/defaults` is available.
pub fn provision_machine_daemon(
    script: &Path,
    new_bin: &Path,
    repo_root: &Path,
) -> ProvisionOutcome {
    let sink = super::scratch_file("loom-daemon-provisioned-bin");
    let status = provision_command(script, new_bin, repo_root, &sink).status();

    let code = status.ok().and_then(|s| s.code()).unwrap_or(98);
    if code == 98 {
        return ProvisionOutcome::NotDefined;
    }
    if code != 0 {
        return ProvisionOutcome::Failed;
    }
    ProvisionOutcome::Provisioned(std::fs::read_to_string(&sink).unwrap_or_default())
}

/// The `bash -c` that sources `script` and calls `provision_machine_daemon`,
/// writing `PROVISIONED_DAEMON_BIN` to `sink`.
///
/// It also names the `loom-daemon` the script should use for its
/// `install-binary` calls ([`INSTALL_HELPER_ENV`]): this running binary,
/// unless the caller's environment already pins one.
fn provision_command(script: &Path, new_bin: &Path, repo_root: &Path, sink: &Path) -> Command {
    let mut cmd = Command::new("bash");
    if std::env::var_os(INSTALL_HELPER_ENV).is_none() {
        // `None` once this process's own file has been replaced (a deferred
        // restart after an earlier roll): the script's fallbacks apply.
        if let Some(me) = selfrepl::running_binary() {
            cmd.env(INSTALL_HELPER_ENV, me);
        }
    }
    cmd.arg("-c")
        .arg(
            r#"set -uo pipefail
# shellcheck source=/dev/null
source "$1" || exit 98
declare -F provision_machine_daemon >/dev/null 2>&1 || exit 98
provision_machine_daemon "$2" "" "$3/defaults"
rc=$?
printf '%s' "${PROVISIONED_DAEMON_BIN:-}" > "$4"
exit "$rc"
"#,
        )
        .arg("loom-daemon-update")
        .arg(script)
        .arg(new_bin)
        .arg(repo_root)
        .arg(sink);
    cmd
}

/// The `LOOM_DAEMON_BIN` override path: install `new_bin` at `dest`, mode
/// 755, atomically (#10708), keeping the binary it replaces (#10983).
///
/// The new bytes are written in full to a uniquely named temp file in the
/// SAME directory as `dest` (so the same filesystem), chmod'ed to 755,
/// fsync'ed, and only then `rename(2)`d over `dest`. At every instant `dest`
/// is either the complete old file or the complete new one; a kill at any
/// point leaves at worst a stray temp file next to an intact `dest`.
///
/// The rename is also what makes replacing a CURRENTLY EXECUTING binary safe
/// (the self-replacement case `selfrepl` documents): `dest` is pointed at a
/// new inode while the running process keeps its old, now-unnamed one until
/// it exits. The old inode is never opened for writing, so there is no
/// `ETXTBSY` on Linux and no in-place rewrite of a running, signed image on
/// macOS (which the kernel answers by killing the process).
///
/// There is deliberately NO fallback. This used to run `install -m 755` and
/// then `cp -f`, and `cp -f` rewrites a writable `dest` in place when it
/// cannot do anything else (for example in a read-only directory): exactly
/// the partial-binary hazard. If the temp file cannot be created, written or
/// renamed, the install fails, the temp file is removed, and `dest` is left
/// as it was.
///
/// Before the rename the live binary is copied to `<dest>.previous` and the
/// transaction is recorded beside it (see [`publish`]). When that copy
/// cannot be made the install is REFUSED and `dest` is left as it was; a
/// first-ever install has nothing to keep and is exempt.
///
/// A symlinked `dest` is REPLACED by a regular file, not followed (same as
/// GNU `install`): the rename swaps the directory entry, so the link's target
/// is left untouched. Pin `LOOM_DAEMON_BIN` to the real path (for example
/// `~/.local/bin/loom-daemon`), not to a symlink such as worker-1's
/// `/usr/local/bin/loom-daemon`, or the link is silently turned into a file.
///
/// Each install first sweeps stale temp files a killed earlier install left
/// next to `dest` (see [`sweep_stale_staging`]).
///
/// `true` on success, `false` on any failure (the reason is printed).
pub fn install_to(new_bin: &Path, dest: &Path) -> bool {
    let done = stage(new_bin, dest)
        .map_err(InstallError::Stage)
        .and_then(|staged| publish(&staged, dest));
    match done {
        Ok(done) => {
            if let Some(line) = done.retained_line() {
                out::say(&line);
            }
            true
        }
        Err(e) => {
            out::err(&e.to_string());
            false
        }
    }
}

/// Step 1 of an install: copy `new_bin`, in full, to a uniquely named temp
/// file beside `dest`, mode 755, fsync'ed. Returns that file's path.
///
/// `dest` is not touched. On failure nothing is left behind. On success the
/// staged file is the caller's: hand it to [`publish`], or remove it. One a
/// killed caller leaves is swept by a later install once it is stale.
pub fn stage(new_bin: &Path, dest: &Path) -> std::io::Result<PathBuf> {
    // Open the source FIRST: a missing or unreadable source must fail before
    // anything is created next to `dest`.
    let mut src = std::fs::File::open(new_bin)?;
    let dir = staging_dir(dest);
    sweep_stale_staging(dir, dest);
    let (tmp_path, mut tmp) = create_staging_file(dir, dest)?;
    let staged = StagedFile(Some(tmp_path.clone()));

    std::io::copy(&mut src, &mut tmp)?;
    make_executable(&tmp)?;
    tmp.sync_all()?;
    drop(tmp);
    staged.disarm();
    Ok(tmp_path)
}

/// Step 2 of an install: keep the live binary, record the transaction, and
/// `rename(2)` the file [`stage`] produced over `dest`.
///
/// In order, and every step before the rename leaves `dest` untouched:
///
/// 1. `staged` is identified (sha256 of its bytes as they are NOW, so after
///    any signing the caller did, and its `--version`).
/// 2. The live binary is copied to a temp file, fsync'ed and renamed to
///    `<dest>.previous`; its sha256 is taken from the bytes copied. A failure
///    here REFUSES the install. No live binary means a first-ever install,
///    which has nothing to keep. A candidate byte-identical to the live
///    binary keeps the existing `<dest>.previous` instead, so re-running an
///    install cannot overwrite the recovery copy with the candidate itself.
/// 3. The record is written with phase `staged`. A failure refuses too.
/// 4. The rename. Then the directory is fsync'ed and the record moves to
///    `published`, and to `committed` once `dest` is confirmed to be the
///    staged file (it is not when a concurrent install got there first).
///
/// `staged` is removed on every failure.
pub fn publish(staged: &Path, dest: &Path) -> Result<Published, InstallError> {
    let guard = StagedFile(Some(staged.to_path_buf()));
    if !is_staged_for(staged, dest) {
        // Not ours to delete: it is not a file `stage` made for this `dest`.
        guard.disarm();
        return Err(InstallError::NotStaged(staged.to_path_buf()));
    }
    let dir = staging_dir(dest);
    let mut record = txn::begin(staged, dest)?;
    let staged_id = file_id(staged);

    std::fs::rename(staged, dest).map_err(InstallError::Publish)?;
    guard.disarm();

    // Persist the rename itself. Best-effort: the swap has already happened
    // and is atomic either way; this only shortens the window in which a
    // power loss could roll the directory entry back to the old file.
    sync_dir(dir);
    txn::advance(dest, &mut record, txn::Phase::Published);
    if staged_id.is_some() && staged_id != file_id(dest) {
        // A concurrent install replaced `dest` between the rename and this
        // check. This one did publish, so it is not a failure, but it is not
        // the install that finished: its record stays at `published`.
        // (Serializing installs against each other is #9734's.)
        out::warn(&format!(
            "{} was replaced by another install while this one was publishing.",
            dest.display()
        ));
        return Ok(Published { record });
    }
    txn::advance(dest, &mut record, txn::Phase::Committed);
    Ok(Published { record })
}

/// Is `staged` a file [`stage`] would have made for `dest`: in `dest`'s own
/// directory, under its staging prefix?
fn is_staged_for(staged: &Path, dest: &Path) -> bool {
    let (Some(name), Some(base)) = (staged.file_name(), dest.file_name()) else {
        return false;
    };
    let prefix = format!(".{}.loom-install.", base.to_string_lossy());
    staging_dir(staged) == staging_dir(dest) && name.to_string_lossy().starts_with(&prefix)
}

/// `(dev, ino)` of whatever `path` names now, without following a symlink.
#[cfg(unix)]
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_id(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// fchmod 755, so the process umask does not apply.
#[cfg(unix)]
fn make_executable(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

/// fsync a directory, so a rename inside it survives a power loss.
/// Best-effort everywhere it is used.
fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// The directory `dest` lives in, where its temp file is staged so the
/// final rename never crosses a filesystem. A bare file name means `.`.
fn staging_dir(dest: &Path) -> &Path {
    match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Age after which a leftover staging file is considered orphaned.
#[cfg(unix)]
const STALE_STAGING_SECS: u64 = 3600;

/// Does `name` look like `.{base}.loom-install.<digits>.<digits>.<digits>`?
///
/// Compared as raw bytes, never through a lossy UTF-8 conversion: two
/// non-UTF-8 basenames that differ only in invalid bytes would otherwise both
/// map to the same `U+FFFD` text and one could sweep the other's files.
#[cfg(unix)]
fn is_staging_name(name: &[u8], base: &[u8]) -> bool {
    let Some(rest) = name
        .strip_prefix(b".")
        .and_then(|r| r.strip_prefix(base))
        .and_then(|r| r.strip_prefix(b".loom-install.".as_slice()))
    else {
        return false;
    };
    let parts: Vec<&[u8]> = rest.split(|&b| b == b'.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.iter().all(u8::is_ascii_digit))
}

/// The orphaned `.{base}.loom-install.*` entries beside `dest` that the sweep
/// may remove: regular files only (not symlinks or directories), owned by the
/// effective user, and older than [`STALE_STAGING_SECS`]. The age gate also
/// protects a concurrent install's live temp file. No pid liveness (pids are
/// reused). Any entry that cannot be inspected is skipped.
#[cfg(unix)]
fn stale_staging_entries(dir: &Path, dest: &Path) -> Vec<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let Some(base) = dest.file_name() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let max_age = std::time::Duration::from_secs(STALE_STAGING_SECS);
    let mut found = Vec::new();
    for entry in rd.flatten() {
        if !is_staging_name(entry.file_name().as_bytes(), base.as_bytes()) {
            continue;
        }
        let path = entry.path();
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !md.file_type().is_file() || md.uid() != euid {
            continue;
        }
        let stale = md
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if stale {
            found.push(path);
        }
    }
    found
}

/// Best-effort removal of the [`stale_staging_entries`] beside `dest` (a kill
/// between write and rename leaves one). Every error is ignored: a sweep
/// failure never fails an install.
#[cfg(unix)]
fn sweep_stale_staging(dir: &Path, dest: &Path) {
    let removed = stale_staging_entries(dir, dest)
        .iter()
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count();
    if removed > 0 {
        out::say(&format!(
            "Removed {removed} stale install temp file(s) next to {}",
            dest.display()
        ));
    }
}

#[cfg(not(unix))]
fn sweep_stale_staging(_dir: &Path, _dest: &Path) {}

/// Create a new, uniquely named temp file next to `dest`.
///
/// The name carries the pid, a per-process counter and the clock, and the
/// file is opened with `create_new`, so two concurrent installs (in one
/// process or several) can never write into the same temp file.
fn create_staging_file(dir: &Path, dest: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let base = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "loom-daemon".to_string());
    let mut last_err = None;
    for _ in 0..16 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let name = format!(
            ".{base}.loom-install.{}.{}.{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(name);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Owner-only until the bytes are complete; chmod 755 after.
            opts.mode(0o600);
        }
        match opts.open(&path) {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no unique temp name")))
}

/// Removes the staged temp file on drop unless the rename consumed it.
struct StagedFile(Option<PathBuf>);

impl StagedFile {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// `DEST_DIR` / `PROVISION_TARGET` — where a restart will invoke the daemon
/// from.
#[must_use]
pub fn provision_target() -> PathBuf {
    if let Some(explicit) = util::env_non_empty("LOOM_DAEMON_BIN") {
        return PathBuf::from(explicit);
    }
    let dest_dir = util::env_non_empty("LOOM_DAEMON_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| util::home().join(".local/bin"));
    dest_dir.join("loom-daemon")
}

/// The "provisioning script is missing" branch — two warnings, no failure.
pub fn warn_no_provision_script(new_bin: &Path) {
    out::warn("scripts/install/provision-daemon.sh not found/sourceable — skipping machine-level provisioning.");
    out::warn(&format!(
        "Freshly-built binary: {} (set LOOM_DAEMON_BIN={} to use it directly)",
        new_bin.display(),
        new_bin.display()
    ));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "provision_tests.rs"]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod writer_scan;
