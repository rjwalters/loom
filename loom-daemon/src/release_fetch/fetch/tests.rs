//! Tests for download + verify orchestration (epic #7810, PR 6a).
//!
//! Drives the real `fetch_and_verify()` against a FAKE `gh` on `PATH` --
//! the same technique `test-loom-daemon-update-fetch.sh`'s `write_fake_gh`
//! fixture uses at the shell level, so this is the equivalence proof at the
//! Rust layer. `PATH` is process-global, so every test here is `#[serial]`.

use super::*;
use serial_test::serial;
use std::io::Write as _;

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
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

fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "loom-daemon-fetch-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn with_fake_bin<F: FnOnce()>(bindir: &Path, f: F) {
    let old = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{old}", bindir.display()));
    // #10088: `gh` resolves through `LOOM_GH_BIN` (a loud-failing stub in
    // test builds), so point it at the fake explicitly.
    let gh = crate::gh_invocation::resolver::test_stub::GhBinGuard::set(&bindir.join("gh"));
    f();
    drop(gh);
    std::env::set_var("PATH", old);
}

/// Point `std::env::temp_dir()` at `dir` for the duration of `f`, restoring
/// the prior value after. nextest runs each test in its own process (see
/// `.config/nextest.toml`), but every one of those processes still shares the
/// real OS temp dir -- so a test that scans it for leaked entries can be
/// fooled by an unrelated sibling test process (e.g.
/// `successful_fetch_verifies_and_reports_the_artifact`, which legitimately
/// creates and holds its own `loom-daemon-fetch.*` scratch dir) creating
/// something there at the same moment. Overriding `TMPDIR` gives
/// [`ScratchDir::create`] a private root so the leak check only ever sees
/// this test's own directories.
fn with_tmp_dir<F: FnOnce()>(dir: &Path, f: F) {
    let old = std::env::var_os("TMPDIR");
    std::env::set_var("TMPDIR", dir);
    f();
    match old {
        Some(v) => std::env::set_var("TMPDIR", v),
        None => std::env::remove_var("TMPDIR"),
    }
}

/// A fake `gh` understanding exactly the two invocations this module makes:
///
/// * `gh release download <tag> -R <slug> -p <name> [-p <name> ...] -D <dir>
///   --clobber` -- copies matching files out of `assets_dir`, exiting 1 if
///   NONE of the `-p` patterns matched anything (mirrors real `gh`'s "no
///   assets match" failure).
/// * `gh release view <tag> --json assets -R <slug>` -- emits the release's
///   own asset list as the `{{"assets":[{{"name":…}}]}}` object real `gh`
///   emits for `--json assets` with no `--jq` (#8197).
///
/// The listing is `ls assets_dir` PLUS `extra_listed`, which is what lets a
/// test express the case this fixture exists for: an asset the release
/// **publishes** but that cannot be **downloaded**. `listing_fails` instead
/// makes every `release view` exit non-zero -- the "could not read the asset
/// list at all" case.
fn write_fake_gh_full(
    dir: &Path,
    assets_dir: &Path,
    extra_listed: &[&str],
    listing_fails: bool,
) -> PathBuf {
    write_script(
        dir,
        "gh",
        &format!(
            r#"ASSETS_DIR="{assets}"
EXTRA_LISTED="{extra}"
LISTING_FAILS="{fails}"
if [[ "$1" == "release" && "$2" == "view" ]]; then
    if [[ "$LISTING_FAILS" == "1" ]]; then
        echo "gh: could not read release metadata" >&2
        exit 1
    fi
    out='{{"assets":['
    first=1
    for n in $(ls "$ASSETS_DIR" 2>/dev/null) $EXTRA_LISTED; do
        [[ "$first" -eq 1 ]] || out+=','
        out+="{{\"name\":\"$n\"}}"
        first=0
    done
    out+=']}}'
    printf '%s\n' "$out"
    exit 0
fi
if [[ "$1" == "release" && "$2" == "download" ]]; then
    shift 2; shift
    dest="."
    patterns=()
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -p) patterns+=("$2"); shift 2 ;;
            -D) dest="$2"; shift 2 ;;
            -R) shift 2 ;;
            --clobber) shift ;;
            *) shift ;;
        esac
    done
    mkdir -p "$dest"
    copied=0
    for pat in "${{patterns[@]}}"; do
        for f in "$ASSETS_DIR"/$pat; do
            [[ -e "$f" ]] || continue
            cp "$f" "$dest/"
            copied=1
        done
    done
    [[ "$copied" -eq 1 ]] && exit 0 || exit 1
