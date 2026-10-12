#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! W5: a REST `gh api --paginate` read is walked page by page — one
//! execution and one ledger row per page — and returns what the single
//! `--paginate` execution would have printed.

use super::*;
use crate::cmd_out::CmdOutcome;
use crate::gh_invocation::{AccessIntent, GhTarget, Operation, ParentContext};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

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

/// Run `body` with this thread's sink at a fresh dir and the walk switched
/// on (`LOOM_GH_PAGE_WALK=1`, through the test seam); return every raw line.
fn lines_after(body: impl FnOnce()) -> Vec<serde_json::Value> {
    lines_after_with(Some("1"), body)
}

/// [`lines_after`] under an explicit switch value (`None` = unset).
fn lines_after_with(switch: Option<&str>, body: impl FnOnce()) -> Vec<serde_json::Value> {
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    set_test_walk(switch);
    body();
    set_test_walk(None);
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
        os(&[
            "api",
            "repos/o/r/issues?per_page=100",
            "--include",
            "--jq",
            ".[].id"
        ])
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

#[test]
fn page_one_asks_for_100_per_page_exactly_as_gh_paginate_does() {
    // `gh` adds `per_page=100` only under `--paginate`, which the page argv
    // no longer carries: without this page 1 is 30 rows and every `next`
    // link inherits that — up to 3.3x the requests.
    let page_one = |endpoint: &str| -> String {
        let args = os(&["api", endpoint, "--paginate"]);
        page_args(&args, 1, None)[1].to_string_lossy().into_owned()
    };
    // No query.
    assert_eq!(
        page_one("repos/o/r/issues/7/comments"),
        "repos/o/r/issues/7/comments?per_page=100"
    );
    // An unrelated query.
    assert_eq!(
        page_one("repos/o/r/issues?state=open&sort=updated"),
        "repos/o/r/issues?state=open&sort=updated&per_page=100"
    );
    // Already has a per_page: the site's own value is kept, wherever it is.
    for own in [
        "repos/o/r/issues?per_page=100",
        "repos/o/r/issues?per_page=50",
        "repos/o/r/issues?state=open&per_page=25&sort=updated",
    ] {
        assert_eq!(page_one(own), own);
    }
    // Not a `per_page`: a look-alike key, or one with no value (gh's
    // `qp.Get("per_page") != ""`).
    assert_eq!(page_one("x?my_per_page=5"), "x?my_per_page=5&per_page=100");
    assert_eq!(page_one("x?per_page="), "x?per_page=&per_page=100");
    assert_eq!(page_one("x?"), "x?&per_page=100");

    // Only page 1: a `next` URL is followed byte for byte.
    let args = os(&["api", "repos/o/r/issues", "--paginate"]);
    assert_eq!(
        page_args(&args, 1, Some("https://api.github.com/repositories/1/issues?page=2"))[1],
        "https://api.github.com/repositories/1/issues?page=2"
    );
    // The endpoint is found by position, not by shape.
    let flagged = os(&[
        "api",
        "-H",
        "Accept: x",
        "repos/o/r/pulls/1/files",
        "--paginate",
    ]);
    assert_eq!(
        page_args(&flagged, 3, None),
        os(&[
            "api",
            "-H",
            "Accept: x",
            "repos/o/r/pulls/1/files?per_page=100",
            "--include"
        ])
    );
}

// ===== the switch =====

#[test]
fn the_walk_is_enabled_only_by_exactly_one() {
    assert!(walk_enabled(Some("1")));
    for off in [
        None,
        Some(""),
        Some("0"),
        Some("true"),
        Some("on"),
        Some(" 1"),
        Some("1 "),
        Some("01"),
        Some("2"),
    ] {
        assert!(!walk_enabled(off), "{off:?}");
    }
    assert_eq!(WALK_ENV, "LOOM_GH_PAGE_WALK");
}

/// A stub that logs its argv and answers page 1 of a two-page listing with
/// a header block (so a walk, if one ran, would be visible as two calls).
fn two_page_stub(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("argv.log");
    let gh = stub(
        dir,
        "gh-two",
        &format!(
            "printf '%s\n' \"$*\" >> '{log}'\n\
             case \"$*\" in\n\
             *--paginate*) printf '[1,2]' ;;\n\
             *'page=2'*) printf 'HTTP/2.0 200 OK\\r\\n\\r\\n[2]' ;;\n\
             *) printf 'HTTP/2.0 200 OK\\r\\nLink: <https://api.github.com/x?page=2>; rel=\"next\"\\r\\n\\r\\n[1]' ;;\n\
             esac",
            log = log.display()
        ),
    );
    (gh, log)
}

#[test]
fn with_the_switch_off_a_paginate_read_is_the_single_execution_it_always_was() {
    for off in [None, Some("0"), Some("true")] {
        let tmp = tempfile::tempdir().unwrap();
        let (gh, log) = two_page_stub(tmp.path());
        let mut out = None;
        let lines = lines_after_with(off, || {
            out = Some(inv(&["api", "repos/o/r/issues", "--paginate"], &gh).run());
        });
        let Some(CmdOutcome::Ran(out)) = out else {
            panic!("did not run under {off:?}");
        };
        assert_eq!(String::from_utf8_lossy(&out.stdout), "[1,2]", "{off:?}");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "api repos/o/r/issues --paginate\n",
            "the argv is untouched under {off:?}"
        );
        assert_eq!(lines.len(), 1, "{off:?}: {lines:?}");
        assert_eq!(lines[0]["pu"], true, "one row, pages unknown, as before");
    }
}

