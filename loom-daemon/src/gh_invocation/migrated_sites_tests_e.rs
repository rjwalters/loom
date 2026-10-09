#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! W5: every daemon `--paginate` site is walkable when the switch is on,
//! or is listed here with the reason it is not.
//!
//! `gh api --paginate` is one process but one request **per page**. With
//! `LOOM_GH_PAGE_WALK=1` the facade walks a REST `--paginate` read page by
//! page (`super::paged`), one ledger row each — but only for an argv it
//! fully understands, and only for a call that goes through the facade at
//! all. The walk is **off by default**, so this lint does not say a site's
//! pages are counted today: it says they *would be* the moment the switch is
//! on, which is what makes the one-host validation of the switch cover every
//! site. It reads every `"--paginate"` literal in the daemon's non-test
//! source and fails on one that is neither:
//!
//! - **walkable when the switch is on** — in a file that builds through the
//!   facade, in a statement whose flags are all ones the walk carries (or
//!   `--include`, whose own status blocks are counted with or without the
//!   switch), with no `graphql` endpoint and no raw spawn;
//! - **listed** in [`UNCOUNTED`] with a reason and the exact number of
//!   literals the file holds, so a second site in a listed file is a failure
//!   too, and an entry that no longer matches must be removed.
//!
//! The issue listings that page with `page=N` themselves
//! (`forge_listing::list_issues_cached_all_as`) never pass `--paginate`: each
//! page is already its own conditional request and row.

use crate::forge_call_stats;
use crate::types::ForgeCallCounts;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// `(file, "--paginate" literals in it, why they are not walkable)`.
/// Adding an entry is a decision to leave pages uncounted even with the
/// switch on: prefer routing the read through `GhInvocation` with a
/// walkable argv.
const UNCOUNTED: &[(&str, usize, &str)] = &[
    (
        "premise_check/cli.rs",
        1,
        "GraphQL cursor pagination issued through the raw `script_helpers::run_gh` spawn: the \
         walk follows REST `Link` headers only, and the call is not a facade row at all",
    ),
    (
        "merge_pr/redate.rs",
        1,
        "`gh_api_with` is a raw `Command` spawn (it pipes a request body on stdin) outside the \
         facade: no ledger row, so no page count; the choke-point ratchet owns its migration",
    ),
    (
        "cli/pr_latency_cmd.rs",
        1,
        "operator report CLI through the raw `script_helpers::run_gh` spawn: not a facade row; \
         the choke-point ratchet owns its migration",
    ),
    (
        "cli/forge_identity_cmd.rs",
        1,
        "`forge verdict` CLI through the raw `script_helpers::run_gh` spawn: not a facade row; \
         the choke-point ratchet owns its migration",
    ),
    (
        "comment_trust/records.rs",
        1,
        "trusted-comment CLI read through the raw `script_helpers::run_gh` spawn (optionally \
         `gh-cached`): not a facade row; the choke-point ratchet owns its migration",
    ),
    (
        "observability/pick_journal.rs",
        1,
        "not a call: the boolean-flag table of the agent argv parser",
    ),
    (
        "live_gh_guard.rs",
        2,
        "not a daemon call: the guard's inline tests spawn a stub to prove a live `gh` is refused",
    ),
];

/// Flags the page walk carries (`paged::CARRIED_VALUED`), plus `--include`:
/// a `--paginate --include` execution is counted from its own status blocks.
const COUNTED_FLAGS: &[&str] = &[
    "--paginate",
    "--jq",
    "-q",
    "-H",
    "--header",
    "-p",
    "--preview",
    "--hostname",
    "--include",
    "-i",
];

/// What makes a statement something the facade does not walk.
const UNWALKED_MARKS: &[&str] = &[
    "\"graphql\"",
    ".passthrough()",
    "run_gh(",
    "gh_api_with(",
    "Command::new",
];

fn is_test_file(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.ends_with("tests.rs")
        || name.contains("_tests_")
        || rel.contains("/tests/")
        || rel.starts_with("tests/")
        || rel.contains("test_support")
}

/// `text` with `//` comments removed, by line.
fn code_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(|l| l.split("//").next().unwrap_or_default())
        .collect()
}

