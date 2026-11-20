//! Runtime fixtures for [`super::gh_route`] (#13744): the argv classifier,
//! the refusal of a mislabelled call, and the routed behaviour of the three
//! entry points against a stub `gh` — including the shed (an exhausted
//! reader pool sends **nothing**, never the writer) and the asker-dependent
//! probe that stays on the facade's audited writer pin.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{argv_access, ArgvAccess};
use crate::cmd_out::{CmdOutcome, Query, Unavailable};
use crate::gh_invocation::telemetry::set_test_record_dir;
use crate::gh_invocation::test_support::{calls, rows_after, stub};
use crate::observability::ops::capture::capture;
use crate::telemetry::trace::SpanRecord;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn access(argv: &[&str]) -> ArgvAccess {
    argv_access(argv)
}

// ---- the classifier: prove read and write shapes, and stay Opaque
// ---- wherever the argv cannot prove either ----

#[test]
fn a_rest_get_is_a_read_whatever_flags_it_carries() {
    assert_eq!(access(&["api", "repos/o/r/issues/7"]), ArgvAccess::Read);
    assert_eq!(access(&["api", "repos/o/r/issues/7/comments", "--paginate"]), ArgvAccess::Read);
    assert_eq!(
        access(&[
            "api",
            "-H",
            "Accept: application/vnd.github.raw",
            "repos/o/r/contents/f"
        ]),
        ArgvAccess::Read
    );
    assert_eq!(access(&["api", "-i", "user"]), ArgvAccess::Read);
    assert_eq!(access(&["api", "-X", "GET", "repos/o/r"]), ArgvAccess::Read);
    assert_eq!(access(&["api", "--method=HEAD", "repos/o/r"]), ArgvAccess::Read);
}

#[test]
fn an_explicit_write_method_or_request_fields_make_a_write() {
    assert_eq!(
        access(&[
            "api",
            "-X",
            "POST",
            "repos/o/r/issues/7/labels",
            "-f",
            "labels[]=x"
        ]),
        ArgvAccess::Write
    );
    assert_eq!(
        access(&["api", "-X", "DELETE", "repos/o/r/issues/comments/9"]),
        ArgvAccess::Write
    );
    assert_eq!(access(&["api", "--method", "PUT", "repos/o/r"]), ArgvAccess::Write);
    assert_eq!(access(&["api", "-XPATCH", "repos/o/r"]), ArgvAccess::Write);
    // `gh api` defaults to POST once fields are supplied.
    assert_eq!(
        access(&["api", "-f", "body=x", "repos/o/r/issues/7/comments"]),
        ArgvAccess::Write
    );
    assert_eq!(access(&["api", "--input", "body.json", "repos/o/r"]), ArgvAccess::Write);
}

#[test]
fn a_graphql_document_decides_not_the_http_transport() {
    // `gh api graphql` POSTs the document; a `query` is still a read.
    assert_eq!(
        access(&[
            "api",
            "graphql",
            "-f",
            "query=query($o:String!){repository(owner:$o)}"
        ]),
        ArgvAccess::Read
    );
    assert_eq!(
        access(&["api", "graphql", "-f", "query={ viewer { login } }"]),
        ArgvAccess::Read
    );
    assert_eq!(
        access(&[
            "api",
            "graphql",
            "-F",
            "query=mutation($i:ID!){updateIssue(input:$i)}"
        ]),
        ArgvAccess::Write
    );
    // The document assembled at runtime stays the caller's declaration.
    assert_eq!(access(&["api", "graphql", "--paginate"]), ArgvAccess::Opaque);
}

