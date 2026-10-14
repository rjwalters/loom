//! The caller scope (#10752): `github.caller` on every span inside a scope,
//! `github.number` / `github.repo` on writes, and the scope's spend counters.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use super::*;
use crate::gh_invocation::{
    AccessIntent, GhBinSource, GhInvocation, GhTarget, Operation, ParentContext,
};
use crate::observability::ops::capture::capture;
use crate::telemetry::trace::SpanRecord;

fn stub(dir: &Path, body: &str) -> String {
    let path = dir.join("gh-stub");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn inv(intent: AccessIntent, args: &[&str]) -> GhInvocation {
    GhInvocation::new(
        Operation::new("api.rest"),
        intent,
        GhTarget::repo("acme/widgets").unwrap(),
        Duration::from_secs(10),
    )
    .parent(ParentContext::Missing)
    .args(args)
}

/// Run each invocation against a stub `gh` with the ops path captured.
fn spans_of(invocations: Vec<GhInvocation>) -> Vec<SpanRecord> {
    let tmp = tempfile::tempdir().unwrap();
    let program = stub(tmp.path(), "echo ok");
    let ((), captured) = capture(|| {
        for i in invocations {
            i.execute_with(&program, GhBinSource::EnvOverride).unwrap();
        }
    });
    captured.spans
}

fn attr<'a>(span: &'a SpanRecord, key: &str) -> Option<&'a str> {
    span.attributes.get(key).map(String::as_str)
}

#[test]
fn no_scope_means_no_caller() {
    assert_eq!(current(), None);
    let spans = spans_of(vec![inv(AccessIntent::Read, &["api", "repos/acme/widgets"])]);
    assert_eq!(attr(&spans[0], "github.caller"), None);
}

#[test]
fn scopes_nest_and_restore() {
    let outer = enter("stale_blocked_release");
    assert_eq!(current(), Some("stale_blocked_release"));
    {
        let _inner = enter("intake_reconcile");
        assert_eq!(current(), Some("intake_reconcile"));
    }
    assert_eq!(current(), Some("stale_blocked_release"));
    drop(outer);
    assert_eq!(current(), None);
}

#[test]
fn every_span_inside_a_scope_carries_the_caller_and_the_scope_counts_them() {
    let scope = enter("stale_blocked_release");
    let spans = spans_of(vec![
        inv(AccessIntent::Read, &["api", "repos/acme/widgets/issues/42"]),
        inv(
            AccessIntent::Write,
            &[
                "api",
                "-X",
                "DELETE",
                "repos/acme/widgets/issues/42/labels/loom%3Ablocked",
            ],
        ),
    ]);
    let spent = scope.finish();
    assert_eq!(spans.len(), 2);
    for span in &spans {
        assert_eq!(attr(span, "github.caller"), Some("stale_blocked_release"));
    }
    assert_eq!(
        spent,
        Spent {
            calls: 2,
            writes: 1,
            not_modified: 0
        }
    );
    // After the scope, nothing is stamped.
    let after = spans_of(vec![inv(AccessIntent::Read, &["api", "repos/acme/widgets"])]);
    assert_eq!(attr(&after[0], "github.caller"), None);
}

#[test]
fn a_write_names_its_target_number_and_repo_a_read_names_only_its_repo() {
    let spans = spans_of(vec![
        inv(
            AccessIntent::Write,
            &[
                "api",
                "-X",
                "DELETE",
                "repos/{owner}/{repo}/issues/42/labels/loom%3Ablocked",
            ],
        ),
        inv(AccessIntent::Read, &["api", "repos/acme/widgets/issues/42"]),
        inv(AccessIntent::Write, &["api", "graphql", "-f", "query=mutation{x}"]),
    ]);
    // The label removal: the number from the path, the repo from the target.
    assert_eq!(attr(&spans[0], "github.number"), Some("42"));
    assert_eq!(attr(&spans[0], "github.repo"), Some("acme/widgets"));
    // A read never carries github.number.
    assert_eq!(attr(&spans[1], "github.number"), None);
    assert_eq!(attr(&spans[1], "github.repo"), Some("acme/widgets"));
    // A GraphQL write names no number.
    assert_eq!(attr(&spans[2], "github.number"), None);
}

#[test]
fn the_new_keys_survive_the_span_bound() {
    let _scope = enter("stale_blocked_release");
    let spans = spans_of(vec![inv(
        AccessIntent::Write,
        &["api", "-X", "POST", "repos/acme/widgets/issues/7/labels"],
    )]);
    let bounded = spans[0].clone().bounded();
    for key in ["github.caller", "github.number", "github.repo"] {
        assert!(super::super::telemetry::SPAN_ATTRIBUTE_KEYS.contains(&key), "{key}");
        assert!(bounded.attributes.contains_key(key), "{key} stripped by bounded()");
    }
}

#[test]
fn a_repo_that_is_not_a_slug_is_not_exported() {
    use super::super::accounting::cred_of_with;
    use super::super::billing::{Billing, BillingClass};
    let cred = cred_of_with(None, false);
    let sent = |repo: Option<&str>| {
        Billing::sent(None, "", Some(1), BillingClass::Ok, "core", &cred, "writer")
            .with_repo(repo)
            .repo
    };
    assert_eq!(sent(Some("acme/widgets")).as_deref(), Some("acme/widgets"));
    assert_eq!(sent(Some("2AMLogic/loom-ui")).as_deref(), Some("2AMLogic/loom-ui"));
    assert_eq!(sent(Some("/Users/me/repo")), None);
    assert_eq!(sent(Some("acme/widgets/extra")), None);
    assert_eq!(sent(Some("acme")), None);
    assert_eq!(sent(None), None);
}
