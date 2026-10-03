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
    // The #9709 probe spawns through the `gh_invocation` choke point (#9985),
    // whose resolver honours `LOOM_GH_BIN` rather than the injected `gh`.
    let prev_gh_bin = std::env::var_os("LOOM_GH_BIN");
    std::env::set_var("LOOM_GH_BIN", &gh);
    std::env::set_var(VERDICT_ANCHOR_ENABLED_ENV, "0");
    let stats = forge::reconcile_pr_verdicts(&gh, &root);
    std::env::remove_var(VERDICT_ANCHOR_ENABLED_ENV);
    match prev_gh_bin {
        Some(v) => std::env::set_var("LOOM_GH_BIN", v),
        None => std::env::remove_var("LOOM_GH_BIN"),
    }
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

/// A fake `gh` whose every `api …/comments` call prints `stdout` (it ignores
/// the `--jq` projection, as the real one would have applied it already).
fn comments_gh(dir: &std::path::Path, stdout: &str) -> std::path::PathBuf {
    let out = dir.join("comments.ndjson");
    std::fs::write(&out, stdout).unwrap();
    let gh = dir.join("fake-gh-comments.sh");
    std::fs::write(&gh, format!("#!/usr/bin/env bash\ncat \"{}\"\n", out.display())).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    gh
}

fn ndjson(login: &str, kind: &str, assoc: &str, fields: &str) -> String {
    format!(
        r#"{{"user":{{"login":"{login}","type":"{kind}"}},"author_association":"{assoc}",{fields}}}"#
    )
}

/// H7: only a trusted author's lease is evidence of a live claim. An
/// outsider's (or a foreign fleet's) fresh lease reads as no lease at all.
#[test]
#[serial]
fn a_lease_from_an_untrusted_author_is_not_found() {
    let dir = tempdir().unwrap();
    let ts = r#""updated_at":"2026-09-29T00:00:00Z""#;
    let untrusted = [
        ndjson("drive-by", "User", "NONE", ts),
        ndjson("loom-fleet-dispatch", "User", "CONTRIBUTOR", ts),
        ndjson("other-fleet[bot]", "Bot", "NONE", ts),
    ]
    .join("\n");
    let gh = comments_gh(dir.path(), &untrusted);
    assert_eq!(
        forge::fetch_freshest_lease_updated_at(&gh, dir.path(), 1),
        forge::LeaseProbe::NotFound
    );
    let gh = comments_gh(dir.path(), &ndjson("loom-fleet-dispatch[bot]", "Bot", "NONE", ts));
    assert!(matches!(
        forge::fetch_freshest_lease_updated_at(&gh, dir.path(), 1),
        forge::LeaseProbe::Found(_)
    ));
    // Unparseable output is a failed read, never "no lease".
    let gh = comments_gh(dir.path(), "not json");
    assert_eq!(
        forge::fetch_freshest_lease_updated_at(&gh, dir.path(), 1),
        forge::LeaseProbe::ReadFailed
    );
}

/// H8: an outsider cannot keep a claim alive by posting this claim's
/// activity marker; the fleet's own heartbeat still counts.
#[test]
#[serial]
fn claim_activity_counts_only_from_trusted_authors() {
    let dir = tempdir().unwrap();
    let claimed_at = DateTime::parse_from_rfc3339("2026-09-29T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let fields = format!(
        r#""created_at":"2026-09-29T01:00:00Z","body":"still working {}""#,
        claim_activity_marker(claimed_at)
    );
    let gh = comments_gh(dir.path(), &ndjson("drive-by", "User", "NONE", &fields));
    assert_eq!(forge::fetch_most_recent_claim_activity_at(&gh, dir.path(), 7, claimed_at), None);
    let gh = comments_gh(dir.path(), &ndjson("loom-fleet-dispatch[bot]", "Bot", "NONE", &fields));
    assert!(forge::fetch_most_recent_claim_activity_at(&gh, dir.path(), 7, claimed_at).is_some());
}

/// #9709: PR #300 carries `loom:pr` at `SHA_B`; the newest TRUSTED approval is
/// for `SHA_A`, and a newer approval for `SHA_B` came from an admin GitHub
/// reports as `CONTRIBUTOR`. The verdict is still cleared (trust unchanged),
/// but the posted notice names the login and `forge.trustedCommenters`
/// instead of asserting a head move. Returns the posted comment bodies.
fn reconcile_capturing_notice(comments: &str) -> (VerdictReconcileStats, String) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let listing = dir.path().join("comments.json");
    std::fs::write(&listing, comments).unwrap();
    let posted = dir.path().join("posted.log");
    let gh = dir.path().join("fake-gh.sh");
    std::fs::write(
        &gh,
        format!(
            r#"#!/usr/bin/env bash
case "$*" in
  "pr list "*"--label loom:pr "*)
    echo '[{{"number":300,"headRefOid":"{SHA_B}","labels":[{{"name":"loom:pr"}}]}}]' ;;
  "pr list "*) echo '[]' ;;
  "pr comment "*) printf '%s\n' "$5" >> "{posted}" ;;
  "api repos/{{owner}}/{{repo}}/issues/300/comments"*) cat "{listing}" ;;
  "api "*compare/*) echo '{{"status":"ahead","files":[{{"filename":"src/lib.rs"}}]}}' ;;
  *) echo '{{}}' ;;
esac
"#,
            listing = listing.display(),
            posted = posted.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The #9709 probe spawns through the `gh_invocation` choke point (#9985),
    // whose resolver honours `LOOM_GH_BIN` rather than the injected `gh`.
    let prev_gh_bin = std::env::var_os("LOOM_GH_BIN");
    std::env::set_var("LOOM_GH_BIN", &gh);
    std::env::set_var(VERDICT_ANCHOR_ENABLED_ENV, "0");
    let stats = forge::reconcile_pr_verdicts(&gh, &root);
    std::env::remove_var(VERDICT_ANCHOR_ENABLED_ENV);
    match prev_gh_bin {
        Some(v) => std::env::set_var("LOOM_GH_BIN", v),
        None => std::env::remove_var("LOOM_GH_BIN"),
    }
    (stats, std::fs::read_to_string(&posted).unwrap_or_default())
}

#[test]
#[serial]
fn a_dropped_newer_approval_is_named_in_the_stale_notice() {
    let listing = format!(
        "[{},{}]",
        comment("maintainer", "User", "COLLABORATOR", SHA_A),
        comment("rjwalters", "User", "CONTRIBUTOR", SHA_B),
    );
    let (stats, posted) = reconcile_capturing_notice(&listing);
    assert_eq!(stats.invalidated, 1, "trust is unchanged: still cleared: {stats:?}");
    assert!(posted.contains(&format!("<!-- loom:verdict-stale from={SHA_A} to={SHA_B} -->")));
    assert!(posted.contains("`rjwalters`"), "{posted}");
    assert!(posted.contains("author_association=CONTRIBUTOR"), "{posted}");
    assert!(posted.contains("forge.trustedCommenters"), "{posted}");
    assert!(!posted.contains("head SHA moved"), "{posted}");
}

#[test]
#[serial]
fn a_genuine_head_move_still_says_head_sha_moved() {
    let listing = format!("[{}]", comment("maintainer", "User", "COLLABORATOR", SHA_A));
    let (stats, posted) = reconcile_capturing_notice(&listing);
    assert_eq!(stats.invalidated, 1, "{stats:?}");
    assert!(posted.contains("**Stale review verdict cleared — head SHA moved**"), "{posted}");
    assert!(!posted.contains("forge.trustedCommenters"), "{posted}");
}