#[test]
fn porcelain_view_and_list_read_while_the_mutating_verbs_write() {
    for read in [
        &["issue", "view", "7", "--json", "state"][..],
        &["issue", "list"][..],
        &["pr", "view", "8", "--json", "files"][..],
        &["pr", "list", "--state", "open"][..],
        &["pr", "diff", "9"][..],
        &["pr", "checks", "9"][..],
        &["pr", "status"][..],
        &["repo", "view", "--json", "nameWithOwner"][..],
        &["release", "view"][..],
        &["label", "list"][..],
    ] {
        assert_eq!(access(read), ArgvAccess::Read, "{read:?}");
    }
    for write in [
        &["issue", "edit", "7", "--add-label", "x"][..],
        &["issue", "comment", "7", "--body", "b"][..],
        &["issue", "reopen", "7"][..],
        &["pr", "edit", "8", "--body", "b"][..],
        &["pr", "create", "--title", "t"][..],
        &["pr", "merge", "8"][..],
        &["repo", "edit", "--description", "d"][..],
        &["release", "create", "v1"][..],
        &["label", "create", "x"][..],
    ] {
        assert_eq!(access(write), ArgvAccess::Write, "{write:?}");
    }
}

#[test]
fn what_the_classifier_cannot_prove_stays_opaque() {
    assert_eq!(access(&[]), ArgvAccess::Opaque);
    assert_eq!(access(&["auth", "status"]), ArgvAccess::Opaque);
    // A bare `api` with neither method nor fields is a GET by gh's own
    // default — not a mutation — even though it has no endpoint to name.
    assert_eq!(access(&["api"]), ArgvAccess::Read);
    assert_eq!(access(&["extension", "run", "x"]), ArgvAccess::Opaque);
    // A subcommand the tables do not know is never guessed at.
    assert_eq!(access(&["issue", "some-future-verb"]), ArgvAccess::Opaque);
}

// ---- the refusal: a mislabelled call sends nothing ----

/// A stub `gh` that logs every invocation, plus the log's path.
fn logging_gh(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let log = dir.join("calls.log");
    let gh = stub(
        dir,
        "gh-log",
        &format!(r#"echo "$*" >> '{}'; echo '{{}}'; exit 0"#, log.display()),
    );
    (gh, log)
}

#[test]
#[serial(loom_config_env)]
fn a_read_labelled_write_is_refused_and_sends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = logging_gh(tmp.path());
    std::env::set_var("LOOM_GH_BIN", &gh);
    let root = tmp.path();
    for argv in [
        &["api", "user"][..],
        &["api", "repos/o/r/issues/7/comments", "--paginate"][..],
        &["pr", "view", "7"][..],
        &["pr", "list"][..],
        // A GraphQL query POSTed: the transport says POST, the document
        // says read — the document wins, and the write label is refused.
        &["api", "graphql", "-f", "query={ viewer { login } }"][..],
    ] {
        let out = crate::script_helpers::gh_write("t.probe", argv, root, false);
        match &out {
            CmdOutcome::Unavailable(Unavailable::Spawn(why)) => {
                assert!(why.contains("provably a Read"), "{why}");
            }
            other => panic!("expected a refusal, got {other:?} for {argv:?}"),
        }
    }
    std::env::remove_var("LOOM_GH_BIN");
    assert!(
        !log.exists() || std::fs::read_to_string(&log).unwrap().trim().is_empty(),
        "nothing may be spawned for a refused call"
    );
}

#[test]
#[serial(loom_config_env)]
fn a_mutation_labelled_read_is_refused_and_sends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = logging_gh(tmp.path());
    std::env::set_var("LOOM_GH_BIN", &gh);
    for argv in [
        &["issue", "edit", "7", "--add-label", "x"][..],
        &["api", "-X", "POST", "repos/o/r/issues/7/labels"][..],
        &["api", "repos/o/r/issues/7/comments", "-fbody=fixture"][..],
        &["api", "-Fbody=fixture", "repos/o/r/issues/7/comments"][..],
        &[
            "api",
            "repos/o/r/issues/7/comments",
            "--raw-field=body=fixture",
        ][..],
        &["api", "--input=fixture.json", "repos/o/r/issues/7/comments"][..],
        &["api", "graphql", "-f", "query=mutation{updateIssue}"][..],
        &["api", "/graphql", "-fquery=mutation{updateIssue}"][..],
    ] {
        let out = crate::script_helpers::gh_read("t.probe", argv, tmp.path(), false);
        assert!(
            matches!(&out, CmdOutcome::Unavailable(Unavailable::Spawn(why))
                if why.contains("provably a Write")),
            "{argv:?}: {out:?}"
        );
        let q: Query<serde_json::Value> = crate::script_helpers::gh_read_json(
            "t.probe",
            argv,
            tmp.path(),
            false,
            |_: &serde_json::Value| false,
        );
        assert!(matches!(q, Query::Unavailable(_)), "{argv:?}");
    }
    std::env::remove_var("LOOM_GH_BIN");
    assert!(
        !log.exists() || std::fs::read_to_string(&log).unwrap().trim().is_empty(),
        "nothing may be spawned for a refused call"
    );
}

