//! Fixtures for #12010: a predecessor that is red or conflicting now is not
//! ready, so no hold is planned behind it and an existing soft hold behind
//! it is released with the cause named.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use serde_json::json;

use super::super::ready::{demote_unlandable, ready, with_readiness, APPROVED_LABEL};
use super::super::{
    hold_action, plan_repo_with, release_comment_body, EdgeReason, HoldAction, PredecessorState,
    SequenceMarker, SEQUENCE_LABEL, SOURCE_PASS,
};
use super::*;

fn sha(n: u32) -> String {
    format!("{n:040x}")
}

fn pr(number: u32, created: &str, labels: &[&str]) -> SequencePr {
    SequencePr {
        number,
        created_at: created.to_string(),
        updated_at: created.to_string(),
        head_sha: Some(sha(number)),
        head_ref: format!("feature/issue-{number}"),
        base_ref: "main".to_string(),
        draft: false,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
    }
}

fn marker(follower: u32, after: u32, source: Option<&str>, plan: &str) -> SequenceMarker {
    SequenceMarker {
        after,
        pred_head: sha(after),
        follower_head: sha(follower),
        plan: plan.into(),
        source: source.map(str::to_string),
    }
}

fn soft(follower: u32, after: u32) -> SequenceMarker {
    marker(follower, after, Some(SOURCE_PASS), "seq-981b04ce")
}

/// Phase 1's decision for `follower`'s hold against this tick's `listing`.
fn decide(m: &SequenceMarker, follower: &SequencePr, listing: &[SequencePr]) -> HoldAction {
    let p = PredecessorState {
        open: true,
        merged: false,
        head_sha: Some(m.pred_head.clone()),
        updated_at: Some(Utc::now().to_rfc3339()),
    };
    let fh = follower.head_sha.as_deref();
    let base = hold_action(m, Some(&p), fh, follower.has(APPROVED_LABEL), 72.0);
    let head = listing.iter().find(|h| h.number == m.after);
    with_readiness(base, m, Some(&p), follower, head)
}

fn edges(prs: &[SequencePr]) -> Vec<(u32, u32, EdgeReason)> {
    let files = prs
        .iter()
        .map(|p| (p.number, BTreeSet::from([".github/workflows/ci.yml".to_string()])))
        .collect();
    plan_repo_with(prs, &files, &BTreeMap::new(), &BTreeSet::new())
        .iter()
        .flat_map(|g| g.edges.iter().map(|e| (e.follower, e.after, e.reason)))
        .collect()
}

fn red(names: &[&str]) -> Landability {
    Landability {
        ci: Ci::Red(names.iter().map(|n| (*n).to_string()).collect()),
        mergeable: Mergeable::Unknown,
    }
}

fn conflicting() -> Landability {
    Landability {
        ci: Ci::Green,
        mergeable: Mergeable::Conflicting,
    }
}

const RED_NAMES: [&str; 2] = ["CI Result", "Structural Checks"];

/// The 2026-10-15 shape: #11729 approved but red, #11976 approved and green,
/// both editing ci.yml, plus a third open PR past the trigger.
fn incident(probe_11729: Landability) -> Vec<SequencePr> {
    let mut head = pr(11729, "2026-10-13T00:00:00Z", &[APPROVED_LABEL]);
    head.head_sha = Some(format!("f23b18356{}", "0".repeat(31)));
    let listing = [
        head,
        pr(11976, "2026-10-14T00:00:00Z", &[APPROVED_LABEL]),
        pr(1, "2026-09-01T00:00:00Z", &[]),
    ];
    tag_unlandable(&listing, |p| {
        if p.number == 11729 {
            probe_11729.clone()
        } else {
            Landability {
                ci: Ci::Green,
                mergeable: Mergeable::Mergeable,
            }
        }
    })
}

// --- tag_unlandable ----------------------------------------------------------

