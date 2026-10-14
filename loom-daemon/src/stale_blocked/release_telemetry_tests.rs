//! The release pass's SigNoz export (#10752), driven through the real pass
//! over the fake forges of [`super::super::release_tests`].

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use super::super::release::{Config, Report};
use super::super::release_items::{blocker_state, BlockerCheck, Item, ItemVerdict};
use super::super::release_tests::{cfg, World};
use super::*;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation, ParentContext};
use crate::observability::ops::capture::capture;
use crate::telemetry::kinds::pass::{PassMode, PassOutcome, PassSummaryRecord, PassVerdictRecord};
use crate::telemetry::TelemetryRecord;

const MECHANISM: &str = "stale_blocked_release";
const HOUR: Duration = Duration::from_secs(3600);

/// One of each verdict the fake forge can produce without a failing write.
fn mixed_world() -> World {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]); // released
    w.parked(11, false, &[1, 3], &[]); // re-parked
    w.parked(12, true, &[3], &[]); // still blocked (a PR)
    w.with_body(13, false, "Blocked, see the thread.", &[]); // no park record
    w.parked(14, false, &[5], &[]); // closed-unmerged PR blocker
    w.state(1, "CLOSED", false);
    w.state(3, "OPEN", false);
    w.state(5, "CLOSED", true);
    w
}

fn split(records: Vec<TelemetryRecord>) -> (PassSummaryRecord, Vec<PassVerdictRecord>) {
    let mut it = records.into_iter();
    let Some(TelemetryRecord::PassSummary(summary)) = it.next() else {
        panic!("the summary comes first");
    };
    let verdicts = it
        .map(|r| match r {
            TelemetryRecord::PassVerdict(v) => v,
            other => panic!("unexpected {}", other.kind()),
        })
        .collect();
    (summary, verdicts)
}

fn verdict(verdicts: &[PassVerdictRecord], number: u64) -> &PassVerdictRecord {
    verdicts
        .iter()
        .find(|v| v.number == number)
        .unwrap_or_else(|| panic!("no verdict for #{number}"))
}

fn blockers(v: &PassVerdictRecord) -> Vec<(String, String)> {
    v.blockers
        .iter()
        .map(|b| (b.reference.clone(), b.state.clone()))
        .collect()
}

fn pair(reference: &str, state: &str) -> (String, String) {
    (reference.to_string(), state.to_string())
}

/// A label write through the real `gh` facade, against a stub `gh`.
fn stub_label_write(number: u64) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("gh-stub");
    std::fs::write(&path, "#!/bin/sh\necho ok\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    GhInvocation::new(
        Operation::new("api.rest"),
        AccessIntent::Write,
        GhTarget::repo("acme/widgets").unwrap(),
        Duration::from_secs(10),
    )
    .parent(ParentContext::Missing)
    .program(&path)
    .args([
        "api",
        "-X",
        "DELETE",
        &format!("repos/acme/widgets/issues/{number}/labels/loom%3Ablocked"),
    ])
    .execute()
    .unwrap();
}

// --- per-artifact verdicts on the report ----------------------------------------

#[test]
fn every_listed_artifact_gets_exactly_one_verdict() {
    let mut w = mixed_world();
    let r = w.run();
    assert_eq!(r.items.len(), r.examined, "{:?}", r.items);
    let mut numbers: Vec<u64> = r.items.iter().map(|i| i.number).collect();
    numbers.sort_unstable();
    assert_eq!(numbers, vec![10, 11, 12, 13, 14]);
    // The counts and the items agree.
    let count = |v: ItemVerdict| r.items.iter().filter(|i| i.verdict == v).count();
    assert_eq!(count(ItemVerdict::Released), r.released.len());
    assert_eq!(count(ItemVerdict::Reparked), r.reparked.len());
    assert_eq!(count(ItemVerdict::StillBlocked), r.still_blocked);
    assert_eq!(count(ItemVerdict::Skipped), r.skipped.values().sum::<usize>());
}

