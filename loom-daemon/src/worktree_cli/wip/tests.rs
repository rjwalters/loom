//! Unit tests for the WIP-shelving family's shared plumbing.
//!
//! Everything here is deliberately free of process-global state (no `cwd`, no
//! env mutation) so it is safe under `cargo test`'s default parallelism. The
//! behavioural evidence — the verbs end to end, including the space-in-path
//! regression the whole port exists for — lives in
//! `loom-daemon/tests/worktree_wip_verbs.rs`, which drives the real binary the
//! way `worktree.sh` does.

use super::*;

// ---------------------------------------------------------------------------
// Target grammar
// ---------------------------------------------------------------------------

#[test]
fn a_bare_number_is_an_issue_target() {
    assert_eq!(Target::parse("42"), Some(Target::Issue(42)));
    assert_eq!(Target::parse("0"), Some(Target::Issue(0)));
}

#[test]
fn main_is_accepted_only_in_exactly_that_spelling() {
    assert_eq!(Target::parse("main"), Some(Target::Main));
    // The retained suite asserts this: a case-folded match would let `MAIN`
    // silently `git reset --hard` the PRIMARY clone, which is the one tree
    // where that is least recoverable.
    assert_eq!(Target::parse("MAIN"), None);
    assert_eq!(Target::parse("Main"), None);
}

#[test]
fn anything_else_is_rejected_rather_than_coerced() {
    for bad in [
        "",
        "not-a-target",
        "42x",
        " 42",
        "42 ",
        "-1",
        "+1",
        "4_2",
        "master",
        "main ",
    ] {
        assert_eq!(Target::parse(bad), None, "{bad:?} must not parse");
    }
}

#[test]
fn slugs_refs_and_json_fields_match_the_shell() {
    let issue = Target::Issue(301);
    assert_eq!(issue.slug(), "issue-301");
    assert_eq!(issue.label(), "301");
    assert_eq!(issue.json_issue(), "301");
    assert_eq!(issue.baseline_ref(), "refs/loom/stash-baseline/issue-301");

    let main = Target::Main;
    assert_eq!(main.slug(), "main");
    assert_eq!(main.label(), "main");
    // Not the string "main": an existing consumer must not mis-parse it as a
    // number, and the retained suite greps for `"issueNumber": null`.
    assert_eq!(main.json_issue(), "null");
    assert_eq!(main.baseline_ref(), "refs/loom/stash-baseline/main");
}

// ---------------------------------------------------------------------------
// JSON escaping
// ---------------------------------------------------------------------------

#[test]
fn a_path_with_a_quote_or_backslash_still_produces_parseable_json() {
    // The shell interpolated paths into `printf '…"%s"…'` raw, so a worktree
    // root containing a quote produced a document nothing could parse. Under
    // LOOM_WORKTREE_ROOT the prefix is operator-supplied, so this is reachable.
    let nasty = r#"/tmp/we"ird\path/issue-1.patch"#;
    let doc = format!("{{\"patchPath\": \"{}\"}}", json_str(nasty));
    let parsed: serde_json::Value = serde_json::from_str(&doc).expect("valid JSON");
    assert_eq!(parsed["patchPath"], nasty);
}

#[test]
fn ordinary_paths_are_passed_through_byte_for_byte() {
    // The escaping must be invisible for every path that was already fine —
    // otherwise it is a stdout-contract change, not a bug fix.
    for p in [
        "/repo/.loom/worktrees/.snapshots/issue-7-20260101T000000Z.patch",
        "/home/user/my repo/issue-1.patch",
        "/tmp/ünïcode/issue-2.patch",
    ] {
        assert_eq!(json_str(p), p);
    }
}

// ---------------------------------------------------------------------------
// Path normalisation
// ---------------------------------------------------------------------------

#[test]
fn normalisation_folds_dot_and_dotdot_the_way_cd_and_pwd_do() {
    assert_eq!(
        lexical_normalize(std::path::Path::new("/repo/.git/..")),
        std::path::PathBuf::from("/repo")
    );
    assert_eq!(
        lexical_normalize(std::path::Path::new("/repo/./sub")),
        std::path::PathBuf::from("/repo/sub")
    );
    // Symlinks are NOT resolved: `cd … && pwd` is logical, and canonicalising
    // would print a different path than the shell did for the same input.
    assert_eq!(
        lexical_normalize(std::path::Path::new("/a/b")),
        std::path::PathBuf::from("/a/b")
    );
}

