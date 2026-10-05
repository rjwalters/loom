//! Call-identity accounting driven from real call sites (Issue #9831).
//!
//! [`super::tests`] proves the identity layer on synthetic sink lines; these
//! tests drive production paths — the shared ETag store's
//! [`crate::forge_etag_store::fetch_conditional`] and the daemon's issue
//! listing — through a stub `gh`, and read the result back out of
//! [`status_report`]. Each test opts its own thread into a tempdir sink
//! ([`set_test_sink_dir`]); no process-global env var is touched.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::forge_etag_store::{fetch_conditional, ConditionalRead, Target};
use crate::types::ForgeOperationCounts;
use std::path::Path;

/// A `gh` stub answering every `gh api --include` with a `200`, an ETag and
/// core rate-limit headers, plus one issue row.
fn stub_gh(dir: &Path) -> PathBuf {
    let path = dir.join("gh-identity-stub");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf 'HTTP/2.0 200 OK\\r\\nEtag: W/\"i1\"\\r\\nX-Ratelimit-Resource: core\\r\\n\
         X-Ratelimit-Remaining: 4000\\r\\n\\r\\n'\n\
         printf '[{\"number\": 7, \"state\": \"open\", \"labels\": []}]\\n'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// Run `body` with this thread's sink pointed at a fresh tempdir and return
/// the identity rows it produced.
fn operations_after(body: impl FnOnce()) -> Vec<ForgeOperationCounts> {
    let sink = tempfile::tempdir().unwrap();
    set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    let report = status_report(Utc::now(), None);
    set_test_sink_dir(None);
    report.operations.expect("sink opted in on this thread")
}

/// Every inventoried constant a call site may name is an active row of the
/// embedded inventory — a typo or a retired row fails here, not in the field.
#[test]
fn every_named_operation_is_inventoried() {
    let inv = crate::forge_inventory::load_embedded().unwrap();
    for op in ops::ALL_INVENTORIED {
        let id = op.id().expect("ALL_INVENTORIED holds inventoried ops only");
        let row = inv.operations.iter().find(|o| o.id == id);
        assert!(
            row.is_some_and(crate::forge_inventory::Operation::is_active),
            "{id} is not an active row of defaults/forge/operations/*.toml"
        );
    }
    assert_eq!(ForgeOp::uninventoried("reason").id(), None);
    assert!(CallIdentity::for_op(ForgeOp::uninventoried("reason")).is_empty());
}

/// AC: two origins sharing one `owner/repo` slug stay two rows end to end,
/// from the ETag store's real call path — not only from synthetic lines.
#[test]
fn two_origins_sharing_one_slug_stay_two_rows_from_a_real_call_site() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub_gh(tmp.path());
    let slug = format!("acme/app-{}", std::process::id());
    let url = format!("repos/{slug}/issues?labels=loom:issue");
    let site = ConditionalRead::new("test_two_origins", ops::ISSUE_LIST);

    let rows = operations_after(|| {
        for host in ["github.com", "ghe.example.com", "ghe.example.com"] {
            let target = Target {
                repo: Some(slug.clone()),
                host: Some(host.to_string()),
            };
            let (_, response, _) =
                fetch_conditional(site, &gh, Some(tmp.path()), &target, &url, None).unwrap();
            assert_eq!(response.map(|r| r.status), Some(200));
        }
    });

    let mine: Vec<&ForgeOperationCounts> = rows
        .iter()
        .filter(|r| r.repo.as_deref() == Some(slug.as_str()))
        .collect();
    assert_eq!(mine.len(), 2, "one row per origin, never merged: {rows:?}");
    let by_origin = |o: &str| {
        mine.iter()
            .find(|r| r.origin.as_deref() == Some(o))
            .unwrap_or_else(|| panic!("no {o} row in {rows:?}"))
    };
    let (dotcom, ghe) = (by_origin("github.com"), by_origin("ghe.example.com"));
    assert_eq!((dotcom.ok, ghe.ok), (1, 2));
    for row in [dotcom, ghe] {
        // The migrated site records its inventoried operation, not `unknown`,
        // and the slug stays in `repo` — the origin is never folded into it.
        assert_eq!(row.operation, "issue.list");
        assert_ne!(row.operation, UNKNOWN_OPERATION);
        assert_eq!(row.provider.as_deref(), Some("github"));
        assert!(!row.repo.as_deref().unwrap().contains(".com"));
    }
}

