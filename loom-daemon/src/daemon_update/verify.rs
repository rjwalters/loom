//! The three post-provision assertions: the destination really is what this
//! run built (#4053), it really is what this run fetched (#5020/#8008), and
//! the supervisor is actually pointed at it (#6009).
//!
//! All three exist for one failure mode — "reports success while shipping
//! nothing". A provision step that returns success says only that a copy
//! command did not error; it says nothing about whether the destination is
//! now the freshly-built binary, whether its signature survived, or whether
//! the process the supervisor relaunches will even be that file.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::cmd_out::{self, CmdOutcome, Unavailable};

use super::out;
use super::selfrepl;
use super::supervisor::Detected;
use super::util;

/// `verify_destination_binary <dest>` — SOURCE-BUILD PATH ONLY.
///
/// Exits **5** on mismatch, which is distinguishable from a compile failure
/// (exit 1) and from a provisioning soft-failure. Skipped only when the source
/// HEAD is unknown (a tarball build with no `.git`), where there is nothing to
/// compare against.
///
/// The artifact path uses [`verify_destination_artifact`] instead: a fetched
/// release's commit has nothing to do with the local checkout's HEAD, so this
/// function's whole premise does not apply there.
pub fn verify_destination_binary(dest: Option<&Path>, source_commit: &str) {
    if source_commit == "unknown" {
        out::warn("Source HEAD is unknown (no .git?) — skipping post-provision verification.");
        return;
    }
    let Some(dest) = dest.filter(|d| util::is_executable(d)) else {
        out::err(&format!(
            "Post-provision verification FAILED: provisioning reported success but no executable binary was found at the destination ('{}').",
            display_or_unknown(dest)
        ));
        super::exit(5);
    };
    let dest_commit = util::extract_commit(&selfrepl::version_of_destination(dest));
    if dest_commit != source_commit {
        let shown = if dest_commit.is_empty() {
            "<none>"
        } else {
            &dest_commit
        };
        out::err(&format!(
            "Post-provision verification FAILED: destination binary at {} embeds commit '{shown}' but the expected source HEAD is '{source_commit}'.",
            dest.display()
        ));
        out::err("Provisioning reported success yet the destination is NOT the freshly-built binary — a silent no-op roll. This is distinct from a compile failure and from a provisioning soft-failure; refusing to report success.");
        super::exit(5);
    }
    out::ok(&format!(
        "Post-provision verification: destination binary at {} embeds source HEAD commit ({dest_commit}).",
        dest.display()
    ));
}

/// `verify_destination_artifact <dest>` — the artifact-fetch counterpart.
///
/// The identity compared is the artifact's own full `--version` STRING, not a
/// checksum and not an embedded commit, for two reasons that are both
/// consequences of what provisioning does:
///
/// * a fetched release's commit is the RELEASE's, unrelated to local HEAD, so
///   comparing against `$SOURCE_COMMIT` would fail on every successful roll;
/// * a byte compare is not usable either, because `provision_machine_daemon`
///   calls `sign_daemon_binary` on the DESTINATION, which on Darwin ad-hoc
///   re-signs a genuinely unsigned artifact in place and therefore changes its
///   bytes versus the verified download.
///
/// The version string is stable across a re-sign and is exactly what
/// `provision_machine_daemon`'s own version-equality short-circuit compares —
/// so this assertion also proves that short-circuit did not silently no-op a
/// real roll.
pub fn verify_destination_artifact(
    dest: Option<&Path>,
    artifact_version_output: &str,
    had_authority: Option<bool>,
) {
    if artifact_version_output.is_empty() {
        out::warn("Fetched artifact reported no --version output — skipping post-provision verification (checksum/signature were already verified pre-provision).");
        return;
    }
    let Some(dest) = dest.filter(|d| util::is_executable(d)) else {
        out::err(&format!(
            "Post-provision verification FAILED: provisioning reported success but no executable binary was found at the destination ('{}').",
            display_or_unknown(dest)
        ));
        super::exit(5);
    };
    let dest_version = selfrepl::version_of_destination(dest);
    if dest_version != artifact_version_output {
        let shown = if dest_version.is_empty() {
            "<none>"
        } else {
            dest_version.as_str()
        };
        out::err(&format!(
            "Post-provision verification FAILED: destination binary at {} reports '{shown}' but the fetched release artifact reports '{}' — provisioning reported success yet the destination is NOT the freshly-fetched binary (a silent no-op roll); refusing to report success.",
            dest.display(),
            artifact_version_output
        ));
        super::exit(5);
    }
    out::ok(&format!(
        "Post-provision verification: destination binary at {} is the fetched release artifact ({}).",
        dest.display(),
        dest_version
    ));

    // ---- signature-preservation assertion (Darwin only, #8008) ----
    // The version compare proves the destination IS the fetched binary; it
    // says nothing about whether provisioning left its SIGNATURE intact.
    // #7932 fixed a guard that had been silently ad-hoc-resigning every
    // Developer ID-signed release artifact on provision since #5020 — a
    // `codesign … | grep -q` pipe form that always reported 141 under
    // `set -o pipefail` — replacing the certificate-anchored designated
    // requirement with a per-build cdhash and orphaning TCC grants on every
    // roll. Nothing caught it because nothing compared the destination's
    // signature against the verified download's.
    if had_authority != Some(true) {
        return;
    }
    if !util::have("codesign") {
        out::warn("'codesign' not available -- cannot verify the destination binary's signature survived provisioning (the verified download carried a Developer ID Authority signature).");
        return;
    }
    // Read-then-match (#6662/#7932): NEVER `codesign … | grep -q`, which is
    // exactly the pipefail bug this whole check exists to guard against.
    //
    // Bounded (#8770, the twin of #8754's `release_fetch::signature`
    // `verify_darwin` fix): a contended host can make `codesign -dvvv` hang
    // indefinitely. A report we could not obtain — deadline-killed, or a
    // codesign that exited without writing anything — is "we could not
    // check", NOT "we checked and it's bad": collapsing it into the exit-5
    // downgrade path below is the exact #8754 conflation one layer further in.
    let desc = match codesign_describe(dest, dest_sig_verify_timeout()) {
        DestSig::Report(desc) => desc,
        DestSig::Inconclusive(why) => {
            out::warn(&format!(
                "'codesign -dvvv' {why} for {} -- cannot verify the destination binary's signature survived provisioning (the verified download carried a Developer ID Authority signature). Treating as inconclusive, not a downgrade.",
                dest.display()
            ));
            return;
        }
    };
    if !desc.lines().any(|l| l.starts_with("Authority=")) {
        out::err(&format!(
            "Post-provision verification FAILED: the fetched release artifact carried a Developer ID (Authority=) signature, but the provisioned destination at {} does not.",
            dest.display()
        ));
        out::err("Provisioning has DOWNGRADED the signature -- this replaces the certificate-anchored designated requirement with a per-build ad-hoc identity and orphans every TCC grant on this host (the #7932 regression class). Refusing to report success.");
        super::exit(5);
    }
    out::ok(&format!(
        "Post-provision verification: destination binary at {} retains its Developer ID Authority signature.",
        dest.display()
    ));
}