// ---- the routed calls: one span, one accounting row, the right intent ----

fn attr<'a>(span: &'a SpanRecord, key: &str) -> Option<&'a str> {
    span.attributes.get(key).map(String::as_str)
}

/// `body` under a fresh failure-record dir: its value and the spans it
/// emitted.
fn spans_of<R>(body: impl FnOnce() -> R) -> (R, Vec<SpanRecord>) {
    let dir = tempfile::tempdir().unwrap();
    set_test_record_dir(Some(dir.path().to_path_buf()));
    let (value, captured) = capture(body);
    set_test_record_dir(None);
    (value, captured.spans)
}

#[test]
#[serial(loom_config_env)]
fn a_read_and_then_a_write_each_emit_one_span_with_their_intent() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = logging_gh(tmp.path());
    std::env::set_var("LOOM_GH_BIN", &gh);
    let root = tmp.path();
    let rows = rows_after(|| {
        let (_, spans) = spans_of(|| {
            // The verdict sequence, representative: read the labels, then
            // add one — through the migrated entry points.
            let read = crate::script_helpers::gh_read(
                "verdict.labels",
                &["api", "repos/o/r/issues/7/labels?per_page=100"],
                root,
                false,
            );
            assert!(read.succeeded(), "{read:?}");
            let write = crate::script_helpers::gh_write(
                "verdict.add_labels",
                &[
                    "api",
                    "-X",
                    "POST",
                    "repos/o/r/issues/7/labels",
                    "-f",
                    "labels[]=loom:pr",
                ],
                root,
                false,
            );
            assert!(write.succeeded(), "{write:?}");
        });
        assert_eq!(spans.len(), 2, "one span per call, nothing hidden");
        assert_eq!(attr(&spans[0], "github.operation"), Some("verdict.labels"));
        assert_eq!(attr(&spans[0], "github.access_intent"), Some("read"));
        assert_eq!(attr(&spans[0], "github.outcome"), Some("ok"));
        assert_eq!(attr(&spans[1], "github.operation"), Some("verdict.add_labels"));
        assert_eq!(attr(&spans[1], "github.access_intent"), Some("write"));
        assert_eq!(attr(&spans[1], "github.outcome"), Some("ok"));
    });
    assert_eq!(calls(&rows, "verdict.labels"), 1, "{rows:?}");
    assert_eq!(calls(&rows, "verdict.add_labels"), 1, "{rows:?}");
    std::env::remove_var("LOOM_GH_BIN");
    // The recorded subprocess argv: the read asked for the labels, the write
    // posted one — the sequencing the caller wrote, unchanged by the routing.
    let argv = std::fs::read_to_string(&log).unwrap();
    assert!(argv.contains("repos/o/r/issues/7/labels?per_page=100"), "{argv}");
    assert!(argv.contains("-X POST repos/o/r/issues/7/labels -f labels[]=loom:pr"), "{argv}");
}

