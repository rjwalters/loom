//! Tests for the Affected-Files preflight classifier (issue #9400).

use std::fs;
use std::path::Path;
use std::process::Command;

use super::*;

fn git_ok(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

/// A repo whose `HEAD` commits `src/kept.rs`, `README.md` and the symlink
/// `link -> src`, with `src/draft.rs` left untracked in the working tree.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git_ok(root, &["init", "-q"]);
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/kept.rs"), "fn kept() {}\n").unwrap();
    fs::write(root.join("README.md"), "# readme\n").unwrap();
    std::os::unix::fs::symlink("src", root.join("link")).unwrap();
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "-q", "-m", "fixture"]);
    fs::write(root.join("src/draft.rs"), "fn draft() {}\n").unwrap();
    dir
}

fn class_of(report: &Report, path: &str) -> PathClass {
    report
        .entries
        .iter()
        .find(|(p, _)| p == path)
        .unwrap_or_else(|| panic!("{path} not in {report:?}"))
        .1
}

#[test]
fn classifies_each_case_against_a_real_repo() {
    let repo = fixture();
    let body = "\
## Affected Files

- `src/kept.rs` - edit
- `src/draft.rs` - edit
- `src/made.rs` (new) - the verb
- `src/typo.rs` - edit

### New files

- `tests/made_tests.rs` - fixture
";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(class_of(&report, "src/kept.rs"), PathClass::Present);
    assert_eq!(class_of(&report, "src/draft.rs"), PathClass::Uncommitted);
    assert_eq!(class_of(&report, "src/made.rs"), PathClass::New);
    assert_eq!(class_of(&report, "tests/made_tests.rs"), PathClass::New);
    assert_eq!(class_of(&report, "src/typo.rs"), PathClass::Unknown);
    assert_eq!(report.exit_code(), EXIT_UNCOMMITTED);
    assert_eq!(
        report.render(),
        "uncommitted\tsrc/draft.rs\npresent\tsrc/kept.rs\nnew\tsrc/made.rs\n\
         unknown\tsrc/typo.rs\nnew\ttests/made_tests.rs\n"
    );
}

/// The reported bug: an issue listing existing files plus files it creates
/// must come back all-clear.
#[test]
fn existing_and_marked_new_paths_are_all_clear() {
    let repo = fixture();
    let body = "\
## Affected Files

- `src/kept.rs` - edit
- `README.md` - document it
- `src/a.rs` (new) - one
- `src/b.rs` (New file) - two
- `docs/c.md` - three (created)
";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(report.exit_code(), EXIT_CLEAR);
    assert_eq!(class_of(&report, "src/a.rs"), PathClass::New);
    assert_eq!(class_of(&report, "src/b.rs"), PathClass::New);
    assert_eq!(class_of(&report, "docs/c.md"), PathClass::New);
}

#[test]
fn unmarked_path_absent_everywhere_is_unknown_not_a_park() {
    let repo = fixture();
    let body = "## Affected Files\n\n- `src/kept.rs`\n- `src/typo.rs`\n";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(class_of(&report, "src/typo.rs"), PathClass::Unknown);
    assert_eq!(report.exit_code(), EXIT_UNKNOWN);
    assert_ne!(EXIT_UNKNOWN, EXIT_UNCOMMITTED);
}

#[test]
fn working_tree_only_path_is_the_parking_verdict() {
    let repo = fixture();
    let body = "## Affected Files\n\n- `src/draft.rs` - edit\n";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(class_of(&report, "src/draft.rs"), PathClass::Uncommitted);
    assert_eq!(report.exit_code(), EXIT_UNCOMMITTED);
}

/// A new marker hides neither a file on the base nor one in the working
/// tree: an untracked draft is the parking case whatever its line says.
#[test]
fn marker_precedence() {
    let repo = fixture();
    let body = "\
## Affected Files

- `src/kept.rs` (new)
- `src/draft.rs` (new)
- `src/made.rs` (new)

### New files

- `src/draft2.rs`
";
    fs::write(repo.path().join("src/draft2.rs"), "").unwrap();
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(class_of(&report, "src/kept.rs"), PathClass::Present);
    assert_eq!(class_of(&report, "src/draft.rs"), PathClass::Uncommitted);
    assert_eq!(class_of(&report, "src/draft2.rs"), PathClass::Uncommitted);
    assert_eq!(class_of(&report, "src/made.rs"), PathClass::New);
    assert_eq!(report.exit_code(), EXIT_UNCOMMITTED);
}

