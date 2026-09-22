//! Wrong-repo-resolution behaviour at the tick level (Issue #8513).
//!
//! `super::super::stale_repo` unit-tests the streak and the WARN wording in
//! isolation; what is here is the part only the tick can answer — that an
//! older resolved release takes the WARN path instead of the soft "nothing to
//! do" skip, that the streak survives across ticks and drops on any other
//! outcome, and that the roll pins the repo it resolved.

use super::*;

#[test]
fn test_classify_older_release_is_stale_repo_not_up_to_date() {
    // The pre-#8513 behaviour folded this into `UpToDate`, which is what made
    // a wrong repository indistinguishable from a healthy host.
    let verdict = classify_artifact(&artifact("0.19.0", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));
    assert_eq!(
        verdict,
        ArtifactVerdict::StaleRepo {
            artifact: "0.19.0".to_string(),
            installed: "0.19.21".to_string(),
            repo: "test-owner/test-repo".to_string(),
        }
    );
}

#[test]
fn test_an_older_release_is_not_actionable() {
    // `is_actionable` gates whether the loop treats the tick as work. A
    // wrong-repo resolution is loud, but it is still nothing to fetch.
    let art = resolved(artifact("0.1.0", Some("0.19.24"), Some(SHA_A), Some(SHA_B)));
    assert!(!art.is_actionable());
}

#[test]
fn test_decide_older_release_warns_instead_of_the_soft_skip() {
    // Must surface as `SkipWarn`, naming the repo and the wrong-repo
    // suspicion — never the plain `Skip` an operator reads past as "nothing
    // to fetch, all fine".
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let art = resolved(ArtifactInfo {
        repo: "consumer-owner/consumer-repo".to_string(),
        ..artifact("0.1.0", Some("0.19.24"), Some(SHA_A), Some(SHA_B))
    });
    let d = st.decide(
        Instant::now(),
        &inputs(&art, &stale("c1"), true, 0),
        Duration::from_secs(0),
        DEFER,
    );
    match d {
        TickDecision::SkipWarn(reason) => {
            assert!(reason.contains("consumer-owner/consumer-repo"), "{reason}");
            assert!(reason.contains("OLDER"), "{reason}");
            assert!(reason.contains("wrong-repo"), "{reason}");
        }
        other => panic!("expected SkipWarn, got {other:?}"),
    }
}

#[test]
fn test_stale_repo_ticks_accumulate_and_reset_on_any_other_outcome() {
    // Feeds `loom-daemon health`'s "no progress for N ticks" surface: the
    // count must be of a CONSECUTIVE run, and must drop back to zero the
    // instant a tick resolves anything else.
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let stale_art = resolved(ArtifactInfo {
        repo: "consumer-owner/consumer-repo".to_string(),
        ..artifact("0.1.0", Some("0.19.24"), Some(SHA_A), Some(SHA_B))
    });

    for expected_ticks in 1..=3u32 {
        let _ = st.decide(
            Instant::now(),
            &inputs(&stale_art, &stale("c1"), true, 0),
            Duration::from_secs(0),
            DEFER,
        );
        let snap = st.snapshot(true, Utc::now(), "note".to_string(), &stale_art);
        assert_eq!(snap.stale_repo_ticks, expected_ticks);
        assert_eq!(snap.stale_repo.as_deref(), Some("consumer-owner/consumer-repo"));
    }

    let healthy_art = resolved(artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_A)));
    let _ = st.decide(
        Instant::now(),
        &inputs(&healthy_art, &stale("c1"), true, 0),
        Duration::from_secs(0),
        DEFER,
    );
    let snap = st.snapshot(true, Utc::now(), "note".to_string(), &healthy_art);
    assert_eq!(snap.stale_repo_ticks, 0);
    assert_eq!(snap.stale_repo, None);
}

#[test]
fn test_a_tick_with_no_artifact_at_all_also_clears_the_streak() {
    // "No artifact resolved" is not a stale-repo tick — a host that stops
    // resolving anything must not keep reporting a streak that has stopped
    // recurring.
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let stale_art = resolved(ArtifactInfo {
        repo: "consumer-owner/consumer-repo".to_string(),
        ..artifact("0.1.0", Some("0.19.24"), Some(SHA_A), Some(SHA_B))
    });
    let _ = st.decide(
        Instant::now(),
        &inputs(&stale_art, &stale("c1"), true, 0),
        Duration::from_secs(0),
        DEFER,
    );
    let none = ArtifactResolution::Unresolved("no releases yet".to_string());
    let _ = st.decide(
        Instant::now(),
        &inputs(&none, &stale("c1"), true, 0),
        Duration::from_secs(0),
        DEFER,
    );
    let snap = st.snapshot(true, Utc::now(), "note".to_string(), &none);
    assert_eq!(snap.stale_repo_ticks, 0);
    assert_eq!(snap.stale_repo, None);
}

#[test]
fn test_the_artifact_fetch_pins_the_child_to_the_resolved_repo() {
    // The script re-derives the release repo from its own cwd's `origin`, so
    // a daemon whose workspace is NOT the Loom checkout would resolve the
    // artifact out of Loom's releases and then ask the workspace's own
    // project to download it. `LOOM_DAEMON_UPDATE_GH_REPO` is the script's
    // documented override, so the roll passes the repo it resolved.
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("seen-repo");
    let s = write_fake_script(
        tmp.path(),
        &format!(
            "printf '%s' \"${{LOOM_DAEMON_UPDATE_GH_REPO:-<unset>}}\" > {}; exit 0",
            out.display()
        ),
    );
    assert_eq!(
        run_update_script_with(
            &s,
            tmp.path(),
            Duration::from_secs(10),
            false,
            &["--fetch"],
            Some("rjwalters/loom"),
        ),
        RebuildOutcome::Success
    );
    assert_eq!(fs::read_to_string(&out).unwrap(), "rjwalters/loom");
}

#[test]
fn test_the_source_rebuild_path_exports_no_repo_pin() {
    // A `cargo build` reads no releases at all, so the source path must not
    // start pinning an env var the script honours on other code paths.
    //
    // Compared against this process's OWN ambient value rather than a mutated
    // one: `set_var`/`remove_var` are process-global and this suite runs
    // threaded, so "the child inherited exactly what we have" is both
    // stronger and race-free.
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("seen-repo");
    let s = write_fake_script(
        tmp.path(),
        &format!(
            "printf '%s' \"${{LOOM_DAEMON_UPDATE_GH_REPO:-<unset>}}\" > {}; exit 0",
            out.display()
        ),
    );
    let ambient = std::env::var("LOOM_DAEMON_UPDATE_GH_REPO").unwrap_or_else(|_| "<unset>".into());
    let outcome = run_update_script(&s, tmp.path(), Duration::from_secs(10), false);
    assert_eq!(outcome, RebuildOutcome::Success);
    assert_eq!(fs::read_to_string(&out).unwrap(), ambient);
}