#[test]
#[serial(loom_config_env)]
fn a_json_query_and_a_repo_flag_target_their_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-json", r#"echo '{"labels":[{"name":"loom:pr"}]}'"#);
    std::env::set_var("LOOM_GH_BIN", &gh);
    let root = tmp.path();
    let q: Query<serde_json::Value> = crate::script_helpers::gh_read_json(
        "dep.issue_view",
        &[
            "issue",
            "view",
            "7",
            "--repo",
            "acme/widgets",
            "--json",
            "labels",
        ],
        root,
        false,
        |_: &serde_json::Value| false,
    );
    assert!(
        matches!(q, Query::Populated(_)),
        "a successful query decodes as populated: {q:?}"
    );
    // A `--repo` argv names its target: the reader route asks for that
    // owner, and the span records it.
    let inv = super::guarded(
        "dep.issue_view",
        crate::gh_invocation::AccessIntent::Read,
        &[
            "issue",
            "view",
            "7",
            "--repo",
            "acme/widgets",
            "--json",
            "labels",
        ],
        root,
        false,
        None,
    )
    .expect("a correctly labelled call builds");
    assert_eq!(inv.target().slug().as_deref(), Some("acme/widgets"));
    std::env::remove_var("LOOM_GH_BIN");
}

#[test]
#[serial(loom_config_env)]
fn the_cached_trusted_comment_listing_runs_the_repo_wrapper() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-plain", r#"echo '[{"user":{"login":"a"},"body":"b"}]'"#);
    // The repo's `gh-cached` read wrapper: passes `--version`, delegates the
    // call to the stub, and leaves a mark so the test proves the routed call
    // went through it — exactly what `use_cache: true` selected before.
    let scripts = tmp.path().join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    let mark = tmp.path().join("wrapper.mark");
    let wrapper = scripts.join("gh-cached");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\ntouch '{}'\nexec \"$LOOM_GH_BIN\" \"$@\"\n",
            mark.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("LOOM_GH_BIN", &gh);

    let listing =
        crate::comment_trust::records::fetch_comment_listing("o/r", "7", tmp.path(), true);
    std::env::remove_var("LOOM_GH_BIN");
    assert!(listing.is_some(), "the cached listing read succeeded");
    assert!(mark.exists(), "the wrapper program ran the call");
}

// ---- the shed: an exhausted reader pool sends nothing, writer included ----

#[test]
#[serial(loom_config_env)]
fn an_exhausted_reader_pool_sheds_the_read_and_sends_nothing() {
    use crate::gh_invocation::cwd_route::CwdAnswer;
    use crate::gh_invocation::test_routing::{install, seen, TestRouting};

    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = logging_gh(tmp.path());
    std::env::set_var("LOOM_GH_BIN", &gh);
    let root = tmp.path();
    let routing = TestRouting {
        decision: crate::forge_identity::RouteDecision::Exhausted {
            until: std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            cause: crate::forge_identity::ExhaustCause::Budget,
        },
        env: Default::default(),
        checkout: CwdAnswer::Sole("acme/widgets".to_string()),
    };
    let _guard = install(routing);
    let out = crate::script_helpers::gh_read(
        "trust.comment_listing",
        &["api", "repos/acme/widgets/issues/7/comments", "--paginate"],
        root,
        false,
    );
    std::env::remove_var("LOOM_GH_BIN");
    assert!(
        matches!(out, CmdOutcome::Unavailable(Unavailable::Shed { .. })),
        "an exhausted pool is a shed, not an answer and not a failure: {out:?}"
    );
    assert!(
        !log.exists() || std::fs::read_to_string(&log).unwrap().trim().is_empty(),
        "no reader and no writer may run for a shed read"
    );
    assert!(
        seen().iter().any(|s| s.op == "trust.comment_listing"),
        "the routed attempt is recorded: {:?}",
        seen()
    );
}

// ---- the asker-dependent probe stays on the audited writer pin ----

#[test]
fn the_workflow_scope_probe_builds_a_writer_pinned_read() {
    // `cli/merge_pr_workflow_scope.rs` builds it inline on the facade; what
    // this asserts is the shape the guard relies on — the pin exists, the
    // intent is a read, and the reader router is never consulted for it.
    let inv = crate::gh_invocation::GhInvocation::new(
        crate::gh_invocation::Operation::new("merge_pr.workflow_scope_user"),
        crate::gh_invocation::AccessIntent::Read,
        crate::gh_invocation::GhTarget::None,
        std::time::Duration::from_secs(30),
    )
    .writer_identity()
    .args(["api", "-i", "user"]);
    assert!(!inv.is_reader_only(), "the probe measures the merging credential");
}
