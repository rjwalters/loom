//! #10089: the review-conflict and merge-sequence passes re-read per-PR
//! facts every tick. N unchanged passes now cost one read per fact, and a
//! version move (`updatedAt`) costs exactly one more.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::claim_reconciliation::merge_sequence::{
    reconcile_merge_sequences, MERGE_SEQUENCE_ENABLED_ENV, SEQUENCE_LABEL,
};
use crate::claim_reconciliation::review_conflict::{
    reconcile_review_conflicts, REVIEW_CONFLICT_ENABLED_ENV,
};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tempfile::tempdir;

/// A trusted (bot-authored) comment row carrying `body`.
fn trusted_comment(body: &str) -> String {
    format!(
        r#"[{{"user":{{"login":"loom-fleet-dispatch[bot]","type":"Bot"}},"author_association":"NONE","created_at":"2026-10-03T09:00:00Z","body":"{body}"}}]"#
    )
}

/// A fake `gh` that logs every argv, then runs `script_body`.
fn fake_gh(dir: &Path, script_body: &str) -> (PathBuf, PathBuf) {
    let log = dir.join("gh.log");
    let bin = dir.join("fake-gh-passes.sh");
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

/// Run `f` with this thread's caching on and `env` cleared, restoring both.
fn cached_without(env: &str, f: impl FnOnce()) {
    let prev = std::env::var(env).ok();
    std::env::remove_var(env);
    set_test_enabled(true);
    f();
    set_test_enabled(false);
    if let Some(v) = prev {
        std::env::set_var(env, v);
    }
}

/// A mergeable PR carrying a `loom:merge-conflict` label this pass did NOT
/// apply (a Judge verdict is the newest state marker) used to cost one
/// paginated comments walk per tick, forever.
#[test]
#[serial]
fn a_foreign_conflict_label_is_scanned_once_per_version() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let updated = dir.path().join("updated");
    std::fs::write(&updated, "2026-10-03T10:00:00Z").unwrap();
    let comments = trusted_comment("<!-- loom:verdict-sha sha=abc verdict=changes-requested -->");
    let body = format!(
        r#"if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo "[{{\"number\":9,\"headRefOid\":\"abc\",\"mergeable\":\"MERGEABLE\",\"updatedAt\":\"$(cat '{u}')\",\"labels\":[{{\"name\":\"loom:merge-conflict\"}},{{\"name\":\"loom:changes-requested\"}}]}}]"
  exit 0
fi
if [ "$1" = "api" ]; then echo '{comments}'; fi"#,
        u = updated.display()
    );
    let (gh, log) = fake_gh(dir.path(), &body);

    cached_without(REVIEW_CONFLICT_ENABLED_ENV, || {
        for _ in 0..3 {
            let stats = reconcile_review_conflicts(&gh, &root);
            assert_eq!((stats.checked, stats.cleared), (1, 0));
        }
        assert_eq!(calls_matching(&log, "issues/9/comments"), 1, "3 passes, 1 walk");

        // New activity bumps `updatedAt`: exactly one re-scan.
        std::fs::write(&updated, "2026-10-03T10:05:00Z").unwrap();
        reconcile_review_conflicts(&gh, &root);
        reconcile_review_conflicts(&gh, &root);
        assert_eq!(calls_matching(&log, "issues/9/comments"), 2);
    });
    assert_eq!(calls_matching(&log, "pr edit"), 0, "a foreign label is never cleared");
}

/// A sequence holder's marker scan and its predecessor's `pulls/N` read
/// were both repeated every tick for every hold.
#[test]
#[serial]
fn an_unchanged_hold_reads_its_marker_and_predecessor_once() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let sha = |n: u32| format!("{n:040x}");
    let updated = dir.path().join("updated");
    std::fs::write(&updated, "2026-10-04T00:00:00Z").unwrap();
    let row = |n: u32, labels: &str| {
        format!(
            r#"{{\"number\":{n},\"createdAt\":\"2026-10-0{n}T00:00:00Z\",\"updatedAt\":\"$(cat '{u}')\",\"headRefOid\":\"{s}\",\"headRefName\":\"feature/issue-{n}\",\"baseRefName\":\"main\",\"isDraft\":false,\"labels\":{labels}}}"#,
            u = updated.display(),
            s = sha(n)
        )
    };
    let held = format!(r#"[{{\"name\":\"{SEQUENCE_LABEL}\"}}]"#);
    let marker = format!(
        "<!-- loom:sequence after=1 pred_head={} follower_head={} plan=p1 source=pass -->",
        sha(1),
        sha(2)
    );
    let body = format!(
        r#"if [ "$1" = "pr" ] && [ "$2" = "list" ]; then echo "[{r1},{r2},{r3}]"; exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then echo '{{"files":[{{"path":"src/a.rs"}}]}}'; exit 0; fi
case "$*" in
  *issues/2/comments*) echo '{c2}' ;;
  *pulls/1*) echo '{{"state":"open","merged":false,"head":{{"sha":"{s1}"}}}}' ;;
  *api*) echo '[]' ;;