fi
exit 1
"#,
            assets = assets_dir.display(),
            extra = extra_listed.join(" "),
            fails = if listing_fails { "1" } else { "0" },
        ),
    )
}

/// The ordinary fixture: the release lists exactly what it can serve.
fn write_fake_gh(dir: &Path, assets_dir: &Path) -> PathBuf {
    write_fake_gh_full(dir, assets_dir, &[], false)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// AC1: a successful fetch downloads, checksum-verifies, and reports the
/// artifact -- WITHOUT ever needing a source build.
#[test]
#[serial]
fn successful_fetch_verifies_and_reports_the_artifact() {
    let dir = tempdir();
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    let bin_name = "loom-daemon-x86_64-unknown-linux-gnu";
    let bin_bytes = b"fake artifact bytes";
    std::fs::write(assets.join(bin_name), bin_bytes).unwrap();
    std::fs::write(
        assets.join(format!("{bin_name}.sha256")),
        format!("{}  {bin_name}\n", sha256_hex(bin_bytes)),
    )
    .unwrap();

    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);

    let inputs = FetchInputs {
        repo_root: &dir,
        target: "x86_64-unknown-linux-gnu",
        repo_slug: "test-owner/test-repo",
        tag: "v0.16.0",
        cosign_pubkey_env: None,
        cosign_identity_env: None,
        cosign_oidc_issuer_env: None,
    };

    let mut outcome = None;
    with_fake_bin(&fakebin, || {
        outcome = Some(fetch_and_verify(&inputs));
    });

    match outcome.unwrap() {
        FetchOutcome::Verified {
            artifact,
            checksum_line,
            ..
        } => {
            assert!(artifact.bin_path.is_file());
            assert!(checksum_line.contains("Checksum verified"));
            assert!(artifact.tmp_dir.is_dir(), "the scratch dir must survive a verified fetch");
            // No `.sig` was published -- the linux branch's silent skip.
            assert_eq!(artifact.had_authority, None);
            // Clean up what `persist()` deliberately left behind.
            let _ = std::fs::remove_dir_all(&artifact.tmp_dir);
        }
        FetchOutcome::VerificationFailed { lines } => {
            panic!("expected Verified, got VerificationFailed: {lines:?}")
        }
        FetchOutcome::DownloadFailed(msg) => panic!("expected Verified, got DownloadFailed: {msg}"),
    }
}

/// AC2: a checksum mismatch aborts the WHOLE update -- never a soft
/// fallback -- and the scratch directory is NOT left behind (trapped, not
/// leaked, even inside this process's own lifetime).
#[test]
#[serial]
fn checksum_mismatch_fails_closed_and_removes_its_scratch_dir() {
    let dir = tempdir();
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    let bin_name = "loom-daemon-x86_64-unknown-linux-gnu";
    std::fs::write(assets.join(bin_name), b"fake artifact bytes").unwrap();
    // Deliberately WRONG checksum.
    std::fs::write(
        assets.join(format!("{bin_name}.sha256")),
        "0000000000000000000000000000000000000000000000000000000000000000  loom-daemon-x86_64-unknown-linux-gnu\n",
    )
    .unwrap();

    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);

    let inputs = FetchInputs {
        repo_root: &dir,
        target: "x86_64-unknown-linux-gnu",
        repo_slug: "test-owner/test-repo",
        tag: "v0.16.0",
        cosign_pubkey_env: None,
        cosign_identity_env: None,
        cosign_oidc_issuer_env: None,
    };

    // A private temp root, isolated via TMPDIR (see `with_tmp_dir`), so the
    // leak check below only ever sees scratch dirs this test itself created.
    let tmp_root = tempdir();

    let mut outcome = None;
    let before_entries: Vec<_> = std::fs::read_dir(&tmp_root)
        .map(|it| it.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    with_fake_bin(&fakebin, || {
        with_tmp_dir(&tmp_root, || {
            outcome = Some(fetch_and_verify(&inputs));
        });
    });

    match outcome.unwrap() {
        FetchOutcome::VerificationFailed { lines } => {
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("Checksum verification FAILED")),
                "{lines:?}"
            );
            assert!(lines.iter().any(|l| l.contains("left untouched")), "{lines:?}");
        }
        FetchOutcome::Verified { .. } => panic!("expected VerificationFailed, got Verified"),
        FetchOutcome::DownloadFailed(msg) => {
            panic!("expected VerificationFailed, got DownloadFailed: {msg}")
        }
    }

    // No NEW loom-daemon-fetch.* scratch dir survived the failure.
    let after_entries: Vec<_> = std::fs::read_dir(&tmp_root)
        .map(|it| it.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    let leaked = after_entries.iter().any(|p| {
        !before_entries.contains(p)
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("loom-daemon-fetch."))
    });
    assert!(!leaked, "a checksum-mismatch abort must not leak its scratch dir");
}

