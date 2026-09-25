//! Tests for present-only artifact signature verification (epic #7810, PR 6a).
//!
//! Exercises the real `verify()` against FAKE `codesign`/`cosign` scripts
//! placed ahead of the real ones on `PATH` — the same technique
//! `test-loom-daemon-update.sh`'s `write_fake_codesign`/`write_fake_cosign`
//! fixtures use, so the darwin branch is testable on a Linux CI runner that
//! has no `codesign` at all. `PATH` is process-global, so every test here is
//! `#[serial]`.

use super::*;
use serial_test::serial;
use std::io::Write as _;

fn write_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    writeln!(f, "#!/usr/bin/env bash\n{body}").unwrap();
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).unwrap();
    }
    p
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "loom-daemon-signature-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Prepends `bindir` to `PATH` for the duration of `f`, then restores it
/// exactly. Callers must be `#[serial]` — `PATH` is process-global.
fn with_fake_bin<F: FnOnce()>(bindir: &Path, f: F) {
    let old = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{old}", bindir.display()));
    f();
    std::env::set_var("PATH", old);
}

fn fake_bin_target(bin_path: &Path) -> std::path::PathBuf {
    std::fs::write(bin_path, b"binary bytes").unwrap();
    bin_path.to_path_buf()
}

// ---------------------------------------------------------------------------
// Absence never blocks
// ---------------------------------------------------------------------------

/// No `.sig` published for this release -- the shell made no ok/warn/err call
/// here either (`# No .sig asset published ... absence never blocks, by
/// design`); the SILENT skip is itself the behavior under test.
#[test]
fn linux_no_sig_asset_is_a_silent_skip() {
    let dir = tempdir();
    let bin = fake_bin_target(&dir.join("loom-daemon-x86_64-unknown-linux-gnu"));
    let result = verify(&VerifyInputs {
        target: "x86_64-unknown-linux-gnu",
        bin_path: &bin,
        sig_path: None,
        cert_path: None,
        repo_root: &dir,
        repo_slug: "rjwalters/loom",
        tag: "v0.16.0",
        cosign_pubkey_env: None,
        cosign_identity_env: None,
        cosign_oidc_issuer_env: None,
    });
    assert_eq!(result.outcome, Outcome::Skipped);
    assert!(result.message.is_empty());
    assert_eq!(result.had_authority, None);
    // #8197: nothing was published, so nothing went unchecked -- the value
    // the `SIGNATURE=` stdout key reports for a genuinely unsigned release.
    assert_eq!(result.state, Some(SignatureState::Skipped));
}

/// An unrecognized target verifies nothing rather than failing -- the
/// shell's `*) return 0 ;;`.
#[test]
fn unrecognized_target_is_a_silent_pass() {
    let dir = tempdir();
    let bin = fake_bin_target(&dir.join("loom-daemon-x86_64-pc-windows-msvc"));
    let result = verify(&VerifyInputs {
        target: "x86_64-pc-windows-msvc",
        bin_path: &bin,
        sig_path: Some(&bin), // irrelevant -- never consulted
        cert_path: None,
        repo_root: &dir,
        repo_slug: "rjwalters/loom",
        tag: "v0.16.0",
        cosign_pubkey_env: None,
        cosign_identity_env: None,
        cosign_oidc_issuer_env: None,
    });
    assert_eq!(result.outcome, Outcome::Verified);
    assert!(result.message.is_empty());
    // `Outcome::Verified` here means "nothing to verify", NOT "verification
    // passed" -- so the reported state is `Skipped`, never `Verified` (#8197).
    assert_eq!(result.state, Some(SignatureState::Skipped));
}

