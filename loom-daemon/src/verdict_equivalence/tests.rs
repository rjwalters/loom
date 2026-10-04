//! Unit coverage for the three equivalence kinds (Issue #9416).
//!
//! Two kinds of fixture, matching the two kinds of evidence:
//!
//! - **A fake `gh`** on a route table, for everything that asks the forge
//!   (`tree_unchanged`, `descends_from`, `patch_identity`). The script logs its
//!   argv so a test can assert that a call was NOT made.
//! - **A real git repository**, for [`super::clean_merge`]. A stub of `git`
//!   would test the stub: the whole question there is what
//!   `merge-tree --write-tree` actually computes, so these build real commits
//!   and run real git. They are skipped (not failed) on a host whose git
//!   predates 2.38, the same floor the two existing `merge-tree` callers use.
//!
//! The emphasis is deliberately on the **fail-closed** arms. A too-permissive
//! equivalence silently skips a real review, so every "could not tell" shape
//! #9576/PR #9581 found expensive for the tree kind has a case here:
//! force-push back to an ancestor, a diverged head, a shallow clone, a missing
//! object, a `merge-tree` conflict, a truncated compare, a patch-less file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tempfile::{tempdir, TempDir};

const REVIEWED: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";

/// Write a fake `gh` that logs its argv to `log` and answers the first route
/// whose pattern appears in `"$*"`. An unrouted call exits 1 with nothing on
/// stdout — the same shape a real `gh` failure has.
pub(super) fn fake_gh(dir: &Path, log: &Path, routes: &[(&str, &str)]) -> PathBuf {
    let mut cases = String::new();
    for (pattern, body) in routes {
        assert!(!body.contains('\''), "fixture bodies must not contain single quotes");
        cases.push_str(&format!("  *\"{pattern}\"*) printf '%s' '{body}'; exit 0 ;;\n"));
    }
    let bin = dir.join("fake-gh.sh");
    std::fs::write(
        &bin,
        format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
case "$*" in
{cases}esac
echo 'fake-gh: no route for: '"$*" 1>&2
exit 1
"#,
            log = log.display(),
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    bin
}

/// `{"baseRefName": "main"}` — what `gh pr view --json baseRefName` returns.
pub(super) const BASE_MAIN: (&str, &str) = ("--json baseRefName", r#"{"baseRefName": "main"}"#);

/// A compare response carrying one changed file with the given patch text.
fn one_file(patch: &str) -> String {
    format!(
        r#"{{"status": "diverged", "files": [{{"filename": "src/a.rs", "status": "modified", "sha": "aaaa", "patch": "{patch}"}}]}}"#
    )
}

// ---------------------------------------------------------------------------
// Pure predicates
// ---------------------------------------------------------------------------

#[test]
fn merge_tree_version_floor_is_2_38() {
    use super::git_objects::merge_tree_in_version;
    assert!(merge_tree_in_version("2.38.0"));
    assert!(merge_tree_in_version("2.51.1"));
    assert!(merge_tree_in_version("3.0.0"));
    assert!(!merge_tree_in_version("2.37.3"));
    assert!(!merge_tree_in_version("1.9"));
    assert!(!merge_tree_in_version("not-a-version"));
}

#[test]
fn ref_names_that_could_address_another_endpoint_are_refused() {
    assert!(is_safe_ref("main"));
    assert!(is_safe_ref("release/v1.2"));
    assert!(is_safe_ref("feature/issue-9416"));
    assert!(!is_safe_ref(""));
    assert!(!is_safe_ref("../../pulls/1"));
    assert!(!is_safe_ref("a..b"));
    assert!(!is_safe_ref("-x"));
    assert!(!is_safe_ref("main;rm -rf /"));
    assert!(!is_safe_ref("main...other"));
}

#[test]
fn kind_tokens_are_the_operators_three_names() {
    assert_eq!(EquivalenceKind::Tree.token(), "tree");
    assert_eq!(EquivalenceKind::CleanMerge.token(), "clean-merge");
    assert_eq!(EquivalenceKind::RebasePatchIdentical.token(), "rebase-patch-identical");
}

/// The re-anchor body must keep the `verdict-sha` marker BYTE-COMPATIBLE with
/// the two scanners that match it (`claim_reconciliation::
/// extract_latest_verdict_sha` and `verdict-staleness-guard.sh`'s MARKER_TEST),
/// both of which anchor on the trailing ` -->`. The equivalence kind therefore
/// goes in a SECOND marker, never inside the first.
#[test]
fn reanchor_body_keeps_the_verdict_marker_shape_and_records_the_kind() {
    let body = reanchor_body("loom:pr", "approved", EquivalenceKind::CleanMerge, REVIEWED, HEAD);
    assert!(
        body.contains(&format!("<!-- loom:verdict-sha sha={HEAD} verdict=approved -->")),
        "the verdict-sha marker must stay exactly what the scanners match: {body}"
    );
    assert!(body.contains(&format!(
        "<!-- loom:verdict-equivalence kind=clean-merge from={REVIEWED} to={HEAD} -->"
    )));
    // CI is never exempted — the comment must say so, because that is the one
    // thing a reader of a carried-over verdict needs to know.
    assert!(body.contains("CI is not exempted"));
}

// ---------------------------------------------------------------------------
// detect(): ordering and the kill switches
// ---------------------------------------------------------------------------

/// The tree-identical kind is asked FIRST and short-circuits: no base lookup,
/// no second compare.
#[test]
fn tree_identical_wins_without_asking_anything_else() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let gh = fake_gh(
        dir.path(),
        &log,
        &[(&format!("compare/{REVIEWED}...{HEAD}"), r#"{"status": "ahead", "files": []}"#)],
    );
    assert_eq!(
        detect_with(true, true, &gh, Some(dir.path()), 9416, REVIEWED, HEAD),
        Equivalence::Equivalent(EquivalenceKind::Tree)
    );
    let argv = std::fs::read_to_string(&log).unwrap();
    assert!(
        !argv.contains("--json baseRefName"),
        "the base ref must not be looked up once the tree test already answered: {argv}"
    );
}

/// The outer kill switch disables all three kinds and makes no call at all.
#[test]
fn outer_kill_switch_answers_nothing_without_calling_gh() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let gh = fake_gh(
        dir.path(),
        &log,
        &[(&format!("compare/{REVIEWED}...{HEAD}"), r#"{"status": "ahead", "files": []}"#)],
    );
    assert_eq!(
        detect_with(false, true, &gh, Some(dir.path()), 9416, REVIEWED, HEAD),
        Equivalence::Indeterminate
    );
    assert!(!log.exists(), "a disabled carve-out must not call gh");
}

/// The inner kill switch leaves the already-shipped tree kind alone and
/// disables only the two kinds #9416 adds.
#[test]
fn inner_kill_switch_keeps_the_tree_kind_and_drops_the_new_ones() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let gh = fake_gh(
        dir.path(),
        &log,
        &[
            (&format!("compare/{REVIEWED}...{HEAD}"), r#"{"status": "ahead", "files": []}"#),
            BASE_MAIN,
        ],
    );
    assert_eq!(
        detect_with(true, false, &gh, Some(dir.path()), 9416, REVIEWED, HEAD),
        Equivalence::Equivalent(EquivalenceKind::Tree)
    );

    // Same switch, a head whose tree DID change: no new kind is tried.
    let log2 = dir.path().join("gh2.log");
    let gh2 = fake_gh(
        dir.path(),
        &log2,
        &[
            (
                &format!("compare/{REVIEWED}...{HEAD}"),
                r#"{"status": "ahead", "files": [{"filename": "x"}]}"#,
            ),
            BASE_MAIN,
        ],
    );
    assert_eq!(
        detect_with(true, false, &gh2, Some(dir.path()), 9416, REVIEWED, HEAD),
        Equivalence::Indeterminate
    );
    let argv = std::fs::read_to_string(&log2).unwrap();
    assert!(
        !argv.contains("--json baseRefName"),
        "the new kinds must not be evaluated with the inner switch off: {argv}"
    );
}

/// A base ref that cannot be resolved is no answer — never a fall-through into
/// an assumed equivalence.
#[test]
fn unresolvable_base_ref_is_indeterminate() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let gh = fake_gh(
        dir.path(),
        &log,
        &[(
            &format!("compare/{REVIEWED}...{HEAD}"),
            r#"{"status": "diverged", "files": [{"filename": "x"}]}"#,
        )],
    );
    assert_eq!(
        detect_with(true, true, &gh, Some(dir.path()), 9416, REVIEWED, HEAD),
        Equivalence::Indeterminate
    );
}

#[test]
fn non_sha_arguments_are_refused_without_calling_gh() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let gh = fake_gh(dir.path(), &log, &[]);
    for (reviewed, head) in [
        ("../../pulls/1", HEAD),
        (REVIEWED, "HEAD"),
        ("", HEAD),
        (REVIEWED, "111111"),
        (REVIEWED, "AAAAAAAAAA"),
    ] {
        assert_eq!(
            detect_with(true, true, &gh, Some(dir.path()), 9416, reviewed, head),
            Equivalence::Indeterminate
        );
    }
    assert!(!log.exists(), "no gh call may be made for a malformed ref");
}

// ---------------------------------------------------------------------------
// patch_identity (the rebase kind)
// ---------------------------------------------------------------------------

#[test]
fn byte_identical_patch_across_a_rebase_carries_the_verdict() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let body = one_file("@@ -1 +1 @@\\n-a\\n+b\\n");
    let gh = fake_gh(
        dir.path(),
        &log,
        &[
            (
                &format!("compare/{REVIEWED}...{HEAD}"),
                r#"{"status": "diverged", "files": [{"filename": "src/a.rs"}]}"#,
            ),
            BASE_MAIN,
            (&format!("compare/main...{REVIEWED}"), &body),
            (&format!("compare/main...{HEAD}"), &body),
        ],
    );
    assert_eq!(
        detect_with(true, true, &gh, Some(dir.path()), 9416, REVIEWED, HEAD),
        Equivalence::Equivalent(EquivalenceKind::RebasePatchIdentical)
    );
}

#[test]
fn a_different_patch_is_positively_refuted() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let gh = fake_gh(
        dir.path(),
        &log,
        &[
            BASE_MAIN,
            (&format!("compare/main...{REVIEWED}"), &one_file("@@ -1 +1 @@\\n-a\\n+b\\n")),
            (&format!("compare/main...{HEAD}"), &one_file("@@ -1 +1 @@\\n-a\\n+c\\n")),
        ],
    );
    assert_eq!(
        patch_identity::evidence(&gh, Some(dir.path()), REVIEWED, HEAD, "main"),
        Evidence::Refuted
    );
}

