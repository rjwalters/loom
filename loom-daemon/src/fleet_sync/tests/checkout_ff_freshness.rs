//! `checkout.freshness` from the fast-forward pass (#12509): one OTLP-only
//! record per checkout per pass, from the head the pass already holds. Origins
//! are local bare repos; a test points `origin` at a GitHub URL only after the
//! objects it needs are in place, and none of these passes may reach it.

use super::*;
use crate::observability::ops::capture::capture;
use crate::telemetry::kinds::checkout_freshness::CheckoutFreshnessRecord;
use crate::telemetry::TelemetryRecord;

fn freshness(
    captured: &crate::observability::ops::capture::Captured,
) -> Vec<CheckoutFreshnessRecord> {
    captured
        .records
        .iter()
        .filter_map(|r| match r {
            TelemetryRecord::CheckoutFreshness(f) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

fn point_at_github(fx: &Fixture) {
    git(
        &fx.host,
        &[
            "remote",
            "set-url",
            "origin",
            "https://user:ghp_FAKE@github.com/o/r.git",
        ],
    );
}

/// Observe only: `fleet.autoApply` off, so the pass moves nothing.
fn observe(tip: Option<String>) -> Knobs {
    Knobs {
        write: false,
        confirmed: tip,
        ..Knobs::default()
    }
}

#[test]
fn a_checkout_three_behind_reports_the_remote_tip_and_the_count() {
    let fx = Fixture::new();
    for i in 0..3 {
        fx.push(&format!("src/{i}.rs"), "fn f() {}\n");
    }
    let tip = fx.origin_tip();
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    point_at_github(&fx);
    let (found, captured) = capture(|| pass(&fx, &observe(Some(tip.clone()))));
    assert_eq!((found.probes, found.fetches), (0, 0), "no network");
    let records = freshness(&captured);
    assert_eq!(records.len(), 1, "{records:?}");
    let r = &records[0];
    assert_eq!(r.repo.as_deref(), Some("o/r"));
    assert_eq!(r.pass_skipped_reason, None);
    let tree = r.prompt.as_ref().unwrap();
    assert_eq!(tree.remote_tip.as_deref(), Some(tip.as_str()));
    assert_eq!(tree.commits_behind_remote, Some(3));
    assert_eq!(tree.origin_ref_stale, Some(false));
    assert_eq!(tree.remote_foreign, Some(false));
    let json = serde_json::to_string(r).unwrap();
    for leak in ["ghp_", "user:", "https://"] {
        assert!(!json.contains(leak), "{leak} in {json}");
    }
}

#[test]
fn a_local_ref_that_disagrees_with_the_heard_head_is_stale() {
    let fx = Fixture::new();
    for i in 0..3 {
        fx.push(&format!("src/{i}.rs"), "fn f() {}\n");
    }
    let tip = fx.origin_tip();
    // The objects are here; `origin/main` was not moved, so HEAD equals it.
    git(&fx.host, &["fetch", "--quiet", "--refmap=", "origin", "refs/heads/main"]);
    let _ = fs::remove_file(fx.host.join(".git/FETCH_HEAD"));
    point_at_github(&fx);
    let refs = git(&fx.host, &["for-each-ref"]);
    let knobs = Knobs {
        network: false,
        ..observe(Some(tip.clone()))
    };
    let (_, captured) = capture(|| pass(&fx, &knobs));
    let tree = freshness(&captured)[0].prompt.clone().unwrap();
    assert_eq!(tree.origin_ref_stale, Some(true));
    assert_eq!(tree.commits_behind_remote, Some(3));
    assert_eq!(tree.commits_behind, Some(0));
    assert_eq!(git(&fx.host, &["for-each-ref"]), refs, "no ref moved");
    assert!(!fx.host.join(".git/FETCH_HEAD").exists(), "no FETCH_HEAD written");
}

#[test]
fn an_origin_that_is_not_the_workspaces_repo_is_foreign() {
    let fx = Fixture::new();
    let tip = fx.push("src/a.rs", "fn a() {}\n");
    let (_, captured) = capture(|| pass(&fx, &observe(Some(tip))));
    let r = &freshness(&captured)[0];
    let tree = r.prompt.as_ref().unwrap();
    assert_eq!(tree.remote_foreign, Some(true), "a local path origin");
    assert_eq!(r.repo, None);
    assert_eq!(tree.remote_tip, None);
}

#[test]
fn unknown_values_are_absent_and_the_reason_is_set() {
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    point_at_github(&fx);
    let knobs = Knobs {
        network: false,
        ..observe(None)
    };
    let (found, captured) = capture(|| pass(&fx, &knobs));
    assert_eq!((found.probes, found.fetches), (0, 0));
    let r = &freshness(&captured)[0];
    assert_eq!(r.pass_skipped_reason.as_deref(), Some("network-off"));
    let tree = r.prompt.as_ref().unwrap();
    assert_eq!(tree.remote_tip, None);
    assert_eq!(tree.commits_behind_remote, None, "unknown is absent, never 0");
    assert_eq!(tree.origin_ref_stale, None);
}

#[test]
fn an_open_breaker_is_the_reason_and_asks_no_remote() {
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    point_at_github(&fx);
    let knobs = Knobs {
        breaker: Rc::new(Cell::new(true)),
        ..observe(None)
    };
    let (found, captured) = capture(|| pass(&fx, &knobs));
    assert_eq!(found.probes, 0);
    assert_eq!(freshness(&captured)[0].pass_skipped_reason.as_deref(), Some("breaker-open"));
}

#[test]
fn a_checkout_the_budget_did_not_reach_still_says_so() {
    let fx = Fixture::new();
    point_at_github(&fx);
    let knobs = Knobs {
        budget: Some(Duration::from_secs(1)),
        elapsed: Box::new(|| Duration::from_secs(2)),
        ..observe(None)
    };
    let (found, captured) = capture(|| pass(&fx, &knobs));
    assert_eq!(found.deferred, 1);
    let records = freshness(&captured);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].pass_skipped_reason.as_deref(), Some("budget"));
    assert_eq!(records[0].repo.as_deref(), Some("o/r"));
    assert!(records[0].prompt.is_none());
}
