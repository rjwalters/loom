#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! W5: a REST `gh api --paginate` read is walked page by page — one
//! execution and one ledger row per page — and returns what the single
//! `--paginate` execution would have printed.

use super::*;
use crate::cmd_out::CmdOutcome;
use crate::gh_invocation::{AccessIntent, GhTarget, Operation, ParentContext};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn inv(args: &[&str], program: &Path) -> GhInvocation {
    GhInvocation::new(
        Operation::new("verdict.pr_comments"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(20),
    )
    .parent(ParentContext::Missing)
    .program(program)
    .args(args.iter().copied())
}

/// Run `body` with this thread's sink at a fresh dir; return every raw line.
fn lines_after(body: impl FnOnce()) -> Vec<serde_json::Value> {
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    crate::forge_call_stats::set_test_sink_dir(None);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(sink.path()).unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with("calls-") {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            out.extend(text.lines().map(|l| serde_json::from_str(l).unwrap()));
        }
    }
    out
}

/// A three-page listing: pages 1 and 2 carry a `rel="next"` link, page 3
/// does not. `PAGE1`..`PAGE3` are the bodies; every call's argv is logged.
fn three_pages(dir: &Path, bodies: [&str; 3]) -> (PathBuf, PathBuf) {
    let log = dir.join("argv.log");
    let link = |n: u32| {
        format!(
            "Link: <https://api.github.com/repositories/1/issues/7/comments?per_page=100&page={n}>; \
             rel=\\\"next\\\", <https://api.github.com/repositories/1/issues/7/comments?per_page=100&page=3>; \
             rel=\\\"last\\\"\\r\\n"
        )
    };
    let head = |extra: &str| {
        format!("printf \"HTTP/2.0 200 OK\\r\\nX-Ratelimit-Resource: core\\r\\n{extra}\\r\\n\"")
    };
    for (i, body) in bodies.iter().enumerate() {
        std::fs::write(dir.join(format!("page{}", i + 1)), body).unwrap();
    }
    let d = dir.display();
    let script = format!(
        "printf '%s\\n' \"$*\" >> '{log}'\n\
         case \"$*\" in\n\
         *'page=3'*) {h3}; cat '{d}/page3' ;;\n\
         *'page=2'*) {h2}; cat '{d}/page2' ;;\n\
         *) {h1}; cat '{d}/page1' ;;\n\
         esac",
        log = log.display(),
        h1 = head(&link(2)),
        h2 = head(&link(3)),
        h3 = head(""),
    );
    (stub(dir, "gh-pages", &script), log)
}

// ===== which argv is walked =====

#[test]
fn only_a_plain_rest_paginate_read_is_walked() {
    let walked: &[(&[&str], usize)] = &[
        (&["api", "repos/o/r/issues/7/comments", "--paginate"], 1),
        (
            &[
                "api",
                "--paginate",
                "repos/o/r/issues?state=open",
                "--jq",
                ".[]",
            ],
            2,
        ),
        (
            &[
                "api",
                "-H",
                "Accept: x",
                "/repos/o/r/pulls/1/files",
                "--paginate",
                "-q",
                ".",
            ],
            3,
        ),
        (
            &[
                "api",
                "--hostname",
                "ghe.example.com",
                "repos/o/r/issues",
                "--paginate",
            ],
            3,
        ),
    ];
    for (args, endpoint) in walked {
        assert_eq!(walkable_endpoint(&os(args)), Some(*endpoint), "{args:?}");
    }
    let single: &[&[&str]] = &[
        // Not paginated at all.
        &["api", "repos/o/r/issues/7/comments"],
        // Cursor pagination.
        &["api", "graphql", "--paginate", "-f", "query=q"],
        &["api", "--paginate", "graphql"],
        // Already asks for headers: counted from its own status blocks.
        &["api", "--paginate", "--include", "repos/o/r/issues"],
        &["api", "--paginate", "-i", "repos/o/r/issues"],
        // Fields, a body, an explicit method, slurp, a template, a cache.
        &["api", "--paginate", "repos/o/r/issues", "-f", "state=open"],
        &[
            "api",
            "--paginate",
            "repos/o/r/issues",
            "-F",
            "per_page=100",
        ],
        &["api", "--paginate", "repos/o/r/issues", "--input", "-"],
        &["api", "--paginate", "repos/o/r/issues", "--method", "GET"],
        &["api", "--paginate", "repos/o/r/issues", "--slurp"],
        &["api", "--paginate", "repos/o/r/issues", "--template", "x"],
        &["api", "--paginate", "repos/o/r/issues", "--cache", "1h"],
        // A flag spelled in a form this module does not parse.
        &["api", "--paginate", "repos/o/r/issues", "--jq=.[]"],
        // Two positionals, a dangling flag value, not `api`.
        &["api", "--paginate", "repos/o/r/issues", "extra"],
        &["api", "--paginate", "repos/o/r/issues", "--jq"],
        &["pr", "list", "--paginate"],
        &["api", "--paginate"],
    ];
    for args in single {
        assert_eq!(walkable_endpoint(&os(args)), None, "{args:?}");
    }
}