/// A download failure (no matching release asset) is reported distinctly
/// from a verification failure -- the caller reacts differently to each
/// (see the module docs).
#[test]
#[serial]
fn missing_release_asset_is_a_download_failure_not_a_verification_failure() {
    let dir = tempdir();
    let assets = dir.join("assets"); // empty -- nothing published
    std::fs::create_dir_all(&assets).unwrap();

    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);

    let inputs = FetchInputs {
        repo_root: &dir,
        target: "x86_64-unknown-linux-gnu",
        repo_slug: "test-owner/test-repo",
        tag: "v0.16.0",
        cosign_pubkey_env: None,
        cosign_identity_env: None,
        cosign_oidc_issuer_env: None,
    };

    let mut outcome = None;
    with_fake_bin(&fakebin, || {
        outcome = Some(fetch_and_verify(&inputs));
    });

    match outcome.unwrap() {
        FetchOutcome::DownloadFailed(msg) => assert!(msg.contains("Failed to download"), "{msg}"),
        FetchOutcome::Verified { .. } => panic!("expected DownloadFailed, got Verified"),
        FetchOutcome::VerificationFailed { lines } => {
            panic!("expected DownloadFailed, got VerificationFailed: {lines:?}")
        }
    }
}

// ---------------------------------------------------------------------------
// #8197: absent vs. unavailable signature material
//
// Both directions, deliberately: a fix in one direction alone would either
// break every unsigned (pre-signing) release or leave the downgrade open.
// ---------------------------------------------------------------------------

/// The binary + its `.sha256`, checksum-consistent, under `dir/assets`.
fn write_checksummed_assets(dir: &Path, bin_name: &str) -> PathBuf {
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    let bin_bytes = b"fake artifact bytes";
    std::fs::write(assets.join(bin_name), bin_bytes).unwrap();
    std::fs::write(
        assets.join(format!("{bin_name}.sha256")),
        format!("{}  {bin_name}\n", sha256_hex(bin_bytes)),
    )
    .unwrap();
    assets
}

fn linux_inputs<'a>(dir: &'a Path) -> FetchInputs<'a> {
    FetchInputs {
        repo_root: dir,
        target: "x86_64-unknown-linux-gnu",
        repo_slug: "test-owner/test-repo",
        tag: "v0.16.0",
        cosign_pubkey_env: None,
        cosign_identity_env: None,
        cosign_oidc_issuer_env: None,
    }
}

/// The bug this issue exists for: a release that DOES publish a `.sig` whose
/// download fails must be refused, not silently downgraded to checksum-only
/// verification. The publisher signed deliberately; dropping that signature
/// because a transfer failed is the one place "best effort" is the wrong
/// default.
#[test]
#[serial]
fn listed_but_unfetchable_sig_is_refused_not_downgraded_to_checksum_only() {
    let dir = tempdir();
    let bin_name = "loom-daemon-x86_64-unknown-linux-gnu";
    // The `.sig` is LISTED by the release but absent from the servable
    // assets, so every download attempt for it fails -- a 500, a timeout, a
    // truncated transfer, all indistinguishable from here.
    let assets = write_checksummed_assets(&dir, bin_name);
    let fakebin = tempdir();
    write_fake_gh_full(&fakebin, &assets, &[&format!("{bin_name}.sig")], false);

    let inputs = linux_inputs(&dir);
    let mut outcome = None;
    with_fake_bin(&fakebin, || {
        outcome = Some(fetch_and_verify(&inputs));
    });

    match outcome.unwrap() {
        FetchOutcome::VerificationFailed { lines } => {
            assert!(lines.iter().any(|l| l.contains("UNAVAILABLE, not absent")), "{lines:?}");
            assert!(lines.iter().any(|l| l.contains(&format!("{bin_name}.sig"))), "{lines:?}");
            assert!(lines.iter().any(|l| l.contains("left untouched")), "{lines:?}");
        }
        FetchOutcome::Verified { .. } => {
            panic!("a listed-but-unfetchable .sig must NOT verify checksum-only")
        }
        FetchOutcome::DownloadFailed(msg) => {
            panic!("expected VerificationFailed, got DownloadFailed: {msg}")
        }
    }
}