#[test]
fn only_a_definite_red_or_conflict_tags_a_row() {
    let tagged = incident(red(&RED_NAMES));
    assert!(tagged[0].has(CI_RED_LABEL) && !ready(&tagged[0]));
    assert!(tagged[0].has(&format!("{RED_CHECK_PREFIX}CI Result")));
    assert!(ready(&tagged[1]), "the green PR is untouched");
    let tagged = incident(conflicting());
    assert!(tagged[0].has(CONFLICTING_LABEL) && !ready(&tagged[0]));
    for ci in [Ci::Pending, Ci::Unknown, Ci::Green] {
        for mergeable in [Mergeable::Unknown, Mergeable::Mergeable] {
            let tagged = incident(Landability {
                ci: ci.clone(),
                mergeable,
            });
            assert!(ready(&tagged[0]), "{ci:?} / {mergeable:?}");
        }
    }
}

#[test]
fn only_ready_pinnable_rows_are_probed_and_small_repos_never() {
    let calls = Cell::new(0);
    let count = |_: &SequencePr| {
        calls.set(calls.get() + 1);
        red(&["x"])
    };
    let mut headless = pr(4, "2026-10-01T00:00:00Z", &[APPROVED_LABEL]);
    headless.head_sha = None;
    let listing = [
        pr(1, "2026-10-01T00:00:00Z", &[]),
        pr(2, "2026-10-01T00:00:00Z", &[APPROVED_LABEL, "loom:changes-requested"]),
        pr(3, "2026-10-01T00:00:00Z", &[APPROVED_LABEL]),
        headless,
    ];
    let tagged = tag_unlandable(&listing, count);
    assert_eq!(calls.get(), 1, "only #3 is ready and pinnable");
    assert!(tagged[2].has(CI_RED_LABEL));
    assert!(!tagged[0].has(CI_RED_LABEL) && !tagged[3].has(CI_RED_LABEL));
    // At or under the trigger: no probe at all.
    let _ = tag_unlandable(&listing[2..], count);
    assert_eq!(calls.get(), 1);
}

#[test]
fn a_suppressed_breaker_makes_no_probe_call() {
    let calls = Cell::new(0);
    let mut probe = guarded(
        || true,
        |_| {
            calls.set(calls.get() + 1);
            red(&["x"])
        },
    );
    assert_eq!(probe(&pr(1, "t", &[APPROVED_LABEL])), Landability::UNKNOWN);
    assert_eq!(calls.get(), 0);
    let mut live = guarded(|| false, |_| red(&["x"]));
    assert_eq!(live(&pr(1, "t", &[APPROVED_LABEL])), red(&["x"]));
}

#[test]
fn check_runs_classify_red_pending_green_and_unknown() {
    let run = |name: &str, status: &str, conclusion: Option<&str>| json!({"name": name, "status": status, "conclusion": conclusion});
    let doc = |rows: Vec<serde_json::Value>| json!({"total_count": rows.len(), "check_runs": rows});
    let mixed = doc(vec![
        run("Structural Checks", "completed", Some("failure")),
        run("CI Result", "completed", Some("timed_out")),
        run("Lint", "in_progress", None),
        run("Shell", "completed", Some("success")),
    ]);
    assert_eq!(ci_of(&mixed), Ci::Red(vec!["CI Result".into(), "Structural Checks".into()]));
    // Superseded twins and concurrency cancels are not red (#11097).
    let cancelled = doc(vec![run("Rust", "completed", Some("cancelled"))]);
    assert_eq!(ci_of(&cancelled), Ci::Green);
    let pending = doc(vec![
        run("Rust", "queued", None),
        run("Lint", "completed", Some("success")),
    ]);
    assert_eq!(ci_of(&pending), Ci::Pending);
    assert_eq!(ci_of(&doc(vec![])), Ci::Unknown);
    assert_eq!(ci_of(&json!({})), Ci::Unknown);
}

// --- AC1: no hold is planned behind a red or conflicting predecessor --------

#[test]
fn no_edge_is_planned_behind_a_red_or_conflicting_predecessor() {
    let control = incident(Landability::UNKNOWN);
    assert!(edges(&control).contains(&(11976, 11729, EdgeReason::SharedFiles)), "control");
    for probe in [red(&RED_NAMES), conflicting()] {
        let planned = edges(&incident(probe.clone()));
        assert!(planned.iter().all(|(_, a, _)| *a != 11729), "{probe:?}: {planned:?}");
    }
}

// --- AC2: an existing hold is released, naming the cause ---------------------

