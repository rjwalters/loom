//! Item facts (#10231): issue, sweep and verdict features, their omission
//! reasons, and the strict-before rule for each source.

use super::{as_of, history_a, provenance};
use crate::eta::explanation::{Explanation, Features};
use crate::eta::history::{StageSamples, VerdictSample};
use crate::eta::tracker::{
    DispatchMeta, Emission, EstimateContext, IssueRow, PrView, ReadyPlan, ReadyRow, RegistryMeta,
    Tracker,
};
use crate::eta::{Kind, Registry};
use crate::types::{DispatchPlanContext, PlanSlots, PlanState, QueueDisposition, RowPlan};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

const REPO: &str = "rjwalters/loom";

fn t(secs: i64) -> DateTime<Utc> {
    as_of() + Duration::seconds(secs)
}

fn stamp(secs: i64) -> String {
    t(secs).to_rfc3339()
}

struct H {
    tracker: Tracker,
    registry: Registry,
    history: StageSamples,
    repo_ids: BTreeMap<String, u64>,
}

impl H {
    fn new() -> Self {
        H {
            tracker: Tracker::new(provenance()),
            registry: Registry::builtin(),
            history: history_a(),
            repo_ids: BTreeMap::from([(REPO.to_string(), 1)]),
        }
    }

    fn at(&mut self, secs: i64) -> Vec<Emission> {
        let ctx = EstimateContext {
            registry: &self.registry,
            current_start: None,
            current_finish: None,
            current_land: None,
            history: &self.history,
            refresh_secs: 0,
            host_id: Some("host-test"),
            repo_ids: &self.repo_ids,
        };
        self.tracker
            .estimate(None, &ctx, t(secs))
            .into_iter()
            .filter(|e| e.primary)
            .collect()
    }

    /// The primary explanation of `issue`'s first emission at `secs`.
    fn explain(&mut self, issue: u32, kind: Kind, secs: i64) -> Explanation {
        self.at(secs)
            .into_iter()
            .find(|e| e.explanation.subject.issue == issue && e.explanation.kind == kind)
            .unwrap_or_else(|| panic!("no {kind} estimate for {issue} at {secs}"))
            .explanation
    }

    fn queue(&mut self, row: ReadyRow, plan_secs: i64, now_secs: i64) {
        let plan = ReadyPlan {
            context: DispatchPlanContext {
                slots: PlanSlots {
                    max_concurrent: 4,
                    occupancy: Some(3),
                    free: Some(1),
                    ..PlanSlots::default()
                },
                tick_interval_secs: Some(60),
                complete: true,
                ..DispatchPlanContext::default()
            },
            at: t(plan_secs),
            listing_failed: Vec::new(),
        };
        self.tracker.on_ready_queue(&[row], &plan, t(now_secs));
    }
}

fn row(issue: u32, facts: IssueRow) -> ReadyRow {
    ReadyRow {
        repo: REPO.to_string(),
        issue,
        plan: RowPlan {
            plan_state: PlanState::Next,
            position: Some(1),
            ..RowPlan::default()
        },
        disposition: QueueDisposition::DeferredCapacity,
        facts,
    }
}

fn facts(priority: u32, created: Option<String>, tier: Option<&str>) -> IssueRow {
    IssueRow {
        workspace_priority: priority,
        created_at: created,
        tier: tier.map(str::to_string),
    }
}

fn reason<'a>(e: &'a Explanation, name: &str) -> Option<&'a str> {
    e.features_omitted
        .iter()
        .find(|o| o.name == name)
        .map(|o| o.reason.as_str())
}

fn feats(e: &Explanation) -> &Features {
    e.features.as_ref().expect("features")
}

fn pr_view(number: u32, issue: u32) -> PrView {
    PrView {
        number,
        issue,
        labels: vec!["loom:review-requested".to_string()],
        created_at: Some(t(-100)),
        updated_at: Some(t(-50)),
    }
}

