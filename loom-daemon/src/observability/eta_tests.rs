//! Delivery: what the ETA hook offers to the OTLP queues.

use super::*;
use crate::eta::tests::{as_of, history_a, provenance};
use crate::eta::tracker::{EstimateContext, IssueState};
use crate::telemetry::trace::story_context;
use std::sync::Mutex as StdMutex;

#[derive(Default)]
struct Capture(StdMutex<Vec<TelemetryEnvelope>>);

impl QueueSink for Capture {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.0.lock().unwrap().push(envelope);
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.offer(envelope);
        Ok(())
    }
}

const REPO: &str = "rjwalters/loom";

fn lifecycle(loom: Provenance) -> (Vec<Emission>, Vec<Resolved>) {
    let mut tracker = Tracker::new(loom);
    let registry = Registry::builtin();
    let history = history_a();
    let mut repo_ids = BTreeMap::new();
    repo_ids.insert(REPO.to_string(), 1_073_994_527_u64);
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
    tracker.on_dispatch(REPO, 9289, "sweep-issue-9289-1", as_of());
    let emissions = tracker.estimate(None, &ctx, as_of());
    tracker.on_phase(REPO, 9289, "curator", None, as_of() + chrono::Duration::seconds(60));
    let mut outcomes = tracker
        .on_terminal(REPO, 9289, "exited", Some(0), as_of() + chrono::Duration::seconds(120))
        .outcomes;
    // The sweep ended before any PR, so only the issue settles `land`: here
    // it was closed as not planned (operator decision 4 on #9289).
    outcomes.extend(
        tracker
            .on_issue_resolved(
                &ItemKey::new(REPO, 9289),
                IssueState::ClosedNotPlanned(as_of() + chrono::Duration::seconds(150)),
                as_of() + chrono::Duration::seconds(180),
            )
            .outcomes,
    );
    (emissions, outcomes)
}

#[test]
fn delivery_offers_story_scoped_estimates_and_outcomes() {
    let (emissions, outcomes) = lifecycle(provenance());
    // The daemon rolled between the estimate and its outcome.
    let rolled = Provenance {
        version: "0.19.999".to_string(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
        tree_state: "dirty".to_string(),
        complete: true,
    };
    let sink = Capture::default();
    let delivered = deliver(emissions, outcomes, &rolled, "host-test", false, Some(&sink));
    // finish-v1, land-v1 (primary) and the land-v2 shadow (#9328).
    assert_eq!(delivered.emitted, 3, "finish + land + the land shadow");
    assert_eq!(delivered.outcomes, 3, "finish finished, both land estimates abandoned");
    assert_eq!(delivered.invalid, 0);
    let offered = sink.0.lock().unwrap();
    let kinds: Vec<&str> = offered.iter().map(|e| e.record.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            "eta.estimate",
            "eta.estimate",
            "eta.estimate",
            "eta.outcome",
            "eta.outcome",
            "eta.outcome"
        ]
    );
    // Exactly one estimate per kind is primary; the rest are shadows.
    let primaries: Vec<bool> = offered
        .iter()
        .filter_map(|e| match &e.record {
            TelemetryRecord::EtaEstimate(r) => Some(r.primary),
            _ => None,
        })
        .collect();
    assert_eq!(primaries.iter().filter(|p| **p).count(), 2, "one per kind");
    let story = story_context(1_073_994_527, 9289).unwrap();
    for envelope in offered.iter() {
        assert_eq!(envelope.trace_context.as_ref(), Some(&story), "inside the issue's story trace");
        assert_eq!(envelope.host_id, "host-test");
        assert_eq!(envelope.schema_version, 12);
    }
    let TelemetryRecord::EtaOutcome(outcome) = &offered[3].record else {
        panic!("outcome")
    };
    assert_eq!(outcome.estimate.loom, provenance(), "the estimating build");
    assert_eq!(outcome.loom, rolled, "the observing build");
    let TelemetryRecord::EtaEstimate(estimate) = &offered[0].record else {
        panic!("estimate")
    };
    assert_eq!(estimate.explanation.loom, provenance());
}