/// Why the `--paginate` literal on line `n` is not a site the walk takes
/// when the switch is on, if it is not.
fn site_problem(lines: &[&str], n: usize, facade: bool) -> Option<String> {
    // The statement: back to the line naming the `api` subcommand, forward
    // to the end of the statement, each at most 8 lines away.
    let mut start = n;
    while start > 0 && n - start < 8 && !lines[start].contains("\"api\"") {
        start -= 1;
    }
    let mut end = n;
    while end + 1 < lines.len() && end - n < 8 && !lines[end].contains(';') {
        end += 1;
    }
    let statement = lines[start..=end].join("\n");
    if let Some(mark) = UNWALKED_MARKS.iter().find(|m| statement.contains(**m)) {
        return Some(format!("the statement has `{mark}`, which the page walk does not take"));
    }
    let flag = regex::Regex::new(r#""(-[A-Za-z-]+)""#).unwrap();
    if let Some(other) = flag
        .captures_iter(&statement)
        .map(|c| c[1].to_string())
        .find(|f| !COUNTED_FLAGS.contains(&f.as_str()))
    {
        return Some(format!("the statement passes `{other}`, which the page walk does not carry"));
    }
    (!facade).then(|| "the file never builds through the gh facade".to_string())
}

/// Every problem in `files` (`(path relative to src/, text)`), given the
/// allowlist. Pure, so the failure modes are themselves tested.
fn lint(files: &[(String, String)], uncounted: &[(&str, usize, &str)]) -> Vec<String> {
    let facade =
        regex::Regex::new(r"GhInvocation|gh_call::read|gh_inv\(|gh_read(_own_write)?\(").unwrap();
    let mut problems = Vec::new();
    for (rel, text) in files {
        let lines = code_lines(text);
        let sites: Vec<usize> = (0..lines.len())
            .filter(|n| lines[*n].contains("\"--paginate\""))
            .collect();
        if let Some((_, count, _)) = uncounted.iter().find(|(f, _, _)| f == rel) {
            if sites.len() != *count {
                problems.push(format!(
                    "{rel}: listed as uncounted with {count} `--paginate` literal(s) but has {}; \
                     a new site must be page-counted, and a removed one must leave the list",
                    sites.len()
                ));
            }
            continue;
        }
        let through_facade = facade.is_match(&lines.join("\n"));
        for n in sites {
            if let Some(why) = site_problem(&lines, n, through_facade) {
                problems.push(format!("{rel}:{}: uncounted `--paginate` site: {why}", n + 1));
            }
        }
    }
    for (file, _, reason) in uncounted {
        if !files.iter().any(|(rel, _)| rel == file) {
            problems.push(format!("{file}: listed as uncounted but is not a daemon source file"));
        }
        if reason.trim().len() < 20 {
            problems.push(format!("{file}: an uncounted entry needs a real reason"));
        }
    }
    problems
}

fn daemon_sources() -> Vec<(String, String)> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            // The facade's own argv handling is not a call site.
            if path.extension().is_none_or(|e| e != "rs")
                || is_test_file(&rel)
                || rel.starts_with("gh_invocation/")
            {
                continue;
            }
            files.push((rel, std::fs::read_to_string(&path).unwrap()));
        }
    }
    files
}

#[test]
fn every_daemon_paginate_site_is_walkable_when_the_switch_is_on_or_listed_with_a_reason() {
    let files = daemon_sources();
    assert!(files.len() > 100, "the scan found only {} files", files.len());
    let problems = lint(&files, UNCOUNTED);
    assert!(
        problems.is_empty(),
        "a `--paginate` read is charged one request per page (W5). Route it through \
         `GhInvocation` with an argv the page walk takes when `LOOM_GH_PAGE_WALK=1` (see \
         `gh_invocation/paged.rs`), or list it in UNCOUNTED with the reason its pages cannot be \
         counted:\n  {}",
        problems.join("\n  ")
    );
}

fn file(rel: &str, text: &str) -> Vec<(String, String)> {
    vec![(rel.to_string(), text.to_string())]
}

const WALKED_SITE: &str = r#"
fn comments(gh: &Path, root: &Path) -> Option<Vec<u8>> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/7/comments?per_page=100");
    gh_call::ok_stdout(gh_call::read("verdict.pr_comments", gh, root).args([
        "api",
        &path,
        "--paginate",
        "--jq",
        ".[].body",
    ]))
}
"#;

#[test]
fn a_rest_site_through_the_facade_passes() {
    assert_eq!(lint(&file("new/site.rs", WALKED_SITE), &[]), Vec::<String>::new());
    // `--include` beside `--paginate` is counted from its status blocks.
    let included = WALKED_SITE.replace("\"--jq\",", "\"--include\",");
    assert_eq!(lint(&file("new/site.rs", &included), &[]), Vec::<String>::new());
}

#[test]
fn a_new_uncounted_site_fails_the_lint() {
    // GraphQL cursor pagination, even through the facade.
    let graphql = r#"
fn nodes(root: &Path) -> CmdOutcome {
    GhInvocation::new(op(), AccessIntent::Read, GhTarget::None, T)
        .args(["api", "graphql", "--paginate", "-f", QUERY])
        .current_dir(root)
        .run()
}
"#;
    let problems = lint(&file("new/graphql.rs", graphql), &[]);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("new/graphql.rs:4: uncounted `--paginate` site"),
        "{problems:?}"
    );

    // A raw spawn: not a ledger row at all.
    let raw = r#"
