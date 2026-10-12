//! #10089: N unchanged reconciliation passes cost at most one read per
//! cached fact, and a version move costs exactly one more.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::claim_reconciliation::open_pr_listing::test_support::{
    listing, pulls_arm, pulls_arm_cmd, row,
};
use crate::claim_reconciliation::{forge, merge_sequence, VERDICT_STALENESS_ENABLED_ENV};
use serial_test::serial;
use std::cell::Cell;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tempfile::tempdir;

const SHA: &str = "1111111111111111111111111111111111111111";

/// Enable this thread's caching for `f`, always switching it back off.
fn cached<T>(f: impl FnOnce() -> T) -> T {
    set_test_enabled(true);
    let out = f();
    set_test_enabled(false);
    out
}

#[test]
fn a_found_answer_is_reused_and_a_missing_one_is_not() {
    let cache: ReadCache<u32> = ReadCache::new(MAX_AGE);
    let calls = Cell::new(0);
    let fetch = |v: Option<u32>| {
        calls.set(calls.get() + 1);
        v
    };
    cached(|| {
        let root = Path::new("/r");
        let k = || key(root, 1, "x", Some("v1"));
        assert_eq!(cache.get_or(k(), || fetch(Some(7))), Some(7));
        assert_eq!(cache.get_or(k(), || fetch(Some(8))), Some(7), "served from cache");
        assert_eq!(calls.get(), 1);

        // A new version re-reads.
        assert_eq!(cache.get_or(key(root, 1, "x", Some("v2")), || fetch(Some(9))), Some(9));
        assert_eq!(calls.get(), 2);

        // `None` (failed or absent) is never stored.
        let miss = || key(root, 2, "x", Some("v1"));
        assert_eq!(cache.get_or(miss(), || fetch(None)), None);
        assert_eq!(cache.get_or(miss(), || fetch(None)), None);
        assert_eq!(calls.get(), 4);

        // No version, no caching.
        assert_eq!(key(root, 3, "x", None), None);
        assert_eq!(key(root, 3, "x", Some("")), None);
    });
}

#[test]
fn an_expired_entry_is_re_read_under_the_same_version() {
    // A zero expiry: every entry is already past it.
    let cache: ReadCache<u32> = ReadCache::new(Duration::ZERO);
    let calls = Cell::new(0);
    cached(|| {
        let k = || key(Path::new("/r"), 1, "x", Some("v1"));
        for _ in 0..3 {
            cache.get_or(k(), || {
                calls.set(calls.get() + 1);
                Some(1)
            });
        }
    });
    assert_eq!(calls.get(), 3, "an expired entry is never served");
}

#[test]
fn the_claim_caches_expire_within_about_one_tick() {
    // `updatedAt` can lag the timeline endpoint (1 s granularity), so a
    // stale claim heartbeat must not outlive ~one tick (#10096 review).
    assert!(CLAIM_MAX_AGE <= Duration::from_secs(15 * 60));
    assert!(CLAIM_MAX_AGE >= Duration::from_secs(10 * 60), "still spans one 10-min tick");
    assert_eq!(CLAIM_LABELED.max_age, CLAIM_MAX_AGE);
    assert_eq!(CLAIM_ACTIVITY.max_age, CLAIM_MAX_AGE);
    assert_eq!(VERDICT_SCAN.max_age, MAX_AGE);
    assert_eq!(CHANGED_FILES.max_age, MAX_AGE);
}

#[test]
fn keys_are_off_unless_enabled() {
    set_test_enabled(false);
    assert_eq!(key(Path::new("/r"), 1, "x", Some("v")), None);
}

/// A fake `gh` that logs every argv and answers from files in `dir`, so a
/// test can move a PR's version between passes.
fn fake_gh(dir: &Path, script_body: &str) -> (PathBuf, PathBuf) {
    let log = dir.join("gh.log");
    let bin = dir.join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> '{}'\n{script_body}\nexit 0\n",
        log.display()
    );
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

fn calls_matching(log: &Path, needle: &str) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(needle))
        .count()
}

