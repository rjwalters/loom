//! Queue-friction features (#10193): parsing, point-in-time copying, read
//! scheduling, and their arrival on every tracker estimate.

use super::{as_of, history_a, provenance};
use crate::eta::explanation::{FeatureOmitted, Features};
use crate::eta::friction::{
    ci_status, open_pr_count, pr_mergeability, typical_ci_duration, CiStatus, FrictionBook,
    PrFriction, RepoFriction, CI_MIN_RUNS, MAX_READING_AGE_SECS,
};
use crate::eta::tracker::{EstimateContext, PrView, Tracker};
use crate::eta::{Kind, Registry};
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use std::collections::BTreeMap;

const REPO: &str = "rjwalters/loom";

const FRICTION: [&str; 9] = [
    "repo_open_prs",
    "repo_pr_open_lockout",
    "repo_ci_typical_duration_sec",
    "repo_friction_observed_at",
    "pr_ci_status",
    "pr_behind_main",
    "pr_merge_conflict",
    "pr_friction_observed_at",
    "operator_hold",
];

fn reason(omitted: &[FeatureOmitted], name: &str) -> Option<String> {
    omitted
        .iter()
        .find(|o| o.name == name)
        .map(|o| o.reason.clone())
}

/// Every friction feature is set, or omitted with a reason — never both,
/// never neither.
fn assert_accounted(features: &Features, omitted: &[FeatureOmitted]) {
    let value = serde_json::to_value(features).unwrap();
    for name in FRICTION {
        let null = value[name].is_null();
        assert_eq!(null, reason(omitted, name).is_some(), "{name}: null={null}");
    }
}

fn repo_reading(at: DateTime<Utc>) -> RepoFriction {
    RepoFriction {
        observed_at: at,
        open_prs: Ok(4),
        lockout: Ok(true),
        ci_typical_sec: Err("too_few_ci_runs"),
    }
}

fn pr_reading(at: DateTime<Utc>) -> PrFriction {
    PrFriction {
        observed_at: at,
        ci: Ok(CiStatus::Failing),
        behind: Ok(true),
        conflict: Err("mergeability_unknown"),
    }
}

/// Additive, so `eta-explanation/v1` is not bumped: a payload logged before
/// the friction fields existed still parses, with them `None`.
#[test]
fn a_pre_friction_v1_payload_still_parses() {
    let mut old: serde_json::Value = serde_json::from_str(super::EXPLANATION_GOLDEN).unwrap();
    let features = old["features"].as_object_mut().unwrap();
    for name in FRICTION {
        assert!(features.remove(name).is_some(), "{name} is on the wire");
    }
    let parsed: crate::eta::explanation::Explanation = serde_json::from_value(old).unwrap();
    assert_eq!(parsed.schema, crate::eta::EXPLANATION_SCHEMA);
    let f = parsed.features.unwrap();
    assert_eq!((f.repo_open_prs, f.operator_hold, f.pr_ci_status), (None, None, None));
}

#[test]
fn nothing_read_yet_is_omitted_with_reasons() {
    let (mut f, mut omitted) = (Features::default(), Vec::new());
    FrictionBook::default().apply(REPO, None, None, as_of(), &mut f, &mut omitted);
    assert_accounted(&f, &omitted);
    assert_eq!(reason(&omitted, "repo_open_prs").as_deref(), Some("not_read_yet"));
    assert_eq!(reason(&omitted, "pr_ci_status").as_deref(), Some("no_pr"));
    assert_eq!(reason(&omitted, "operator_hold").as_deref(), Some("labels_not_listed"));
}

