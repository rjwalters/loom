//! #9548: the daemon's verdict backstop reads `loom:verdict-sha` markers only
//! from trusted authors. End-to-end through `forge::reconcile_pr_verdicts`
//! with a fake `gh`, in its own sibling file because neither this module nor
//! `tests.rs` has ratchet headroom (scripts/file-size-baseline.txt).

use super::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

const SHA_A: &str = "1111111111111111111111111111111111111111";
const SHA_B: &str = "2222222222222222222222222222222222222222";

fn comment(login: &str, kind: &str, assoc: &str, sha: &str) -> String {
    format!(
        r#"{{"user":{{"login":"{login}","type":"{kind}"}},"author_association":"{assoc}","created_at":"2026-09-29T00:00:00Z","body":"ok\n\n<!-- loom:verdict-sha sha={sha} verdict=approved -->"}}"#
    )
}

/// PR #300 carries `loom:pr` at head `SHA_B`; its comment listing is
/// `comments` (a JSON array).
fn reconcile(comments: &str) -> VerdictReconcileStats {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let listing = dir.path().join("comments.json");
    std::fs::write(&listing, comments).unwrap();
    let gh = dir.path().join("fake-gh.sh");
    std::fs::write(
        &gh,
        format!(
            r#"#!/usr/bin/env bash
case "$*" in
  "pr list "*"--label loom:pr "*)
    echo '[{{"number":300,"headRefOid":"{SHA_B}","labels":[{{"name":"loom:pr"}}]}}]' ;;
  "pr list "*) echo '[]' ;;
  "api repos/{{owner}}/{{repo}}/issues/300/comments"*) cat "{listing}" ;;
  "api "*compare/*) echo '{{"status":"ahead","files":[{{"filename":"src/lib.rs"}}]}}' ;;
  *) echo '{{}}' ;;
esac
"#,
            listing = listing.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var(VERDICT_ANCHOR_ENABLED_ENV, "0");
    let stats = forge::reconcile_pr_verdicts(&gh, &root);
    std::env::remove_var(VERDICT_ANCHOR_ENABLED_ENV);
    stats
}

#[test]
#[serial]
fn an_untrusted_marker_for_the_current_head_cannot_keep_a_stale_approval() {
    // The fleet approved SHA_A; the head moved to SHA_B. Later markers
    // naming SHA_B from an outsider, a contributor, a user squatting the
    // fleet App's bare name, and another fleet's App are all prose.
    let listing = format!(
        "[{},{},{},{},{}]",
        comment("loom-fleet-dispatch[bot]", "Bot", "NONE", SHA_A),
        comment("drive-by", "User", "NONE", SHA_B),
        comment("merged-once", "User", "CONTRIBUTOR", SHA_B),
        comment("loom-fleet-dispatch", "User", "NONE", SHA_B),
        comment("other-fleet-dispatch[bot]", "Bot", "NONE", SHA_B),
    );
    let stats = reconcile(&listing);
    assert_eq!(stats.invalidated, 1, "{stats:?}");
}

#[test]
#[serial]
fn only_untrusted_markers_read_as_no_marker() {
    let listing = format!("[{}]", comment("drive-by", "User", "NONE", SHA_B));
    let stats = reconcile(&listing);
    assert_eq!((stats.invalidated, stats.unverifiable), (0, 1), "{stats:?}");
}

#[test]
#[serial]
fn an_insiders_marker_still_counts() {
    let listing = format!("[{}]", comment("maintainer", "User", "COLLABORATOR", SHA_B));
    let stats = reconcile(&listing);
    assert_eq!((stats.invalidated, stats.unverifiable), (0, 0), "{stats:?}");
}
