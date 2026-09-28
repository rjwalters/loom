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

// --- The resync restamp (#9065) ---------------------------------------------

/// What `chore: resync installed Loom surfaces` actually lands: no `VERSION`
/// edit at all, and — for every such commit on `main` in the 24 h #9065
/// measured — nothing but this file.
fn resync_stamp() -> ChangedFile {
    f(
        ".loom/install-metadata.json",
        "modified",
        Some(
            "@@ -2,2 +2,2 @@\n-  \"loom_commit\": \"fa2dba6bd\",\n\
             +  \"loom_commit\": \"7f783396c\",\n\
             @@ -38,2 +38,2 @@\n-  \"last_resync\": \"2026-09-27\",\n\
             -  \"loom_source_remote\": \"git@github.com:rjwalters/loom.git\"\n\
             +  \"last_resync\": \"2026-09-28\",\n\
             +  \"loom_source_remote\": \"https://github.com/rjwalters/loom.git\"\n",
        ),
    )
}

#[test]
fn a_resync_only_base_move_is_discounted_without_any_version_bump() {
    // THE #9065 FINDING: ~15 of these land on `main` every day, none of them
    // bumps VERSION, and every one of them used to make D non-empty — which
    // the conflict-marker gate's `**` scanned set turns straight into a
    // refusal for every open PR.
    assert!(
        strip_validated_restamps(&[resync_stamp()]).is_empty(),
        "a resync stamp is a machine restamp, not a base move any gate reads"
    );
}

#[test]
fn a_resync_that_also_rewrote_an_installed_surface_still_counts_as_stale() {
    // The input-scoping guarantee: the discount is per FILE, never per COMMIT.
    // A resync that actually copied a script through leaves that script in D,
    // where the ordinary clauses judge it exactly as before.
    let files = vec![
        resync_stamp(),
        f(".loom/scripts/worktree.sh", "modified", Some("@@ -1,1 +1,1 @@\n-old\n+new\n")),
        f(
            ".loom/docs/troubleshooting.md",
            "modified",
            Some("@@ -1,1 +1,1 @@\n-old\n+new\n"),
        ),
    ];
    let kept: Vec<String> = strip_validated_restamps(&files)
        .into_iter()
        .map(|f| f.path)
        .collect();
    assert_eq!(kept, vec![".loom/scripts/worktree.sh", ".loom/docs/troubleshooting.md"]);
}

#[test]
fn a_non_restamp_field_of_install_metadata_is_a_real_change() {
    // Only the three fields `restamp_metadata()` writes are discountable, and
    // each only in its own shape. Anything else in that file is a real edit.
    for patch in [
        // An installed-file list entry: a genuine surface change.
        "@@ -5,1 +5,1 @@\n-    \".loom/scripts/worktree.sh\",\n+    \".loom/scripts/gone.sh\",\n",
        // The right key, the wrong shape.
        "@@ -3,1 +3,1 @@\n-  \"loom_commit\": \"aaaaaaa\",\n+  \"loom_commit\": \"not-a-sha\",\n",
        "@@ -38,1 +38,1 @@\n-  \"last_resync\": \"2026-09-27\",\n+  \"last_resync\": \"yesterday\",\n",
        "@@ -39,1 +39,1 @@\n-  \"loom_source_remote\": \"git@github.com:rjwalters/loom.git\"\n\
         +  \"loom_source_remote\": \"file:///tmp/evil\"\n",
        // The version field with NO validated pair to justify it.
        "@@ -2,1 +2,1 @@\n-  \"loom_version\": \"0.19.398\",\n+  \"loom_version\": \"9.9.9\",\n",
    ] {
        let files = vec![f(".loom/install-metadata.json", "modified", Some(patch))];
        assert_eq!(
            strip_validated_restamps(&files).len(),
            1,
            "must survive as a real change: {patch}"
        );
    }
}

#[test]
fn a_resync_stamp_riding_along_with_a_version_bump_is_still_discounted() {
    // The two restamps land in the same compare all the time: `D` spans many
    // commits, so a release bump and a resync are usually both in it.
    let files = vec![version_bump(), resync_stamp()];
    assert!(strip_validated_restamps(&files).is_empty(), "both are machine restamps");
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
