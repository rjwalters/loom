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

/// Seed the daemon's head cache for the fixture's checkout, `minutes` ago.
fn hear(fx: &Fixture, tip: &str, minutes: i64) {
    crate::fleet_sync::checkout_ff::note_remote_head_at(
        &fx.host,
        "main",
        tip,
        chrono::Utc::now() - chrono::Duration::minutes(minutes),
    );
}

fn network_off() -> Knobs {
    Knobs {
        network: false,
        ..observe(None)
    }
}

#[test]
fn a_network_off_pass_reuses_a_head_heard_minutes_ago() {
    let fx = Fixture::new();
    for i in 0..3 {
        fx.push(&format!("src/{i}.rs"), "fn f() {}\n");
    }
    let tip = fx.origin_tip();
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    point_at_github(&fx);
    hear(&fx, &tip, 5);
    let (found, captured) = capture(|| pass(&fx, &network_off()));
    assert_eq!((found.probes, found.fetches), (0, 0), "no network");
    let r = &freshness(&captured)[0];
    assert_eq!(r.pass_skipped_reason, None);
    let tree = r.prompt.as_ref().unwrap();
    assert_eq!(tree.remote_tip.as_deref(), Some(tip.as_str()));
    assert_eq!(tree.commits_behind_remote, Some(3));
    let age = r.remote_tip_age_s.expect("a cached tip carries its age");
    assert!((300..310).contains(&age), "{age}");
}

#[test]
fn a_network_off_pass_with_nothing_heard_says_network_off() {
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    point_at_github(&fx);
    let (_, captured) = capture(|| pass(&fx, &network_off()));
    let r = &freshness(&captured)[0];
    assert_eq!(r.pass_skipped_reason.as_deref(), Some("network-off"));
    assert_eq!(r.remote_tip_age_s, None);
    assert_eq!(r.prompt.as_ref().unwrap().remote_tip, None);
}

#[test]
fn a_head_older_than_the_window_counts_as_nothing_heard() {
    let fx = Fixture::new();
    let tip = fx.push("src/a.rs", "fn a() {}\n");
    point_at_github(&fx);
    hear(&fx, &tip, 16);
    let (_, captured) = capture(|| pass(&fx, &network_off()));
    let r = &freshness(&captured)[0];
    assert_eq!(r.pass_skipped_reason.as_deref(), Some("network-off"));
    assert_eq!(r.remote_tip_age_s, None);
    assert_eq!(r.prompt.as_ref().unwrap().remote_tip, None);
}

#[test]
fn a_cached_head_for_another_branch_is_not_used() {
    let fx = Fixture::new();
    let tip = fx.push("src/a.rs", "fn a() {}\n");
    point_at_github(&fx);
    crate::fleet_sync::checkout_ff::note_remote_head_at(
        &fx.host,
        "other",
        &tip,
        chrono::Utc::now(),
    );
    let (_, captured) = capture(|| pass(&fx, &network_off()));
    let r = &freshness(&captured)[0];
    assert_eq!(r.pass_skipped_reason.as_deref(), Some("network-off"));
    assert_eq!(r.remote_tip_age_s, None);
}

#[test]
fn a_foreign_origin_with_a_cached_head_still_reports_no_tip() {
    let fx = Fixture::new();
    let tip = fx.push("src/a.rs", "fn a() {}\n");
    hear(&fx, &tip, 1);
    let (_, captured) = capture(|| pass(&fx, &network_off()));
    let r = &freshness(&captured)[0];
    let tree = r.prompt.as_ref().unwrap();
    assert_eq!(tree.remote_foreign, Some(true));
    assert_eq!(tree.remote_tip, None);
    assert_eq!(r.remote_tip_age_s, None, "no tip, so no age");
}

#[test]
fn a_pass_that_learned_the_head_itself_has_no_tip_age() {
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    let tip = fx.origin_tip();
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    point_at_github(&fx);
    hear(&fx, &tip, 5);
    let (_, captured) = capture(|| pass(&fx, &observe(Some(tip.clone()))));
    let r = &freshness(&captured)[0];
    assert_eq!(r.prompt.as_ref().unwrap().remote_tip.as_deref(), Some(tip.as_str()));
    assert_eq!(r.remote_tip_age_s, None);
}

#[test]
fn every_record_has_a_tip_or_a_reason_unless_foreign() {
    for network in [true, false] {
        for foreign in [true, false] {
            for heard in [true, false] {
                let fx = Fixture::new();
                let tip = fx.push("src/a.rs", "fn a() {}\n");
                if !foreign {
                    point_at_github(&fx);
                }
                if heard {
                    hear(&fx, &tip, 1);
                }
                let knobs = Knobs {
                    network,
                    ..observe(None)
                };
                let (_, captured) = capture(|| pass(&fx, &knobs));
                let r = &freshness(&captured)[0];
                let tree = r.prompt.as_ref().unwrap();
                let case = format!("network={network} foreign={foreign} heard={heard}");
                assert_eq!(tree.remote_foreign, Some(foreign), "{case}");
                assert!(
                    tree.remote_tip.is_some() || r.pass_skipped_reason.is_some() || foreign,
                    "{case}: no tip and no reason: {r:?}"
                );
            }
        }
    }
}
