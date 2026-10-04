#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! #10089: every facade execution that reached `gh` is exactly one
//! `forge_call_stats` row under its operation name.

use super::*;
use crate::gh_invocation::{AccessIntent, GhTarget, Operation, ParentContext};
use crate::types::ForgeCallCounts;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[test]
fn static_pool_follows_the_argv() {
    let cases: &[(&[&str], Pool)] = &[
        (&["api", "repos/{owner}/{repo}/pulls/1"], Pool::Core),
        (
            &[
                "api",
                "--paginate",
                "--jq",
                ".[]",
                "repos/o/r/issues/1/comments",
            ],
            Pool::Core,
        ),
        (&["api", "-H", "Accept: x", "/repos/o/r"], Pool::Core),
        (&["api", "graphql", "-f", "query=q"], Pool::Graphql),
        (&["api", "-i", "graphql", "-f", "query=q"], Pool::Graphql),
        (&["api", "rate_limit"], Pool::Other),
        (&["api", "search/issues?q=x"], Pool::Search),
        (&["pr", "view", "1", "--json", "files"], Pool::Graphql),
        (&["issue", "list", "--label", "x"], Pool::Graphql),
        (&["search", "issues", "x"], Pool::Search),
        (&["run", "list"], Pool::Core),
        (&["auth", "status"], Pool::Other),
        (&[], Pool::Other),
    ];
    for (args, want) in cases {
        assert_eq!(static_pool(&os(args)), *want, "{args:?}");
    }
}

fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn inv(op: &'static str, args: &[&str], program: &Path) -> GhInvocation {
    GhInvocation::new(
        Operation::new(op),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    )
    .parent(ParentContext::Missing)
    .program(program)
    .args(args.iter().copied())
}

/// Run `body` with THIS thread's call-stats sink at a fresh dir, returning the
/// host-window rows it produced.
fn rows_after(body: impl FnOnce()) -> Vec<ForgeCallCounts> {
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    let report = crate::forge_call_stats::status_report(chrono::Utc::now(), None);
    crate::forge_call_stats::set_test_sink_dir(None);
    report.host_window.unwrap_or_default()
}

fn row<'a>(rows: &'a [ForgeCallCounts], caller: &str) -> &'a ForgeCallCounts {
    rows.iter()
        .find(|r| r.caller == caller)
        .unwrap_or_else(|| panic!("no row for {caller}: {rows:?}"))
}

#[test]
fn each_execution_is_exactly_one_row_under_its_operation() {
    let tmp = tempfile::tempdir().unwrap();
    let ok = stub(tmp.path(), "gh-ok", "echo '{}'");
    let fail = stub(tmp.path(), "gh-fail", "echo 'HTTP 404: Not Found' >&2; exit 1");
    let rows = rows_after(|| {
        for _ in 0..3 {
            let _ = inv("claim.pr_view", &["pr", "view", "1"], &ok).run();
        }
        let _ = inv("claim.issue_comments", &["api", "repos/o/r/issues/1/comments"], &fail).run();
    });
    let pr = row(&rows, "claim.pr_view");
    assert_eq!((pr.pool.as_str(), pr.ok, pr.error), ("graphql", 3, 0));
    let c = row(&rows, "claim.issue_comments");
    assert_eq!((c.pool.as_str(), c.ok, c.error), ("core", 0, 1));
    assert_eq!(rows.len(), 2, "no other rows: {rows:?}");
}

#[test]
fn rate_limited_stderr_is_booked_as_rate_limited_not_spent() {
    let tmp = tempfile::tempdir().unwrap();
    let limited = stub(
        tmp.path(),
        "gh-limited",
        "echo 'gh: API rate limit exceeded for user ID 1.' >&2; exit 1",
    );
    let rows = rows_after(|| {
        let _ = inv("visibility.repo", &["api", "repos/o/r"], &limited).run();
    });
    let r = row(&rows, "visibility.repo");
    assert_eq!((r.rate_limited, r.ok, r.error), (1, 0, 0));
}