#[test]
fn a_page_argv_swaps_paginate_for_include_in_place_and_follows_the_next_url() {
    let args = os(&["api", "repos/o/r/issues", "--paginate", "--jq", ".[].id"]);
    assert_eq!(
        page_args(&args, 1, None),
        os(&["api", "repos/o/r/issues", "--include", "--jq", ".[].id"])
    );
    assert_eq!(
        page_args(&args, 1, Some("https://api.github.com/repositories/1/issues?page=2")),
        os(&[
            "api",
            "https://api.github.com/repositories/1/issues?page=2",
            "--include",
            "--jq",
            ".[].id"
        ])
    );
}

// ===== header split, link, join =====

#[test]
fn the_split_ends_at_the_first_blank_line_and_never_reads_the_body() {
    // A body that itself looks like a response: only the leading block is
    // the header block.
    let raw = b"HTTP/2.0 200 OK\r\nEtag: x\r\n\r\nHTTP/2.0 500 Nope\n\nstill body\n";
    let (head, body) = split(raw);
    assert_eq!(head, Some(&b"HTTP/2.0 200 OK\r\nEtag: x"[..]));
    assert_eq!(body, b"HTTP/2.0 500 Nope\n\nstill body\n");
    // LF-only headers.
    let (head, body) = split(b"HTTP/1.1 200 OK\nA: b\n\n[1]");
    assert_eq!((head, body), (Some(&b"HTTP/1.1 200 OK\nA: b"[..]), &b"[1]"[..]));
    // No header block: all body (a failure before the request, or a stub).
    assert_eq!(split(b"[1,2]\n"), (None, &b"[1,2]\n"[..]));
    assert_eq!(split(b""), (None, &b""[..]));
    // Headers and nothing else.
    assert_eq!(split(b"HTTP/2.0 204 No Content\r\n").1, b"");
}

#[test]
fn next_link_reads_only_the_https_rel_next_of_the_link_header() {
    let head = b"HTTP/2.0 200 OK\r\nlink: <https://api.github.com/x?page=1>; rel=\"prev\", \
                 <https://api.github.com/x?page=3>; rel=\"next\", <https://api.github.com/x?page=9>; rel=\"last\"\r\nEtag: y";
    assert_eq!(next_link(head).as_deref(), Some("https://api.github.com/x?page=3"));
    assert_eq!(next_link(b"HTTP/2.0 200 OK\r\nLink: <https://a/x?page=9>; rel=\"last\""), None);
    assert_eq!(next_link(b"HTTP/2.0 200 OK\r\nEtag: y"), None);
    assert_eq!(
        next_link(b"HTTP/2.0 200 OK\r\nLink: <http://plain/x?page=2>; rel=\"next\""),
        None
    );
    assert_eq!(next_link(b"HTTP/2.0 200 OK\r\nLink: <https://a/x?page=2"), None);
}

#[test]
fn join_merges_arrays_and_concatenates_everything_else() {
    let b =
        |parts: &[&str]| -> Vec<Vec<u8>> { parts.iter().map(|p| p.as_bytes().to_vec()).collect() };
    // One page: untouched, whatever it is.
    assert_eq!(join(&b(&["[{\"a\":1}]\n"]), false), b"[{\"a\":1}]\n");
    // Arrays merge into one array; an empty page adds nothing.
    assert_eq!(join(&b(&["[1,2]", " [3]\n", "[]"]), false), b"[1,2,3]");
    assert_eq!(join(&b(&["[]", "[]"]), false), b"[]");
    // Objects (and any non-array page) are concatenated, as gh prints them.
    assert_eq!(join(&b(&["{\"n\":1}", "{\"n\":2}"]), false), b"{\"n\":1}{\"n\":2}");
    assert_eq!(join(&b(&["[1]", "{\"n\":2}"]), false), b"[1]{\"n\":2}");
    // A `--jq` stream is never re-shaped, even when it looks like arrays.
    assert_eq!(join(&b(&["[1]\n", "[2]\n"]), true), b"[1]\n[2]\n");
}

// ===== end to end through the facade =====

#[test]
fn a_walk_is_one_row_per_page_and_one_merged_array() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) =
        three_pages(tmp.path(), ["[{\"n\":1},{\"n\":2}]", "[{\"n\":3}]", "[{\"n\":4}]"]);
    let mut out = None;
    let lines = lines_after(|| {
        out = Some(
            inv(
                &[
                    "api",
                    "repos/o/r/issues/7/comments?per_page=100",
                    "--paginate",
                ],
                &gh,
            )
            .run(),
        );
    });
    let Some(CmdOutcome::Ran(out)) = out else {
        panic!("the walk did not run");
    };
    assert!(out.status.success());
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rows.len(), 4, "{}", String::from_utf8_lossy(&out.stdout));

    let argv = std::fs::read_to_string(&log).unwrap();
    let calls: Vec<&str> = argv.lines().collect();
    assert_eq!(
        calls,
        [
            "api repos/o/r/issues/7/comments?per_page=100 --include",
            "api https://api.github.com/repositories/1/issues/7/comments?per_page=100&page=2 --include",
            "api https://api.github.com/repositories/1/issues/7/comments?per_page=100&page=3 --include",
        ]
    );

    // Three requests, three rows — each charged once, none "pages unknown",
    // each carrying the resource its own headers named.
    assert_eq!(lines.len(), 3, "{lines:?}");
    for line in &lines {
        assert_eq!(line["c"], "verdict.pr_comments");
        assert_eq!(line["rr"], "core");
        assert!(line.get("pu").is_none() && line.get("pg").is_none(), "{line}");
    }
}