fn listing(root: &Path) -> CmdOutcome {
    crate::script_helpers::run_gh(&["api", "repos/o/r/issues", "--paginate"], root, false)
}
"#;
    let problems = lint(&file("new/raw.rs", raw), &[]);
    assert!(problems.len() == 1 && problems[0].contains("run_gh("), "{problems:?}");

    // A flag the walk does not carry keeps the single execution.
    let slurped = WALKED_SITE.replace("\"--jq\",", "\"--slurp\",");
    let problems = lint(&file("new/slurp.rs", &slurped), &[]);
    assert!(problems.len() == 1 && problems[0].contains("`--slurp`"), "{problems:?}");

    // A wrapper this lint does not know to be the facade.
    let unknown = "fn f() { my_gh(&[\"api\", \"repos/o/r/issues\", \"--paginate\"]); }\n";
    let problems = lint(&file("new/wrapper.rs", unknown), &[]);
    assert!(problems.len() == 1 && problems[0].contains("gh facade"), "{problems:?}");

    // A comment is not a site.
    let prose = "// a `--paginate` read and \"--paginate\" in prose\nfn f() {}\n";
    assert_eq!(lint(&file("new/prose.rs", prose), &[]), Vec::<String>::new());
}

#[test]
fn the_allowlist_is_exact_and_cannot_go_stale() {
    let raw = "fn f() { run_gh(&[\"api\", \"x\", \"--paginate\"], root, false); }\n";
    let listed: &[(&str, usize, &str)] = &[("old/raw.rs", 1, "a raw spawn outside the facade")];
    assert_eq!(lint(&file("old/raw.rs", raw), listed), Vec::<String>::new());
    // A second site in a listed file is not covered by the entry.
    let two = format!("{raw}{raw}");
    let problems = lint(&file("old/raw.rs", &two), listed);
    assert!(problems.len() == 1 && problems[0].contains("has 2"), "{problems:?}");
    // An entry whose site is gone, or whose file is gone, must be removed.
    let problems = lint(&file("old/raw.rs", "fn f() {}\n"), listed);
    assert!(problems.len() == 1 && problems[0].contains("has 0"), "{problems:?}");
    let problems = lint(&file("other.rs", "fn f() {}\n"), listed);
    assert!(
        problems.len() == 1 && problems[0].contains("not a daemon source file"),
        "{problems:?}"
    );
    // A reason is required.
    let problems = lint(&file("old/raw.rs", raw), &[("old/raw.rs", 1, "todo")]);
    assert!(problems.len() == 1 && problems[0].contains("real reason"), "{problems:?}");
}

// ---- a migrated site end to end: one row per page with the switch on,
// ---- one row with it off ----

fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn rows_after(switch: Option<&str>, body: impl FnOnce()) -> Vec<ForgeCallCounts> {
    let sink = tempfile::tempdir().unwrap();
    forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    super::paged::set_test_walk(switch);
    body();
    super::paged::set_test_walk(None);
    let report = forge_call_stats::status_report(chrono::Utc::now(), None);
    forge_call_stats::set_test_sink_dir(None);
    report.host_window.unwrap_or_default()
}

/// `fetch_trusted_bodies` against a two-page listing; returns the bodies it
/// read, the `sequence.trusted_bodies` calls booked, and every argv.
fn two_page_comment_read(switch: Option<&str>) -> (Option<Vec<String>>, u64, String) {
    use crate::merge_pr::sequence::fetch_trusted_bodies;
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("argv.log");
    let gh = stub(
        tmp.path(),
        "gh-two",
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'\n\
             case \"$*\" in\n\
             *--paginate*) printf '[]' ;;\n\
             *'page=2'*) printf 'HTTP/2.0 200 OK\\r\\n\\r\\n[]' ;;\n\
             *) printf 'HTTP/2.0 200 OK\\r\\nLink: <https://api.github.com/repositories/1/issues/7/comments?per_page=100&page=2>; rel=\"next\"\\r\\n\\r\\n[]' ;;\n\
             esac",
            log.display()
        ),
    );
    let mut bodies = None;
    let rows = rows_after(switch, || {
        bodies = fetch_trusted_bodies(&gh.to_string_lossy(), tmp.path(), "o/r", 7);
    });
    let calls: u64 = rows
        .iter()
        .filter(|r| r.caller == "sequence.trusted_bodies")
        .map(|r| r.ok + r.error)
        .sum();
    (bodies, calls, std::fs::read_to_string(&log).unwrap_or_default())
}

#[test]
#[serial_test::serial]
fn a_two_page_comment_read_is_two_counted_rows_with_the_switch_on() {
    let (bodies, calls, argv) = two_page_comment_read(Some("1"));
    assert_eq!(bodies, Some(vec![]), "two empty pages merge into one empty listing");
    assert_eq!(calls, 2, "{argv}");
    assert_eq!(argv.lines().count(), 2, "{argv}");
    assert!(argv.lines().all(|l| l.contains("--include")), "{argv}");
}

#[test]
#[serial_test::serial]
fn the_same_read_is_one_paginate_call_by_default() {
    let (bodies, calls, argv) = two_page_comment_read(None);
    assert_eq!(bodies, Some(vec![]));
    assert_eq!(calls, 1, "{argv}");
    assert_eq!(argv.lines().count(), 1, "{argv}");
    assert!(argv.contains("--paginate") && !argv.contains("--include"), "{argv}");
}