/// A marker spreads to every path on its line; an untracked one among them
/// still parks.
#[test]
fn line_marker_does_not_hide_an_untracked_path_on_the_same_line() {
    let repo = fixture();
    let body = "## Affected Files\n\n- `src/kept.rs` - move helpers into `src/draft.rs` (new)\n";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(class_of(&report, "src/draft.rs"), PathClass::Uncommitted);
    assert_eq!(report.exit_code(), EXIT_UNCOMMITTED);
}

/// An untracked root file is not a mention of a same-named base file in a
/// subdirectory.
#[test]
fn untracked_root_file_is_not_hidden_by_a_base_basename() {
    let repo = fixture();
    let root = repo.path();
    fs::write(root.join("kept.rs"), "").unwrap();
    fs::write(root.join("draft.rs"), "").unwrap();
    let body = "## Affected Files\n\n- `kept.rs`\n- `draft.rs`\n";
    let report = check(body, root, "HEAD").unwrap();
    assert_eq!(class_of(&report, "kept.rs"), PathClass::Uncommitted);
    assert_eq!(class_of(&report, "draft.rs"), PathClass::Uncommitted);
    assert_eq!(report.exit_code(), EXIT_UNCOMMITTED);
}

/// Glob-shaped spans that are real files in the working tree are checked;
/// ones that exist nowhere are patterns and are skipped.
#[test]
fn glob_shaped_span_that_exists_on_disk_is_classified() {
    let repo = fixture();
    let root = repo.path();
    fs::create_dir_all(root.join("app/[id]")).unwrap();
    fs::write(root.join("app/[id]/page.tsx"), "").unwrap();
    fs::create_dir(root.join("routes")).unwrap();
    fs::write(root.join("routes/$id.tsx"), "").unwrap();
    let body = "\
## Affected Files

- `app/[id]/page.tsx`
- `routes/$id.tsx`
- `src/*.rs` - every module
- `src/<name>.rs` - placeholder
";
    let report = check(body, root, "HEAD").unwrap();
    assert_eq!(class_of(&report, "app/[id]/page.tsx"), PathClass::Uncommitted);
    assert_eq!(class_of(&report, "routes/$id.tsx"), PathClass::Uncommitted);
    assert_eq!(report.entries.len(), 2, "{report:?}");
    assert_eq!(report.exit_code(), EXIT_UNCOMMITTED);
}

#[test]
fn unreachable_base_could_not_run() {
    let repo = fixture();
    let body = "## Affected Files\n\n- `src/kept.rs`\n";
    let err = check(body, repo.path(), "origin/main").unwrap_err();
    assert!(err.contains("unreachable"), "{err}");
    assert!(check(body, repo.path(), "--output=x").is_err());
    assert!(check(body, repo.path(), "").is_err());
}

#[test]
fn outside_a_git_checkout_could_not_run() {
    let dir = tempfile::tempdir().unwrap();
    assert!(check("## Affected Files\n\n- `src/a.rs`\n", dir.path(), "HEAD").is_err());
}