/// The other direction (#5054): a genuinely UNSIGNED release -- no `.sig`
/// listed at all -- still succeeds on the checksum alone, exactly as before.
/// Over-correcting here would break the update path for every pre-signing
/// release.
#[test]
#[serial]
fn unsigned_release_with_no_sig_listed_still_succeeds_checksum_only() {
    let dir = tempdir();
    let bin_name = "loom-daemon-x86_64-unknown-linux-gnu";
    let assets = write_checksummed_assets(&dir, bin_name);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets); // lists exactly what it serves: no `.sig`

    let inputs = linux_inputs(&dir);
    let mut outcome = None;
    with_fake_bin(&fakebin, || {
        outcome = Some(fetch_and_verify(&inputs));
    });

    match outcome.unwrap() {
        FetchOutcome::Verified {
            artifact,
            signature_state,
            ..
        } => {
            assert_eq!(signature_state, signature::SignatureState::Skipped);
            let _ = std::fs::remove_dir_all(&artifact.tmp_dir);
        }
        FetchOutcome::VerificationFailed { lines } => {
            panic!("an unsigned release must still fetch on its checksum alone: {lines:?}")
        }
        FetchOutcome::DownloadFailed(msg) => panic!("expected Verified, got DownloadFailed: {msg}"),
    }
}

/// When the asset list itself cannot be read, "unsigned" and "signature
/// download failed" stay indistinguishable -- so the artifact is refused
/// rather than accepted on an assumption. This costs one skipped update tick
/// (the running daemon keeps running and retries later); the opposite choice
/// would silently reinstate the downgrade under exactly the conditions an
/// attacker would arrange.
#[test]
#[serial]
fn unreadable_asset_list_with_unfetchable_sig_is_refused() {
    let dir = tempdir();
    let bin_name = "loom-daemon-x86_64-unknown-linux-gnu";
    let assets = write_checksummed_assets(&dir, bin_name);
    let fakebin = tempdir();
    write_fake_gh_full(&fakebin, &assets, &[], true); // `release view` always fails

    let inputs = linux_inputs(&dir);
    let mut outcome = None;
    with_fake_bin(&fakebin, || {
        outcome = Some(fetch_and_verify(&inputs));
    });

    match outcome.unwrap() {
        FetchOutcome::VerificationFailed { lines } => {
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("asset list could not be read")),
                "{lines:?}"
            );
        }
        FetchOutcome::Verified { .. } => {
            panic!("an unreadable asset list must not be read as 'unsigned'")
        }
        FetchOutcome::DownloadFailed(msg) => {
            panic!("expected VerificationFailed, got DownloadFailed: {msg}")
        }
    }
}