/// THE PR #9581 HOLE, in this kind's terms: a force-push that rewinds the head
/// onto an ancestor of the base. Its own merge-base-relative diff is empty
/// while the reviewed head's is not, so the two are not equal and the verdict
/// must NOT carry.
#[test]
fn force_push_back_to_a_base_ancestor_is_refuted() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let gh = fake_gh(
        dir.path(),
        &log,
        &[
            BASE_MAIN,
            (&format!("compare/main...{REVIEWED}"), &one_file("@@ -1 +1 @@\\n-a\\n+b\\n")),
            (&format!("compare/main...{HEAD}"), r#"{"status": "behind", "files": []}"#),
        ],
    );
    assert_eq!(
        patch_identity::evidence(&gh, Some(dir.path()), REVIEWED, HEAD, "main"),
        Evidence::Refuted
    );
}

/// Two EMPTY merge-base-relative diffs prove nothing: each head merely equals
/// its own merge base, and those merge bases can differ — so the trees can
/// differ. Same class of hole as reading `files: []` without a `status`.
#[test]
fn two_empty_diffs_are_indeterminate_not_equivalent() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let empty = r#"{"status": "behind", "files": []}"#;
    let gh = fake_gh(
        dir.path(),
        &log,
        &[
            BASE_MAIN,
            (&format!("compare/main...{REVIEWED}"), empty),
            (&format!("compare/main...{HEAD}"), empty),
        ],
    );
    assert_eq!(
        patch_identity::evidence(&gh, Some(dir.path()), REVIEWED, HEAD, "main"),
        Evidence::Indeterminate
    );
}

