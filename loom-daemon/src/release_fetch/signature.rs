//! Artifact signature verification, present-only (epic #7810, PR 6a).
//!
//! Port of `loom-daemon-update.sh`'s `verify_artifact_signature`. Absence
//! always passes (there is nothing to verify); a PRESENT-but-invalid
//! signature always blocks. Those two are never allowed to collapse into one
//! another, and neither is allowed to collapse with "could not check" — see
//! [`Outcome`].
//!
//! ## The invariant a port must not break (from the #8028 issue thread)
//!
//! Every path splits three ways, not two:
//!
//! | situation | [`Outcome`] | [`SignatureState`] (reporting only) |
//! |---|---|---|
//! | `codesign`/`cosign` not installed | [`Outcome::Skipped`] | [`SignatureState::Unavailable`] |
//! | no `.sig` asset published | [`Outcome::Skipped`] (silently — the shell made no call here either) | [`SignatureState::Skipped`] |
//! | macOS: unsigned (`code object is not signed at all`) | [`Outcome::Skipped`] | [`SignatureState::Skipped`] |
//! | keyless: signer identity underivable | [`Outcome::Skipped`] | [`SignatureState::Unavailable`] |
//! | key mode: no resolvable public key | [`Outcome::Skipped`] | [`SignatureState::Unavailable`] |
//! | macOS: embedded signature present but invalid | [`Outcome::Failed`] | none (aborts before any report) |
//! | cosign: signature present and verification fails | [`Outcome::Failed`] | none (aborts before any report) |
//! | verification actually ran and passed | [`Outcome::Verified`] | [`SignatureState::Verified`] |
//!
//! A `.sig` the release PUBLISHES but that will not download appears in
//! neither column: [`super::fetch`] refuses the artifact outright before
//! calling here (#8197), because a `sig_path: None` produced by a failed
//! transfer is not the same fact as the "no `.sig` asset published" row above
//! and must not be reported as it.
//!
//! A type that collapses `Skipped` into `Verified` accepts a tampered
//! artifact whose signature does not validate. A type that collapses it into
//! `Failed` bricks updates on every host with no `cosign` installed, and on
//! every host with no distributed public key — #5054 chose keyless as the
//! default trust root and deliberately shipped no key at all.
//!
//! On Linux the ARTIFACT'S OWN SHAPE selects the verification mode — never
//! local configuration (#5054): a `.sig` accompanied by its `.pem` signing
//! certificate was signed keylessly and is verified against the expected
//! signer identity; a bare `.sig` was signed with a private key and needs a
//! resolvable public key. Deciding by artifact shape is what makes a stale
//! operator-set `LOOM_DAEMON_UPDATE_COSIGN_PUBKEY` harmless against keyless
//! releases instead of a fleet-wide false "tamper" abort.

use super::cosign;
use crate::cmd_out::{self, CmdOutcome, Unavailable};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Ceiling for a `codesign`/`cosign` invocation. These are local
/// crypto/keychain operations, not forge calls, so the default is generous
/// relative to how long they actually take.
const VERIFY_TIMEOUT: Duration = cmd_out::DEFAULT_TIMEOUT;

/// Ceiling for the `cosign version` presence probe — deliberately short, the
/// same reasoning as `script_helpers::gh_cmd`'s probe: anything slow here is
/// already the breakage it is testing for.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// What verification concluded. Never collapse two of these into one — see
/// the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Verification ran and passed, or there was nothing to verify (absence
    /// always passes).
    Verified,
    /// Could not be checked, and that is fine: absent tooling, no signature
    /// published, or an underivable identity/key. Never a block.
    Skipped,
    /// A signature IS present and did not validate. Tamper evidence — always
    /// a hard block, never confused with `Skipped`.
    Failed,
}

/// How much signature assurance an artifact ended up carrying, as reported at
/// the process boundary (`SIGNATURE=` in `cli/release_fetch.rs`'s `KEY=value`
/// stdout contract, #8197).
///
/// A strictly finer split of the NON-blocking half of [`Outcome`], for
/// REPORTING only — it never decides anything. [`Outcome`] alone cannot tell a
/// caller whether a signature was checked, because it folds "there was nothing
/// to verify" and "something was published and went unchecked" into the single
/// [`Outcome::Skipped`] value. Both are legitimately non-blocking (#5054 ships
/// no key and does not require `cosign`), but only the second means the
/// artifact carries an unverified signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureState {
    /// Verification actually ran and passed.
    Verified,
    /// There was nothing to verify: the release publishes no `.sig`, the macOS
    /// binary is unsigned, or the target is unrecognized. Checksum-only, by
    /// design — the #5054 case that must keep working.
    Skipped,
    /// Signature material IS present but could not be checked on this host: no
    /// `cosign`/`codesign` installed, an underivable signer identity, no
    /// resolvable public key. A loud skip, never a block.
    Unavailable,
}