// ---------------------------------------------------------------------------
// The Loom-marker filter
// ---------------------------------------------------------------------------

/// Build a throwaway repo with `real.txt` committed, every Loom runtime marker
/// present as an untracked file, and deliberately NO `.gitignore` — the drift
/// this filter guards only shows in a repo that does not ignore the markers,
/// which is exactly the case the filter exists for.
fn repo_with_markers(dir: &std::path::Path, extra: &[&str]) {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git runs");
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("real.txt"), "x\n").expect("write");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    for m in [
        ".loom-managed",
        ".loom-in-use",
        ".loom-checkpoint",
        ".no-changes-needed",
    ] {
        std::fs::write(dir.join(m), "").expect("marker");
    }
    for e in extra {
        let p = dir.join(e);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&p, "wip\n").expect("extra");
    }
}

#[test]
fn loom_runtime_markers_are_never_reported_as_untracked_work() {
    // This is not cosmetic. `stash-push --include-untracked` MOVES every file
    // this function reports out of the worktree; carrying `.loom-managed` away
    // makes every cleanup path refuse the worktree afterwards (#3548).
    let d = tempfile::tempdir().expect("tempdir");
    repo_with_markers(d.path(), &["real-wip.txt"]);

    let seen = untracked_files(d.path());
    assert_eq!(
        seen,
        vec!["real-wip.txt".to_string()],
        "only genuine WIP may be reported; got {seen:?}"
    );
}

#[test]
fn a_marker_in_a_subdirectory_is_filtered_too() {
    // The shell twin anchors on `(^|/)\.loom-managed$`, i.e. the final path
    // component — so a nested marker is bookkeeping there, and must be here.
    let d = tempfile::tempdir().expect("tempdir");
    repo_with_markers(d.path(), &["nested/.loom-in-use", "nested/real.txt"]);

    let seen = untracked_files(d.path());
    assert_eq!(seen, vec!["nested/real.txt".to_string()], "got {seen:?}");
}

#[test]
fn a_file_merely_starting_with_a_marker_name_is_still_work() {
    // The filter must be exact-name, not prefix: `.loom-managed.bak` is a
    // user's file and moving it out would be data loss.
    let d = tempfile::tempdir().expect("tempdir");
    repo_with_markers(d.path(), &[".loom-managed.bak", "loom-managed"]);

    let mut seen = untracked_files(d.path());
    seen.sort();
    assert_eq!(
        seen,
        vec![".loom-managed.bak".to_string(), "loom-managed".to_string()],
        "got {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// move_file
// ---------------------------------------------------------------------------

#[test]
fn moving_a_file_leaves_exactly_one_copy() {
    let d = tempfile::tempdir().expect("tempdir");
    let src = d.path().join("a.txt");
    let dest = d.path().join("sub/a.txt");
    std::fs::create_dir_all(d.path().join("sub")).expect("mkdir");
    std::fs::write(&src, "payload").expect("write");

    assert!(move_file(&src, &dest));
    assert!(!src.exists(), "the source must be gone");
    assert_eq!(
        std::fs::read_to_string(&dest).expect("read"),
        "payload",
        "content must survive the move"
    );
}

#[test]
fn a_move_that_cannot_happen_reports_failure_rather_than_half_doing_it() {
    let d = tempfile::tempdir().expect("tempdir");
    let src = d.path().join("missing.txt");
    let dest = d.path().join("dest.txt");
    assert!(!move_file(&src, &dest));
    assert!(!dest.exists(), "a failed move must not leave a destination behind");
}

#[test]
fn a_path_with_spaces_and_shell_metacharacters_moves_intact() {
    // #7858's class, at the smallest scale it occurs: in bash every one of
    // these names had to survive word splitting at each interpolation, and one
    // that did not turned a cleanup into an `rm -rf` on a live worktree.
    let d = tempfile::tempdir().expect("tempdir");
    for name in [
        "a file with spaces.txt",
        "semi;colon.txt",
        "dollar $(whoami).txt",
        "tab\tname.txt",
        "quote\"name.txt",
        "star*.txt",
    ] {
        let src = d.path().join(name);
        let dest = d.path().join(format!("moved-{name}"));
        std::fs::write(&src, name).expect("write");
        assert!(move_file(&src, &dest), "must move {name:?}");
        assert_eq!(std::fs::read_to_string(&dest).expect("read"), name);
        assert!(!src.exists(), "{name:?} must not remain at the source");
    }
}