#[test]
fn with_the_switch_on_the_same_read_is_walked() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = two_page_stub(tmp.path());
    let mut out = None;
    let lines = lines_after_with(Some("1"), || {
        out = Some(inv(&["api", "repos/o/r/issues", "--paginate"], &gh).run());
    });
    let Some(CmdOutcome::Ran(out)) = out else {
        panic!("the walk did not run");
    };
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[1,2]");
    let argv = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        [
            "api repos/o/r/issues?per_page=100 --include",
            "api https://api.github.com/x?page=2 --include"
        ]
    );
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines.iter().all(|l| l.get("pu").is_none()), "{lines:?}");
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
    // No header block: all body (a failure before the request).
    assert_eq!(split(b"[1,2]\n"), (None, &b"[1,2]\n"[..]));
    assert_eq!(split(b""), (None, &b""[..]));
    // Headers and nothing else.
    assert_eq!(split(b"HTTP/2.0 204 No Content\r\n").1, b"");
}

#[test]
fn next_link_reads_only_the_https_rel_next_of_the_link_header() {
    let head = b"HTTP/2.0 200 OK\r\nlink: <https://api.github.com/x?page=1>; rel=\"prev\", \
                 <https://api.github.com/x?page=3>; rel=\"next\", <https://api.github.com/x?page=9>; rel=\"last\"\r\nEtag: y";
    assert_eq!(next_link(head), Next::Url("https://api.github.com/x?page=3".to_string()));
    // The last page: a `Link` with no next, or no `Link` at all.
    assert_eq!(
        next_link(b"HTTP/2.0 200 OK\r\nLink: <https://a/x?page=9>; rel=\"last\""),
        Next::End
    );
    assert_eq!(next_link(b"HTTP/2.0 200 OK\r\nEtag: y"), Next::End);
    // A next page this walk will not fetch is NOT the end of the listing.
    assert_eq!(
        next_link(b"HTTP/2.0 200 OK\r\nLink: <http://plain/x?page=2>; rel=\"next\""),
        Next::Unfollowable
    );
    assert_eq!(
        next_link(b"HTTP/2.0 200 OK\r\nLink: </relative?page=2>; rel=\"next\""),
        Next::Unfollowable
    );
    assert_eq!(
        next_link(b"HTTP/2.0 200 OK\r\nLink: <https://a/x?page=2"),
        Next::Unfollowable,
        "a Link value that cannot be read may have named a next page"
    );
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

// ===== a walk never looks complete when it is not =====

#[test]
fn a_successful_page_without_a_header_block_is_refused_not_passed_off_as_the_listing() {
    // Whatever printed this, where the listing ends cannot be told from it:
    // a gating read must fail rather than see page 1 as everything.
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-plain", "echo '[{\"n\":1}]'");
    let mut out = None;
    let lines = lines_after(|| {
        out = Some(inv(&["api", "repos/o/r/issues", "--paginate"], &gh).run());
    });
    assert!(
        !matches!(out, Some(CmdOutcome::Ran(_))),
        "a headerless page must not be a completed read: {out:?}"
    );
    assert_eq!(lines.len(), 1, "the request that ran is still one row");

    // The same refusal straight from the walk, with the reason.
    let args = &["api", "repos/o/r/issues", "--paginate"];
    let err = walk(&inv(args, Path::new("gh")), 1, Duration::from_secs(20), |_| {
        Ok(exited(0, b"[{\"n\":1}]\n"))
    })
    .unwrap_err();
    assert!(matches!(err, ExecError::Collect(_)), "{err:?}");
    assert!(format!("{err:?}").contains("no HTTP header block"), "{err:?}");
}

#[test]
fn a_failing_page_without_a_header_block_keeps_its_own_failure() {
    // `gh` failed before any request (no auth, bad argv): nothing to walk,
    // and the caller sees exactly that failure.
    let args = &["api", "repos/o/r/issues", "--paginate"];
    let done = walk(&inv(args, Path::new("gh")), 1, Duration::from_secs(20), |_| {
        Ok(Completion::Exited(Output {
            status: ExitStatus::from_raw(4 << 8),
            stdout: Vec::new(),
            stderr: b"gh: not logged in\n".to_vec(),
        }))
    })
    .unwrap();
    let Completion::Exited(out) = done else {
        panic!("not an exit");
    };
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(out.stderr, b"gh: not logged in\n");
}

#[test]
fn a_next_link_that_is_not_https_fails_the_read() {
    let args = &["api", "repos/o/r/issues", "--paginate"];
    let mut calls = 0;
    let err = walk(&inv(args, Path::new("gh")), 1, Duration::from_secs(20), |_| {
        calls += 1;
        Ok(exited(
            0,
            b"HTTP/2.0 200 OK\r\nLink: <http://api.github.com/x?page=2>; rel=\"next\"\r\n\r\n[1]",
        ))
    })
    .unwrap_err();
    assert_eq!(calls, 1, "the link is not followed");
    assert!(format!("{err:?}").contains("not an https URL"), "{err:?}");
}

fn exited(code: i32, stdout: &[u8]) -> Completion {
    Completion::Exited(Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
    })
}