/// A changed file whose `patch` the endpoint omitted (binary content, or a diff
/// too large to serialize) is no byte evidence at all — two DIFFERENT binary
/// changes would otherwise compare equal.
#[test]
fn a_patchless_file_entry_is_indeterminate() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let binary = r#"{"status": "diverged", "files": [{"filename": "logo.png", "status": "modified", "sha": "aaaa"}]}"#;
    let gh = fake_gh(
        dir.path(),
        &log,
        &[
            BASE_MAIN,
            (&format!("compare/main...{REVIEWED}"), binary),
            (&format!("compare/main...{HEAD}"), binary),
        ],
    );
    assert_eq!(
        patch_identity::evidence(&gh, Some(dir.path()), REVIEWED, HEAD, "main"),
        Evidence::Indeterminate
    );
}

/// Same resulting content, same patch text, but a different resulting blob id
/// would be a contradiction — and a differing `sha` must refute regardless,
/// because it is the one field that speaks about content the patch text may not.
#[test]
fn a_differing_result_blob_id_refutes() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let patch = "@@ -1 +1 @@\\n-a\\n+b\\n";
    let gh = fake_gh(
        dir.path(),
        &log,
        &[
            BASE_MAIN,
            (&format!("compare/main...{REVIEWED}"), &one_file(patch)),
            (
                &format!("compare/main...{HEAD}"),
                &format!(
                    r#"{{"status": "diverged", "files": [{{"filename": "src/a.rs", "status": "modified", "sha": "bbbb", "patch": "{patch}"}}]}}"#
                ),
            ),
        ],
    );
    assert_eq!(
        patch_identity::evidence(&gh, Some(dir.path()), REVIEWED, HEAD, "main"),
        Evidence::Refuted
    );
}