#[test]
fn exit_codes_are_distinct_and_avoid_the_usage_code() {
    let codes = [
        EXIT_CLEAR,
        EXIT_COULD_NOT_RUN,
        EXIT_UNKNOWN,
        EXIT_UNCOMMITTED,
    ];
    for (i, a) in codes.iter().enumerate() {
        // 2 is clap's "unrecognized subcommand" on a daemon predating the
        // verb; 127 is "no daemon at all". Neither may collide with a verdict.
        assert!(*a != 2 && *a != 127);
        for b in &codes[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn no_section_or_no_paths_is_a_clear_no_op() {
    let repo = fixture();
    for body in [
        "## Summary\n\nTouches `src/typo.rs`.\n",
        "## Affected Files\n\nTo be determined.\n",
        "",
    ] {
        let report = check(body, repo.path(), "HEAD").unwrap();
        assert!(report.entries.is_empty(), "{body:?}");
        assert_eq!(report.exit_code(), EXIT_CLEAR);
        assert_eq!(report.render(), "");
    }
}

#[test]
fn directories_and_symlinked_paths_on_base_are_present() {
    let repo = fixture();
    let body = "## Affected Files\n\n- `src/` - the module\n- `link/kept.rs` - via the link\n";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(class_of(&report, "src"), PathClass::Present);
    assert_eq!(class_of(&report, "link/kept.rs"), PathClass::Present);
    assert_eq!(report.exit_code(), EXIT_CLEAR);
}

/// A bare file name in a bullet's description ("lower the `kept.rs` entry")
/// refers to the base file of that name; a bare name matching nothing is
/// still a miss.
#[test]
fn bare_file_name_matching_a_base_file_is_a_mention_not_a_miss() {
    let repo = fixture();
    let body = "## Affected Files\n\n- `src/kept.rs` - the `kept.rs` entry, not `other.rs`\n";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(class_of(&report, "kept.rs"), PathClass::Present);
    assert_eq!(class_of(&report, "other.rs"), PathClass::Unknown);
    assert_eq!(report.exit_code(), EXIT_UNKNOWN);
}

#[test]
fn check_resolves_the_checkout_root_from_a_subdirectory() {
    let repo = fixture();
    let body = "## Affected Files\n\n- `src/kept.rs`\n- `src/draft.rs`\n";
    let report = check(body, &repo.path().join("src"), "HEAD").unwrap();
    assert_eq!(class_of(&report, "src/kept.rs"), PathClass::Present);
    assert_eq!(class_of(&report, "src/draft.rs"), PathClass::Uncommitted);
}

#[test]
fn marker_grammar() {
    for marked in [
        "- `a/b.rs` (new)",
        "- `a/b.rs` (NEW) - what",
        "- `a/b.rs` - (New File)",
        "- (create) `a/b.rs`",
        "- `a/b.rs` (Created)",
    ] {
        assert!(has_new_marker(marked), "{marked}");
    }
    for unmarked in [
        "- `a/b.rs` - new helper",
        "- `a/b.rs` - renew (newer)",
        "- `a/(new)/b.rs` - edit",
        "- `a/b.rs` `(new)`",
    ] {
        assert!(!has_new_marker(unmarked), "{unmarked}");
    }
}

#[test]
fn paths_are_normalised_before_lookup() {
    let body = "\
## Affected Files

- `./src/a.rs:42` - one line
- `src/b.rs:40-60` - a range
- `src/dir/` - a directory
- `Cargo.toml`
";
    let listed = listed_paths(body);
    let paths: Vec<&str> = listed.keys().map(String::as_str).collect();
    assert_eq!(paths, ["Cargo.toml", "src/a.rs", "src/b.rs", "src/dir"]);
}

#[test]
fn spans_that_cannot_name_one_repo_file_are_not_checked() {
    let body = "\
## Affected Files

- `https://example.com/a/b.md`
- `/tmp/scratch.txt` and `~/notes.md`
- `../sibling/x.rs`
- `--body-file=/x/y.md`
- `WorkItem.body`, `loom:blocked`
";
    assert!(listed_paths(body).is_empty(), "{:?}", listed_paths(body));
}

#[test]
fn section_walk_keeps_sub_headings_and_ends_at_a_peer() {
    let body = "\
# Title

Mentions `before/x.rs`.

## Affected Files

- `a/one.rs`

### New files

- `a/two.rs`

### Modified

- `a/three.rs`

#### New tests

- `a/four.rs`

## Test Plan

- `after/y.rs` (new)
";
    let listed = listed_paths(body);
    let got: Vec<(&str, bool)> = listed.iter().map(|(p, n)| (p.as_str(), *n)).collect();
    assert_eq!(
        got,
        [
            ("a/four.rs", true),
            ("a/one.rs", false),
            ("a/three.rs", false),
            ("a/two.rs", true),
        ]
    );
}

#[test]
fn a_path_marked_on_any_line_is_marked() {
    let body = "## Affected Files\n\n- `a/x.rs` - wire it\n- `a/x.rs` (new)\n";
    assert_eq!(listed_paths(body).get("a/x.rs"), Some(&true));
}

#[test]
fn issue_refs_and_hash_runs_are_not_headings() {
    assert_eq!(heading("## Affected Files"), Some((2, "affected files".into())));
    assert_eq!(heading("###"), Some((3, String::new())));
    assert_eq!(heading("#9781 overlap gate"), None);
    assert_eq!(heading("####### seven"), None);
    let body = "## Affected Files\n\n- `a/x.rs`\n#9781 is related\n- `a/y.rs`\n";
    assert_eq!(listed_paths(body).len(), 2);
}

/// The overlap gate's surface is untouched: it still ends at any heading.
#[test]
fn overlap_surface_behaviour_is_unchanged() {
    let body = "## Affected Files\n\n- `a/x.rs`\n\n### New files\n\n- `a/y.rs`\n";
    let surface = super::super::affected_surface(body).unwrap();
    assert_eq!(surface.into_iter().collect::<Vec<_>>(), ["a/x.rs"]);
    assert_eq!(listed_paths(body).len(), 2);
}
