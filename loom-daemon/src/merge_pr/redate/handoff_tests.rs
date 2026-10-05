//! Tests for the #10388 Doctor handoff on re-date budget exhaustion: the pure
//! pieces, and the whole remedy against the stub `gh` from `redate::tests`.

use super::*;
use crate::merge_pr::redate::budget::attempt_marker;
use crate::merge_pr::redate::tests::{now, split_stub_calls, tmp_dir, write_stub_gh};
use crate::merge_pr::redate::{
    hold_marker, redate_marker, remedy_full, BudgetConfig, RemedyOutcome, HOLD_LABEL,
};
use serde_json::json;
use std::fs;

const FLEET: &str = "loom-fleet-dispatch[bot]";

// --- Pure ----------------------------------------------------------------

#[test]
fn config_resolves_env_over_config_over_default_and_clamps() {
    assert_eq!(HandoffConfig::resolve(None, &json!({})), HandoffConfig { max: DEFAULT_MAX });
    let cfg = json!({"champion": {"redateDoctorHandoffs": 1}});
    assert_eq!(HandoffConfig::resolve(None, &cfg).max, 1);
    assert_eq!(HandoffConfig::resolve(Some("0"), &cfg).max, 0, "0 = straight to operator");
    assert_eq!(HandoffConfig::resolve(Some("junk"), &cfg).max, 1, "invalid env falls through");
    assert_eq!(HandoffConfig::resolve(Some("500"), &cfg).max, MAX_MAX);
}

#[test]
fn state_counts_handoffs_across_heads() {
    let bodies = format!(
        "{}\nprose\n{}\n{}",
        marker("aaa", 1),
        marker("bbb", 2),
        "<!-- loom:stale-check-doctor-handoff head=ccc n=x -->"
    );
    assert_eq!(
        state(&bodies, "bbb"),
        HandoffState {
            done: 2,
            this_head: Some(2)
        }
    );
    assert_eq!(
        state(&bodies, "zzz"),
        HandoffState {
            done: 2,
            this_head: None
        }
    );
    assert_eq!(
        state("", "zzz"),
        HandoffState {
            done: 0,
            this_head: None
        }
    );
}

#[test]
fn decide_hands_off_until_the_bound_then_escalates() {
    let cfg = HandoffConfig { max: 2 };
    let st = |done, this_head| HandoffState { done, this_head };
    assert_eq!(decide(&st(0, None), &cfg), HandoffDecision::HandOff { n: 1 });
    assert_eq!(decide(&st(1, None), &cfg), HandoffDecision::HandOff { n: 2 });
    assert_eq!(decide(&st(2, None), &cfg), HandoffDecision::Escalate { done: 2 });
    // The same head again: re-assert, never double-count or escalate early.
    assert_eq!(decide(&st(1, Some(1)), &cfg), HandoffDecision::Reassert { n: 1 });
    // Handoffs disabled: pre-#10388 behaviour.
    assert_eq!(
        decide(&st(0, None), &HandoffConfig { max: 0 }),
        HandoffDecision::Escalate { done: 0 }
    );
}

#[test]
fn the_notice_routes_to_doctor_and_never_names_the_hold_as_applied() {
    let c = comment_body("42", "abc0000", 1, &HandoffConfig { max: 2 }, 3, 3);
    assert!(c.starts_with(&marker("abc0000", 1)), "{c}");
    assert!(c.contains("1 of 2") && c.contains("3 of 3"), "{c}");
    assert!(c.contains("Rebase") && c.contains(DOCTOR_LABEL), "{c}");
    assert!(c.contains("loom:review-requested"), "{c}");
}

// --- The remedy end to end (stub gh) --------------------------------------

const CFG3: BudgetConfig = BudgetConfig {
    budget: 3,
    backoff_secs: 600,
};

fn exhausted_body(head: &str) -> String {
    format!("{}\n{}\nprose", redate_marker(head), attempt_marker(head, 3))
}

fn stub(name: &str, bodies: &[&str]) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = tmp_dir(name);
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "");
    let listing: Vec<_> = bodies
        .iter()
        .map(|b| {
            json!({
                "user": {"login": FLEET, "type": "Bot"},
                "author_association": "NONE",
                "body": b,
                "created_at": "2026-09-30T08:00:00Z",
            })
        })
        .collect();
    fs::write(dir.join("comments.json"), json!(listing).to_string()).expect("listing");
    (dir, gh)
}

fn run(gh: &std::path::Path, max: u32) -> RemedyOutcome {
    remedy_full(
        gh.to_str().unwrap(),
        "o/r",
        "feature/x",
        "abc0000",
        "42",
        CFG3,
        HandoffConfig { max },
        now(),
        || None,
    )
}

