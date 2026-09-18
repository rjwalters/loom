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
    f();
    std::env::set_var("PATH", old);
}

/// A fake `gh` understanding exactly `gh release download <tag> -R <slug>
/// -p <name> [-p <name> ...] -D <dir> --clobber` -- copies matching files
/// out of `assets_dir`, exiting 1 if NONE of the `-p` patterns matched
/// anything (mirrors real `gh`'s "no assets match" failure).
fn write_fake_gh(dir: &Path, assets_dir: &Path) -> PathBuf {
    write_script(
        dir,
        "gh",
        &format!(
            r#"ASSETS_DIR="{assets}"
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
            assets = assets_dir.display()
        ),
    )
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

    let mut outcome = None;
    let before_entries: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .map(|it| it.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    with_fake_bin(&fakebin, || {
        outcome = Some(fetch_and_verify(&inputs));
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
    let after_entries: Vec<_> = std::fs::read_dir(std::env::temp_dir())
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