/// AC: a migrated daemon site (the work-finder-style ETag listing) records
/// its operation ID and repository, not `unknown`.
#[test]
fn a_migrated_listing_site_records_its_operation_not_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let gh = stub_gh(tmp.path());
    let slug = format!("acme/listing-{}", std::process::id());

    crate::forge_etag_store::set_test_daemon_store_dir(Some(store.path().to_path_buf()));
    let rows = operations_after(|| {
        let issues = crate::forge_listing::list_issues_cached_as(
            "test_migrated_listing",
            &gh,
            Some(tmp.path()),
            Some(&slug),
            "loom:issue",
            "open",
        )
        .unwrap();
        assert_eq!(issues.len(), 1);
    });
    crate::forge_etag_store::set_test_daemon_store_dir(None);

    let row = rows
        .iter()
        .find(|r| r.repo.as_deref() == Some(slug.as_str()))
        .unwrap_or_else(|| panic!("no row for {slug} in {rows:?}"));
    assert_eq!(row.operation, "issue.list");
    assert_eq!(row.provider.as_deref(), Some("github"));
    assert!(row.origin.is_some(), "origin is resolved, never left blank");
    assert_eq!(row.ok, 1);
}

/// AC: the observed-vs-inventory diff reports both directions, keeps
/// `unknown` apart with its callers, and states the exhaustiveness caveat.
#[test]
fn observed_vs_inventory_diff_covers_both_directions() {
    let sink = tempfile::tempdir().unwrap();
    set_test_sink_dir(Some(sink.path().to_path_buf()));
    let gh_row = CallIdentity::for_op(ops::ISSUE_LIST)
        .with_provider("github")
        .with_origin("github.com")
        .with_repo("acme/app");
    record_with_identity("diff_listing", &gh_row, Pool::Core, Outcome::Ok, None);
    record_with_identity("diff_listing", &gh_row, Pool::Core, Outcome::NotModified, None);
    let stray = CallIdentity::operation("made.up-operation");
    record_with_identity("diff_typo_site", &stray, Pool::Core, Outcome::Ok, None);
    record("diff_unmapped_site", Pool::Graphql, Outcome::Ok, None);
    let seen = observed_operations(Utc::now()).expect("sink opted in");
    set_test_sink_dir(None);

    let inv = crate::forge_inventory::load_embedded().unwrap();
    let diff = crate::forge_inventory::observed::diff(&inv, &seen);

    let listed = diff
        .observed_and_inventoried
        .iter()
        .find(|r| r.id == "issue.list")
        .expect("observed and inventoried");
    assert_eq!((listed.calls, listed.disposition), (2, Some("required")));
    // Observed but not inventoried.
    let typo = &diff.observed_not_inventoried;
    assert_eq!(typo.len(), 1, "{typo:?}");
    assert_eq!(typo[0].id, "made.up-operation");
    assert_eq!(typo[0].callers, vec!["diff_typo_site".to_string()]);
    // Inventoried but never observed — and an observed row is never listed.
    let unobserved: Vec<&str> = diff
        .inventoried_never_observed
        .iter()
        .map(|r| r.id.as_str())
        .collect();
    assert!(unobserved.contains(&"issue.create"), "{unobserved:?}");
    assert!(!unobserved.contains(&"issue.list"));
    // `unknown` is its own line, with the callers still to map.
    let unknown = diff.unknown.as_ref().expect("an unmapped call is visible");
    assert_eq!(unknown.callers, vec!["diff_unmapped_site".to_string()]);
    assert!(!typo.iter().any(|r| r.id == UNKNOWN_OPERATION));

    let text = crate::forge_inventory::observed::render_text(&diff);
    for heading in [
        "OBSERVED BUT NOT INVENTORIED (1)",
        "INVENTORIED BUT NEVER OBSERVED",
        "never prove exhaustiveness",
        "diff_unmapped_site",
    ] {
        assert!(text.contains(heading), "missing {heading:?} in:\n{text}");
    }
}

/// No credential or response body reaches the sink from a real call site:
/// the stub answers with a body and the URL carries a token-shaped query, yet
/// the sink line holds only the four identity tokens and counters.
#[test]
fn a_call_site_never_writes_a_body_or_credential_to_the_sink() {
    let tmp = tempfile::tempdir().unwrap();
    let sink = tempfile::tempdir().unwrap();
    let gh = stub_gh(tmp.path());
    let target = Target {
        repo: Some("acme/app".to_string()),
        // A credential-shaped origin is refused by the sanitizer backstop.
        host: Some("https://user:hunter2@ghe.example.com".to_string()),
    };
    set_test_sink_dir(Some(sink.path().to_path_buf()));
    let site = ConditionalRead::new("test_no_secrets", ops::ISSUE_LIST);
    let url = "repos/acme/app/issues?access_token=abc123";
    let _ = fetch_conditional(site, &gh, Some(tmp.path()), &target, url, None).unwrap();
    set_test_sink_dir(None);

    let mut written = String::new();
    for entry in std::fs::read_dir(sink.path()).unwrap().flatten() {
        written.push_str(&std::fs::read_to_string(entry.path()).unwrap());
    }
    assert!(written.contains("\"op\":\"issue.list\""), "{written}");
    for leaked in [
        "hunter2",
        "access_token",
        "abc123",
        "\"number\"",
        "Etag",
        "W/",
    ] {
        assert!(!written.contains(leaked), "{leaked:?} reached the sink: {written}");
    }
}