/// A page that always names another page.
fn endless(n: usize) -> Completion {
    exited(
        0,
        format!(
            "HTTP/2.0 200 OK\r\nLink: <https://api.github.com/x?page={}>; rel=\"next\"\r\n\r\n[{n}]",
            n + 1
        )
        .as_bytes(),
    )
}

#[test]
fn a_link_loop_ends_at_max_pages_as_an_error() {
    let args = &["api", "repos/o/r/issues", "--paginate"];
    let mut calls = 0;
    let result = walk(&inv(args, Path::new("gh")), 1, Duration::from_secs(600), |_| {
        calls += 1;
        Ok(endless(calls))
    });
    assert_eq!(calls, MAX_PAGES, "exactly MAX_PAGES requests, then stop");
    let err = result.unwrap_err();
    assert!(matches!(err, ExecError::Collect(_)), "{err:?}");
    assert!(format!("{err:?}").contains("incomplete"), "{err:?}");
}

#[test]
fn a_deadline_that_expires_mid_walk_is_timed_out_and_never_a_whole_array() {
    let args = &["api", "repos/o/r/issues", "--paginate"];
    let budget = Duration::from_millis(300);
    let mut calls = 0;
    let mut budgets = Vec::new();
    let done = walk(&inv(args, Path::new("gh")), 1, budget, |page| {
        calls += 1;
        if let OutputContract::Captured { timeout } = page.contract {
            budgets.push(timeout);
        }
        // Two whole pages, then the third outlives what is left.
        if calls == 3 {
            std::thread::sleep(budget);
        }
        Ok(endless(calls))
    })
    .unwrap();
    assert_eq!(calls, 3, "no page is started after the deadline");
    assert!(budgets.iter().all(|left| *left <= budget), "{budgets:?}");
    assert!(budgets[2] <= budgets[0], "each page gets only what is left: {budgets:?}");
    let Completion::TimedOut { stdout, .. } = done else {
        panic!("a walk cut short by its deadline must be TimedOut, got {done:?}");
    };
    // What a killed `gh api --paginate` leaves: the array opened, the pages
    // read so far, and no closing bracket.
    assert_eq!(String::from_utf8_lossy(&stdout), "[1,2,3");
    assert!(
        serde_json::from_slice::<serde_json::Value>(&stdout).is_err(),
        "the partial listing must not parse as a complete one"
    );
}

#[test]
fn a_page_killed_at_the_deadline_contributes_nothing() {
    let args = &["api", "repos/o/r/issues", "--paginate"];
    let mut calls = 0;
    let done = walk(&inv(args, Path::new("gh")), 1, Duration::from_secs(20), |_| {
        calls += 1;
        if calls == 1 {
            return Ok(endless(1));
        }
        Ok(Completion::TimedOut {
            stdout: b"HTTP/2.0 200 OK\r\n\r\n[2,".to_vec(),
            stderr: b"killed\n".to_vec(),
        })
    })
    .unwrap();
    let Completion::TimedOut { stdout, stderr } = done else {
        panic!("not timed out: {done:?}");
    };
    assert_eq!(String::from_utf8_lossy(&stdout), "[1");
    assert_eq!(stderr, b"killed\n");
}

#[test]
fn cut_short_never_yields_a_well_formed_listing() {
    let b =
        |parts: &[&str]| -> Vec<Vec<u8>> { parts.iter().map(|p| p.as_bytes().to_vec()).collect() };
    // Arrays: opened, never closed — one page, several, or all empty.
    assert_eq!(cut_short(&b(&["[{\"a\":1}]\n"]), false), b"[{\"a\":1}");
    assert_eq!(cut_short(&b(&["[1,2]", "[3]"]), false), b"[1,2,3");
    assert_eq!(cut_short(&b(&["[]"]), false), b"[");
    // Nothing read, or pages that are not arrays: nothing at all.
    assert_eq!(cut_short(&[], false), b"");
    assert_eq!(cut_short(&b(&["{\"n\":1}"]), false), b"");
    for partial in [&b(&["[1,2]", "[3]"]), &b(&["[]"]), &b(&["{\"n\":1}"])] {
        let text = cut_short(partial, false);
        assert!(serde_json::from_slice::<serde_json::Value>(&text).is_err(), "{partial:?}");
    }
    // A `--jq` stream: the pages' output so far, as `gh` had printed it.
    assert_eq!(cut_short(&b(&["a\n", "b\n"]), true), b"a\nb\n");
    assert_eq!(cut_short(&[], true), b"");
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