#[test]
#[serial]
fn unchanged_verdict_prs_are_scanned_once_across_passes() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let updated = dir.path().join("updated");
    std::fs::write(&updated, "2026-10-03T10:00:00Z").unwrap();
    // #10349: the REST listing; `updated_at` is re-read from the file.
    let listing_at = |at: &str| listing(&[row(42, &["loom:pr"]).sha(SHA).updated(at)]);
    let (a, b) = (listing_at("2026-10-03T10:00:00Z"), listing_at("2026-10-03T10:05:00Z"));
    let u = updated.display();
    let emit = format!("if grep -q 10:05 '{u}'; then echo '{b}'; else echo '{a}'; fi");
    let body = format!(
        r#"{pulls}if [ "$1" = "api" ]; then
  echo '[{{"user":{{"login":"loom-fleet-dispatch[bot]","type":"Bot"}},"author_association":"NONE","created_at":"2026-10-03T09:00:00Z","body":"LGTM <!-- loom:verdict-sha sha={SHA} verdict=approved -->"}}]'
fi"#,
        pulls = pulls_arm_cmd(&emit)
    );
    let (gh, log) = fake_gh(dir.path(), &body);

    std::env::set_var(VERDICT_STALENESS_ENABLED_ENV, "1");
    cached(|| {
        for _ in 0..3 {
            let stats = forge::reconcile_pr_verdicts(&gh, &root);
            assert_eq!((stats.checked, stats.invalidated), (1, 0));
        }
        assert_eq!(calls_matching(&log, "issues/42/comments"), 1, "3 unchanged passes, 1 scan");

        // A new comment bumps `updatedAt`: exactly one re-scan.
        std::fs::write(&updated, "2026-10-03T10:05:00Z").unwrap();
        forge::reconcile_pr_verdicts(&gh, &root);
        forge::reconcile_pr_verdicts(&gh, &root);
        assert_eq!(calls_matching(&log, "issues/42/comments"), 2);
    });
    std::env::remove_var(VERDICT_STALENESS_ENABLED_ENV);
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("comments?per_page=100 --paginate"),
        "the paginated walk asks for 100 per page"
    );
}

#[test]
#[serial]
fn unchanged_claimed_prs_read_their_label_timeline_once() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let pulls = pulls_arm(&[row(7, &["loom:reviewing"]).updated("2026-10-03T10:00:00Z")]);
    let body = format!(r#"{pulls}case "$*" in *timeline*) echo '"2026-10-03T09:30:00Z"' ;; esac"#);
    let (gh, log) = fake_gh(dir.path(), &body);
    cached(|| {
        for _ in 0..3 {
            let (checked, _) = forge::reconcile_pr_claims_report(&gh, &root, false);
            assert_eq!(checked, 1);
        }
    });
    assert_eq!(calls_matching(&log, "issues/7/timeline"), 1, "3 unchanged passes, 1 walk");
    // No activity was found, so that read is never cached (a failed read
    // looks the same and must not hide a heartbeat) — but it is bounded to
    // comments since the claim label.
    assert_eq!(calls_matching(&log, "issues/7/comments"), 3);
    assert_eq!(calls_matching(&log, "since=2026-10-03T09:30:00Z"), 3);
}

#[test]
#[serial]
fn unchanged_open_prs_read_their_changed_files_once() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let seq_row = |n: u32| {
        row(n, &[])
            .sha(SHA)
            .created(&format!("2026-10-0{n}T00:00:00Z"))
            .updated("2026-10-03T00:00:00Z")
    };
    let body = format!(
        r#"{}case "$*" in *'/files?'*) printf 'HTTP/2.0 200 OK\r\n\r\n'; echo '[{{"filename":"src/a.rs"}}]' ;; esac"#,
        pulls_arm(&[seq_row(3), seq_row(2), seq_row(1)])
    );
    let (gh, log) = fake_gh(dir.path(), &body);
    cached(|| {
        for _ in 0..3 {
            let report = merge_sequence::plan_report(&gh, &root).unwrap();
            assert_eq!(report.open_prs, 3);
        }
    });
    assert_eq!(calls_matching(&log, "/files?"), 3, "one files read per PR, not per pass");
    assert_eq!(calls_matching(&log, "--json files"), 0, "no GraphQL files read (#10382)");
}

#[test]
fn the_kill_switch_name_is_stable() {
    // Operators disable the caches by this name; renaming it is a breaking change.
    assert_eq!(READ_CACHE_ENV, "LOOM_RECONCILE_READ_CACHE");
}
