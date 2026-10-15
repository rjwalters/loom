//! #11972: a Judge pass that skipped on pending CI releases its lease without
//! recording a no-verdict pass, and the `--skip-reason` flag that carries it.

use super::*;

/// Acceptance (#11972): a Judge pass that skipped because CI was pending
/// writes no no-verdict record, even when CI completed (green) between its
/// check and its release; without the skip reason the release re-reads CI and
/// records the pass, as before.
#[test]
fn a_ci_pending_skip_is_never_recorded_even_if_ci_finished_before_release() {
    let green = r#"{"total_count":1,"check_runs":[{"status":"completed","started_at":"2026-10-14T05:00:00Z"}]}"#;
    let judge = on("host-a", "run-1");
    // With the pass's own reason: nothing recorded, no PR or CI read.
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    waiting_pr(dir.path());
    fs::write(dir.path().join("checks"), green).unwrap();
    assert_eq!(ctx(dir.path(), judge.clone()).claim(), EXIT_HELD);
    // The claim's backstop check read the head; only the release's read counts.
    let _ = fs::remove_file(dir.path().join("pulled"));
    assert_eq!(
        ctx(dir.path(), judge.clone()).release(Some(loom_daemon::judge_backstop::SKIP_CI_PENDING)),
        EXIT_HELD
    );
    let patch = patched(dir.path());
    assert!(patch.contains(&format!("body={}", judge.released_marker())), "{patch}");
    assert!(!patch.contains("verdict=none"), "{patch}");
    assert!(!dir.path().join("pulled").exists(), "no PR head read for a ci-pending skip");
    assert_eq!(ctx(dir.path(), on("host-b", "run-2")).claim(), EXIT_HELD, "not held");
    // No reason (an older prompt), or one this binary does not know: the
    // default, CI re-read at release; green now, so the pass is recorded.
    for reason in [None, Some("other")] {
        let dir = TempDir::new().unwrap();
        fake_gh(dir.path());
        waiting_pr(dir.path());
        fs::write(dir.path().join("checks"), green).unwrap();
        assert_eq!(ctx(dir.path(), judge.clone()).claim(), EXIT_HELD);
        assert_eq!(ctx(dir.path(), judge.clone()).release(reason), EXIT_HELD);
        let patch = patched(dir.path());
        assert!(
            patch.contains(&judge.released_marker_with(&format!(
                " head={HEAD} verdict=none labels=review-requested"
            ))),
            "{reason:?}: {patch}"
        );
        assert_eq!(
            ctx(dir.path(), on("host-b", "run-2")).claim(),
            backstop::EXIT_BACKSTOP,
            "{reason:?}"
        );
    }
}

/// The `lease pr release` CLI takes `--skip-reason`, and omits it by default.
#[test]
fn release_parses_an_optional_skip_reason() {
    use clap::Parser;
    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        action: PrLeaseAction,
    }
    let parse = |args: &[&str]| match Cli::try_parse_from(args).unwrap().action {
        PrLeaseAction::Release(r) => (r.common.pr, r.skip_reason),
        _ => panic!("not a release"),
    };
    let base = [
        "lease",
        "release",
        "12",
        "--role",
        "judge",
        "--watch-pid",
        "7",
    ];
    assert_eq!(parse(&base), (12, None));
    let skipped: Vec<&str> = base
        .iter()
        .copied()
        .chain(["--skip-reason", "ci-pending"])
        .collect();
    assert_eq!(parse(&skipped), (12, Some("ci-pending".to_string())));
}