impl SignatureState {
    /// The stdout-contract spelling (`verified` / `skipped` / `unavailable`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Skipped => "skipped",
            Self::Unavailable => "unavailable",
        }
    }
}

/// One verification's full result.
#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub outcome: Outcome,
    /// What to report at the process boundary — `None` exactly on
    /// [`Outcome::Failed`], which aborts the update before anything is
    /// reported. Set explicitly at every return site rather than derived from
    /// `outcome`/`message`, so a future branch cannot silently inherit the
    /// wrong reporting value.
    pub state: Option<SignatureState>,
    /// A human-readable line worded like the shell's `ok`/`warn`/`err` call
    /// it replaces — empty exactly where the shell made no call at all (an
    /// absent `.sig`, or an unrecognized target).
    pub message: String,
    /// macOS only: whether `codesign -dv` reported an `Authority=` line,
    /// checked BEFORE any verdict is reached — mirrors the shell's
    /// `ARTIFACT_SIGNATURE_HAD_AUTHORITY`, which the darwin branch sets
    /// unconditionally regardless of what it ultimately returns (#8008 reads
    /// this after provisioning to catch a post-provision signature
    /// downgrade). `None` off-darwin: there is no destination-signature
    /// comparison there.
    pub had_authority: Option<bool>,
}

/// Inputs to one verification.
pub struct VerifyInputs<'a> {
    pub target: &'a str,
    pub bin_path: &'a Path,
    pub sig_path: Option<&'a Path>,
    pub cert_path: Option<&'a Path>,
    pub repo_root: &'a Path,
    pub repo_slug: &'a str,
    pub tag: &'a str,
    pub cosign_pubkey_env: Option<&'a str>,
    pub cosign_identity_env: Option<&'a str>,
    pub cosign_oidc_issuer_env: Option<&'a str>,
}

