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

// ---- Issue #11440: gaps the #11420 review found ----

fn marks(body: &str) -> Vec<(String, bool)> {
    listed_paths(body).into_iter().collect()
}

/// Gap 1: a `# comment` inside a code fence is not a heading, so it neither
/// ends the section nor opens a sub-section.
#[test]
fn fenced_comment_lines_are_not_headings() {
    let body = "\
## Affected Files

- `a/one.rs`

```bash
# comment: build first
## Affected Files
### New files
cargo run
```

- `a/two.rs`

~~~
# Test Plan
```
### New files
~~~

- `a/three.rs`

## Test Plan

- `after/y.rs`
";
    assert_eq!(
        marks(body),
        [
            ("a/one.rs".to_string(), false),
            ("a/three.rs".to_string(), false),
            ("a/two.rs".to_string(), false),
        ]
    );
}

/// Gap 1 through `check`: the untracked file listed after the fence parks.
#[test]
fn untracked_path_after_a_fenced_comment_is_uncommitted() {
    let repo = fixture();
    let body = "\
## Affected Files

- `src/kept.rs` - edit

```bash
# build first
cargo build
```

- `src/draft.rs` - edit
";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(class_of(&report, "src/draft.rs"), PathClass::Uncommitted);
    assert_eq!(report.exit_code(), EXIT_UNCOMMITTED);
}

/// Gap 1, the pinned rule for a fence that never closes: it is not a fence.
/// One opened above the section must not hide the section, and one opened
/// inside it must not swallow the heading that ends it.
#[test]
fn unclosed_fence_is_not_a_fence() {
    let repo = fixture();
    let above = "```bash\ncargo build\n\n## Affected Files\n\n- `src/draft.rs`\n";
    let report = check(above, repo.path(), "HEAD").unwrap();
    assert_eq!(class_of(&report, "src/draft.rs"), PathClass::Uncommitted);
    assert_eq!(report.exit_code(), EXIT_UNCOMMITTED);

    let inside = "## Affected Files\n\n- `a/x.rs`\n\n~~~\n\n## Test Plan\n\n- `after/y.rs`\n";
    assert_eq!(marks(inside), [("a/x.rs".to_string(), false)]);
}

/// Gap 1: fences pair as in CommonMark, so a line that cannot close the open
/// fence does not end it early and expose the `#` lines after it.
#[test]
fn fence_closes_only_on_a_matching_run() {
    for (open, not_a_close, close) in [
        ("```", "~~~", "```"),
        ("~~~", "```", "~~~"),
        ("````", "```", "`````"),
        ("```", "```bash", "```"),
        ("~~~sh", "~~~ x", "  ~~~~  "),
    ] {
        let body = format!(
            "## Affected Files\n\n{open}\n{not_a_close}\n# Test Plan\n{close}\n\n- `a/x.rs`\n"
        );
        assert_eq!(marks(&body), [("a/x.rs".to_string(), false)], "{open} / {not_a_close}");
    }
    // An inline span that starts a line is not a fence opener: its path is
    // listed and the heading below it still ends the section.
    let body = "## Affected Files\n\n```a/x.rs``` and `a/y.rs`\n# Test Plan\n- `after/z.rs`\n";
    assert_eq!(marks(body), [("a/x.rs".to_string(), false), ("a/y.rs".to_string(), false)]);
    // A fence line's info string is not a listed path.
    let body = "## Affected Files\n\n```src/info.rs\n# c\n```\n- `a/x.rs`\n";
    assert_eq!(marks(body), [("a/x.rs".to_string(), false)]);
}

/// Gap 2: a `#fragment` is not part of the file name, with or without a `/`.
#[test]
fn fragment_is_stripped_before_lookup() {
    let repo = fixture();
    let root = repo.path();
    fs::create_dir(root.join("docs")).unwrap();
    fs::write(root.join("docs/notes.md"), "").unwrap();
    fs::write(root.join("notes.md"), "").unwrap();
    for (span, file) in [
        ("docs/notes.md#usage", "docs/notes.md"),
        ("notes.md#usage", "notes.md"),
        ("./docs/notes.md#L3-L9", "docs/notes.md"),
    ] {
        let body = format!("## Affected Files\n\n- `{span}` - see the anchor (new)\n");
        let report = check(&body, root, "HEAD").unwrap();
        assert_eq!(report.render(), format!("uncommitted\t{file}\n"), "{span}");
        assert_eq!(report.exit_code(), EXIT_UNCOMMITTED, "{span}");
    }

    let body = "\
## Affected Files

- `src/kept.rs#L10` and `README.md#install` - tracked
- `owner/repo#12` - an issue reference
- `docs/missing.md#x` and `missing.md#x` - exist nowhere
- `src/made.rs#L1` (new) - created by the issue
";
    let report = check(body, root, "HEAD").unwrap();
    assert_eq!(report.render(), "present\tREADME.md\npresent\tsrc/kept.rs\n");
    assert_eq!(report.exit_code(), EXIT_CLEAR);
}

