//! Unit tests for the `B`/`D`/`P` derivations of #8919.
//!
//! The log-parsing tests use the line `actions/checkout` really printed on run
//! 36145858487 (PR #8692) — the verification that made the in-place re-run
//! unsound — because a parser that "looks right" against a hand-written line is
//! exactly how this class of bug survives.

use super::*;

fn f(path: &str, status: &str, patch: Option<&str>) -> ChangedFile {
    ChangedFile {
        path: path.to_string(),
        status: status.to_string(),
        previous_filename: None,
        patch: patch.map(String::from),
    }
}

// --- B: the tested base -----------------------------------------------------

/// The real attempt-1 / attempt-7 line: identical across a two-hour gap and
/// several `main` merges, which is the whole finding.
const REAL_LOG: &str = "2026-09-25T14:12:18.1234567Z /usr/bin/git checkout --progress --force refs/remotes/pull/8692/merge
2026-09-25T14:12:18.2345678Z HEAD is now at cb7c91f Merge 162b0f05e2a14c2f2d9b7c6a1e3f4d5b6a7c8d9e into 803f0c7dab19f2c3d4e5f6a7b8c9d0e1f2a3b4c5
2026-09-25T14:12:18.3456789Z ##[endgroup]";

#[test]
fn the_tested_base_is_read_from_the_real_checkout_log() {
    let head = "162b0f05e2a14c2f2d9b7c6a1e3f4d5b6a7c8d9e";
    assert_eq!(
        parse_tested_base(REAL_LOG, head).unwrap(),
        "803f0c7dab19f2c3d4e5f6a7b8c9d0e1f2a3b4c5"
    );
    // The PR head as `gh pr view` abbreviates it still matches: either side may
    // be short, so the comparison is a prefix relation in either direction.
    assert_eq!(
        parse_tested_base(REAL_LOG, "162b0f0").unwrap(),
        "803f0c7dab19f2c3d4e5f6a7b8c9d0e1f2a3b4c5"
    );
}

#[test]
fn an_ellipsised_rendering_still_parses() {
    let log = "HEAD is now at cb7c91f Merge 162b0f05… into 803f0c7d…";
    assert_eq!(parse_tested_base(log, "162b0f05").unwrap(), "803f0c7d");
    let log = "HEAD is now at cb7c91f Merge 162b0f05... into 803f0c7d...";
    assert_eq!(parse_tested_base(log, "162b0f05").unwrap(), "803f0c7d");
}

#[test]
fn a_log_whose_head_is_not_this_prs_head_is_refused() {
    // The failure this guards: attributing SOME OTHER commit's tested base to
    // this head would measure the wrong base move, in an unknown direction.
    let err = parse_tested_base(REAL_LOG, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        .expect_err("a head mismatch must not yield a base");
    assert!(err.contains("162b0f05"), "{err}");
    assert!(err.contains("deadbeef"), "{err}");
}

#[test]
fn a_log_without_a_merge_line_is_refused() {
    for log in [
        "",
        "2026-09-25T14:12:18Z ##[group]Run actions/checkout@v7",
        // Prose containing the word merge but no SHAs must not match.
        "HEAD is now at cb7c91f Merge the feature branch into main",
        // A too-short hex run is not a SHA.
        "HEAD is now at cb7c91f Merge abc123 into def456",
    ] {
        assert!(parse_tested_base(log, "162b0f05").is_err(), "must refuse: {log:?}");
    }
}

// --- D: the compare answer's usability --------------------------------------

#[test]
fn only_an_ahead_or_identical_compare_is_usable() {
    assert!(compare_usable("ahead", 4).is_ok());
    assert!(compare_usable("identical", 0).is_ok());
    for status in ["diverged", "behind", ""] {
        let err = compare_usable(status, 1).expect_err("must fail closed");
        assert!(err.contains(status) || status.is_empty(), "{err}");
    }
}

#[test]
fn a_possibly_truncated_compare_is_refused() {
    assert!(compare_usable("ahead", COMPARE_FILE_CAP - 1).is_ok());
    let err = compare_usable("ahead", COMPARE_FILE_CAP).expect_err("at the cap = maybe truncated");
    assert!(err.contains("truncated"), "{err}");
    assert!(compare_usable("ahead", COMPARE_FILE_CAP + 50).is_err());
}

// --- D: validated version restamps ------------------------------------------

fn version_bump() -> ChangedFile {
    f("VERSION", "modified", Some("@@ -1 +1 @@\n-0.19.398\n+0.19.399\n"))
}

fn restamp_set() -> Vec<ChangedFile> {
    vec![
        version_bump(),
        f(
            "Cargo.toml",
            "modified",
            Some("@@ -3,1 +3,1 @@\n-version = \"0.19.398\"\n+version = \"0.19.399\"\n"),
        ),
        f(
            "Cargo.lock",
            "modified",
            Some(
                "@@ -1,2 +1,2 @@\n-version = \"0.19.398\"\n+version = \"0.19.399\"\n\
                 -version = \"0.19.398\"\n+version = \"0.19.399\"\n",
            ),
        ),
        f(
            "mcp-loom/package.json",
            "modified",
            Some("@@ -2 +2 @@\n-  \"version\": \"0.19.398\",\n+  \"version\": \"0.19.399\",\n"),
        ),
        f(
            "mcp-loom/package-lock.json",
            "modified",
            Some("@@ -3 +3 @@\n-  \"version\": \"0.19.398\",\n+  \"version\": \"0.19.399\",\n"),
        ),
        f(
            ".loom/install-metadata.json",
            "modified",
            Some(
                "@@ -2,2 +2,2 @@\n-  \"loom_version\": \"0.19.398\",\n\
                 -  \"loom_commit\": \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\n\
                 +  \"loom_version\": \"0.19.399\",\n\
                 +  \"loom_commit\": \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",\n",
            ),
        ),
    ]
}

#[test]
fn a_pure_restamp_move_is_discounted_entirely() {
    // The whole point: `main` restamps on nearly every merge, so without this
    // D is never empty and the input-scoped predicate degenerates to "any move
    // is stale" — i.e. back to the rule #8919 exists to replace.
    let kept = strip_validated_restamps(&restamp_set());
    assert!(kept.is_empty(), "nothing should survive: {kept:?}");
}

#[test]
fn a_real_change_alongside_a_restamp_survives() {
    let mut files = restamp_set();
    files.push(f("scripts/file-size-baseline.txt", "modified", Some("@@\n-1845 x\n+1815 x\n")));
    let kept = strip_validated_restamps(&files);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].path, "scripts/file-size-baseline.txt");
}

