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

/// The `LOOM_DAEMON_BIN` override path:
/// `install -m 755 "$NEW_BIN" "$dest" || { cp -f … && chmod 755 … }`.
///
/// Run as the real tools rather than reimplemented with `std::fs::copy`, and
/// that is load-bearing for the self-replacement case this whole script is
/// about: both `install(1)` and `cp -f` UNLINK a destination they cannot
/// rewrite in place, which is the only way to replace a binary that is
/// currently executing (`ETXTBSY`). A `File::create` on the destination
/// truncates it instead, and truncating a running executable is the one
/// outcome worse than failing.
pub fn install_to(new_bin: &Path, dest: &Path) -> bool {
    let installed = Command::new("install")
        .arg("-m")
        .arg("755")
        .arg(new_bin)
        .arg(dest)
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if installed {
        return true;
    }
    let copied = Command::new("cp")
        .arg("-f")
        .arg(new_bin)
        .arg(dest)
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !copied {
        return false;
    }
    Command::new("chmod")
        .arg("755")
        .arg(dest)
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
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