/// `cosign` not installed: a detached signature IS present, but the tool to
/// check it is not -- a LOUD skip (non-empty message), never a block. This is
/// the distinction turian's survey (#8028) flagged as sharper than "absence
/// never blocks": absence of the SIGNATURE is silent, absence of the TOOLING
/// is loud.
#[test]
#[serial]
fn linux_cosign_absent_is_a_loud_skip_not_a_failure() {
    let dir = tempdir();
    let empty_bin_dir = tempdir(); // deliberately no `cosign` on this PATH
    let bin = fake_bin_target(&dir.join("loom-daemon-x86_64-unknown-linux-gnu"));
    let sig = dir.join("loom-daemon-x86_64-unknown-linux-gnu.sig");
    std::fs::write(&sig, b"sig").unwrap();

    let mut result = None;
    with_fake_bin(&empty_bin_dir, || {
        // Route PATH lookups only through the empty dir + a minimal system
        // path that has no `cosign` either, so the probe genuinely fails.
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", empty_bin_dir.display()));
        result = Some(verify(&VerifyInputs {
            target: "x86_64-unknown-linux-gnu",
            bin_path: &bin,
            sig_path: Some(&sig),
            cert_path: None,
            repo_root: &dir,
            repo_slug: "rjwalters/loom",
            tag: "v0.16.0",
            cosign_pubkey_env: None,
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        }));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Skipped);
    assert!(result.message.contains("'cosign' is not installed"), "{}", result.message);
    assert!(result.message.contains("SKIPPING verification"), "{}", result.message);
    // #8197: a signature IS present and went unchecked -- distinct from the
    // silent no-`.sig` skip above, which `Outcome` alone cannot express.
    assert_eq!(result.state, Some(SignatureState::Unavailable));
}

/// Key mode, signature present, no resolvable public key -- AC3's loud skip:
/// the update still proceeds (checksum already verified).
#[test]
#[serial]
fn linux_key_mode_no_pubkey_is_a_loud_skip() {
    let dir = tempdir();
    let fakebin = tempdir();
    write_script(&fakebin, "cosign", "if [[ \"$1\" == version ]]; then exit 0; fi\nexit 0\n");
    let bin = fake_bin_target(&dir.join("loom-daemon-x86_64-unknown-linux-gnu"));
    let sig = dir.join("loom-daemon-x86_64-unknown-linux-gnu.sig");
    std::fs::write(&sig, b"sig").unwrap();

    let mut result = None;
    with_fake_bin(&fakebin, || {
        result = Some(verify(&VerifyInputs {
            target: "x86_64-unknown-linux-gnu",
            bin_path: &bin,
            sig_path: Some(&sig),
            cert_path: None, // key mode: no .pem sibling
            repo_root: &dir, // no .loom/cosign.pub or defaults/cosign.pub here
            repo_slug: "rjwalters/loom",
            tag: "v0.16.0",
            cosign_pubkey_env: None,
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        }));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Skipped);
    assert!(
        result
            .message
            .contains("no cosign public key is resolvable"),
        "{}",
        result.message
    );
    assert!(result.message.contains("SKIPPING verification"), "{}", result.message);
    assert_eq!(result.state, Some(SignatureState::Unavailable));
}

// ---------------------------------------------------------------------------
// Presence + validity/invalidity -- the checks that DO block
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn linux_key_mode_valid_signature_verifies() {
    let dir = tempdir();
    let fakebin = tempdir();
    write_script(
        &fakebin,
        "cosign",
        "if [[ \"$1\" == version ]]; then exit 0; fi\nif [[ \"$1\" == verify-blob ]]; then exit 0; fi\nexit 0\n",
    );
    let bin = fake_bin_target(&dir.join("loom-daemon-x86_64-unknown-linux-gnu"));
    let sig = dir.join("loom-daemon-x86_64-unknown-linux-gnu.sig");
    std::fs::write(&sig, b"sig").unwrap();
    let pubkey = dir.join("cosign.pub");
    std::fs::write(&pubkey, b"key").unwrap();

    let mut result = None;
    with_fake_bin(&fakebin, || {
        result = Some(verify(&VerifyInputs {
            target: "x86_64-unknown-linux-gnu",
            bin_path: &bin,
            sig_path: Some(&sig),
            cert_path: None,
            repo_root: &dir,
            repo_slug: "rjwalters/loom",
            tag: "v0.16.0",
            cosign_pubkey_env: Some(pubkey.to_str().unwrap()),
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        }));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Verified);
    assert!(
        result
            .message
            .contains("cosign signature verification passed"),
        "{}",
        result.message
    );
    assert_eq!(result.state, Some(SignatureState::Verified));
}

/// A present-but-invalid signature is tamper evidence, never a soft skip --
/// the twin of the macOS `signed-bad` case below.
#[test]
#[serial]
fn linux_key_mode_invalid_signature_fails_closed() {
    let dir = tempdir();
    let fakebin = tempdir();
    write_script(
        &fakebin,
        "cosign",
        "if [[ \"$1\" == version ]]; then exit 0; fi\nif [[ \"$1\" == verify-blob ]]; then exit 1; fi\nexit 0\n",
    );
    let bin = fake_bin_target(&dir.join("loom-daemon-x86_64-unknown-linux-gnu"));
    let sig = dir.join("loom-daemon-x86_64-unknown-linux-gnu.sig");
    std::fs::write(&sig, b"sig").unwrap();
    let pubkey = dir.join("cosign.pub");
    std::fs::write(&pubkey, b"key").unwrap();

    let mut result = None;
    with_fake_bin(&fakebin, || {
        result = Some(verify(&VerifyInputs {
            target: "x86_64-unknown-linux-gnu",
            bin_path: &bin,
            sig_path: Some(&sig),
            cert_path: None,
            repo_root: &dir,
            repo_slug: "rjwalters/loom",
            tag: "v0.16.0",
            cosign_pubkey_env: Some(pubkey.to_str().unwrap()),
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        }));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Failed);
    assert!(
        result
            .message
            .contains("cosign signature verification FAILED"),
        "{}",
        result.message
    );
}

/// #5054, the core regression this whole slice exists for: keyless
/// verification runs by DEFAULT with no env override, against the DERIVED
/// signer identity (repo slug + release tag) and the GitHub Actions issuer.
#[test]
#[serial]
fn linux_keyless_default_verifies_with_derived_identity() {
    let dir = tempdir();
    let fakebin = tempdir();
    let args_log = dir.join("cosign-args.log");
    write_script(
        &fakebin,
        "cosign",
        &format!(
            "printf '%s\\n' \"$*\" >> {log}\nif [[ \"$1\" == version ]]; then exit 0; fi\nif [[ \"$1\" == verify-blob ]]; then exit 0; fi\nexit 0\n",
            log = args_log.display()
        ),
    );
    let bin = fake_bin_target(&dir.join("loom-daemon-x86_64-unknown-linux-gnu"));
    let sig = dir.join("loom-daemon-x86_64-unknown-linux-gnu.sig");
    std::fs::write(&sig, b"sig").unwrap();
    let cert = dir.join("loom-daemon-x86_64-unknown-linux-gnu.pem");
    std::fs::write(&cert, b"cert").unwrap();

    let mut result = None;
    with_fake_bin(&fakebin, || {
        result = Some(verify(&VerifyInputs {
            target: "x86_64-unknown-linux-gnu",
            bin_path: &bin,
            sig_path: Some(&sig),
            cert_path: Some(&cert),
            repo_root: &dir,
            repo_slug: "test-owner/test-repo",
            tag: "v0.16.0",
            cosign_pubkey_env: None,
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        }));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Verified);
    assert!(
        result
            .message
            .contains("cosign keyless signature verification passed"),
        "{}",
        result.message
    );

    let argv = std::fs::read_to_string(&args_log).unwrap();
    assert!(argv.contains("--certificate "), "{argv}");
    assert!(
        argv.contains(
            r"--certificate-identity-regexp ^https://github\.com/test-owner/test-repo/\.github/workflows/[^@]+@refs/tags/v0\.16\.0$"
        ),
        "{argv}"
    );
    assert!(
        argv.contains("--certificate-oidc-issuer https://token.actions.githubusercontent.com"),
        "{argv}"
    );
    assert!(!argv.contains("--key "), "{argv}");
}

#[test]
#[serial]
fn linux_keyless_invalid_signature_fails_closed() {
    let dir = tempdir();
    let fakebin = tempdir();
    write_script(
        &fakebin,
        "cosign",
        "if [[ \"$1\" == version ]]; then exit 0; fi\nif [[ \"$1\" == verify-blob ]]; then exit 1; fi\nexit 0\n",
    );
    let bin = fake_bin_target(&dir.join("loom-daemon-x86_64-unknown-linux-gnu"));
    let sig = dir.join("loom-daemon-x86_64-unknown-linux-gnu.sig");
    std::fs::write(&sig, b"sig").unwrap();
    let cert = dir.join("loom-daemon-x86_64-unknown-linux-gnu.pem");
    std::fs::write(&cert, b"cert").unwrap();

    let mut result = None;
    with_fake_bin(&fakebin, || {
        result = Some(verify(&VerifyInputs {
            target: "x86_64-unknown-linux-gnu",
            bin_path: &bin,
            sig_path: Some(&sig),
            cert_path: Some(&cert),
            repo_root: &dir,
            repo_slug: "test-owner/test-repo",
            tag: "v0.16.0",
            cosign_pubkey_env: None,
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        }));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Failed);
    assert!(
        result
            .message
            .contains("cosign keyless signature verification FAILED"),
        "{}",
        result.message
    );
}

/// The artifact's own SHAPE selects the mode, never local config (#5054): a
/// keyless release (.sig + .pem) verifies keylessly even with a stale
/// `LOOM_DAEMON_UPDATE_COSIGN_PUBKEY` set.
#[test]
#[serial]
fn linux_artifact_shape_not_local_config_selects_the_mode() {
    let dir = tempdir();
    let fakebin = tempdir();
    let args_log = dir.join("cosign-args.log");
    write_script(
        &fakebin,
        "cosign",
        &format!(
            "printf '%s\\n' \"$*\" >> {log}\nif [[ \"$1\" == version ]]; then exit 0; fi\nif [[ \"$1\" == verify-blob ]]; then exit 0; fi\nexit 0\n",
            log = args_log.display()
        ),
    );
    let bin = fake_bin_target(&dir.join("loom-daemon-x86_64-unknown-linux-gnu"));
    let sig = dir.join("loom-daemon-x86_64-unknown-linux-gnu.sig");
    std::fs::write(&sig, b"sig").unwrap();
    let cert = dir.join("loom-daemon-x86_64-unknown-linux-gnu.pem");
    std::fs::write(&cert, b"cert").unwrap();
    let stale_pubkey = dir.join("stale-cosign.pub");
    std::fs::write(&stale_pubkey, b"stale key").unwrap();

    let mut result = None;
    with_fake_bin(&fakebin, || {
        result = Some(verify(&VerifyInputs {
            target: "x86_64-unknown-linux-gnu",
            bin_path: &bin,
            sig_path: Some(&sig),
            cert_path: Some(&cert),
            repo_root: &dir,
            repo_slug: "test-owner/test-repo",
            tag: "v0.16.0",
            cosign_pubkey_env: Some(stale_pubkey.to_str().unwrap()),
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        }));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Verified);
    assert!(
        result
            .message
            .contains("cosign keyless signature verification passed"),
        "{}",
        result.message
    );
    let argv = std::fs::read_to_string(&args_log).unwrap();
    assert!(!argv.contains("--key "), "{argv}");
}

// ---------------------------------------------------------------------------
// macOS (darwin) branch -- exercised via a fake `codesign` on ANY host
// ---------------------------------------------------------------------------

fn write_fake_codesign(dir: &Path, mode: &str) -> std::path::PathBuf {
    write_script(
        dir,
        "codesign",
        &format!(
            r#"target="${{@: -1}}"
MODE="{mode}"
if [[ "$1" == "-dv" || "$1" == "-dvvv" ]]; then
    if [[ "$MODE" == "unsigned" ]]; then
        echo "$target: code object is not signed at all" >&2
        exit 1
    fi
    {{
        echo "Executable=$target"
        echo "Identifier=com.rjwalters.loom-daemon"
        echo "Authority=Developer ID Application: Test Authority (TESTTEAM)"
    }} >&2
    exit 0
fi
if [[ "$1" == "--verify" ]]; then
    [[ "$MODE" == "signed-ok" ]] && exit 0
    echo "$target: invalid signature (code or signature have been modified)" >&2
    exit 1
fi
exit 0
"#
        ),
    )
}

/// Unsigned is expected/allowed -- must NOT be confused with tamper evidence
/// (the exact distinction the shell's doc comment calls out by name).
#[test]
#[serial]
fn macos_unsigned_is_a_soft_skip_not_tamper_evidence() {
    let dir = tempdir();
    let fakebin = tempdir();
    write_fake_codesign(&fakebin, "unsigned");
    let bin = fake_bin_target(&dir.join("loom-daemon-aarch64-apple-darwin"));

    let mut result = None;
    with_fake_bin(&fakebin, || {
        result = Some(verify(&VerifyInputs {
            target: "aarch64-apple-darwin",
            bin_path: &bin,
            sig_path: None,
            cert_path: None,
            repo_root: &dir,
            repo_slug: "rjwalters/loom",
            tag: "v0.16.0",
            cosign_pubkey_env: None,
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        }));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Skipped);
    assert!(result.message.contains("unsigned"), "{}", result.message);
    assert_eq!(result.had_authority, Some(false));
    assert_eq!(result.state, Some(SignatureState::Skipped));
}

#[test]
#[serial]
fn macos_signed_and_valid_verifies_and_reports_authority() {
    let dir = tempdir();
    let fakebin = tempdir();
    write_fake_codesign(&fakebin, "signed-ok");
    let bin = fake_bin_target(&dir.join("loom-daemon-aarch64-apple-darwin"));

    let mut result = None;
    with_fake_bin(&fakebin, || {
        result = Some(verify(&VerifyInputs {
            target: "aarch64-apple-darwin",
            bin_path: &bin,
            sig_path: None,
            cert_path: None,
            repo_root: &dir,
            repo_slug: "rjwalters/loom",
            tag: "v0.16.0",
            cosign_pubkey_env: None,
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        }));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Verified);
    assert!(
        result
            .message
            .contains("macOS codesign verification passed"),
        "{}",
        result.message
    );
    // #8008: HAD_AUTHORITY must be observable from a SUCCESSFUL verification
    // too, not only a failed one -- it feeds the post-provision
    // signature-preservation check outside this module's scope.
    assert_eq!(result.had_authority, Some(true));
}

/// Signed but INVALID is tamper evidence -- must hard-fail, distinct from the
/// "unsigned" soft-skip above even though both start from a signed-looking
/// artifact.
#[test]
#[serial]
fn macos_signed_but_invalid_fails_closed() {
    let dir = tempdir();
    let fakebin = tempdir();
    write_fake_codesign(&fakebin, "signed-bad");
    let bin = fake_bin_target(&dir.join("loom-daemon-aarch64-apple-darwin"));

    let mut result = None;
    with_fake_bin(&fakebin, || {
        result = Some(verify(&VerifyInputs {
            target: "aarch64-apple-darwin",
            bin_path: &bin,
            sig_path: None,
            cert_path: None,
            repo_root: &dir,
            repo_slug: "rjwalters/loom",
            tag: "v0.16.0",
            cosign_pubkey_env: None,
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        }));
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Failed);
    assert!(result.message.contains("codesign verification FAILED"), "{}", result.message);
    assert!(result.message.contains("NOT the 'unsigned' case"), "{}", result.message);
    // A blocked artifact is never reported at all -- the process exits before
    // any `SIGNATURE=` line is printed (#8197).
    assert_eq!(result.state, None);
    // Authority WAS present (a real, invalid signature) -- distinct from the
    // unsigned case's `Some(false)` above.
    assert_eq!(result.had_authority, Some(true));
}

/// A `codesign` that HANGS: `-dv` for `hang_dv`, `--verify` for
/// `hang_verify`. The hang is a `sleep` in a process the deadline's
/// process-group kill reaps, so nothing outlives the test.
fn write_hanging_fake_codesign(dir: &Path, hang_dv: bool, hang_verify: bool) -> std::path::PathBuf {
    write_script(
        dir,
        "codesign",
        &format!(
            r#"target="${{@: -1}}"
if [[ "$1" == "-dv" || "$1" == "-dvvv" ]]; then
    if [[ "{hang_dv}" == "true" ]]; then
        sleep 120
        exit 0
    fi
    {{
        echo "Executable=$target"
        echo "Identifier=com.rjwalters.loom-daemon"
        echo "Authority=Developer ID Application: Test Authority (TESTTEAM)"
    }} >&2
    exit 0
fi
if [[ "$1" == "--verify" ]]; then
    if [[ "{hang_verify}" == "true" ]]; then
        sleep 120
        exit 0
    fi
    exit 0
fi
exit 0
"#
        ),
    )
}

/// Runs `f` with [`verify_timeout`] lowered to `ms`, restoring it after. Test
/// callers must be `#[serial]` -- the override is process-global, exactly like
/// the `PATH` these tests already share.
fn with_short_verify_timeout<F: FnOnce()>(ms: u64, f: F) {
    use std::sync::atomic::Ordering;
    VERIFY_TIMEOUT_OVERRIDE_MS.store(ms, Ordering::Relaxed);
    f();
    VERIFY_TIMEOUT_OVERRIDE_MS.store(0, Ordering::Relaxed);
}

/// #8754, the second `codesign` call: `codesign --verify` that never answers
/// is UNKNOWN, not tamper evidence. It fails `succeeded()` exactly like the
/// `signed-bad` case above -- and that is precisely why `succeeded()` alone
/// cannot decide this: only a `codesign` that RAN and reported a bad signature
/// may reach `Outcome::Failed`.
#[test]
#[serial]
fn macos_verify_timeout_is_inconclusive_not_tamper_evidence() {
    let dir = tempdir();
    let fakebin = tempdir();
    write_hanging_fake_codesign(&fakebin, false, true);
    let bin = fake_bin_target(&dir.join("loom-daemon-aarch64-apple-darwin"));

    let mut result = None;
    with_fake_bin(&fakebin, || {
        with_short_verify_timeout(2_000, || {
            result = Some(verify(&VerifyInputs {
                target: "aarch64-apple-darwin",
                bin_path: &bin,
                sig_path: None,
                cert_path: None,
                repo_root: &dir,
                repo_slug: "rjwalters/loom",
                tag: "v0.16.0",
                cosign_pubkey_env: None,
                cosign_identity_env: None,
                cosign_oidc_issuer_env: None,
            }));
        });
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Skipped);
    // A loud skip, reported as signature-present-but-unchecked -- never
    // `Verified` (no assurance was established) and never the silent
    // `Skipped` state (something WAS there).
    assert_eq!(result.state, Some(SignatureState::Unavailable));
    assert!(result.message.contains("could not be completed"), "{}", result.message);
    assert!(result.message.contains("NOT tamper evidence"), "{}", result.message);
    // The alarming wording reserved for a real invalid signature must NOT
    // appear -- an operator reading this must not distrust the release.
    assert!(!result.message.contains("treating as tamper evidence"), "{}", result.message);
    assert!(!result.message.contains("verification FAILED"), "{}", result.message);
    // `-dv` DID answer, so what it reported is still known.
    assert_eq!(result.had_authority, Some(true));
}

/// #8754, the first `codesign` call: a hang in `codesign -dv` must return an
/// inconclusive skip immediately. Before the fix it fell through with the
/// `Unavailable`'s Display string standing in for codesign's report, then
/// asked the same wedged `codesign` a second time -- which timed out too and
/// produced the tamper-evidence verdict.
#[test]
#[serial]
fn macos_dv_timeout_is_inconclusive_and_does_not_reach_the_verify_call() {
    let dir = tempdir();
    let fakebin = tempdir();
    // `--verify` would exit 0 here: if the fix ever regresses to falling
    // through, this test would silently PASS as `Verified`, so assert the
    // reported state too, not just "not Failed".
    write_hanging_fake_codesign(&fakebin, true, false);
    let bin = fake_bin_target(&dir.join("loom-daemon-aarch64-apple-darwin"));

    let mut result = None;
    with_fake_bin(&fakebin, || {
        with_short_verify_timeout(2_000, || {
            result = Some(verify(&VerifyInputs {
                target: "aarch64-apple-darwin",
                bin_path: &bin,
                sig_path: None,
                cert_path: None,
                repo_root: &dir,
                repo_slug: "rjwalters/loom",
                tag: "v0.16.0",
                cosign_pubkey_env: None,
                cosign_identity_env: None,
                cosign_oidc_issuer_env: None,
            }));
        });
    });
    let result = result.unwrap();
    assert_eq!(result.outcome, Outcome::Skipped);
    assert_eq!(result.state, Some(SignatureState::Unavailable));
    assert!(result.message.contains("could not be completed"), "{}", result.message);
    assert!(result.message.contains("NOT tamper evidence"), "{}", result.message);
    // Nothing was learned about the signature, so nothing is claimed about it
    // -- `None`, not a fabricated `Some(false)` that #8008's post-provision
    // downgrade check would read as "this artifact had no Authority".
    assert_eq!(result.had_authority, None);
}

/// `codesign` not available at all (a Linux host resolving `aarch64-apple-
/// darwin` verification, or a macOS host with a broken toolchain) -- best-
/// effort soft skip, never a block.
#[test]
#[serial]
fn macos_codesign_absent_is_a_soft_skip() {
    let dir = tempdir();
    let empty_bin_dir = tempdir(); // no `codesign` here
    let bin = fake_bin_target(&dir.join("loom-daemon-aarch64-apple-darwin"));

    let mut result = None;
    with_fake_bin(&empty_bin_dir, || {
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", empty_bin_dir.display()));
        result = Some(verify(&VerifyInputs {
            target: "aarch64-apple-darwin",
            bin_path: &bin,
            sig_path: None,
            cert_path: None,
            repo_root: &dir,
            repo_slug: "rjwalters/loom",
            tag: "v0.16.0",
            cosign_pubkey_env: None,
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        }));
    });
    let result = result.unwrap();
    // On a REAL macOS host with codesign at /usr/bin/codesign this would
    // instead exercise the real tool against a plain-bytes fake binary (which
    // codesign also reports as unsigned) -- either way the outcome must never
    // be `Failed`.
    assert_ne!(result.outcome, Outcome::Failed);
}