/// Verify `inputs.bin_path`'s signature, present-only.
#[must_use]
pub fn verify(inputs: &VerifyInputs<'_>) -> VerifyResult {
    if inputs.target.ends_with("-apple-darwin") {
        return verify_darwin(inputs.bin_path);
    }
    if inputs.target.contains("-linux-") {
        return verify_linux(inputs);
    }
    // An unrecognized target verifies nothing rather than failing — the
    // shell's `*) return 0 ;;`.
    VerifyResult {
        outcome: Outcome::Verified,
        state: Some(SignatureState::Skipped),
        message: String::new(),
        had_authority: None,
    }
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn verify_darwin(bin_path: &Path) -> VerifyResult {
    let name = file_name(bin_path);

    let mut dv_cmd = Command::new("codesign");
    dv_cmd.arg("-dv").arg(bin_path).stdin(Stdio::null());
    let dv_outcome = cmd_out::run_command(dv_cmd, VERIFY_TIMEOUT);
    if matches!(&dv_outcome, CmdOutcome::Unavailable(Unavailable::Spawn(_))) {
        return VerifyResult {
            outcome: Outcome::Skipped,
            state: Some(SignatureState::Unavailable),
            message: "'codesign' not available -- skipping macOS signature verification \
                      (best-effort; checksum already verified)."
                .to_string(),
            had_authority: None,
        };
    }

    // codesign -dv writes its report to stderr; merge with stdout the same
    // way the shell's `2>&1` capture did, so nothing depends on which stream
    // a particular codesign version chose.
    let desc = match &dv_outcome {
        CmdOutcome::Ran(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        }
        CmdOutcome::Unavailable(u) => u.to_string(),
    };
    let had_authority = desc.lines().any(|l| l.starts_with("Authority="));

    if desc.contains("code object is not signed at all") {
        return VerifyResult {
            outcome: Outcome::Skipped,
            state: Some(SignatureState::Skipped),
            message: "Downloaded artifact is unsigned (no Developer ID secrets were configured \
                      for this release) -- proceeding without signature verification, per design \
                      (checksum is unconditional; signature is optional)."
                .to_string(),
            had_authority: Some(had_authority),
        };
    }

    let mut verify_cmd = Command::new("codesign");
    verify_cmd
        .arg("--verify")
        .arg("--strict")
        .arg(bin_path)
        .stdin(Stdio::null());
    let verify_outcome = cmd_out::run_command(verify_cmd, VERIFY_TIMEOUT);
    if verify_outcome.succeeded() {
        VerifyResult {
            outcome: Outcome::Verified,
            state: Some(SignatureState::Verified),
            message: format!("macOS codesign verification passed for {name}."),
            had_authority: Some(had_authority),
        }
    } else {
        VerifyResult {
            outcome: Outcome::Failed,
            state: None,
            message: format!(
                "macOS codesign verification FAILED for {name} -- an embedded signature is \
                 present but invalid. This is NOT the 'unsigned' case; treating as tamper \
                 evidence."
            ),
            had_authority: Some(had_authority),
        }
    }
}

fn cosign_available() -> bool {
    let mut cmd = Command::new("cosign");
    cmd.arg("version").stdin(Stdio::null());
    !matches!(
        cmd_out::run_command(cmd, PROBE_TIMEOUT),
        CmdOutcome::Unavailable(Unavailable::Spawn(_))
    )
}

fn verify_linux(inputs: &VerifyInputs<'_>) -> VerifyResult {
    let name = file_name(inputs.bin_path);

    // No .sig asset published for this release (cosign secret was not
    // configured for it) -- absence never blocks, by design. The shell made
    // no `ok`/`warn`/`err` call here either.
    let Some(sig_path) = inputs.sig_path else {
        return VerifyResult {
            outcome: Outcome::Skipped,
            state: Some(SignatureState::Skipped),
            message: String::new(),
            had_authority: None,
        };
    };
    let sig_name = file_name(sig_path);

    if !cosign_available() {
        return VerifyResult {
            outcome: Outcome::Skipped,
            state: Some(SignatureState::Unavailable),
            message: format!(
                "A detached signature ({sig_name}) is present for this release but 'cosign' is \
                 not installed -- SKIPPING verification (loud skip, not a block; checksum \
                 already verified)."
            ),
            had_authority: None,
        };
    }

    // ---- keyless (Sigstore/OIDC): the default since #5054 ----
    if let Some(cert_path) = inputs.cert_path {
        let cert_name = file_name(cert_path);
        let issuer = cosign::oidc_issuer(inputs.cosign_oidc_issuer_env);
        let (identity_flag, identity_desc) =
            match inputs.cosign_identity_env.filter(|v| !v.is_empty()) {
                Some(exact) => ("--certificate-identity", exact.to_string()),
                None => match cosign::identity_regexp(inputs.repo_slug, inputs.tag) {
                    Some(re) => ("--certificate-identity-regexp", re),
                    None => {
                        return VerifyResult {
                            outcome: Outcome::Skipped,
                            state: Some(SignatureState::Unavailable),
                            message: format!(
                            "A detached signature ({sig_name}) and its signing certificate are \
                             present but the expected signer identity could not be derived (no \
                             release slug/tag in scope) -- SKIPPING verification (loud skip, not \
                             a block; checksum already verified)."
                        ),
                            had_authority: None,
                        };
                    }
                },
            };

        let mut cmd = Command::new("cosign");
        cmd.arg("verify-blob")
            .arg("--certificate")
            .arg(cert_path)
            .arg("--signature")
            .arg(sig_path)
            .arg(identity_flag)
            .arg(&identity_desc)
            .arg("--certificate-oidc-issuer")
            .arg(&issuer)
            .arg(inputs.bin_path)
            .stdin(Stdio::null());
        let outcome = cmd_out::run_command(cmd, VERIFY_TIMEOUT);
        return if outcome.succeeded() {
            VerifyResult {
                outcome: Outcome::Verified,
                state: Some(SignatureState::Verified),
                message: format!(
                    "cosign keyless signature verification passed for {name} (signer identity \
                     {identity_desc}, issuer {issuer})."
                ),
                had_authority: None,
            }
        } else {
            VerifyResult {
                outcome: Outcome::Failed,
                state: None,
                message: format!(
                    "cosign keyless signature verification FAILED for {name} against \
                     {sig_name} + {cert_name} (expected signer identity {identity_desc}, issuer \
                     {issuer})."
                ),
                had_authority: None,
            }
        };
    }

    // ---- key mode: a bare `.sig`, pre-#5054 signing ----
    let Some(pubkey) = cosign::resolve_pubkey(inputs.repo_root, inputs.cosign_pubkey_env) else {
        return VerifyResult {
            outcome: Outcome::Skipped,
            state: Some(SignatureState::Unavailable),
            message: format!(
                "A detached signature ({sig_name}) is present without a signing certificate \
                 (key-signed release) and no cosign public key is resolvable (set \
                 LOOM_DAEMON_UPDATE_COSIGN_PUBKEY) -- SKIPPING verification (loud skip, not a \
                 block; checksum already verified)."
            ),
            had_authority: None,
        };
    };

    let mut cmd = Command::new("cosign");
    cmd.arg("verify-blob")
        .arg("--key")
        .arg(&pubkey)
        .arg("--signature")
        .arg(sig_path)
        .arg(inputs.bin_path)
        .stdin(Stdio::null());
    let outcome = cmd_out::run_command(cmd, VERIFY_TIMEOUT);
    if outcome.succeeded() {
        VerifyResult {
            outcome: Outcome::Verified,
            state: Some(SignatureState::Verified),
            message: format!("cosign signature verification passed for {name}."),
            had_authority: None,
        }
    } else {
        VerifyResult {
            outcome: Outcome::Failed,
            state: None,
            message: format!(
                "cosign signature verification FAILED for {name} against {sig_name} using key \
                 {}.",
                pubkey.display()
            ),
            had_authority: None,
        }
    }
}

#[cfg(test)]
mod tests;