#[test]
fn the_budget_floor_leaves_every_eligible_artifact_unevaluated_with_its_reason() {
    let mut w = mixed_world();
    w.budget(1, 1_000);
    let r = w.run();
    let unevaluated: Vec<&Item> = r
        .items
        .iter()
        .filter(|i| i.verdict == ItemVerdict::Unevaluated)
        .collect();
    assert_eq!(unevaluated.len(), 4, "{:?}", r.items);
    assert!(unevaluated[0]
        .detail
        .as_deref()
        .unwrap()
        .contains("budget floor"));
    assert_eq!(r.items.len(), r.examined);
}

#[test]
fn a_failed_write_is_one_failed_verdict_with_its_detail() {
    let mut r = Report::default();
    r.fail(7, "removing loom:blocked failed: HTTP 502".to_string());
    assert_eq!(r.failed.len(), 1);
    assert_eq!(r.items[0].verdict, ItemVerdict::Failed);
    assert_eq!(r.items[0].detail.as_deref(), Some("removing loom:blocked failed: HTTP 502"));
}

// --- the records ------------------------------------------------------------------

#[test]
fn a_pass_emits_one_summary_and_a_verdict_per_artifact_and_its_calls_carry_the_caller() {
    let mut w = mixed_world();
    let (records, captured) = capture(|| {
        let observer = Observer::start(MECHANISM);
        stub_label_write(10);
        let r = w.run();
        observer.records("acme/widgets", "host-a", &r, HOUR)
    });

    // The pass's GitHub call carries the mechanism, its number and repo.
    assert_eq!(captured.spans.len(), 1);
    let span = &captured.spans[0].attributes;
    assert_eq!(span.get("github.caller").map(String::as_str), Some(MECHANISM));
    assert_eq!(span.get("github.number").map(String::as_str), Some("10"));
    assert_eq!(span.get("github.repo").map(String::as_str), Some("acme/widgets"));

    let (summary, verdicts) = split(records);
    assert_eq!(summary.mechanism, MECHANISM);
    assert_eq!((summary.repo.as_str(), summary.host.as_str()), ("acme/widgets", "host-a"));
    assert_eq!(summary.mode, PassMode::On);
    assert_eq!(summary.outcome, PassOutcome::Completed);
    assert_eq!(summary.refusal, None);
    assert_eq!(summary.examined, 5);
    let expected: BTreeMap<String, u64> = [
        ("released", 1),
        ("reparked", 1),
        ("still_blocked", 1),
        ("skipped", 2),
        ("unevaluated", 0),
        ("failed", 0),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(summary.verdicts, expected);
    assert_eq!(summary.skipped.get("no-park-record"), Some(&1));
    assert_eq!(summary.skipped.get("closed-unmerged-pr"), Some(&1));
    assert!(!summary.write_cap_hit);
    assert_eq!((summary.github.calls, summary.github.writes), (1, 1));
    assert_eq!((summary.verdicts_emitted, summary.verdicts_unchanged), (5, 0));
    assert!(summary.ended_at >= summary.started_at);
    assert!(summary.loom.is_valid());

    assert_eq!(verdicts.len(), 5);
    for v in &verdicts {
        assert_eq!(v.pass_id, summary.pass_id, "every verdict joins its pass");
        assert_eq!((v.mechanism.as_str(), v.repo.as_str()), (MECHANISM, "acme/widgets"));
        assert_eq!(v.role, None, "a daemon pass has no role");
    }
    let released = verdict(&verdicts, 10);
    assert_eq!(released.verdict, "released");
    assert_eq!(released.labels_removed, vec!["loom:blocked".to_string()]);
    assert!(released.applied);
    assert_eq!(blockers(released), vec![pair("#1", "closed")]);

    let reparked = verdict(&verdicts, 11);
    assert_eq!(reparked.verdict, "reparked");
    assert!(reparked.labels_removed.is_empty());
    assert_eq!(blockers(reparked), vec![pair("#1", "closed"), pair("#3", "open")]);

    let held = verdict(&verdicts, 12);
    assert_eq!((held.verdict.as_str(), held.artifact.as_str()), ("still_blocked", "pr"));
    assert_eq!(blockers(held), vec![pair("#3", "open")]);

    let prose = verdict(&verdicts, 13);
    assert_eq!(prose.verdict, "skipped");
    assert_eq!(prose.reason.as_deref(), Some("no-park-record"));
    assert!(prose.blockers.is_empty());

    let unmerged = verdict(&verdicts, 14);
    assert_eq!(unmerged.reason.as_deref(), Some("closed-unmerged-pr"));
    assert_eq!(blockers(unmerged), vec![pair("#5", "closed_unmerged")]);
}

#[test]
fn finish_emits_through_the_ops_sink() {
    let mut w = mixed_world();
    let root = tempfile::tempdir().unwrap();
    let ((), captured) = capture(|| {
        let observer = Observer::start(MECHANISM);
        let r = w.run();
        observer.finish(root.path(), &r);
    });
    let kinds: Vec<&str> = captured.records.iter().map(TelemetryRecord::kind).collect();
    assert_eq!(kinds.first(), Some(&"pass.summary"));
    assert_eq!(kinds.iter().filter(|k| **k == "pass.verdict").count(), 5);
    assert_eq!(caller_scope::current(), None, "finish ends the scope");
}

#[test]
fn without_an_exporter_finish_builds_nothing_and_still_ends_the_scope() {
    let root = tempfile::tempdir().unwrap();
    let observer = Observer::start(MECHANISM);
    assert_eq!(caller_scope::current(), Some(MECHANISM));
    observer.finish(root.path(), &Report::default());
    assert_eq!(caller_scope::current(), None);
}

#[test]
fn a_dry_run_pass_says_so_and_applies_nothing() {
    let mut w = mixed_world();
    let r = w.run_with(Config {
        dry_run: true,
        ..cfg()
    });
    let (summary, verdicts) =
        split(Observer::start(MECHANISM).records("acme/widgets", "h", &r, HOUR));
    assert_eq!(summary.mode, PassMode::DryRun);
    assert!(verdicts
        .iter()
        .all(|v| v.mode == PassMode::DryRun && !v.applied));
    // The planned release still names the label it would remove.
    assert_eq!(verdict(&verdicts, 10).labels_removed, vec!["loom:blocked".to_string()]);
}

#[test]
fn a_refused_or_archived_pass_has_no_verdicts_and_says_why() {
    let refused = Report {
        enumerate_error: Some("rate-limit breaker is suppressing forge calls".into()),
        ..Report::default()
    };
    let (summary, verdicts) =
        split(Observer::start(MECHANISM).records("acme/widgets", "h", &refused, HOUR));
    assert_eq!(summary.outcome, PassOutcome::Refused);
    assert!(summary.refusal.as_deref().unwrap().contains("breaker"));
    assert!(verdicts.is_empty());

    let archived = Report {
        archived: true,
        ..Report::default()
    };
    let (summary, _) =
        split(Observer::start(MECHANISM).records("acme/widgets", "h", &archived, HOUR));
    assert_eq!(summary.outcome, PassOutcome::Archived);
}

#[test]
fn the_write_cap_is_reported() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.parked(11, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    let r = w.run_with(Config {
        max_writes: 1,
        ..cfg()
    });
    let (summary, verdicts) =
        split(Observer::start(MECHANISM).records("acme/widgets", "h", &r, HOUR));
    assert!(summary.write_cap_hit);
    assert_eq!(summary.skipped.get("write-cap"), Some(&1));
    assert!(verdicts
        .iter()
        .any(|v| v.reason.as_deref() == Some("write-cap")));
}

// --- verdict volume: changes plus a heartbeat ---------------------------------------

fn item(number: u64, verdict: ItemVerdict, state: &'static str) -> Item {
    Item {
        number,
        artifact: "issue",
        verdict,
        reason: None,
        detail: None,
        blockers: vec![BlockerCheck {
            reference: "#1".to_string(),
            state,
        }],
        labels_added: Vec::new(),
        labels_removed: Vec::new(),
        applied: verdict != ItemVerdict::StillBlocked,
    }
}

#[test]
fn an_unchanged_verdict_waits_for_the_heartbeat_a_changed_one_does_not() {
    let mut seen = Seen::new();
    let t0 = Instant::now();
    let held = vec![item(12, ItemVerdict::StillBlocked, blocker_state::OPEN)];
    let pick = |seen: &mut Seen, items: &[Item], at: Instant| {
        select(items, "acme/widgets", PassMode::On, true, seen, HOUR, at)
    };

    assert_eq!(pick(&mut seen, &held, t0), vec![0], "first sight is emitted");
    assert!(pick(&mut seen, &held, t0 + Duration::from_secs(300)).is_empty());
    assert_eq!(pick(&mut seen, &held, t0 + HOUR), vec![0], "the heartbeat re-emits");

    let changed = vec![item(12, ItemVerdict::StillBlocked, blocker_state::UNREAD)];
    assert_eq!(
        pick(&mut seen, &changed, t0 + HOUR + Duration::from_secs(300)),
        vec![0],
        "a changed blocker state is emitted at once"
    );
}

#[test]
fn a_verdict_that_wrote_is_always_emitted() {
    let mut seen = Seen::new();
    let t0 = Instant::now();
    let failed = vec![item(7, ItemVerdict::Failed, blocker_state::CLOSED)];
    for pass in 0..3 {
        let at = t0 + Duration::from_secs(300 * pass);
        assert_eq!(select(&failed, "a/b", PassMode::On, true, &mut seen, HOUR, at), vec![0]);
    }
    // Under dry-run nothing was written, so a repeated plan waits.
    let mut seen = Seen::new();
    let planned = vec![item(7, ItemVerdict::Released, blocker_state::CLOSED)];
    assert_eq!(select(&planned, "a/b", PassMode::DryRun, true, &mut seen, HOUR, t0), vec![0]);
    let later = t0 + Duration::from_secs(300);
    assert!(select(&planned, "a/b", PassMode::DryRun, true, &mut seen, HOUR, later).is_empty());
}

#[test]
fn a_zero_heartbeat_emits_every_pass_and_unlisted_artifacts_are_forgotten() {
    let mut seen = Seen::new();
    let t0 = Instant::now();
    let held = vec![item(12, ItemVerdict::StillBlocked, blocker_state::OPEN)];
    for pass in 0..3 {
        let at = t0 + Duration::from_secs(pass);
        let zero = Duration::ZERO;
        assert_eq!(select(&held, "a/b", PassMode::On, true, &mut seen, zero, at), vec![0]);
    }
    // #12 left the listing (released by hand): a completed pass forgets it,
    // so a re-block is first sight again.
    select(&[], "a/b", PassMode::On, true, &mut seen, HOUR, t0);
    assert!(seen.is_empty());
    // A refused pass (nothing listed) forgets nothing.
    select(&held, "a/b", PassMode::On, true, &mut seen, HOUR, t0);
    select(&[], "a/b", PassMode::On, false, &mut seen, HOUR, t0);
    assert_eq!(seen.len(), 1);
}

#[test]
fn a_second_pass_emits_only_what_changed_and_keeps_its_counts_exact() {
    let mut w = World::new();
    w.parked(12, true, &[3], &[]);
    w.with_body(13, false, "Blocked, see the thread.", &[]);
    w.parked(14, false, &[5], &[]);
    w.state(3, "OPEN", false);
    w.state(5, "CLOSED", true);
    let records =
        |r: &Report| split(Observer::start(MECHANISM).records("acme/widgets", "h", r, HOUR));

    let (first, _) = records(&w.run());
    assert_eq!((first.verdicts_emitted, first.verdicts_unchanged), (3, 0));

    let (second, verdicts) = records(&w.run());
    assert!(verdicts.is_empty(), "nothing changed: {verdicts:?}");
    assert_eq!((second.verdicts_emitted, second.verdicts_unchanged), (0, 3));
    assert_eq!(second.verdicts.get("still_blocked"), Some(&1), "counts stay per pass");
    assert_eq!(second.verdicts.get("skipped"), Some(&2));

    // #12's blocker merges: only #12's verdict is new.
    w.state(3, "MERGED", true);
    let (_, verdicts) = records(&w.run());
    let numbers: Vec<u64> = verdicts.iter().map(|v| v.number).collect();
    assert_eq!(numbers, vec![12]);
    assert_ne!(verdicts[0].verdict, "still_blocked");
}