/// The `.pem` signing certificate gets the same treatment as the `.sig`: a
/// keyless release whose certificate is listed but unfetchable would
/// otherwise fall through to key mode and end as a loud skip -- the same
/// downgrade by a different door. A key-signed release (no `.pem` listed at
/// all) is untouched; that is the `unsigned_release_...` case's sibling and
/// is exercised by `linux_key_mode_*` in the signature suite.
#[test]
#[serial]
fn listed_but_unfetchable_cert_is_refused() {
    let dir = tempdir();
    let bin_name = "loom-daemon-x86_64-unknown-linux-gnu";
    let assets = write_checksummed_assets(&dir, bin_name);
    // The `.sig` IS servable; only its `.pem` sibling is listed-but-missing.
    std::fs::write(assets.join(format!("{bin_name}.sig")), b"sig").unwrap();
    let fakebin = tempdir();
    write_fake_gh_full(&fakebin, &assets, &[&format!("{bin_name}.pem")], false);

    let inputs = linux_inputs(&dir);
    let mut outcome = None;
    with_fake_bin(&fakebin, || {
        outcome = Some(fetch_and_verify(&inputs));
    });

    match outcome.unwrap() {
        FetchOutcome::VerificationFailed { lines } => {
            assert!(lines.iter().any(|l| l.contains(&format!("{bin_name}.pem"))), "{lines:?}");
        }
        FetchOutcome::Verified { .. } => {
            panic!("a listed-but-unfetchable .pem must not fall through to key mode")
        }
        FetchOutcome::DownloadFailed(msg) => {
            panic!("expected VerificationFailed, got DownloadFailed: {msg}")
        }
    }
}

/// A release whose signature material is both published AND fetchable, with
/// `cosign` present, reports `SIGNATURE=verified` -- the value that tells the
/// caller a verification actually ran, which no stdout key carried before
/// (#8197 AC3).
#[test]
#[serial]
fn fetched_and_verified_signature_reports_the_verified_state() {
    let dir = tempdir();
    let bin_name = "loom-daemon-x86_64-unknown-linux-gnu";
    let assets = write_checksummed_assets(&dir, bin_name);
    std::fs::write(assets.join(format!("{bin_name}.sig")), b"sig").unwrap();
    std::fs::write(assets.join(format!("{bin_name}.pem")), b"cert").unwrap();

    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    write_script(
        &fakebin,
        "cosign",
        "if [[ \"$1\" == version ]]; then exit 0; fi\nif [[ \"$1\" == verify-blob ]]; then exit 0; fi\nexit 1\n",
    );

    let inputs = linux_inputs(&dir);
    let mut outcome = None;
    with_fake_bin(&fakebin, || {
        outcome = Some(fetch_and_verify(&inputs));
    });

    match outcome.unwrap() {
        FetchOutcome::Verified {
            artifact,
            signature_state,
            signature_line,
            ..
        } => {
            assert_eq!(signature_state, signature::SignatureState::Verified);
            assert!(
                signature_line.contains("keyless signature verification passed"),
                "{signature_line}"
            );
            let _ = std::fs::remove_dir_all(&artifact.tmp_dir);
        }
        FetchOutcome::VerificationFailed { lines } => {
            panic!("expected Verified, got VerificationFailed: {lines:?}")
        }
        FetchOutcome::DownloadFailed(msg) => panic!("expected Verified, got DownloadFailed: {msg}"),
    }
}

// ---------------------------------------------------------------------------
// #10470: required-assurance policy
// ---------------------------------------------------------------------------

fn required() -> SignaturePolicy {
    SignaturePolicy {
        require_signature: true,
        approved_workflow: None,
    }
}

fn run_with(fakebin: &Path, inputs: &FetchInputs<'_>, policy: &SignaturePolicy) -> FetchOutcome {
    let mut outcome = None;
    with_fake_bin(fakebin, || {
        outcome = Some(fetch_and_verify_with_policy(inputs, policy));
    });
    outcome.unwrap()
}

fn expect_refusal(outcome: FetchOutcome) -> Vec<String> {
    match outcome {
        FetchOutcome::VerificationFailed { lines } => lines,
        FetchOutcome::Verified { artifact, .. } => {
            let _ = std::fs::remove_dir_all(&artifact.tmp_dir);
            panic!("expected a refusal, got Verified")
        }
        FetchOutcome::DownloadFailed(m) => panic!("expected a refusal, got DownloadFailed: {m}"),
    }
}

fn signed_assets(dir: &Path, bin_name: &str, pem: bool) -> PathBuf {
    let assets = write_checksummed_assets(dir, bin_name);
    std::fs::write(assets.join(format!("{bin_name}.sig")), b"sig").unwrap();
    if pem {
        std::fs::write(assets.join(format!("{bin_name}.pem")), b"cert").unwrap();
    }
    assets
}

const BIN: &str = "loom-daemon-x86_64-unknown-linux-gnu";