#[test]
fn an_include_response_is_classified_from_its_status_line_and_headers() {
    let tmp = tempfile::tempdir().unwrap();
    let not_modified = stub(
        tmp.path(),
        "gh-304",
        "printf 'HTTP/2.0 304 Not Modified\\r\\nX-Ratelimit-Resource: core\\r\\n\
         X-Ratelimit-Remaining: 4321\\r\\n\\r\\n'; exit 1",
    );
    let rows = rows_after(|| {
        let _ =
            inv("claim.pr_get", &["api", "--include", "repos/o/r/pulls/1"], &not_modified).run();
    });
    let r = row(&rows, "claim.pr_get");
    assert_eq!((r.not_modified, r.ok, r.error), (1, 0, 0));
}

#[test]
fn a_spawn_failure_sent_nothing_and_is_not_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no-such-gh");
    let rows = rows_after(|| {
        let _ = inv("claim.pr_view", &["pr", "view", "1"], &missing).run();
    });
    assert!(rows.is_empty(), "{rows:?}");
}

#[test]
fn a_bare_gh_program_is_not_an_injection() {
    let pinned = inv("claim.pr_view", &[], Path::new("/stub/gh"));
    assert_eq!(pinned.program.as_deref(), Some("/stub/gh"));
    let bare = inv("claim.pr_view", &[], Path::new("gh"));
    assert_eq!(bare.program, None, "a bare `gh` must go through the resolver");
}

// ===== #9831 call identity =====

fn bare(op: &'static str, args: &[&str]) -> GhInvocation {
    GhInvocation::new(
        Operation::new(op),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    )
    .args(args.iter().copied())
}

#[test]
fn an_unmapped_site_is_unknown_but_still_names_provider_and_origin() {
    let id = resolved_identity_with(&bare("issue.view", &["issue", "view", "1"]), None, None);
    assert_eq!(id.operation, None, "no guessed mapping: records as `unknown`");
    assert_eq!(id.provider.as_deref(), Some("github"));
    assert_eq!(id.origin.as_deref(), Some("github.com"), "gh's default host");
    assert_eq!(id.repo, None);
}

#[test]
fn origin_and_repo_fall_back_in_gh_resolution_order() {
    let host = bare("api.rest", &["api", "--hostname", "ghe.example.com", "repos/o/r"]);
    let id = resolved_identity_with(&host, Some("other.example.com".into()), None);
    assert_eq!(id.origin.as_deref(), Some("ghe.example.com"), "--hostname wins");

    let env = resolved_identity_with(
        &bare("api.rest", &["api", "x"]),
        Some("gh-host.example.com".into()),
        Some("env/repo".into()),
    );
    assert_eq!(env.origin.as_deref(), Some("gh-host.example.com"));
    assert_eq!(env.repo.as_deref(), Some("env/repo"), "LOOM_REPO, as GH_REPO");

    let typed = GhInvocation::new(
        Operation::new("api.rest"),
        AccessIntent::Read,
        GhTarget::repo("typed/repo").unwrap(),
        Duration::from_secs(10),
    );
    let typed = resolved_identity_with(&typed, None, Some("env/repo".into()));
    assert_eq!(typed.repo.as_deref(), Some("typed/repo"), "the typed target wins");
}

#[test]
fn a_site_scope_and_operation_win_and_never_merge_origin_into_repo() {
    let inv = bare("fleet_store", &["api", "--hostname", "argv.example.com", "x"])
        .forge_op(crate::forge_call_stats::ops::GIT_READ_OBJECTS)
        .identity_scope(Some("gitea.example.com"), Some("acme/app"));
    let id = resolved_identity_with(&inv, Some("env.example.com".into()), Some("e/r".into()));
    assert_eq!(id.operation.as_deref(), Some("git.read-objects"));
    assert_eq!(id.origin.as_deref(), Some("gitea.example.com"));
    assert_eq!(id.repo.as_deref(), Some("acme/app"));
}