#[test]
fn dry_run_offers_nothing_and_counts_everything() {
    let (emissions, outcomes) = lifecycle(provenance());
    let sink = Capture::default();
    let delivered = deliver(emissions, outcomes, &provenance(), "host-test", true, Some(&sink));
    assert_eq!((delivered.emitted, delivered.outcomes), (3, 3));
    assert!(sink.0.lock().unwrap().is_empty());
}

#[test]
fn records_without_valid_provenance_are_never_offered() {
    let bad = Provenance {
        revision: "bf2fb67".to_string(),
        ..provenance()
    };
    // Estimates computed by a build with a short SHA …
    let (emissions, outcomes) = lifecycle(bad.clone());
    let sink = Capture::default();
    let delivered = deliver(emissions, Vec::new(), &provenance(), "host-test", false, Some(&sink));
    assert_eq!(delivered.invalid, 3);
    assert!(sink.0.lock().unwrap().is_empty());
    // … and outcomes observed by one, or scoring one.
    let delivered =
        deliver(Vec::new(), outcomes.clone(), &provenance(), "host-test", false, Some(&sink));
    assert_eq!(delivered.invalid, 3, "the estimating build's provenance is checked too");
    let (_, good_outcomes) = lifecycle(provenance());
    let delivered = deliver(Vec::new(), good_outcomes, &bad, "host-test", false, Some(&sink));
    assert_eq!(delivered.invalid, 3, "the observing build's provenance is checked too");
    assert!(sink.0.lock().unwrap().is_empty());
}

#[test]
fn no_repo_id_means_no_story_context() {
    let (emissions, _) = lifecycle(provenance());
    let mut emission = emissions.into_iter().next().unwrap();
    emission.explanation.subject.repo_id = None;
    let sink = Capture::default();
    deliver(vec![emission], Vec::new(), &provenance(), "host-test", false, Some(&sink));
    assert_eq!(sink.0.lock().unwrap()[0].trace_context, None, "never derived from the name");
}

#[test]
fn pr_views_key_each_pr_to_the_issue_it_closes() {
    let row = |number: u32, body: &str, pr: bool| RestIssue {
        number,
        title: None,
        labels: vec!["loom:review-requested".to_string()],
        created_at: Some("2026-09-28T10:00:00Z".to_string()),
        updated_at: Some("2026-09-28T11:00:00Z".to_string()),
        closed_at: None,
        state: "open".to_string(),
        body: Some(body.to_string()),
        author: None,
        is_pull_request: pr,
    };
    let listings = vec![
        vec![row(501, "Closes #50", true), row(50, "an issue", false)],
        vec![row(501, "Closes #50", true), row(502, "no reference", true)],
    ];
    let views = pr_views(&listings);
    assert_eq!(views.len(), 1, "deduped; issues and unreferenced PRs skipped");
    assert_eq!((views[0].number, views[0].issue), (501, 50));
    assert!(views[0].updated_at.is_some());
}

#[test]
fn incomplete_provenance_is_emitted_and_marked() {
    let tarball = Provenance {
        revision: "unknown".to_string(),
        tree_state: "unknown".to_string(),
        complete: false,
        ..provenance()
    };
    let (emissions, outcomes) = lifecycle(tarball.clone());
    let sink = Capture::default();
    let delivered = deliver(emissions, outcomes, &tarball, "host-test", false, Some(&sink));
    assert_eq!(
        (delivered.emitted, delivered.outcomes, delivered.invalid),
        (3, 3, 0),
        "no data lost"
    );
    for envelope in sink.0.lock().unwrap().iter() {
        let complete = match &envelope.record {
            TelemetryRecord::EtaEstimate(r) => r.explanation.loom.complete,
            TelemetryRecord::EtaOutcome(r) => r.estimate.loom.complete || r.loom.complete,
            other => panic!("{}", other.kind()),
        };
        assert!(!complete, "marked incomplete, so accuracy queries drop it");
    }
}
