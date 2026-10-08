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

use std::path::{Path, PathBuf};
use std::process::Command;

use super::out;
use super::util;

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
/// (the Mach-O signature survives `install`/`cp`). Signing does NOT make a TCC
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
    let status = Command::new("bash")
        .arg("-c")
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
        .arg(&sink)
        .status();

    let code = status.ok().and_then(|s| s.code()).unwrap_or(98);
    if code == 98 {
        return ProvisionOutcome::NotDefined;
    }
    if code != 0 {
        return ProvisionOutcome::Failed;
    }
    ProvisionOutcome::Provisioned(std::fs::read_to_string(&sink).unwrap_or_default())
}

/// The `LOOM_DAEMON_BIN` override path: install `new_bin` at `dest`, mode
/// 755, atomically (#10708).
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
/// A symlinked `dest` is REPLACED by a regular file, not followed (same as
/// GNU `install`): the rename swaps the directory entry, so the link's target
/// is left untouched. Pin `LOOM_DAEMON_BIN` to the real path (for example
/// `~/.local/bin/loom-daemon`), not to a symlink such as worker-1's
/// `/usr/local/bin/loom-daemon`, or the link is silently turned into a file.
///
/// Each install first sweeps stale temp files a killed earlier install left
/// next to `dest` (see [`sweep_stale_staging`]).
///
/// `true` on success, `false` on any failure.
pub fn install_to(new_bin: &Path, dest: &Path) -> bool {
    atomic_install(new_bin, dest).is_ok()
}

fn atomic_install(new_bin: &Path, dest: &Path) -> std::io::Result<()> {
    // Open the source FIRST: a missing or unreadable source must fail before
    // anything is created next to `dest`.
    let mut src = std::fs::File::open(new_bin)?;
    let dir = staging_dir(dest);
    sweep_stale_staging(dir, dest);
    let (tmp_path, mut tmp) = create_staging_file(dir, dest)?;
    let staged = StagedFile(Some(tmp_path.clone()));

    std::io::copy(&mut src, &mut tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // fchmod, so the process umask does not apply.
        tmp.set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    tmp.sync_all()?;
    drop(tmp);

    std::fs::rename(&tmp_path, dest)?;
    staged.disarm();

    // Persist the rename itself. Best-effort: the swap has already happened
    // and is atomic either way; this only shortens the window in which a
    // power loss could roll the directory entry back to the old file.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
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
const STALE_STAGING_SECS: u64 = 3600;

/// Does `name` look like `.{base}.loom-install.<digits>.<digits>.<digits>`?
fn is_staging_name(name: &str, base: &str) -> bool {
    let prefix = format!(".{base}.loom-install.");
    let Some(rest) = name.strip_prefix(&prefix) else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Best-effort removal of orphaned `.{base}.loom-install.*` files beside
/// `dest` (a kill between write and rename leaves one). Only regular files
/// (not symlinks or directories) owned by the effective user and older than
/// [`STALE_STAGING_SECS`] are removed; the age gate also protects a
/// concurrent install's live temp file. No pid liveness (pids are reused).
/// Every error is ignored: a sweep failure never fails an install.
#[cfg(unix)]
fn sweep_stale_staging(dir: &Path, dest: &Path) {
    use std::os::unix::fs::MetadataExt;

    let Some(base) = dest.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let max_age = std::time::Duration::from_secs(STALE_STAGING_SECS);
    let mut removed = 0usize;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_staging_name(&name, &base) {
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
        if stale && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
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
