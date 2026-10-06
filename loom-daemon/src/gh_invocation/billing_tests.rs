//! Tests for the `invoke github` span's billing facts (#10343).
#![allow(clippy::unwrap_used)]

use super::*;
use crate::gh_invocation::accounting::{self, cred_of_with};
use crate::gh_invocation::telemetry::Outcome as InvokeOutcome;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use std::collections::BTreeSet;
use std::time::Duration;

fn inv(args: &[&str]) -> GhInvocation {
    GhInvocation::new(
        Operation::new("billing.test"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(5),
    )
    .args(args.iter().copied())
}

/// `accounting::record` over a captured run's `(stdout, stderr)`.
fn billed(args: &[&str], outcome: InvokeOutcome, stdout: &str, stderr: &str) -> Billing {
    accounting::record(&inv(args), outcome, Some((stdout.as_bytes(), stderr.as_bytes())))
}

fn block(status: &str) -> String {
    format!(
        "HTTP/2.0 {status}\r\nX-Ratelimit-Resource: core\r\nX-Ratelimit-Remaining: 9\r\n\r\n{{}}"
    )
}

#[test]
fn include_blocks_give_the_status_and_not_modified() {
    let args = ["api", "--include", "repos/acme/widgets"];
    let ok = billed(&args, InvokeOutcome::Ok, &block("200 OK"), "");
    assert_eq!(
        (ok.status, ok.source, ok.class),
        (Some(200), BillingSource::Headers, BillingClass::Ok)
    );
    assert_eq!(ok.requests, Some(1));
    assert_eq!(ok.resource, "core");

    let nm = billed(&args, InvokeOutcome::ExitNonzero, &block("304 Not Modified"), "");
    assert_eq!((nm.status, nm.class), (Some(304), BillingClass::NotModified));
    let attrs: std::collections::BTreeMap<_, _> = nm.attributes().into_iter().collect();
    assert_eq!(attrs["github.http.not_modified"], "true");
    assert_eq!(attrs["github.billing"], "not_modified");

    let forbidden = billed(&args, InvokeOutcome::ExitNonzero, &block("403 Forbidden"), "");
    assert_eq!((forbidden.status, forbidden.class), (Some(403), BillingClass::Error));
    let attrs: std::collections::BTreeMap<_, _> = forbidden.attributes().into_iter().collect();
    assert_eq!(attrs["github.http.not_modified"], "false");
}

#[test]
fn stderr_markers_give_the_status_when_no_headers_were_asked_for() {
    let nf = billed(
        &["pr", "view", "1"],
        InvokeOutcome::ExitNonzero,
        "",
        "gh: Not Found (HTTP 404)\n",
    );
    assert_eq!((nf.status, nf.source), (Some(404), BillingSource::Stderr));
    assert_eq!(nf.requests, None, "porcelain request count is not known");

    let rl = billed(
        &["api", "repos/acme/widgets"],
        InvokeOutcome::ExitNonzero,
        "",
        "HTTP 403: API rate limit exceeded for installation ID 1.\n",
    );
    assert_eq!(
        (rl.status, rl.source, rl.class),
        (Some(403), BillingSource::Stderr, BillingClass::RateLimited)
    );

    assert_eq!(stderr_status_of("GraphQL: Could not resolve to a Repository"), None);
    assert_eq!(stderr_status_of("error connecting to api.github.com"), None);
    assert_eq!(stderr_status_of("(HTTP 99)"), None);
    assert_eq!(stderr_status_of("(HTTP 4044)"), None);
    assert_eq!(stderr_status_of("HTTP 999: nope"), None);
    assert_eq!(stderr_status_of("x\n  gh: Bad credentials (HTTP 401)"), Some(401));
}

fn stderr_status_of(s: &str) -> Option<u16> {
    accounting::stderr_status(s)
}

#[test]
fn a_passthrough_run_is_unknown_never_guessed() {
    let b = accounting::record(&inv(&["pr", "checkout", "1"]), InvokeOutcome::ExitNonzero, None);
    assert_eq!((b.status, b.source, b.requests), (None, BillingSource::None, None));
    let attrs: std::collections::BTreeMap<_, _> = b.attributes().into_iter().collect();
    assert_eq!(attrs["github.http.status"], "unknown");
    assert_eq!(attrs["github.http.not_modified"], "unknown");
    assert_eq!(attrs["github.http.requests"], "unknown");
    assert_eq!(attrs["github.http.source"], "none");
    assert_eq!(attrs["github.billing"], "error");
}

#[test]
fn requests_count_pages_downloads_and_nothing_sent() {
    let pages =
        "HTTP/2.0 200 OK\r\n\r\n[1]\nHTTP/2.0 200 OK\r\n\r\n[2]\nHTTP/1.1 200 OK\r\n\r\n[3]\n";
    let paged = billed(
        &["api", "--paginate", "--include", "repos/o/r/issues"],
        InvokeOutcome::Ok,
        pages,
        "",
    );
    assert_eq!(paged.requests, Some(3));
    let bare = billed(&["api", "--paginate", "repos/o/r/issues"], InvokeOutcome::Ok, "[]", "");
    assert_eq!(bare.requests, None, "a bare --paginate is unknown");
    let download = billed(&["run", "download", "1"], InvokeOutcome::Ok, "", "");
    assert_eq!(download.requests, Some(2));
    for not_sent in [
        InvokeOutcome::SpawnFailed,
        InvokeOutcome::RoutingRefused,
        InvokeOutcome::RoutingBlocked,
        InvokeOutcome::AdapterUnavailable,
    ] {
        let b = billed(&["api", "repos/o/r"], not_sent, "", "");
        assert_eq!(
            (b.requests, b.class, b.status),
            (Some(0), BillingClass::NotSent, None),
            "{not_sent:?}"
        );
        assert_eq!(b.resource, "core");
    }
    assert_eq!(requests(None, Some(true), true), None);
    assert_eq!(requests(Some(4), None, true), Some(4));
    assert_eq!(requests(None, None, true), Some(1));
    assert_eq!(requests(None, None, false), None);
}

#[test]
fn identity_values_never_carry_a_token_hash_or_path() {
    let reader =
        cred_of_with(Some(std::path::Path::new("/w/.loom/gh-config-by-owner/Acme/123456")), false);
    let b = Billing::not_sent("core", &reader, "reader");
    assert_eq!(
        (b.account.as_str(), b.cred_owner.as_str(), b.role.as_str()),
        ("app-123456", "acme", "reader")
    );

    let env = Billing::not_sent("core", &cred_of_with(None, true), "writer");
    assert_eq!((env.account.as_str(), env.cred_owner.as_str()), ("env-token", "-"));

    let pat = format!("{}_{}", "ghp", "0123456789abcdefghijABCDEFGHIJ012345");
    let hostile = CredAttr {
        account: pat.clone(),
        owner: Some(pat.clone()),
        kind: "env",
    };
    let b = Billing::not_sent(&pat, &hostile, &pat);
    for (key, value) in b.attributes() {
        assert!(!value.contains("ghp_"), "{key}={value}");
        assert!(!value.contains('/'), "{key}={value}");
    }
    let ambient = cred_of_with(Some(std::path::Path::new("/home/u/.config/gh")), false);
    for (key, value) in Billing::not_sent("core", &ambient, "writer").attributes() {
        assert!(!value.contains('/'), "{key}={value}");
    }
}

#[test]
fn the_billing_vocabulary_is_closed() {
    let names: BTreeSet<_> = BillingClass::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(names.len(), BillingClass::ALL.len());
    let keys: BTreeSet<_> = Billing::not_sent("core", &cred_of_with(None, false), "writer")
        .attributes()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(keys.len(), 9);
    for key in keys {
        assert!(crate::gh_invocation::telemetry::SPAN_ATTRIBUTE_KEYS.contains(&key), "{key}");
    }
}