const ITEM_NAMES: [&str; 14] = [
    "tier",
    "urgent",
    "workspace_priority",
    "issue_created_at",
    "issue_age_sec",
    "sweep_runtime",
    "sweep_model",
    "sweep_effort",
    "attempt",
    "judge_verdicts_so_far",
    "repo_first_pass_approval_rate",
    "ahead",
    "queue_rank",
    "hour_utc",
];

#[test]
fn issue_facts_come_from_the_queue_row_and_age_is_issue_age() {
    let mut h = H::new();
    h.queue(row(10, facts(2, Some(stamp(-86_400)), Some("tier:sonnet"))), -30, 0);
    let e = h.explain(10, Kind::Start, 60);
    let f = feats(&e);
    assert_eq!(f.tier.as_deref(), Some("tier:sonnet"));
    assert_eq!(f.workspace_priority, Some(2));
    assert_eq!(f.issue_created_at, Some(t(-86_400)));
    assert_eq!(f.issue_age_sec, Some(86_460));
    assert_eq!(reason(&e, "urgent"), Some("deprecated"));
    assert!(f.urgent.is_none());

    // Dispatch, then a PR: the facts are kept, and the age is the issue's,
    // not the PR's (created at t(-100)).
    h.tracker.on_dispatch(REPO, 10, "sweep-issue-10-1", t(100));
    h.tracker.on_phase(REPO, 10, "builder", Some(50), t(150));
    h.tracker.on_listing(REPO, &[pr_view(50, 10)], t(200), 300);
    let e = h.explain(10, Kind::Land, 300);
    let f = feats(&e);
    assert_eq!(f.tier.as_deref(), Some("tier:sonnet"));
    assert_eq!(f.workspace_priority, Some(2));
    assert_eq!(f.issue_age_sec, Some(86_700));
}

#[test]
fn a_later_queue_refresh_does_not_reach_an_earlier_as_of() {
    let mut h = H::new();
    h.queue(row(10, facts(2, Some(stamp(-1000)), Some("tier:sonnet"))), -30, 0);
    h.queue(row(10, facts(1, Some(stamp(-1000)), Some("tier:opus"))), 500, 500);
    // An instant before the first observation reads nothing.
    let e = h.explain(10, Kind::Start, -40);
    assert_eq!(reason(&e, "tier"), Some("not_observed_yet"));
    assert_eq!(reason(&e, "issue_age_sec"), Some("not_observed_yet"));
    // Before, and exactly at, the refresh: the first observation.
    for secs in [400, 500] {
        let f = h.explain(10, Kind::Start, secs).features.unwrap();
        assert_eq!(f.tier.as_deref(), Some("tier:sonnet"), "{secs}");
        assert_eq!(f.workspace_priority, Some(2), "{secs}");
    }
    let f = h.explain(10, Kind::Start, 501).features.unwrap();
    assert_eq!(f.tier.as_deref(), Some("tier:opus"));
    assert_eq!(f.workspace_priority, Some(1));
}

#[test]
fn unusable_creation_timestamps_and_missing_tiers_have_their_own_reasons() {
    let mut h = H::new();
    h.queue(row(10, facts(0, None, None)), -30, 0);
    let e = h.explain(10, Kind::Start, 60);
    assert_eq!(reason(&e, "tier"), Some("no_tier_label"));
    assert_eq!(reason(&e, "issue_created_at"), Some("issue_created_at_missing"));
    assert_eq!(reason(&e, "issue_age_sec"), Some("issue_created_at_missing"));
    assert_eq!(feats(&e).workspace_priority, Some(0));

    h.queue(row(10, facts(0, Some("yesterday".to_string()), None)), 100, 100);
    let e = h.explain(10, Kind::Start, 200);
    assert_eq!(reason(&e, "issue_created_at"), Some("issue_created_at_malformed"));
    assert_eq!(reason(&e, "issue_age_sec"), Some("issue_created_at_malformed"));

    h.queue(row(10, facts(0, Some(stamp(5000)), None)), 300, 300);
    let e = h.explain(10, Kind::Start, 400);
    assert_eq!(feats(&e).issue_created_at, Some(t(5000)));
    assert_eq!(reason(&e, "issue_age_sec"), Some("issue_created_in_future"));
    assert!(feats(&e).issue_age_sec.is_none());
}