/// Gap 2: a file whose own name holds a `#` is still looked up literally.
#[test]
fn file_with_a_hash_in_its_name_is_classified_as_itself() {
    let repo = fixture();
    let root = repo.path();
    fs::write(root.join("src/c#.rs"), "").unwrap();
    fs::write(root.join("notes#1.md"), "").unwrap();
    let body = "## Affected Files\n\n- `src/c#.rs`\n- `notes#1.md`\n";
    let report = check(body, root, "HEAD").unwrap();
    assert_eq!(report.render(), "uncommitted\tnotes#1.md\nuncommitted\tsrc/c#.rs\n");
}

/// `owner/repo#12` is an issue reference, not a file (the case
/// `spans_that_cannot_name_one_repo_file_are_not_checked` used to hold).
#[test]
fn issue_reference_span_is_not_reported() {
    let repo = fixture();
    let body = "## Affected Files\n\n- `owner/repo#12`, `rjwalters/loom#9400`, `#12`\n";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert!(report.entries.is_empty(), "{report:?}");
    assert_eq!(report.exit_code(), EXIT_CLEAR);
}

/// Gap 3: an untracked path is uncommitted whatever it points at. Only
/// symlinks committed on the base are followed.
#[test]
fn untracked_symlink_to_a_tracked_file_is_uncommitted() {
    use std::os::unix::fs::symlink;
    let repo = fixture();
    let root = repo.path();
    symlink("README.md", root.join("alias.md")).unwrap();
    symlink("kept.rs", root.join("src/alias.rs")).unwrap();
    symlink("src", root.join("udir")).unwrap();
    let body = "\
## Affected Files

- `alias.md` - untracked link to a tracked file
- `src/alias.rs` and `link/alias.rs` - the same, in a tracked directory
- `udir/kept.rs` - through an untracked directory link
- `link/kept.rs` and `link/draft.rs` - through the committed link
";
    let report = check(body, root, "HEAD").unwrap();
    for path in [
        "alias.md",
        "src/alias.rs",
        "link/alias.rs",
        "udir/kept.rs",
        "link/draft.rs",
    ] {
        assert_eq!(class_of(&report, path), PathClass::Uncommitted, "{path}");
    }
    assert_eq!(class_of(&report, "link/kept.rs"), PathClass::Present);
    assert_eq!(report.exit_code(), EXIT_UNCOMMITTED);

    let alone = "## Affected Files\n\n- `alias.md`\n";
    assert_eq!(check(alone, root, "HEAD").unwrap().exit_code(), EXIT_UNCOMMITTED);
}

/// Gap 3: a committed symlink that leaves the checkout or loops never reads
/// as `present`. It does not park either: what is behind it is not a file in
/// the working tree.
#[test]
fn committed_symlink_leaving_the_checkout_is_not_followed() {
    use std::os::unix::fs::symlink;
    let repo = fixture();
    let root = repo.path();
    symlink("..", root.join("up")).unwrap();
    symlink("/", root.join("abs")).unwrap();
    symlink("loop", root.join("loop")).unwrap();
    git_ok(root, &["add", "-A", "up", "abs", "loop"]);
    git_ok(root, &["commit", "-q", "-m", "links"]);
    // `up/<checkout>/README.md` is a real file on disk, reached by leaving
    // the checkout and coming back in.
    let own = root.canonicalize().unwrap();
    let own = own.file_name().unwrap().to_str().unwrap();
    let body = format!(
        "## Affected Files\n\n- `up/nope/x.rs`\n- `abs/nope/x.rs`\n- `loop/x.rs`\n\
         - `up/{own}/README.md`\n- `up/{own}/src/draft.rs`\n"
    );
    let report = check(&body, root, "HEAD").unwrap();
    assert_eq!(report.entries.len(), 5, "{report:?}");
    for (path, class) in &report.entries {
        assert_eq!(*class, PathClass::Unknown, "{path}");
    }
    assert_eq!(report.exit_code(), EXIT_UNKNOWN);
}