#[test]
fn fresh_readings_are_copied_and_failed_parts_keep_their_reason() {
    let read = as_of() - Duration::minutes(5);
    let mut book = FrictionBook::default();
    book.set_repo("RJWalters/Loom", repo_reading(read));
    book.set_pr(REPO, 7, pr_reading(read));
    let labels = vec![
        "loom:review-requested".to_string(),
        "loom:operator".to_string(),
    ];
    let (mut f, mut omitted) = (Features::default(), Vec::new());
    book.apply(REPO, Some(7), Some(&labels), as_of(), &mut f, &mut omitted);
    assert_accounted(&f, &omitted);
    assert_eq!(f.repo_open_prs, Some(4));
    assert_eq!(f.repo_pr_open_lockout, Some(true));
    assert_eq!(f.repo_friction_observed_at, Some(read));
    assert_eq!(f.pr_ci_status.as_deref(), Some("failing"));
    assert_eq!(f.pr_behind_main, Some(true));
    assert_eq!(f.operator_hold, Some(true), "merge-risk hold is explicit");
    assert_eq!(
        reason(&omitted, "repo_ci_typical_duration_sec").as_deref(),
        Some("too_few_ci_runs")
    );
    assert_eq!(reason(&omitted, "pr_merge_conflict").as_deref(), Some("mergeability_unknown"));

    let clean = vec!["loom:pr".to_string()];
    let (mut f, mut omitted) = (Features::default(), Vec::new());
    book.apply(REPO, Some(7), Some(&clean), as_of(), &mut f, &mut omitted);
    assert_eq!(f.operator_hold, Some(false));
}

#[test]
fn readings_after_as_of_or_stale_are_never_used() {
    let mut book = FrictionBook::default();
    book.set_repo(REPO, repo_reading(as_of() + Duration::seconds(1)));
    book.set_pr(REPO, 7, pr_reading(as_of() - Duration::seconds(MAX_READING_AGE_SECS + 1)));
    let (mut f, mut omitted) = (Features::default(), Vec::new());
    book.apply(REPO, Some(7), None, as_of(), &mut f, &mut omitted);
    assert_accounted(&f, &omitted);
    assert_eq!(f.repo_open_prs, None);
    assert_eq!(reason(&omitted, "repo_open_prs").as_deref(), Some("read_after_as_of"));
    assert_eq!(reason(&omitted, "pr_ci_status").as_deref(), Some("stale_reading"));
}

#[test]
fn parsers_read_the_forge_payloads() {
    assert_eq!(open_pr_count(&json!([{}, {}, {}])), Ok(3));
    assert_eq!(open_pr_count(&json!(vec![json!({}); 100])), Err("over_one_page"));
    assert!(open_pr_count(&json!({"message": "x"})).is_err());

    let pull = |mergeable: serde_json::Value, state: &str| {
        pr_mergeability(&json!({"mergeable": mergeable, "mergeable_state": state}))
    };
    assert_eq!(pull(json!(true), "behind"), (Ok(true), Ok(false)));
    assert_eq!(pull(json!(false), "dirty"), (Err("masked_by_merge_state"), Ok(true)));
    assert_eq!(pull(json!(true), "clean"), (Ok(false), Ok(false)));
    assert_eq!(
        pull(json!(null), "unknown"),
        (Err("mergeability_unknown"), Err("mergeability_unknown"))
    );

    let runs =
        |runs: serde_json::Value, total: u64| json!({"total_count": total, "check_runs": runs});
    let done = |c: &str| json!({"status": "completed", "conclusion": c});
    let running = json!({"status": "in_progress", "conclusion": null});
    assert_eq!(ci_status(&runs(json!([]), 0)), Ok(CiStatus::None));
    assert_eq!(
        ci_status(&runs(json!([done("success"), done("skipped")]), 2)),
        Ok(CiStatus::Passing)
    );
    assert_eq!(
        ci_status(&runs(json!([done("success"), running.clone()]), 2)),
        Ok(CiStatus::Pending)
    );
    assert_eq!(ci_status(&runs(json!([done("failure"), running]), 2)), Ok(CiStatus::Failing));
    assert_eq!(ci_status(&runs(json!([done("success")]), 140)), Err("over_one_page"));
    assert!(ci_status(&json!({})).is_err());
}