#[test]
fn an_item_never_seen_in_the_queue_says_so() {
    let mut h = H::new();
    h.tracker.on_listing(REPO, &[pr_view(50, 10)], t(0), 300);
    let e = h.explain(10, Kind::Land, 60);
    assert_eq!(reason(&e, "tier"), Some("never_in_ready_queue"));
    assert_eq!(reason(&e, "workspace_priority"), Some("never_in_ready_queue"));
    assert_eq!(reason(&e, "issue_age_sec"), Some("never_in_ready_queue"));
    // No sweep, and the PR's earlier verdicts are not known to this process.
    assert_eq!(reason(&e, "attempt"), Some("no_sweep_yet"));
    assert_eq!(reason(&e, "judge_verdicts_so_far"), Some("verdict_history_unobserved"));
}

fn registry(model: Option<&str>, effort: Option<&str>, started: i64) -> Option<RegistryMeta> {
    Some(RegistryMeta {
        model: model.map(str::to_string),
        effort: effort.map(str::to_string),
        started_at: t(started),
    })
}

#[test]
fn sweep_facts_come_from_the_dispatch_and_the_registry() {
    let mut h = H::new();
    h.tracker.on_dispatch(REPO, 10, "sweep-issue-10-1", t(10));
    let meta = DispatchMeta {
        runtime: Some("claude".to_string()),
        registry: registry(Some("opus"), None, 5),
    };
    h.tracker
        .on_sweep_dispatch(REPO, 10, "sweep-issue-10-1", meta.clone(), t(10));
    // Spawned at t(5) but observed at t(10): the registry's earlier
    // `started_at` is not when the value was known here, and equality is
    // not before.
    for secs in [5, 10] {
        let e = h.explain(10, Kind::Finish, secs);
        assert_eq!(reason(&e, "sweep_model"), Some("no_sweep_yet"), "{secs}");
        assert_eq!(reason(&e, "attempt"), Some("no_sweep_yet"), "{secs}");
    }
    assert_eq!(feats(&h.explain(10, Kind::Finish, 11)).attempt, Some(1));
    let e = h.explain(10, Kind::Finish, 60);
    let f = feats(&e);
    assert_eq!(f.sweep_runtime.as_deref(), Some("claude"));
    assert_eq!(f.sweep_model.as_deref(), Some("opus"));
    assert!(f.sweep_effort.is_none());
    assert_eq!(reason(&e, "sweep_effort"), Some("runtime_default"));
    assert_eq!(f.attempt, Some(1));

    // A repeated publication of the same sweep is one attempt; a new sweep
    // is the second.
    h.tracker
        .on_sweep_dispatch(REPO, 10, "sweep-issue-10-1", meta, t(11));
    h.tracker.on_dispatch(REPO, 10, "sweep-issue-10-2", t(500));
    h.tracker.on_sweep_dispatch(
        REPO,
        10,
        "sweep-issue-10-2",
        DispatchMeta {
            runtime: None,
            registry: None,
        },
        t(500),
    );
    assert_eq!(feats(&h.explain(10, Kind::Finish, 400)).attempt, Some(1));
    let e = h.explain(10, Kind::Finish, 600);
    assert_eq!(feats(&e).attempt, Some(2));
    assert_eq!(reason(&e, "sweep_runtime"), Some("runtime_not_recorded"));
    assert_eq!(reason(&e, "sweep_model"), Some("registry_unavailable"));
    assert_eq!(reason(&e, "sweep_effort"), Some("registry_unavailable"));
}

#[test]
fn a_sweep_adopted_without_its_dispatch_is_not_observed() {
    let mut h = H::new();
    // A phase arrives for a sweep this process never saw dispatched.
    h.tracker.on_phase(REPO, 10, "curator", None, t(10));
    let e = h.explain(10, Kind::Finish, 60);
    assert_eq!(reason(&e, "sweep_runtime"), Some("sweep_not_observed"));
    assert_eq!(reason(&e, "attempt"), Some("sweep_not_observed"));
}