esac"#,
        r1 = row(1, "[]"),
        r2 = row(2, &held),
        r3 = row(3, "[]"),
        c2 = trusted_comment(&marker),
        s1 = sha(1),
    );
    let (gh, log) = fake_gh(dir.path(), &body);

    cached_without(MERGE_SEQUENCE_ENABLED_ENV, || {
        for _ in 0..3 {
            let stats = reconcile_merge_sequences(&gh, &root);
            assert_eq!(stats.checked, 3);
            assert_eq!(stats.held, 1, "the hold stays in place");
        }
        assert_eq!(calls_matching(&log, "issues/2/comments"), 1, "3 passes, 1 marker walk");
        assert_eq!(calls_matching(&log, "pulls/1"), 1, "3 passes, 1 predecessor read");

        // Activity on both PRs: exactly one more read of each.
        std::fs::write(&updated, "2026-10-04T00:05:00Z").unwrap();
        reconcile_merge_sequences(&gh, &root);
        reconcile_merge_sequences(&gh, &root);
        assert_eq!(calls_matching(&log, "issues/2/comments"), 2);
        assert_eq!(calls_matching(&log, "pulls/1"), 2);
    });
}

/// A predecessor that left the open listing (merged / closed) is always read
/// live — its cached state must never outlive the listing that keyed it.
/// Driven through `reconcile_merge_sequences`, so a regression in
/// `predecessor()`'s key construction (not just `get_or(None)`) is caught.
#[test]
#[serial]
fn a_predecessor_outside_the_listing_is_never_served_from_cache() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let sha = |n: u32| format!("{n:040x}");
    let row = |n: u32, labels: &str| {
        format!(
            r#"{{\"number\":{n},\"createdAt\":\"2026-10-0{n}T00:00:00Z\",\"updatedAt\":\"2026-10-04T00:00:00Z\",\"headRefOid\":\"{s}\",\"headRefName\":\"feature/issue-{n}\",\"baseRefName\":\"main\",\"isDraft\":false,\"labels\":{labels}}}"#,
            s = sha(n)
        )
    };
    let held = format!(r#"[{{\"name\":\"{SEQUENCE_LABEL}\"}}]"#);
    let marker = format!(
        "<!-- loom:sequence after=1 pred_head={} follower_head={} plan=p1 source=pass -->",
        sha(1),
        sha(2)
    );
    // PR 1 is in the listing only while `$DIR/listed` exists; `pulls/1`
    // answers merged only while `$DIR/merged` exists (open otherwise).
    let body = format!(
        r#"if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  if [ -f '{d}/listed' ]; then echo "[{r1},{r2},{r3},{r4}]"; else echo "[{r2},{r3},{r4}]"; fi
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then echo '{{"files":[{{"path":"src/a.rs"}}]}}'; exit 0; fi
case "$*" in
  *issues/2/comments*) echo '{c2}' ;;
  *pulls/1*)
    if [ -f '{d}/merged' ]; then echo '{{"state":"closed","merged":true,"head":{{"sha":"{s1}"}}}}'
    else echo '{{"state":"open","merged":false,"head":{{"sha":"{s1}"}}}}'; fi ;;
  *api*) echo '[]' ;;
esac"#,
        d = dir.path().display(),
        r1 = row(1, "[]"),
        r2 = row(2, &held),
        r3 = row(3, "[]"),
        r4 = row(4, "[]"),
        c2 = trusted_comment(&marker),
        s1 = sha(1),
    );
    let (gh, log) = fake_gh(dir.path(), &body);
    let flag = |name: &str, on: bool| {
        let p = dir.path().join(name);
        if on {
            std::fs::write(p, "").unwrap();
        } else {
            let _ = std::fs::remove_file(p);
        }
    };

    cached_without(MERGE_SEQUENCE_ENABLED_ENV, || {
        // Unlisted + merged: every pass reads `pulls/1` live and releases.
        flag("merged", true);
        for pass in 1..=3 {
            let stats = reconcile_merge_sequences(&gh, &root);
            assert_eq!(stats.released, 1, "pass {pass}: the hold is released");
            assert_eq!(calls_matching(&log, "pulls/1"), pass, "pass {pass}: one live read");
        }

        // Listed + open: cached by `updatedAt@head` after the first read.
        flag("merged", false);
        flag("listed", true);
        let before = calls_matching(&log, "pulls/1");
        for _ in 0..3 {
            let stats = reconcile_merge_sequences(&gh, &root);
            assert_eq!((stats.released, stats.held), (0, 1));
        }
        assert_eq!(calls_matching(&log, "pulls/1"), before + 1, "listed: read once");

        // Dropped from the listing and merged: the cached "open" must not be
        // served — a live re-read releases the hold.
        flag("listed", false);
        flag("merged", true);
        let stats = reconcile_merge_sequences(&gh, &root);
        assert_eq!(stats.released, 1, "an unlisted predecessor is re-read live");
        assert_eq!(calls_matching(&log, "pulls/1"), before + 2);
    });
    assert_eq!(HOLD_MARKER.max_age, CLAIM_MAX_AGE);
    assert_eq!(PREDECESSOR.max_age, CLAIM_MAX_AGE);
    assert_eq!(CONFLICT_FLAG_OURS.max_age, CLAIM_MAX_AGE);
}