#[test]
fn a_filtered_walk_concatenates_each_pages_output() {
    let tmp = tempfile::tempdir().unwrap();
    // The stub does not evaluate `--jq`: each body is that page's output.
    let (gh, log) = three_pages(tmp.path(), ["a\nb\n", "c\n", ""]);
    let mut out = None;
    let lines = lines_after(|| {
        out = Some(
            inv(
                &[
                    "api",
                    "repos/o/r/pulls/1/files",
                    "--paginate",
                    "--jq",
                    ".[].filename",
                ],
                &gh,
            )
            .run(),
        );
    });
    let Some(CmdOutcome::Ran(out)) = out else {
        panic!("the walk did not run");
    };
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a\nb\nc\n");
    assert_eq!(lines.len(), 3);
    let argv = std::fs::read_to_string(&log).unwrap();
    assert!(
        argv.lines()
            .all(|l| l.ends_with("--include --jq .[].filename")),
        "{argv}"
    );
}

#[test]
fn a_body_that_looks_like_headers_is_returned_whole() {
    // The reason pages are separate executions: forge content in a `--jq`
    // body can imitate a header block, and must reach the caller untouched.
    let tmp = tempfile::tempdir().unwrap();
    let forged = "HTTP/2.0 200 OK\nLink: <https://evil.example/x>; rel=\"next\"\n\nafter\n";
    let (gh, log) = three_pages(tmp.path(), ["first\n", "second\n", forged]);
    let mut out = None;
    lines_after(|| {
        out = Some(
            inv(
                &[
                    "api",
                    "repos/o/r/issues/7/comments",
                    "--paginate",
                    "--jq",
                    ".[].body",
                ],
                &gh,
            )
            .run(),
        );
    });
    let Some(CmdOutcome::Ran(out)) = out else {
        panic!("the walk did not run");
    };
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("first\nsecond\n{forged}"));
    let argv = std::fs::read_to_string(&log).unwrap();
    assert_eq!(argv.lines().count(), 3, "the forged link is never followed: {argv}");
}

#[test]
fn a_failing_page_ends_the_walk_with_its_status_and_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("argv.log");
    let gh = stub(
        tmp.path(),
        "gh-fail2",
        &format!(
            "printf '%s\\n' \"$*\" >> '{log}'\n\
             case \"$*\" in\n\
             *'page=2'*) printf 'HTTP/2.0 502 Bad Gateway\\r\\n\\r\\n{{\"message\":\"x\"}}'; \
             echo 'gh: Bad Gateway (HTTP 502)' >&2; exit 1 ;;\n\
             *) printf 'HTTP/2.0 200 OK\\r\\nLink: <https://api.github.com/x?page=2>; rel=\"next\"\\r\\n\\r\\n[1]' ;;\n\
             esac",
            log = log.display()
        ),
    );
    let mut out = None;
    let lines = lines_after(|| {
        out = Some(inv(&["api", "repos/o/r/issues", "--paginate"], &gh).run());
    });
    let Some(CmdOutcome::Ran(out)) = out else {
        panic!("the walk did not run");
    };
    assert!(!out.status.success(), "a failed page is a failed read, never a short one");
    assert!(String::from_utf8_lossy(&out.stderr).contains("HTTP 502"));
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[1]["o"], "error", "{lines:?}");
}

#[test]
fn a_child_without_a_header_block_is_one_page_of_plain_body() {
    // Fake `gh`s across the test suite print a body only.
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-plain", "echo '[{\"n\":1}]'");
    let mut out = None;
    let lines = lines_after(|| {
        out = Some(inv(&["api", "repos/o/r/issues", "--paginate"], &gh).run());
    });
    let Some(CmdOutcome::Ran(out)) = out else {
        panic!("the walk did not run");
    };
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[{\"n\":1}]\n");
    assert_eq!(lines.len(), 1);
}

#[test]
fn an_unwalked_paginate_call_runs_once_with_its_argv_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("argv.log");
    let gh = stub(
        tmp.path(),
        "gh-log",
        &format!("printf '%s\\n' \"$*\" >> '{}'\necho '{{}}'", log.display()),
    );
    let lines = lines_after(|| {
        let _ = inv(&["api", "graphql", "--paginate", "-f", "query=q"], &gh).run();
    });
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "api graphql --paginate -f query=q\n");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["pu"], true, "cursor pagination stays pages-unknown");
}

#[test]
fn every_child_is_marked_as_already_booked() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-env", "printf '%s' \"$LOOM_GH_BOOKED\"");
    let CmdOutcome::Ran(out) = inv(&["api", "repos/o/r"], &gh).run() else {
        panic!("did not run");
    };
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1");
}
