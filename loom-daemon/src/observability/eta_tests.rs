//! Delivery: what the ETA hook offers to the OTLP queues.

use super::authority::{drop_pending, gate_delivery};
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
        stalls: &crate::eta::stall::StallSnapshot::default(),
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
    // finish-v1, land-v1 (primary) and the land-v2 /
    // land-2026-10-06-calm-plover / land-v4 shadows (#9328, #10489, #10210)
    // answer (land-v3 and amber-heron retired, #10484; fresh-tide retired,
    // #10549); the land-2026-10-04-twin-otter shadow (#10243) and
    // little-v0 (#10208) refuse this pre-PR stage; the twin-otter -b
    // composition (#10244) answers it from land-v2's path, and so does its
    // land-2026-10-06-quick-tern calibration wrapper (#10524).
    assert_eq!(delivered.emitted, 7, "finish + land + the five answering land shadows");
    assert_eq!(delivered.refused, 2, "twin-otter and little-v0: unknown_stage before a PR");
    assert_eq!(delivered.outcomes, 9, "finish finished, every land estimate abandoned");
    assert_eq!(delivered.invalid, 0);
    let offered = sink.0.lock().unwrap();
    let kinds: Vec<&str> = offered.iter().map(|e| e.record.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            "eta.estimate",
            "eta.estimate",
            "eta.estimate",
            "eta.estimate",
            "eta.estimate",
            "eta.estimate",
            "eta.estimate",
            "eta.estimate",
            "eta.estimate",
            "eta.outcome",
            "eta.outcome",
            "eta.outcome",
            "eta.outcome",
            "eta.outcome",
            "eta.outcome",
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
    let TelemetryRecord::EtaOutcome(outcome) = &offered[9].record else {
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
    assert_eq!((delivered.emitted, delivered.refused, delivered.outcomes), (7, 2, 9));
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
    assert_eq!(delivered.invalid, 9);
    assert!(sink.0.lock().unwrap().is_empty());
    // … and outcomes observed by one, or scoring one.
    let delivered =
        deliver(Vec::new(), outcomes.clone(), &provenance(), "host-test", false, Some(&sink));
    assert_eq!(delivered.invalid, 9, "the estimating build's provenance is checked too");
    let (_, good_outcomes) = lifecycle(provenance());
    let delivered = deliver(Vec::new(), good_outcomes, &bad, "host-test", false, Some(&sink));
    assert_eq!(delivered.invalid, 9, "the observing build's provenance is checked too");
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
        comments: 0,
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
        (7, 9, 0),
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

// ------------------------------------------- the fitted heuristics' file (#10243)

/// What `land-2026-10-04-twin-otter` answers from `registry`: the fit id its
/// explanation records, or the refusal.
fn twin_otter_answer(registry: &Registry) -> Result<String, crate::eta::NoEstimateReason> {
    use crate::eta::heuristics::LAND_TWIN_OTTER;
    let input = crate::eta::tests::land_twin_otter::review_input();
    let e = registry
        .get(LAND_TWIN_OTTER)
        .expect("always registered")
        .estimate(&input, &StageSamples::default());
    match (e.twin_otter, e.no_estimate_reason) {
        (Some(record), None) => Ok(record.fit_id),
        (_, reason) => Err(reason.expect("a refusal names its reason")),
    }
}

#[test]
fn swap_fit_rebuilds_the_registry_only_when_the_fit_id_changes() {
    use crate::eta::tests::land_twin_otter::{fit_as_of, fixture_fit};
    let a = fixture_fit(fit_as_of());
    let b = fixture_fit(fit_as_of() + chrono::Duration::hours(1));
    assert_ne!(a.id, b.id);
    // The same file again: nothing to do.
    assert!(swap_fit(Some(&a.id), Some(a.clone())).is_none());
    assert!(swap_fit(None, None).is_none());
    // A new id (the daily refit): rebuilt around it.
    let swapped = swap_fit(Some(&a.id), Some(b.clone())).expect("a new id swaps");
    assert_eq!(swapped.fit_id(), Some(b.id.as_str()));
    assert_eq!(twin_otter_answer(&swapped), Ok(b.id.clone()));
    // A file appearing, and one disappearing.
    let appeared = swap_fit(None, Some(a.clone())).expect("a first file swaps");
    assert_eq!(appeared.fit_id(), Some(a.id.as_str()));
    let gone = swap_fit(Some(&b.id), None).expect("a missing file swaps");
    assert_eq!(gone.fit_id(), None, "the unloaded registry");
    assert_eq!(gone.ids(), Registry::builtin().ids());
    assert_eq!(twin_otter_answer(&gone), Err(crate::eta::NoEstimateReason::NoModel));
}

/// The per-pass hot reload, end to end through the files: fit A, then a
/// newer fit B, then a pass with no change. The root is passed explicitly;
/// `LOOM_ETA_FIT_DIR` is never set (env mutation races parallel tests).
#[test]
fn a_pass_hot_reloads_a_newer_fit_and_ignores_an_unchanged_one() {
    use crate::eta::tests::land_twin_otter::{fit_as_of, fixture_fit, review_input};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let now = review_input().as_of;
    let mut registry = Registry::load(root, now);
    let reload =
        |registry: &mut Registry| match swap_fit(registry.fit_id(), fit::load_latest(root, now)) {
            Some(rebuilt) => {
                *registry = rebuilt;
                true
            }
            None => false,
        };
    assert_eq!(registry.fit_id(), None, "an empty directory loads nothing");
    assert_eq!(twin_otter_answer(&registry), Err(crate::eta::NoEstimateReason::NoModel));
    assert!(!reload(&mut registry), "still nothing: no swap");

    let a = fixture_fit(fit_as_of());
    fit::write(&fit::fit_dir(root).join(fit::path_for(a.as_of)), &a).unwrap();
    assert!(reload(&mut registry), "fit A appears");
    assert_eq!(twin_otter_answer(&registry), Ok(a.id.clone()));

    let b = fixture_fit(fit_as_of() + chrono::Duration::days(1));
    fit::write(&fit::fit_dir(root).join(fit::path_for(b.as_of)), &b).unwrap();
    assert!(reload(&mut registry), "the newer fit B replaces A");
    assert_eq!(twin_otter_answer(&registry), Ok(b.id.clone()));

    assert!(!reload(&mut registry), "no change: no swap");
    assert_eq!(registry.fit_id(), Some(b.id.as_str()));
    // A fit cut off after `now` is never loaded for it.
    let future = fixture_fit(now + chrono::Duration::hours(1));
    fit::write(&fit::fit_dir(root).join(fit::path_for(future.as_of)), &future).unwrap();
    assert!(!reload(&mut registry), "a future fit is invisible at `now`");
}

/// A `gh` stub for the starred-issue reads (#10389): page 1 of any label is
/// a full page (issues 1..=99 plus PR 100), `&page=2` is issues 101..=104,
/// failing instead when `fail2` exists.
fn starred_stub(dir: &std::path::Path) -> PathBuf {
    let row = |n: u32, pr: bool| {
        let pr = if pr { r#", "pull_request": {}"# } else { "" };
        format!(r#"{{"number": {n}, "state": "open", "labels": []{pr}}}"#)
    };
    let page = |rows: Vec<String>| format!("[{}]\n", rows.join(","));
    let p1: Vec<String> = (1..=100).map(|n| row(n, n == 100)).collect();
    std::fs::write(dir.join("p1.json"), page(p1)).unwrap();
    std::fs::write(dir.join("p2.json"), page((101..=104).map(|n| row(n, false)).collect()))
        .unwrap();
    let path = dir.join("fake-gh-star.sh");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
d={dir}
case "$*" in
  *'&page=2'*)
    if [ -f "$d/fail2" ]; then echo 'gh: Server Error (HTTP 502)' 1>&2; exit 1; fi
    printf 'HTTP/2.0 200 OK\r\n\r\n'; cat "$d/p2.json" ;;
  *) printf 'HTTP/2.0 200 OK\r\n\r\n'; cat "$d/p1.json" ;;
esac
"#,
            dir = dir.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// More than 100 starred items: an issue past page 1 is starred, PRs are
/// left out, and labels' listings are merged once each. An incomplete walk
/// is `Err`, which `starred_issues` turns into `None` (unknown).
#[test]
fn starred_issues_read_every_page_and_an_incomplete_walk_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let gh = starred_stub(dir.path());
    let pid = std::process::id();
    let (a, b) = (format!("test:star-a-{pid}"), format!("test:star-b-{pid}"));
    let labels = [a.as_str(), b.as_str()];

    let issues = read_starred_issues(&gh, dir.path(), &labels).unwrap();
    let want: Vec<u32> = (1..=99).chain(101..=104).collect();
    assert_eq!(issues, want, "issue 104 is past page 1; PR 100 is not an issue");

    std::fs::write(dir.path().join("fail2"), "").unwrap();
    assert!(read_starred_issues(&gh, dir.path(), &labels).is_err());
}

// ===== One ETA authority per fleet (#10498) =====

#[test]
fn a_non_authority_host_emits_nothing() {
    let (emissions, outcomes) = lifecycle(provenance());
    assert!(!emissions.is_empty() && !outcomes.is_empty());
    let sink = Capture::default();
    let delivered =
        gate_delivery(false, emissions, outcomes, &provenance(), "host-test", false, Some(&sink));
    assert_eq!(delivered, Delivered::default());
    assert!(sink.0.lock().unwrap().is_empty(), "no eta.* record from a non-authority host");
}

#[test]
fn the_authority_still_emits() {
    let (emissions, outcomes) = lifecycle(provenance());
    let sink = Capture::default();
    let delivered =
        gate_delivery(true, emissions, outcomes, &provenance(), "host-test", false, Some(&sink));
    assert!(delivered.emitted > 0 && delivered.outcomes > 0);
    assert!(!sink.0.lock().unwrap().is_empty());
}

#[test]
fn demotion_drops_the_pending_store_once() {
    let (emissions, _) = lifecycle(provenance());
    assert!(!emissions.is_empty());
    let dir = tempfile::tempdir().unwrap();
    let path = pending_path(dir.path());
    let mut tracker = Tracker::new(provenance());
    let pending = vec![EstimateSummary::of(&emissions[0].explanation)];
    write_pending(&path, &pending);
    tracker.restore_pending(pending, &Registry::builtin());
    assert!(path.exists());
    assert_eq!(drop_pending(&mut tracker, &path), 1);
    assert!(tracker.pending().is_empty());
    assert!(!path.exists(), "the persisted store is deleted");
    // Idempotent: a second drop finds nothing and does not fail.
    assert_eq!(drop_pending(&mut tracker, &path), 0);
    assert!(read_pending(&path).is_empty(), "nothing is restored after a restart either");
}