#[test]
fn policy_from_values_parses_flag_and_workflow() {
    assert_eq!(SignaturePolicy::from_values(None, None), SignaturePolicy::default());
    assert_eq!(SignaturePolicy::from_values(Some("0"), None), SignaturePolicy::default());
    assert_eq!(SignaturePolicy::from_values(Some(""), Some("  ")), SignaturePolicy::default());
    let p = SignaturePolicy::from_values(Some("1"), Some(" release.yml "));
    assert!(p.require_signature);
    assert_eq!(p.approved_workflow.as_deref(), Some("release.yml"));
}

#[test]
#[serial]
fn required_mode_refuses_an_unsigned_release_as_unsigned() {
    let dir = tempdir();
    let assets = write_checksummed_assets(&dir, BIN);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    let lines = expect_refusal(run_with(&fakebin, &linux_inputs(&dir), &required()));
    assert!(lines.iter().any(|l| l.contains("unsigned release")), "{lines:?}");
    assert!(!lines.iter().any(|l| l.contains("assurance unavailable")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("left untouched")), "{lines:?}");
}

#[test]
#[serial]
fn required_mode_refuses_missing_cosign_as_unavailable_not_tampering() {
    let dir = tempdir();
    let assets = signed_assets(&dir, BIN, true);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    // No fake cosign; route PATH through the fake dir plus a minimal system
    // path (as the signature tests do) so the `cosign version` probe fails.
    let mut outcome = None;
    with_fake_bin(&fakebin, || {
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", fakebin.display()));
        outcome = Some(fetch_and_verify_with_policy(&linux_inputs(&dir), &required()));
    });
    let lines = expect_refusal(outcome.unwrap());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("assurance unavailable (tooling)")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("NOT evidence of tampering")),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|l| l.contains("unsigned release")), "{lines:?}");
}

#[test]
#[serial]
fn required_mode_refuses_underivable_key_as_unavailable() {
    // Bare `.sig` (key mode) with no resolvable public key.
    let dir = tempdir();
    let assets = signed_assets(&dir, BIN, false);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    write_script(&fakebin, "cosign", "exit 0\n");
    let lines = expect_refusal(run_with(&fakebin, &linux_inputs(&dir), &required()));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("assurance unavailable (tooling)")),
        "{lines:?}"
    );
}

#[test]
#[serial]
fn required_mode_leaves_an_invalid_signature_classified_as_invalid() {
    let dir = tempdir();
    let assets = signed_assets(&dir, BIN, true);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    write_script(&fakebin, "cosign", "if [[ \"$1\" == version ]]; then exit 0; fi\nexit 1\n");
    let lines = expect_refusal(run_with(&fakebin, &linux_inputs(&dir), &required()));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("signature verification FAILED")),
        "{lines:?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("Required signature assurance")),
        "{lines:?}"
    );
}

#[test]
#[serial]
fn required_mode_passes_a_verified_signature_and_emits_sanitized_evidence() {
    let dir = tempdir();
    let assets = signed_assets(&dir, BIN, true);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    write_script(&fakebin, "cosign", "exit 0\n");
    let policy = SignaturePolicy {
        require_signature: true,
        approved_workflow: Some("release.yml".to_string()),
    };
    match run_with(&fakebin, &linux_inputs(&dir), &policy) {
        FetchOutcome::Verified {
            artifact,
            signature_state,
            signature_line,
            ..
        } => {
            assert_eq!(signature_state, signature::SignatureState::Verified);
            let ev = signature_line
                .lines()
                .find(|l| l.starts_with("LOOM_SIGNATURE_EVIDENCE "))
                .unwrap_or_else(|| panic!("no evidence line: {signature_line}"));
            let v: serde_json::Value =
                serde_json::from_str(ev.trim_start_matches("LOOM_SIGNATURE_EVIDENCE ")).unwrap();
            assert_eq!(v["tag"], "v0.16.0");
            assert_eq!(v["signature_state"], "verified");
            assert_eq!(v["policy_revision"], "release.yml");
            assert_eq!(v["asset_sha256"], sha256_hex(b"fake artifact bytes"));
            assert!(v["identity_regexp"]
                .as_str()
                .unwrap()
                .contains(r"workflows/release\.yml@"));
            let _ = std::fs::remove_dir_all(&artifact.tmp_dir);
        }
        FetchOutcome::VerificationFailed { lines } => panic!("expected Verified: {lines:?}"),
        FetchOutcome::DownloadFailed(m) => panic!("expected Verified: {m}"),
    }
}