#[test]
fn a_dependency_bump_hiding_in_cargo_lock_is_not_a_restamp() {
    // The forged case: the lines LOOK like version lines, but they carry some
    // other package's version, not VERSION@B -> VERSION@tip.
    let mut files = vec![version_bump()];
    files.push(f(
        "Cargo.lock",
        "modified",
        Some("@@ -10,1 +10,1 @@\n-version = \"1.0.3\"\n+version = \"1.0.4\"\n"),
    ));
    let kept = strip_validated_restamps(&files);
    assert_eq!(
        kept.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        vec!["Cargo.lock"],
        "a dep bump must stay in D"
    );
}

#[test]
fn an_extra_line_beside_the_version_line_is_not_a_restamp() {
    let mut files = vec![version_bump()];
    files.push(f(
        "Cargo.toml",
        "modified",
        Some(
            "@@ -3,2 +3,3 @@\n-version = \"0.19.398\"\n+version = \"0.19.399\"\n\
             +rand = \"0.9\"\n",
        ),
    ));
    let kept = strip_validated_restamps(&files);
    assert_eq!(kept.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["Cargo.toml"]);
}

#[test]
fn a_version_decrease_disables_all_discounting() {
    // Not a release restamp — somebody rewrote history, and nothing about that
    // move may be assumed harmless.
    let mut files = restamp_set();
    files[0] = f("VERSION", "modified", Some("@@ -1 +1 @@\n-0.19.399\n+0.19.398\n"));
    assert_eq!(
        strip_validated_restamps(&files).len(),
        files.len(),
        "a decrease must discount nothing at all"
    );
}

#[test]
fn version_absent_from_the_diff_disables_all_discounting() {
    let files = vec![f(
        "Cargo.lock",
        "modified",
        Some("@@\n-version = \"0.19.398\"\n+version = \"0.19.399\"\n"),
    )];
    assert_eq!(
        strip_validated_restamps(&files).len(),
        1,
        "with no VERSION edit there is no validated pair, so nothing is discounted"
    );
}

#[test]
fn a_missing_patch_is_a_real_change() {
    let mut files = vec![version_bump()];
    files.push(f("Cargo.lock", "modified", None));
    let kept = strip_validated_restamps(&files);
    assert_eq!(kept.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["Cargo.lock"]);
}

#[test]
fn a_loom_commit_line_counts_only_in_install_metadata() {
    let commit_lines =
        "@@ -1,1 +1,1 @@\n-  \"loom_commit\": \"aaaaaaa\",\n+  \"loom_commit\": \"bbbbbbb\",\n";
    // In install-metadata.json it is part of the restamp…
    let files = vec![
        version_bump(),
        f(".loom/install-metadata.json", "modified", Some(commit_lines)),
    ];
    assert!(strip_validated_restamps(&files).is_empty());
    // …and nowhere else.
    let files = vec![
        version_bump(),
        f("Cargo.lock", "modified", Some(commit_lines)),
    ];
    assert_eq!(strip_validated_restamps(&files).len(), 1);
}

#[test]
fn a_non_modified_status_on_a_version_file_is_a_real_change() {
    // An ADDED or REMOVED version-bearing file is not a restamp of a value.
    for status in ["added", "removed", "renamed"] {
        let files = vec![
            version_bump(),
            f("package.json", status, Some("@@\n+  \"version\": \"0.19.399\",\n")),
        ];
        assert_eq!(strip_validated_restamps(&files).len(), 1, "status {status} must survive");
    }
}

// --- P / D projection -------------------------------------------------------

#[test]
fn a_rename_contributes_both_names_and_records_the_vacated_one() {
    let renamed = ChangedFile {
        path: "docs/new.md".to_string(),
        status: "renamed".to_string(),
        previous_filename: Some("docs/old.md".to_string()),
        patch: None,
    };
    let set = to_file_set(&[renamed, f("a.rs", "removed", None)]);
    assert!(set.paths.contains("docs/new.md"));
    assert!(set.paths.contains("docs/old.md"));
    assert!(set.removed.contains("docs/old.md"), "the vacated name is what breaks a link");
    assert!(!set.removed.contains("docs/new.md"));
    assert!(set.removed.contains("a.rs"));
}