#[test]
fn an_exhausted_budget_hands_off_to_doctor_before_the_operator() {
    let body = exhausted_body("abc0000");
    let (dir, gh) = stub("handoff-first", &[&body]);
    assert_eq!(
        run(&gh, 2),
        RemedyOutcome::HandedOff {
            notice_posted: true,
            n: 1,
            max: 2,
            spent: 3,
            budget: 3
        }
    );
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    let calls = split_stub_calls(&argv);
    assert!(!argv.contains("-X PATCH"), "no re-date push: {argv}");
    assert!(argv.contains(&marker("abc0000", 1)), "handoff notice posted: {argv}");
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("repos/o/r/issues/42/labels") && c.contains(HOLD_LABEL)),
        "never loom:operator with the handoff: {argv}"
    );
    assert!(!argv.contains(&hold_marker("abc0000")), "no hold notice: {argv}");
    let label_add = calls
        .iter()
        .position(|c| c.starts_with("repos/o/r/issues/42/labels") && c.contains(DOCTOR_LABEL))
        .expect("loom:changes-requested applied");
    let notice = calls
        .iter()
        .position(|c| c.contains(&marker("abc0000", 1)))
        .expect("notice");
    assert!(
        notice < label_add,
        "marker first, so a label failure is re-asserted not re-counted"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("-X DELETE") && c.contains("issues/42/labels/loom:pr")),
        "loom:pr withdrawn: {argv}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_second_exhaustion_on_a_new_head_is_handoff_two() {
    let prior = marker("oldhead", 1);
    let body = exhausted_body("abc0000");
    let (dir, gh) = stub("handoff-second", &[&prior, &body]);
    assert!(matches!(
        run(&gh, 2),
        RemedyOutcome::HandedOff {
            n: 2,
            notice_posted: true,
            ..
        }
    ));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn handoffs_spent_escalates_to_the_operator() {
    let (h1, h2) = (marker("head1", 1), marker("head2", 2));
    let body = exhausted_body("abc0000");
    let (dir, gh) = stub("handoff-spent", &[&h1, &h2, &body]);
    assert_eq!(
        run(&gh, 2),
        RemedyOutcome::Escalated {
            notice_posted: true,
            spent: 3,
            budget: 3
        }
    );
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    assert!(argv.contains(HOLD_LABEL) && argv.contains(&hold_marker("abc0000")), "{argv}");
    assert!(argv.contains("2 Doctor rebase handoff(s)"), "the hold says why: {argv}");
    assert!(!argv.contains(&format!("\"{DOCTOR_LABEL}\"")), "not both labels: {argv}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_repeat_on_an_already_handed_off_head_reasserts_without_reposting() {
    let mine = marker("abc0000", 1);
    let body = exhausted_body("abc0000");
    let (dir, gh) = stub("handoff-reassert", &[&body, &mine]);
    assert!(matches!(
        run(&gh, 2),
        RemedyOutcome::HandedOff {
            notice_posted: false,
            n: 1,
            ..
        }
    ));
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    assert!(
        !argv.contains("STDIN:<!-- loom:stale-check-doctor-handoff"),
        "no duplicate notice: {argv}"
    );
    assert!(argv.contains(DOCTOR_LABEL), "label re-asserted: {argv}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_untrusted_handoff_marker_is_not_counted() {
    let body = exhausted_body("abc0000");
    let dir = tmp_dir("handoff-untrusted");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "");
    let listing = json!([
        {"user": {"login": FLEET, "type": "Bot"}, "author_association": "NONE",
         "body": body, "created_at": "2026-09-30T08:00:00Z"},
        {"user": {"login": "drive-by", "type": "User"}, "author_association": "NONE",
         "body": format!("{}\n{}", marker("x", 1), marker("y", 2)),
         "created_at": "2026-09-30T09:00:00Z"},
    ]);
    fs::write(dir.join("comments.json"), listing.to_string()).expect("listing");
    assert!(matches!(run(&gh, 2), RemedyOutcome::HandedOff { n: 1, .. }));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_label_write_failure_is_reported_not_swallowed() {
    let dir = tmp_dir("handoff-label-fail");
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "label");
    let listing = json!([{"user": {"login": FLEET, "type": "Bot"}, "author_association": "NONE",
        "body": exhausted_body("abc0000"), "created_at": "2026-09-30T08:00:00Z"}]);
    fs::write(dir.join("comments.json"), listing.to_string()).expect("listing");
    assert!(matches!(run(&gh, 2), RemedyOutcome::Failed(ref w) if w.contains(DOCTOR_LABEL)));
    let _ = fs::remove_dir_all(&dir);
}