/// A valid signature from a workflow that is not the approved one is refused:
/// the fake cosign only accepts an identity regexp naming `release.yml`.
#[test]
#[serial]
fn required_mode_refuses_a_valid_signature_from_an_unapproved_workflow() {
    let dir = tempdir();
    let assets = signed_assets(&dir, BIN, true);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    // Models a certificate issued to `other.yml`: verify-blob succeeds only
    // if the expected identity regexp matches that workflow path.
    write_script(
        &fakebin,
        "cosign",
        r#"if [[ "$1" == version ]]; then exit 0; fi
for a in "$@"; do
  if [[ "$a" == *'workflows/other\.yml@'* || "$a" == *'workflows/[^@]+@'* ]]; then exit 0; fi
done
exit 1
"#,
    );
    let pinned = SignaturePolicy {
        require_signature: true,
        approved_workflow: Some("release.yml".to_string()),
    };
    let lines = expect_refusal(run_with(&fakebin, &linux_inputs(&dir), &pinned));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("signature verification FAILED")),
        "{lines:?}"
    );

    // Same artifact, unpinned default: the wildcard accepts it (unchanged).
    match run_with(&fakebin, &linux_inputs(&dir), &required()) {
        FetchOutcome::Verified { artifact, .. } => {
            let _ = std::fs::remove_dir_all(&artifact.tmp_dir);
        }
        _ => panic!("unpinned identity must still accept"),
    }
}

#[test]
#[serial]
fn default_policy_still_accepts_unsigned_and_prints_no_evidence() {
    let dir = tempdir();
    let assets = write_checksummed_assets(&dir, BIN);
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);
    match run_with(&fakebin, &linux_inputs(&dir), &SignaturePolicy::default()) {
        FetchOutcome::Verified {
            artifact,
            signature_state,
            signature_line,
            ..
        } => {
            assert_eq!(signature_state, signature::SignatureState::Skipped);
            assert!(signature_line.is_empty(), "{signature_line}");
            let _ = std::fs::remove_dir_all(&artifact.tmp_dir);
        }
        _ => panic!("present-only mode must accept an unsigned release"),
    }
}

/// Ordering regression: no candidate `--version` (or any execution of the
/// downloaded binary) before verification passes. The candidate is a script
/// that drops a marker file when run.
#[test]
#[serial]
fn candidate_is_never_executed_before_verification_passes() {
    let dir = tempdir();
    let marker = dir.join("candidate-ran");
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    let body = format!("#!/bin/sh\ntouch {}\necho 'loom-daemon 0.0.1'\n", marker.display());
    std::fs::write(assets.join(BIN), body.as_bytes()).unwrap();
    std::fs::write(
        assets.join(format!("{BIN}.sha256")),
        format!("{}  {BIN}\n", sha256_hex(body.as_bytes())),
    )
    .unwrap();
    let fakebin = tempdir();
    write_fake_gh(&fakebin, &assets);

    // Unsigned + required: refused, candidate never ran.
    let _ = expect_refusal(run_with(&fakebin, &linux_inputs(&dir), &required()));
    assert!(!marker.exists(), "candidate ran before verification passed");

    // Invalid signature: refused, candidate never ran.
    std::fs::write(assets.join(format!("{BIN}.sig")), b"sig").unwrap();
    std::fs::write(assets.join(format!("{BIN}.pem")), b"cert").unwrap();
    write_script(&fakebin, "cosign", "if [[ \"$1\" == version ]]; then exit 0; fi\nexit 1\n");
    let _ = expect_refusal(run_with(&fakebin, &linux_inputs(&dir), &SignaturePolicy::default()));
    assert!(!marker.exists(), "candidate ran before verification passed");

    // Control: once verification passes, the post-verification identity query runs it.
    write_script(&fakebin, "cosign", "exit 0\n");
    if let FetchOutcome::Verified { artifact, .. } =
        run_with(&fakebin, &linux_inputs(&dir), &required())
    {
        assert!(marker.exists(), "control: --version should run after verification");
        let _ = std::fs::remove_dir_all(&artifact.tmp_dir);
    } else {
        panic!("control run should verify");
    }
}