#[test]
fn verdicts_are_known_when_settled_not_when_given() {
    let mut h = H::new();
    h.tracker.on_dispatch(REPO, 10, "sweep-issue-10-1", t(0));
    h.tracker
        .on_sweep_dispatch(REPO, 10, "sweep-issue-10-1", DispatchMeta::default(), t(0));
    h.tracker.on_phase(REPO, 10, "builder", Some(50), t(50));
    // The Judge finishes at 100 with a verdict nobody sees until the Doctor
    // starts at 300.
    h.tracker.on_phase(REPO, 10, "judge", Some(50), t(100));
    h.tracker.on_phase(REPO, 10, "doctor", Some(50), t(300));
    assert_eq!(
        feats(&h.explain(10, Kind::Finish, 200)).judge_verdicts_so_far,
        Some(vec![]),
        "an instant before the settlement does not see the verdict"
    );
    assert_eq!(
        feats(&h.explain(10, Kind::Finish, 300)).judge_verdicts_so_far,
        Some(vec![]),
        "equal to the settlement is not before it"
    );
    assert_eq!(
        feats(&h.explain(10, Kind::Finish, 301)).judge_verdicts_so_far,
        Some(vec!["fail".to_string()])
    );
}

fn verdict(repo: &str, attempt: u32, rejected: bool, at: i64) -> VerdictSample {
    VerdictSample {
        repo: repo.to_string(),
        attempt,
        rejected,
        observed_at: t(at),
    }
}

#[test]
fn the_first_pass_rate_counts_first_verdicts_observed_before_as_of() {
    let mut h = H::new();
    h.history = StageSamples {
        verdicts: vec![
            verdict(REPO, 1, false, -300),
            verdict(REPO, 1, false, -200),
            verdict(REPO, 1, true, -100),
            // Not first verdicts, another repo, and not yet observed.
            verdict(REPO, 2, false, -90),
            verdict("someone/else", 1, true, -90),
            verdict(REPO, 1, true, 60),
            verdict(REPO, 1, true, 120),
        ],
        ..StageSamples::default()
    };
    h.tracker.on_dispatch(REPO, 10, "sweep-issue-10-1", t(-400));
    let rate = |h: &mut H, secs: i64| {
        let e = h.explain(10, Kind::Finish, secs);
        (
            feats(&e).repo_first_pass_approval_rate,
            reason(&e, "repo_first_pass_approval_rate").map(str::to_string),
        )
    };
    // Before any first verdict: an empty denominator.
    let (r, why) = rate(&mut h, -301);
    assert!(r.is_none());
    assert_eq!(why.as_deref(), Some("no_first_verdicts_before_as_of"));
    let (r, _) = rate(&mut h, 0);
    assert!((r.unwrap() - 2.0 / 3.0).abs() < 1e-12);
    // The verdict recorded at 60 is not seen at 60, and is at 61.
    let (r, _) = rate(&mut h, 60);
    assert!((r.unwrap() - 2.0 / 3.0).abs() < 1e-12);
    let (r, _) = rate(&mut h, 61);
    assert!((r.unwrap() - 0.5).abs() < 1e-12);
    // No history at all.
    h.history = StageSamples::default();
    let (r, why) = rate(&mut h, 100);
    assert!(r.is_none());
    assert_eq!(why.as_deref(), Some("no_verdict_history"));
}

#[test]
fn no_item_fact_falls_back_to_not_collected() {
    let mut h = H::new();
    h.queue(row(10, facts(1, None, None)), -30, 0);
    h.tracker.on_listing(REPO, &[pr_view(50, 11)], t(0), 300);
    h.tracker.on_dispatch(REPO, 12, "sweep-issue-12-1", t(0));
    let emitted = h.at(60);
    assert!(!emitted.is_empty());
    for e in emitted.iter().map(|e| &e.explanation) {
        for name in ITEM_NAMES {
            assert_ne!(reason(e, name), Some("not_collected"), "{name} on {}", e.subject.issue);
        }
    }
}
