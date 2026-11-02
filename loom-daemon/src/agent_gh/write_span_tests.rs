//! Tests for the front's spanned write passthrough (#13170).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::{head_sha, operation_of, run};
use crate::gh_invocation::telemetry::set_test_ambient;
use crate::observability::ops::capture::capture;

const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

fn words(args: &[&str]) -> Vec<String> {
    args.iter().map(ToString::to_string).collect()
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn stub(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("gh-stub");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn writes_map_to_a_fixed_operation_vocabulary() {
    for (argv, want) in [
        (&["pr", "create", "--title", "t"][..], Some("pr.create")),
        (&["pr", "comment", "7", "--body", "x"], Some("comment.create")),
        (&["issue", "comment", "7", "-F", "-"], Some("comment.create")),
        (&["issue", "comment", "7", "--edit-last"], Some("comment.edit")),
        (&["pr", "edit", "7", "--add-label", "a"], Some("label.add")),
        (&["issue", "edit", "7", "--remove-label=a"], Some("label.remove")),
        (
            &["pr", "edit", "7", "--add-label", "a", "--remove-label", "b"],
            Some("label.edit"),
        ),
        (&["pr", "edit", "7", "--title", "t"], Some("pr.edit")),
        (&["issue", "edit", "7", "--title", "t"], Some("issue.edit")),
        // Not spanned: reads, merges (#12029), help, other commands.
        (&["pr", "view", "7"], None),
        (&["pr", "merge", "7"], None),
        (&["pr", "create", "--help"], None),
        (&["pr", "create", "--web"], None),
        (&["api", "repos/o/r/issues/7/labels"], None),
        (&["pr"], None),
    ] {
        assert_eq!(operation_of(&words(argv)), want, "{argv:?}");
    }
}

#[test]
fn head_sha_is_omitted_when_the_call_names_its_own_head() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(head_sha(&words(&["pr", "create", "--head", "x"]), Some(dir.path())), None);
    assert_eq!(head_sha(&words(&["pr", "create", "-H", "x"]), Some(dir.path())), None);
    // Not a git checkout, and no cwd: unknown, not guessed.
    assert_eq!(head_sha(&words(&["pr", "create"]), Some(dir.path())), None);
    assert_eq!(head_sha(&words(&["pr", "create"]), None), None);
}

#[test]
fn no_ambient_context_declines_so_the_caller_execs() {
    set_test_ambient(None, None);
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(dir.path(), "exit 0");
    let (got, captured) = capture(|| {
        run(
            &gh,
            &os(&["pr", "edit", "7", "--add-label", "a"]),
            ("S", "1".into()),
            None,
            None,
        )
    });
    assert_eq!(got, None);
    assert!(captured.spans.is_empty());
}

#[test]
fn an_unclassified_call_declines_even_with_a_context() {
    set_test_ambient(Some(TRACEPARENT), None);
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(dir.path(), "exit 0");
    let (got, captured) =
        capture(|| run(&gh, &os(&["pr", "merge", "7"]), ("S", "1".into()), None, None));
    set_test_ambient(None, None);
    assert_eq!(got, None);
    assert!(captured.spans.is_empty());
}

#[test]
fn a_traced_write_emits_one_span_and_returns_the_childs_exit_code() {
    set_test_ambient(Some(TRACEPARENT), None);
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(dir.path(), "exit 3");
    let (got, captured) = capture(|| {
        run(
            &gh,
            &os(&[
                "issue",
                "edit",
                "13170",
                "--add-label",
                "loom:building",
                "-R",
                "acme/widgets",
            ]),
            ("S", "1".into()),
            None,
            Some("acme/widgets"),
        )
    });
    set_test_ambient(None, None);
    assert_eq!(got, Some(3));
    assert_eq!(captured.spans.len(), 1);
    let span = &captured.spans[0];
    let attr = |k: &str| span.attributes.get(k).map(String::as_str);
    assert_eq!(attr("github.operation"), Some("label.add"));
    assert_eq!(attr("github.repo"), Some("acme/widgets"));
    assert_eq!(attr("github.number"), Some("13170"));
    assert_eq!(attr("github.outcome"), Some("exit_nonzero"));
    assert_eq!(attr("github.exit_code"), Some("3"));
    assert_eq!(attr("github.access_intent"), Some("write"));
    assert_eq!(attr("loom.head_sha"), None, "only pr.create carries a head");
    assert_eq!(span.parent_span_id.as_ref().map(|s| s.as_str()), Some("b7ad6b7169203331"));
}

#[test]
fn a_successful_write_is_an_ok_span_with_exit_zero() {
    set_test_ambient(Some(TRACEPARENT), None);
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(dir.path(), "exit 0");
    let (got, captured) = capture(|| {
        run(&gh, &os(&["pr", "comment", "9", "--body", "x"]), ("S", "1".into()), None, None)
    });
    set_test_ambient(None, None);
    assert_eq!(got, Some(0));
    let span = &captured.spans[0];
    assert_eq!(span.attributes["github.operation"], "comment.create");
    assert_eq!(span.attributes["github.outcome"], "ok");
    assert_eq!(span.attributes["github.number"], "9");
}

#[test]
fn the_child_gets_the_argv_and_sentinel_unchanged_and_streams_pass_through() {
    set_test_ambient(Some(TRACEPARENT), None);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("argv");
    let gh = stub(
        dir.path(),
        &format!(
            "printf '%s|' \"$@\" > {}; printf '%s' \"$S\" >> {}",
            out.display(),
            out.display()
        ),
    );
    let argv = ["pr", "comment", "9", "--body", "two words"];
    let (got, _) = capture(|| run(&gh, &os(&argv), ("S", "1".into()), None, None));
    set_test_ambient(None, None);
    assert_eq!(got, Some(0));
    assert_eq!(std::fs::read_to_string(out).unwrap(), "pr|comment|9|--body|two words|1");
}

#[test]
fn a_spawn_failure_is_a_span_and_exit_127() {
    set_test_ambient(Some(TRACEPARENT), None);
    let missing = Path::new("/nonexistent/loom-test-gh");
    let (got, captured) = capture(|| {
        run(
            missing,
            &os(&["pr", "comment", "9", "--body", "x"]),
            ("S", "1".into()),
            None,
            None,
        )
    });
    set_test_ambient(None, None);
    assert_eq!(got, Some(127));
    assert_eq!(captured.spans[0].attributes["github.outcome"], "spawn_failed");
}

#[test]
fn a_pr_create_span_carries_the_checkouts_head_and_drops_a_non_sha() {
    use crate::gh_invocation::telemetry::InvocationSpan;
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "x"]);
    let head = git(&["rev-parse", "HEAD"]);
    assert_eq!(
        head_sha(&words(&["pr", "create"]), Some(repo.path())).as_deref(),
        Some(head.as_str())
    );

    set_test_ambient(Some(TRACEPARENT), None);
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(dir.path(), "exit 0");
    let (got, captured) = capture(|| {
        run(
            &gh,
            &os(&["pr", "create", "--title", "t"]),
            ("S", "1".into()),
            Some(repo.path()),
            None,
        )
    });
    set_test_ambient(None, None);
    assert_eq!(got, Some(0));
    assert_eq!(captured.spans[0].attributes["github.operation"], "pr.create");
    assert_eq!(captured.spans[0].attributes["loom.head_sha"], head);

    let inv = crate::gh_invocation::GhInvocation::new(
        crate::gh_invocation::Operation::new("pr.create"),
        crate::gh_invocation::AccessIntent::Write,
        crate::gh_invocation::GhTarget::None,
        std::time::Duration::from_secs(1),
    )
    .parent(crate::gh_invocation::ParentContext::Missing);
    for bad in ["", "HEAD", "abc", &"z".repeat(40)] {
        let open = InvocationSpan::open(&inv).with_head_sha(Some(bad));
        let rec = open.record(
            &inv,
            crate::gh_invocation::GhBinSource::Path,
            crate::gh_invocation::telemetry::Outcome::Ok,
            Some(0),
            chrono::Utc::now(),
            &crate::gh_invocation::billing::Billing::not_sent(
                "other",
                &crate::gh_invocation::accounting::cred_of_with(None, false),
                "writer",
            ),
        );
        assert!(!rec.attributes.contains_key("loom.head_sha"), "{bad:?}");
    }
}