/// A response at the endpoint's 300-entry page cap may be truncated, so the
/// file set is unknown and nothing over it can be compared.
#[test]
fn a_possibly_truncated_file_list_is_indeterminate() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let mut files = String::new();
    for i in 0..300 {
        if i > 0 {
            files.push(',');
        }
        files.push_str(&format!(
            r#"{{"filename": "f{i}", "status": "modified", "sha": "s{i}", "patch": "p"}}"#
        ));
    }
    let big = format!(r#"{{"status": "diverged", "files": [{files}]}}"#);
    let gh = fake_gh(
        dir.path(),
        &log,
        &[
            BASE_MAIN,
            (&format!("compare/main...{REVIEWED}"), &big),
            (&format!("compare/main...{HEAD}"), &big),
        ],
    );
    assert_eq!(
        patch_identity::evidence(&gh, Some(dir.path()), REVIEWED, HEAD, "main"),
        Evidence::Indeterminate
    );
}

/// A `gh` outage, an unparsable body, and a body missing its `files` key are
/// all the same fail-closed arm.
#[test]
fn unreadable_compare_responses_are_indeterminate() {
    for body in [
        None,                              // no route -> exit 1
        Some("not json at all"),           // unparsable
        Some(r#"{"status": "diverged"}"#), // no `files` key
    ] {
        let dir = tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let mut routes: Vec<(&str, &str)> = vec![BASE_MAIN];
        if let Some(b) = body {
            routes.push(("compare/main...", b));
        }
        let gh = fake_gh(dir.path(), &log, &routes);
        assert_eq!(
            patch_identity::evidence(&gh, Some(dir.path()), REVIEWED, HEAD, "main"),
            Evidence::Indeterminate,
            "body {body:?} must not prove equivalence"
        );
    }
}

// ---------------------------------------------------------------------------
// clean_merge (the merge-of-base kind) — real git fixtures
// ---------------------------------------------------------------------------

/// Run git in `repo`, asserting success.
pub(super) fn git_ok(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

pub(super) fn write(repo: &Path, rel: &str, content: &str) {
    std::fs::write(repo.join(rel), content).unwrap();
}

/// Build a repo with:
///   base0 --- base1(main)        (main moved: touches `m.txt`)
///       \
///        reviewed(feature)       (the PR's own change: touches `f.txt`)
///
/// Returns `(tempdir, reviewed_sha, base1_sha)`. `pr_file` lets a caller point
/// the PR's change at the SAME file main touched, to produce a conflict.
fn fixture(pr_file: &str, pr_content: &str) -> (TempDir, String, String) {
    let dir = tempdir().unwrap();
    let repo = dir.path();
    // `git init` then an explicit symbolic-ref, rather than
    // `--initial-branch=main` (git >= 2.28 only): these fixtures must build on
    // any git, so that the merge-tree floor check below is what decides whether
    // the test runs, not the fixture setup.
    git_ok(repo, &["init", "--quiet", "."]);
    git_ok(repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    write(repo, "m.txt", "main-0\n");
    write(repo, "f.txt", "feature-0\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "base0"]);

    git_ok(repo, &["checkout", "--quiet", "-b", "feature"]);
    write(repo, pr_file, pr_content);
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "the PR's own change"]);
    let reviewed = git_ok(repo, &["rev-parse", "HEAD"]);

    git_ok(repo, &["checkout", "--quiet", "main"]);
    write(repo, "m.txt", "main-1\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "main moved"]);
    let base1 = git_ok(repo, &["rev-parse", "HEAD"]);

    git_ok(repo, &["checkout", "--quiet", "feature"]);
    (dir, reviewed, base1)
}

/// Skip rather than fail on a git with no `merge-tree --write-tree`, the same
/// floor the two existing callers use.
pub(super) fn merge_tree_available(repo: &Path) -> bool {
    if super::git_objects::supports_merge_tree(repo) {
        return true;
    }
    eprintln!("SKIP: git < 2.38 has no `merge-tree --write-tree`");
    false
}

/// `gh` answering only "yes, that commit is on main".
pub(super) fn gh_base_descends(dir: &Path, log: &Path, base_sha: &str) -> PathBuf {
    fake_gh(
        dir,
        log,
        &[
            (&format!("compare/{base_sha}...main"), r#"{"status": "identical", "files": []}"#),
            BASE_MAIN,
        ],
    )
}

#[test]
fn a_clean_merge_of_the_base_carries_the_verdict() {
    let (dir, reviewed, base1) = fixture("f.txt", "feature-1\n");
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    git_ok(repo, &["merge", "--quiet", "--no-ff", "-m", "Merge main", "main"]);
    let head = git_ok(repo, &["rev-parse", "HEAD"]);

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = gh_base_descends(ghdir.path(), &log, &base1);
    assert_eq!(
        clean_merge::evidence(&gh, Some(repo), repo, &reviewed, &head, "main"),
        Evidence::Proven
    );
}

/// A merge whose author edited something while merging is NOT the automatic
/// merge, so the verdict must not carry.
#[test]
fn a_merge_with_a_hand_edit_is_refuted() {
    let (dir, reviewed, base1) = fixture("f.txt", "feature-1\n");
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    git_ok(repo, &["merge", "--quiet", "--no-ff", "--no-commit", "main"]);
    write(repo, "f.txt", "feature-1-plus-a-sneaky-line\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "Merge main (with an edit)"]);
    let head = git_ok(repo, &["rev-parse", "HEAD"]);

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = gh_base_descends(ghdir.path(), &log, &base1);
    assert_eq!(
        clean_merge::evidence(&gh, Some(repo), repo, &reviewed, &head, "main"),
        Evidence::Refuted
    );
}

/// A conflicted merge: there IS no clean automatic merge, so a head that
/// resolved one cannot be it. Positive evidence, hence `Refuted`.
#[test]
fn a_conflict_resolution_merge_is_refuted() {
    // The PR touches the SAME file+line main moved, so merge-tree conflicts.
    let (dir, reviewed, base1) = fixture("m.txt", "feature-touched-main-file\n");
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    // Resolve by hand and commit the merge anyway.
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge", "--quiet", "--no-ff", "main"])
        .output()
        .unwrap();
    write(repo, "m.txt", "resolved-by-hand\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "Merge main"]);
    let head = git_ok(repo, &["rev-parse", "HEAD"]);

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = gh_base_descends(ghdir.path(), &log, &base1);
    assert_eq!(
        clean_merge::evidence(&gh, Some(repo), repo, &reviewed, &head, "main"),
        Evidence::Refuted
    );
}

/// A single-parent head (an ordinary new commit) is not a merge of the base.
#[test]
fn a_non_merge_head_is_refuted() {
    let (dir, reviewed, base1) = fixture("f.txt", "feature-1\n");
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    write(repo, "f.txt", "feature-2\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "another real commit"]);
    let head = git_ok(repo, &["rev-parse", "HEAD"]);

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = gh_base_descends(ghdir.path(), &log, &base1);
    assert_eq!(
        clean_merge::evidence(&gh, Some(repo), repo, &reviewed, &head, "main"),
        Evidence::Refuted
    );
}

/// A merge whose FIRST parent is not the reviewed head (the merge was made the
/// other way round) is refused — even though its tree may well be the same.
#[test]
fn a_merge_whose_first_parent_is_not_the_reviewed_head_is_refuted() {
    let (dir, reviewed, base1) = fixture("f.txt", "feature-1\n");
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    git_ok(repo, &["checkout", "--quiet", "main"]);
    git_ok(
        repo,
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "Merge feature",
            "feature",
        ],
    );
    let head = git_ok(repo, &["rev-parse", "HEAD"]);

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = gh_base_descends(ghdir.path(), &log, &base1);
    assert_eq!(
        clean_merge::evidence(&gh, Some(repo), repo, &reviewed, &head, "main"),
        Evidence::Refuted
    );
}

/// THE HAZARD this kind's third condition exists for: a perfectly clean merge
/// of an ARBITRARY branch, whose content nobody reviewed. The forge says the
/// merged parent is not on the base branch, so the verdict must not carry.
#[test]
fn a_clean_merge_of_a_branch_that_is_not_the_base_is_refuted() {
    let (dir, reviewed, _base1) = fixture("f.txt", "feature-1\n");
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    git_ok(repo, &["checkout", "--quiet", "-b", "someone-elses-work", "main"]);
    write(repo, "x.txt", "unreviewed content\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "--quiet", "-m", "unreviewed"]);
    let other = git_ok(repo, &["rev-parse", "HEAD"]);
    git_ok(repo, &["checkout", "--quiet", "feature"]);
    git_ok(
        repo,
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "Merge someone else",
            "someone-elses-work",
        ],
    );
    let head = git_ok(repo, &["rev-parse", "HEAD"]);

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = fake_gh(
        ghdir.path(),
        &log,
        &[(
            &format!("compare/{other}...main"),
            r#"{"status": "diverged", "files": [{"filename": "x.txt"}]}"#,
        )],
    );
    assert_eq!(
        clean_merge::evidence(&gh, Some(repo), repo, &reviewed, &head, "main"),
        Evidence::Refuted
    );
}

/// An unanswerable ancestry check (a `gh` outage) is `Indeterminate`, never a
/// fall-through into "probably the base".
#[test]
fn an_unanswerable_ancestry_check_is_indeterminate() {
    let (dir, reviewed, _base1) = fixture("f.txt", "feature-1\n");
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    git_ok(repo, &["merge", "--quiet", "--no-ff", "-m", "Merge main", "main"]);
    let head = git_ok(repo, &["rev-parse", "HEAD"]);

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = fake_gh(ghdir.path(), &log, &[]); // every call fails
    assert_eq!(
        clean_merge::evidence(&gh, Some(repo), repo, &reviewed, &head, "main"),
        Evidence::Indeterminate
    );
}

/// A cwd that is not a git repository at all (the CLI path on a host run from
/// somewhere unexpected) answers nothing.
#[test]
fn a_non_repository_is_indeterminate() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let gh = fake_gh(dir.path(), &log, &[]);
    let empty = tempdir().unwrap();
    assert_eq!(
        clean_merge::evidence(&gh, Some(dir.path()), empty.path(), REVIEWED, HEAD, "main"),
        Evidence::Indeterminate
    );
}

/// A shallow clone can hold the head and still lack the merge-base history
/// `merge-tree` needs — in which case it would produce a DIFFERENT tree rather
/// than an error. Refused before anything is computed.
#[test]
fn a_shallow_clone_is_indeterminate() {
    let (src, reviewed, base1) = fixture("f.txt", "feature-1\n");
    let repo = src.path();
    if !merge_tree_available(repo) {
        return;
    }
    git_ok(repo, &["merge", "--quiet", "--no-ff", "-m", "Merge main", "main"]);
    let head = git_ok(repo, &["rev-parse", "HEAD"]);

    let shallow_dir = tempdir().unwrap();
    let shallow = shallow_dir.path().join("clone");
    let out = std::process::Command::new("git")
        .args(["clone", "--quiet", "--depth=1", "--no-local"])
        .arg(format!("file://{}", repo.display()))
        .arg(&shallow)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        super::git_objects::is_shallow(&shallow),
        Some(true),
        "fixture must actually be shallow"
    );

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = gh_base_descends(ghdir.path(), &log, &base1);
    assert_eq!(
        clean_merge::evidence(&gh, Some(&shallow), &shallow, &reviewed, &head, "main"),
        Evidence::Indeterminate
    );
    assert!(!log.exists(), "a shallow clone must be refused before any forge call");
}

/// A head the object store does not have, in a repo with no `origin` to fetch
/// it from, is `Indeterminate` — the missing-object arm.
#[test]
fn a_missing_object_is_indeterminate() {
    let (dir, reviewed, base1) = fixture("f.txt", "feature-1\n");
    let repo = dir.path();
    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = gh_base_descends(ghdir.path(), &log, &base1);
    assert_eq!(
        clean_merge::evidence(&gh, Some(repo), repo, &reviewed, HEAD, "main"),
        Evidence::Indeterminate
    );
}

/// End to end through [`detect`]: a clean merge of the base reports the
/// `clean-merge` kind, not the broader patch-identity one.
#[test]
fn detect_reports_clean_merge_for_a_merge_of_the_base() {
    let (dir, reviewed, base1) = fixture("f.txt", "feature-1\n");
    let repo = dir.path();
    if !merge_tree_available(repo) {
        return;
    }
    git_ok(repo, &["merge", "--quiet", "--no-ff", "-m", "Merge main", "main"]);
    let head = git_ok(repo, &["rev-parse", "HEAD"]);

    let ghdir = tempdir().unwrap();
    let log = ghdir.path().join("gh.log");
    let gh = fake_gh(
        ghdir.path(),
        &log,
        &[
            (
                &format!("compare/{reviewed}...{head}"),
                r#"{"status": "ahead", "files": [{"filename": "m.txt"}]}"#,
            ),
            (&format!("compare/{base1}...main"), r#"{"status": "identical", "files": []}"#),
            BASE_MAIN,
        ],
    );
    assert_eq!(
        detect_with(true, true, &gh, Some(repo), 9416, &reviewed, &head),
        Equivalence::Equivalent(EquivalenceKind::CleanMerge)
    );
}