/// Gap 3: the link target is the base's, not the working tree's. A Builder
/// worktree gets the committed link whatever the Curator did to its copy.
#[test]
fn committed_symlink_target_is_read_from_the_base() {
    use std::os::unix::fs::symlink;
    let repo = fixture();
    let root = repo.path();
    let body = "## Affected Files\n\n- `link/kept.rs`\n";

    fs::remove_file(root.join("link")).unwrap();
    let report = check(body, root, "HEAD").unwrap();
    assert_eq!(report.render(), "present\tlink/kept.rs\n");

    // Repointed locally at a directory holding an untracked `kept.rs`.
    fs::create_dir(root.join("other")).unwrap();
    fs::write(root.join("other/kept.rs"), "").unwrap();
    symlink("other", root.join("link")).unwrap();
    let report = check(body, root, "HEAD").unwrap();
    assert_eq!(report.render(), "present\tlink/kept.rs\n");
}

#[test]
fn base_links_resolve_relative_targets_inside_the_checkout() {
    let target_of = |path: &str| match path {
        "a/roles" => Some("../defaults/roles".to_string()),
        "defaults/roles/cur.md" => Some("./base.md".to_string()),
        "out" => Some("../elsewhere".to_string()),
        "abs" => Some("/etc".to_string()),
        "spin" => Some("spin".to_string()),
        _ => None,
    };
    let via = |path: &str| through_base_links(path, target_of);
    assert_eq!(via("src/x.rs").as_deref(), Some("src/x.rs"));
    assert_eq!(via("a/roles/x.md").as_deref(), Some("defaults/roles/x.md"));
    assert_eq!(via("a/roles/cur.md").as_deref(), Some("defaults/roles/base.md"));
    assert_eq!(via("out/x.rs"), None);
    assert_eq!(via("abs/hosts"), None);
    assert_eq!(via("spin/x.rs"), None);
}

/// Gap 4: a sub-heading marks its paths new only when `New` is a whole word.
#[test]
fn new_sub_heading_needs_the_whole_word() {
    for (title, marked) in [
        ("New", true),
        ("New files", true),
        ("New modules", true),
        ("NEW: tests", true),
        ("New/created", true),
        ("Newsletter templates", false),
        ("Newer modules", false),
        ("Renewed", false),
    ] {
        let body = format!("## Affected Files\n\n### {title}\n\n- `a/x.rs`\n");
        assert_eq!(listed_paths(&body)["a/x.rs"], marked, "{title}");
    }
    // Through `check`: a typo under `### Newsletter` asks instead of clearing.
    let repo = fixture();
    let body = "## Affected Files\n\n### Newsletter templates\n\n- `src/typo.rs`\n";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(class_of(&report, "src/typo.rs"), PathClass::Unknown);
    assert_eq!(report.exit_code(), EXIT_UNKNOWN);
}

/// Gap 5: a tracked file spelled in the wrong case is a typo on every
/// platform. On a case-insensitive filesystem a plain stat finds the tracked
/// file and would park the issue.
#[test]
fn wrong_case_spelling_of_a_tracked_file_is_not_uncommitted() {
    let repo = fixture();
    let body = "## Affected Files\n\n- `readme.md`\n- `SRC/kept.rs`\n- `Link/kept.rs`\n";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(class_of(&report, "readme.md"), PathClass::Unknown);
    assert_eq!(class_of(&report, "SRC/kept.rs"), PathClass::Unknown);
    assert_eq!(class_of(&report, "Link/kept.rs"), PathClass::Unknown);
    assert_eq!(report.exit_code(), EXIT_UNKNOWN);
}

/// No false park: none of the #11440 rules parks a file the issue creates
/// (absent from both trees) or a tracked file that is listed correctly.
#[test]
fn created_and_correctly_listed_files_never_park() {
    let repo = fixture();
    let body = "\
## Affected Files

- `src/kept.rs:12-40`, `README.md` and `README.md#install` - edit
- `link/kept.rs` - the same file through the committed link
- `src/` and `link/` - the directory, both ways
- `src/made.rs` (new) and `link/made.rs` (new) - created here

```bash
# New files come from this
touch `src/gen.rs`
```

### New files

- `docs/made.md` - created here
- `tests/Made.rs` - created here
";
    let report = check(body, repo.path(), "HEAD").unwrap();
    assert_eq!(
        report.render(),
        "present\tREADME.md\nnew\tdocs/made.md\npresent\tlink\npresent\tlink/kept.rs\n\
         new\tlink/made.rs\npresent\tsrc\nunknown\tsrc/gen.rs\npresent\tsrc/kept.rs\n\
         new\tsrc/made.rs\nnew\ttests/Made.rs\n"
    );
    assert!(report
        .entries
        .iter()
        .all(|(_, c)| *c != PathClass::Uncommitted));
    assert_eq!(report.exit_code(), EXIT_UNKNOWN);

    let clear = body.replace("touch `src/gen.rs`", "touch src/gen.rs");
    assert_eq!(check(&clear, repo.path(), "HEAD").unwrap().exit_code(), EXIT_CLEAR);
}