#[test]
fn typical_ci_duration_uses_only_runs_finished_before_the_instant() {
    let at = as_of();
    let run = |start_min: i64, minutes: i64, event: &str| {
        let started = at - Duration::minutes(start_min);
        json!({
            "status": "completed",
            "event": event,
            "run_started_at": started.to_rfc3339(),
            "updated_at": (started + Duration::minutes(minutes)).to_rfc3339(),
        })
    };
    let mut runs = vec![
        run(300, 10, "pull_request"),
        run(200, 20, "pull_request"),
        run(100, 30, "pull_request"),
    ];
    // Finished after `at`: knowable only later, so it must not count.
    runs.push(run(5, 600, "pull_request"));
    // Not a PR run, and one outside the window.
    runs.push(run(50, 1, "push"));
    runs.push(run(9 * 24 * 60, 1, "pull_request"));
    let page = json!({"workflow_runs": runs});
    assert_eq!(typical_ci_duration(&page, at), Ok(20 * 60));
    // Thin history refuses rather than guessing.
    let thin = json!({"workflow_runs": runs[..CI_MIN_RUNS - 1].to_vec()});
    assert_eq!(typical_ci_duration(&thin, at), Err("too_few_ci_runs"));
}

#[test]
fn reads_are_scheduled_never_read_first_then_oldest_within_budget() {
    let now = as_of();
    let mut book = FrictionBook::default();
    book.set_pr(REPO, 1, pr_reading(now - Duration::minutes(30)));
    book.set_pr(REPO, 2, pr_reading(now - Duration::minutes(60)));
    book.set_pr(REPO, 3, pr_reading(now - Duration::minutes(1)));
    let candidates: Vec<(String, u32)> = (1..=4).map(|n| (REPO.to_string(), n)).collect();
    let due = book.due_prs(&candidates, now, 600, 2);
    assert_eq!(due, vec![(REPO.to_string(), 4), (REPO.to_string(), 2)]);
    book.retain_prs(REPO, &[3]);
    assert!(book.pr(REPO, 1).is_none() && book.pr(REPO, 3).is_some());

    book.set_repo(REPO, repo_reading(now - Duration::minutes(1)));
    let repos = vec![REPO.to_string(), "o/other".to_string()];
    assert_eq!(book.due_repos(&repos, now, 900), vec!["o/other".to_string()]);
}

#[test]
fn every_tracker_estimate_accounts_for_every_friction_feature() {
    let at = as_of();
    let mut tracker = Tracker::new(provenance());
    tracker
        .friction
        .set_repo(REPO, repo_reading(at - Duration::minutes(3)));
    tracker
        .friction
        .set_pr(REPO, 70, pr_reading(at - Duration::minutes(3)));
    let pr = PrView {
        number: 70,
        issue: 7,
        labels: vec!["loom:review-requested".to_string()],
        created_at: Some(at - Duration::hours(2)),
        updated_at: Some(at - Duration::hours(1)),
    };
    tracker.on_listing(REPO, &[pr], at, 300);
    // A running sweep with no PR yet, in the same repo.
    tracker.on_dispatch(REPO, 8, "sweep-issue-8-1", at);
    let (registry, history, repo_ids) = (Registry::builtin(), history_a(), BTreeMap::new());
    let ctx = EstimateContext {
        registry: &registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history: &history,
        refresh_secs: 300,
        host_id: Some("host-test"),
        repo_ids: &repo_ids,
    };
    let emissions = tracker.estimate(None, &ctx, at);
    assert!(emissions.iter().any(|e| e.explanation.kind == Kind::Land));
    for e in &emissions {
        let ex = &e.explanation;
        assert_accounted(ex.features.as_ref().unwrap(), &ex.features_omitted);
        let f = ex.features.as_ref().unwrap();
        assert_eq!(f.repo_open_prs, Some(4));
        if ex.subject.issue == 7 {
            assert_eq!(f.pr_ci_status.as_deref(), Some("failing"));
            assert_eq!(f.operator_hold, Some(false));
        } else {
            assert_eq!(reason(&ex.features_omitted, "pr_ci_status").as_deref(), Some("no_pr"));
        }
    }
}