#[test]
fn an_existing_hold_behind_a_predecessor_that_turned_red_is_released_with_its_cause() {
    let follower = pr(11976, "2026-10-14T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    let m = soft(11976, 11729);
    let listing = incident(red(&RED_NAMES));
    assert_eq!(decide(&m, &follower, &listing), HoldAction::ReleaseNotReady);
    let head = listing.iter().find(|p| p.number == 11729);
    let body = release_comment_body(&m, HoldAction::ReleaseNotReady, head);
    assert!(body.contains("#11729 is not landable now"), "{body}");
    assert!(body.contains("red checks on its head `f23b18356`"), "{body}");
    assert!(body.contains("(CI Result, Structural Checks)"), "{body}");

    let listing = incident(conflicting());
    assert_eq!(decide(&m, &follower, &listing), HoldAction::ReleaseNotReady);
    let head = listing.iter().find(|p| p.number == 11729);
    let body = release_comment_body(&m, HoldAction::ReleaseNotReady, head);
    assert!(body.contains("#11729 is not landable now: it conflicts with its base `main`"));
}

// --- AC3: pending or unknown changes nothing ---------------------------------

#[test]
fn pending_or_unknown_state_keeps_the_hold_and_the_plan() {
    let follower = pr(11976, "2026-10-14T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    for ci in [Ci::Pending, Ci::Unknown] {
        let listing = incident(Landability {
            ci: ci.clone(),
            mergeable: Mergeable::Unknown,
        });
        assert_eq!(decide(&soft(11976, 11729), &follower, &listing), HoldAction::HoldSoft);
        assert!(edges(&listing).contains(&(11976, 11729, EdgeReason::SharedFiles)), "{ci:?}");
    }
}

// --- AC4: hard holds, stacked bases and reservations are unaffected ----------

#[test]
fn hard_stacked_and_reservation_holds_survive_a_red_predecessor() {
    let follower = pr(11976, "2026-10-14T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    let listing = incident(red(&RED_NAMES));
    let hard = marker(11976, 11729, None, "manual");
    assert_eq!(decide(&hard, &follower, &listing), HoldAction::HoldHard);
    let cons = marker(11976, 11729, Some(SOURCE_PASS), "cons-ab12cd34");
    assert_eq!(decide(&cons, &follower, &listing), HoldAction::HoldSoft);
    let mut stacked = follower.clone();
    stacked.base_ref = listing[0].head_ref.clone();
    assert_eq!(decide(&soft(11976, 11729), &stacked, &listing), HoldAction::HoldSoft);
}

// --- AC5: transitivity -------------------------------------------------------

#[test]
fn a_ready_pr_held_behind_a_red_predecessor_is_itself_not_landable() {
    // #2 (approved) is sequenced behind #1, which is red now; #3 (approved)
    // overlaps #2 and must not queue behind it.
    let listing = [
        pr(1, "2026-10-01T00:00:00Z", &[APPROVED_LABEL]),
        pr(2, "2026-10-02T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]),
        pr(3, "2026-10-03T00:00:00Z", &[APPROVED_LABEL]),
    ];
    let tagged = tag_unlandable(&listing, |p| {
        if p.number == 1 {
            red(&["x"])
        } else {
            Landability::UNKNOWN
        }
    });
    let markers = BTreeMap::from([(2, soft(2, 1))]);
    let demoted = demote_unlandable(&tagged, &markers);
    assert!(!ready(&demoted[1]), "#2 sits behind a red chain");
    assert_eq!(decide(&soft(3, 2), &listing[2], &demoted), HoldAction::ReleaseNotReady);
}

// --- AC6: the tags never reach the forge -------------------------------------

#[test]
fn the_only_label_the_pass_writes_is_the_hold_label() {
    let src = include_str!("merge_sequence.rs");
    let writes: Vec<&str> = src
        .lines()
        .filter(|l| l.contains("\"--add-label\","))
        .collect();
    assert!(!writes.is_empty());
    assert!(writes.iter().all(|l| l.contains("SEQUENCE_LABEL")), "{writes:?}");
    for tag in [CI_RED_LABEL, CONFLICTING_LABEL, RED_CHECK_PREFIX] {
        assert!(tag.starts_with("loom-internal:"), "{tag}");
    }
}