/// Deadline for the post-provision `codesign -dvvv` (#8770). A local
/// crypto/keychain operation, not a forge call, so the default mirrors
/// `release_fetch::signature`'s own `VERIFY_TIMEOUT` (30s =
/// [`cmd_out::DEFAULT_TIMEOUT`]). `LOOM_DAEMON_UPDATE_DEST_SIG_TIMEOUT_SECS`
/// overrides it — the knob the pre-port shell read — so a test that needs the
/// timeout to actually fire need not wait out the production ceiling.
fn dest_sig_verify_timeout() -> Duration {
    util::env_non_empty("LOOM_DAEMON_UPDATE_DEST_SIG_TIMEOUT_SECS")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(cmd_out::DEFAULT_TIMEOUT, Duration::from_secs)
}

/// What the bounded `codesign -dvvv` produced.
enum DestSig {
    /// Ran to completion and wrote a report (possibly a negative one, e.g.
    /// "not signed at all") — a definitive answer about `Authority=`.
    Report(String),
    /// No report: timed out, could not run, or exited without writing one.
    /// The payload completes the sentence "'codesign -dvvv' …".
    Inconclusive(String),
}

/// `codesign -dvvv <path> 2>&1` under `timeout` — `codesign` writes its
/// description to STDERR, so both streams are captured and concatenated.
fn codesign_describe(path: &Path, timeout: Duration) -> DestSig {
    let mut cmd = Command::new("codesign");
    cmd.arg("-dvvv").arg(path).stdin(Stdio::null());
    match cmd_out::run_command(cmd, timeout) {
        CmdOutcome::Ran(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            if text.is_empty() {
                let rc = o
                    .status
                    .code()
                    .map_or_else(|| "on a signal".to_string(), |c| c.to_string());
                DestSig::Inconclusive(format!("exited {rc} without writing a report"))
            } else {
                DestSig::Report(text)
            }
        }
        CmdOutcome::Unavailable(Unavailable::TimedOut { after, .. }) => {
            DestSig::Inconclusive(format!("timed out after {}s", after.as_secs()))
        }
        CmdOutcome::Unavailable(u) => DestSig::Inconclusive(format!("produced no report ({u})")),
    }
}

/// `verify_supervisor_matches_provisioned <provisioned>` (#6009).
///
/// ADVISORY ONLY — it never exits non-zero. Provisioning to `provisioned`
/// already succeeded at what it set out to do; correcting the supervisor's own
/// config is what `--relaunch` is for.
pub fn verify_supervisor_matches_provisioned(provisioned: Option<&Path>, sup: &Detected) {
    let Some(supervisor_bin) = sup.supervisor_bin.as_ref() else {
        return;
    };
    let Some(provisioned) = provisioned else {
        return;
    };
    let provisioned_real = util::realpath(provisioned);
    let supervisor_real = util::realpath(supervisor_bin);
    if provisioned_real.is_empty() || provisioned_real == supervisor_real {
        return;
    }
    let manager = sup.manager.word();
    out::warn(&format!(
        "The {manager}-managed binary ({}) is NOT the one just provisioned ({}) — restarting below re-launches the daemon via the EXISTING {manager} config, not from this build, so it may still come back on the OLD binary. Run '{} --relaunch' to re-render the {manager} config so it points at the current binary.",
        supervisor_bin.display(),
        provisioned.display(),
        super::argv0_basename()
    ));
}

fn display_or_unknown(p: Option<&Path>) -> String {
    match p {
        Some(p) if !p.as_os_str().is_empty() => p.display().to_string(),
        _ => "<unknown>".to_string(),
    }
}
