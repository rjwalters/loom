//! The roll verdict's running-version basis (Issue #10710).
//!
//! `with_running_basis` is pure, so the decision matrix (running x on-disk x
//! newest release) is pinned with plain values, including the two platform
//! failure shapes: macOS reading the swapped-in NEW file, and Linux getting
//! ` (deleted)` (no on-disk answer at all). The tick-level tests pin the two
//! loops this must not create.

use super::*;
use crate::auto_update::native_probe::{staged_note, with_running_basis};

/// A resolution as `native_resolution` builds it: the release, the on-disk
/// probe (`None` = Linux's deleted inode), re-based on `running`.
fn probed(
    running: &str,
    on_disk: Option<&str>,
    release: &str,
    asset_sha: &str,
    disk_sha: Option<&str>,
) -> ArtifactInfo {
    with_running_basis(artifact(release, on_disk, Some(asset_sha), disk_sha), running)
}

fn newer(installed: &str, artifact: &str) -> ArtifactVerdict {
    ArtifactVerdict::Newer {
        installed: Some(installed.to_string()),
        artifact: artifact.to_string(),
    }
}

#[test]
fn the_verdict_compares_the_running_version_not_the_on_disk_probe() {
    // (running, on disk, release, asset sha, on-disk sha) -> verdict kind.
    let staged = probed("0.19.827", Some("0.19.831"), "0.19.831", SHA_A, Some(SHA_A));
    // macOS after an abandoned drain: the new file is on disk, the old process
    // runs. Pre-#10710 this read `UpToDate` and never re-armed.
    assert_eq!(classify_artifact(&staged), newer("0.19.827", "0.19.831"));
    assert_eq!(staged.on_disk_version.as_deref(), Some("0.19.831"));
    // Linux: /proc/self/exe is ` (deleted)`, so no probe and no sha.
    let deleted = probed("0.19.827", None, "0.19.831", SHA_A, None);
    assert_eq!(classify_artifact(&deleted), newer("0.19.827", "0.19.831"));
    // ...and once the running version IS the release, nothing to do — a
    // missing on-disk sha must not invent a `ShaDiffers` refetch.
    let deleted_current = probed("0.19.831", None, "0.19.831", SHA_A, None);
    assert!(matches!(classify_artifact(&deleted_current), ArtifactVerdict::UpToDate { .. }));
    // A newer build staged over a current process: its sha is some other
    // build's, so it must not drive a `ShaDiffers` overwrite of that file.
    let ahead = probed("0.19.831", Some("0.19.832"), "0.19.831", SHA_A, Some(SHA_B));
    assert!(matches!(classify_artifact(&ahead), ArtifactVerdict::UpToDate { .. }));
    assert_eq!(ahead.installed_sha256, None);
    // A dev build newer than the release: never a downgrade roll.
    let dev = probed("0.19.900", Some("0.19.900"), "0.19.831", SHA_A, Some(SHA_B));
    assert!(matches!(classify_artifact(&dev), ArtifactVerdict::StaleRepo { .. }));
}

#[test]
fn running_equal_to_on_disk_is_byte_for_byte_todays_verdict() {
    // The common case (and the "no floor set" acceptance criterion): when the
    // file on disk IS the running version, re-basing changes nothing but the
    // diagnostic field.
    for (version, release, asset, disk) in [
        ("0.19.831", "0.19.831", SHA_A, SHA_A), // up to date
        ("0.19.831", "0.19.831", SHA_A, SHA_B), // ShaDiffers (re-sign guard path)
        ("0.19.820", "0.19.831", SHA_A, SHA_B), // stale -> Newer
        ("0.19.840", "0.19.831", SHA_A, SHA_B), // StaleRepo
    ] {
        let probe = artifact(release, Some(version), Some(asset), Some(disk));
        let rebased = with_running_basis(probe.clone(), version);
        assert_eq!(
            classify_artifact(&rebased),
            classify_artifact(&probe),
            "{version} vs {release}"
        );
        assert_eq!(rebased.installed_sha256, probe.installed_sha256);
        assert_eq!(staged_note(&rebased), "");
    }
}

#[test]
fn the_staged_binary_is_named_in_the_fetch_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let art = resolved(probed("0.19.827", Some("0.19.831"), "0.19.831", SHA_A, Some(SHA_A)));
    let d = st.decide(Instant::now(), &inputs(&art, &stale("c1"), true, 0), Duration::ZERO, DEFER);
    match d {
        TickDecision::FetchArtifact { version, why, .. } => {
            assert_eq!(version, "0.19.831");
            assert!(why.contains("> installed 0.19.827"), "{why}");
            assert!(why.contains("on disk: 0.19.831, staged but not running"), "{why}");
        }
        other => panic!("expected the roll to re-arm, got {other:?}"),
    }
}

#[test]
fn running_the_newest_release_never_rolls_even_when_macos_re_signed_it() {
    // After the re-armed roll lands: running == on disk == release, but macOS
    // re-signed the file so its sha differs. `already_converged` must still
    // hold — the running basis must not turn this into a restart loop.
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let now = Instant::now();
    let info = probed("0.19.831", Some("0.19.831"), "0.19.831", SHA_A, Some(SHA_B));
    st.record_artifact_roll(now, &RebuildOutcome::Success, true, &info);
    for _ in 0..3 {
        let d = st.decide(
            now,
            &inputs(&resolved(info.clone()), &stale("c1"), true, 0),
            Duration::ZERO,
            DEFER,
        );
        assert!(
            matches!(d, TickDecision::Skip(ref r) if r.contains("not re-fetching")),
            "got {d:?}"
        );
    }
    // And with matching bytes it is plainly up to date, tick after tick.
    let current = resolved(probed("0.19.831", Some("0.19.831"), "0.19.831", SHA_A, Some(SHA_A)));
    for _ in 0..3 {
        let d = st.decide(now, &inputs(&current, &stale("c1"), true, 0), Duration::ZERO, DEFER);
        assert!(matches!(d, TickDecision::Skip(ref r) if r.contains("up to date")), "got {d:?}");
    }
}
