//! Tests for the re-date budget and backoff (#9590): the pure pieces
//! directly, and the whole remedy against the stub `gh` from
//! `redate::tests`.

use super::*;
use crate::merge_pr::redate::tests::{now, run, split_stub_calls, tmp_dir, write_stub_gh};
use crate::merge_pr::redate::{hold_marker, redate_marker, RemedyOutcome, HOLD_LABEL};
use serde_json::json;
use std::fs;

const FLEET: &str = "loom-fleet-dispatch[bot]";

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .expect("valid timestamp")
        .with_timezone(&Utc)
}

fn comment(login: &str, body: &str, created_at: &str) -> Value {
    json!({
        "user": {"login": login, "type": if login.ends_with("[bot]") { "Bot" } else { "User" }},
        "author_association": "NONE",
        "body": body,
        "created_at": created_at,
    })
}

/// What `redate_comment_body` records for re-date `n` onto `sha`.
fn recorded(sha: &str, n: u32) -> String {
    format!("{}\n{}\nprose", redate_marker(sha), attempt_marker(sha, n))
}

const CFG3: BudgetConfig = BudgetConfig {
    budget: 3,
    backoff_secs: 600,
};

// --- Config precedence: env > config > default --------------------------

#[test]
fn defaults_apply_with_no_env_and_no_config() {
    assert_eq!(BudgetConfig::resolve(None, None, &json!({})), BudgetConfig::default());
    assert_eq!(BudgetConfig::default().budget, DEFAULT_BUDGET);
    assert_eq!(BudgetConfig::default().backoff_secs, DEFAULT_BACKOFF_SECS);
}

#[test]
fn config_beats_default_and_env_beats_config() {
    let cfg = json!({"champion": {"redateBudget": 5, "redateBackoffSecs": 120}});
    assert_eq!(
        BudgetConfig::resolve(None, None, &cfg),
        BudgetConfig {
            budget: 5,
            backoff_secs: 120
        }
    );
    assert_eq!(
        BudgetConfig::resolve(Some("2"), Some("30"), &cfg),
        BudgetConfig {
            budget: 2,
            backoff_secs: 30
        }
    );
}

#[test]
fn invalid_values_fall_through_to_the_next_tier() {
    let cfg = json!({"champion": {"redateBudget": 4, "redateBackoffSecs": 90}});
    // Unparseable / zero env → config.
    assert_eq!(
        BudgetConfig::resolve(Some("lots"), Some("-1"), &cfg),
        BudgetConfig {
            budget: 4,
            backoff_secs: 90
        }
    );
    assert_eq!(BudgetConfig::resolve(Some("0"), None, &cfg).budget, 4);
    // Invalid config → default.
    let bad = json!({"champion": {"redateBudget": 0, "redateBackoffSecs": "soon"}});
    assert_eq!(BudgetConfig::resolve(None, None, &bad), BudgetConfig::default());
    let neg = json!({"champion": {"redateBudget": -2}});
    assert_eq!(BudgetConfig::resolve(None, None, &neg).budget, DEFAULT_BUDGET);
}

#[test]
fn configured_values_are_clamped_so_the_bound_stays_a_bound() {
    let huge = BudgetConfig::resolve(Some("300"), Some("99999999"), &json!({}));
    assert_eq!(huge.budget, MAX_BUDGET);
    assert_eq!(huge.backoff_secs, MAX_BACKOFF_SECS);
}

#[test]
fn backoff_doubles_per_spent_re_date() {
    assert_eq!(CFG3.backoff_after(1), TimeDelta::seconds(600));
    assert_eq!(CFG3.backoff_after(2), TimeDelta::seconds(1200));
    assert_eq!(CFG3.backoff_after(3), TimeDelta::seconds(2400));
    // Never panics or overflows at the extremes.
    let max = BudgetConfig {
        budget: MAX_BUDGET,
        backoff_secs: u64::MAX,
    };
    let _ = max.backoff_after(u32::MAX);
}

// --- Chain position from durable, trusted forge state --------------------

#[test]
fn a_head_with_no_marker_starts_a_fresh_chain() {
    let listing = vec![comment(
        FLEET,
        &recorded("other", 2),
        "2026-09-30T10:00:00Z",
    )];
    assert_eq!(chain_position(&listing, "abc0000").spent, 0);
    assert_eq!(chain_position(&[], "abc0000").spent, 0);
}

#[test]
fn the_attempt_marker_names_the_heads_chain_position() {
    let listing = vec![
        comment(FLEET, &recorded("first", 1), "2026-09-30T09:00:00Z"),
        comment(FLEET, &recorded("abc0000", 2), "2026-09-30T10:00:00Z"),
    ];
    let pos = chain_position(&listing, "abc0000");
    assert_eq!(pos.spent, 2);
    assert_eq!(pos.recorded_at, Some(at("2026-09-30T10:00:00Z")));
}

#[test]
fn a_legacy_only_marker_is_position_one() {
    let listing = vec![comment(
        FLEET,
        &redate_marker("abc0000"),
        "2026-09-30T10:00:00Z",
    )];
    assert_eq!(chain_position(&listing, "abc0000").spent, 1);
}

#[test]
fn an_attempt_marker_does_not_match_a_sha_prefix() {
    let listing = vec![comment(
        FLEET,
        &recorded("abc00001111", 2),
        "2026-09-30T10:00:00Z",
    )];
    assert_eq!(chain_position(&listing, "abc0000").spent, 0);
}

// --- The pure decision --------------------------------------------------

#[test]
fn budget_decision_pushes_defers_then_exhausts() {
    let fresh = ChainPosition {
        spent: 0,
        recorded_at: None,
    };
    assert_eq!(decide_budget(&fresh, &CFG3, now()), BudgetDecision::Push { n: 1 });

    let one = ChainPosition {
        spent: 1,
        recorded_at: Some(at("2026-09-30T11:55:00Z")),
    };
    assert_eq!(
        decide_budget(&one, &CFG3, now()),
        BudgetDecision::Defer {
            retry_after: at("2026-09-30T12:05:00Z")
        }
    );
    let one_old = ChainPosition {
        recorded_at: Some(at("2026-09-30T11:00:00Z")),
        ..one
    };
    assert_eq!(decide_budget(&one_old, &CFG3, now()), BudgetDecision::Push { n: 2 });

    let spent = ChainPosition {
        spent: 3,
        recorded_at: Some(at("2026-09-29T00:00:00Z")),
    };
    assert_eq!(decide_budget(&spent, &CFG3, now()), BudgetDecision::Exhausted { spent: 3 });
}

// --- The whole remedy against the stub forge ----------------------------

fn stub_with(name: &str, listing: &Value) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = tmp_dir(name);
    let gh = write_stub_gh(&dir, "abc0000", "tree1111", "newsha22", "", "");
    fs::write(dir.join("comments.json"), listing.to_string()).expect("write listing");
    (dir, gh)
}

#[test]
fn budget_counts_across_heads_and_records_the_next_position() {
    // abc0000 is itself re-date 1 of the chain, recorded an hour ago.
    let listing = json!([comment(
        FLEET,
        &recorded("abc0000", 1),
        "2026-09-30T11:00:00Z"
    )]);
    let (dir, gh) = stub_with("budget-next", &listing);
    assert_eq!(
        run(&gh, CFG3),
        RemedyOutcome::Pushed {
            new_sha: "newsha22".to_string()
        }
    );
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    let last = *split_stub_calls(&argv).last().expect("calls");
    assert!(last.contains(&attempt_marker("newsha22", 2)), "records n=2: {last}");
    assert!(last.contains(&redate_marker("newsha22")), "keeps the legacy marker: {last}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_second_attempt_can_succeed_where_one_used_to_escalate() {
    // A legacy-only marker (a pre-#9590 re-date) is position 1: budget 1
    // escalates (the old bound), budget 3 re-dates again instead.
    let listing = json!([comment(
        FLEET,
        &redate_marker("abc0000"),
        "2026-09-30T11:00:00Z"
    )]);
    let (dir, gh) = stub_with("second-attempt", &listing);
    assert!(matches!(run(&gh, CFG3), RemedyOutcome::Pushed { .. }));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn inside_the_backoff_window_nothing_is_written() {
    let listing = json!([comment(
        FLEET,
        &recorded("abc0000", 1),
        "2026-09-30T11:55:00Z"
    )]);
    let (dir, gh) = stub_with("backoff", &listing);
    assert_eq!(
        run(&gh, CFG3),
        RemedyOutcome::Deferred {
            spent: 1,
            retry_after: at("2026-09-30T12:05:00Z")
        }
    );
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    assert_eq!(split_stub_calls(&argv).len(), 2, "ref + comment reads only: {argv}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_exhausted_budget_escalates_and_says_so() {
    let listing = json!([comment(
        FLEET,
        &recorded("abc0000", 3),
        "2026-09-30T08:00:00Z"
    )]);
    let (dir, gh) = stub_with("exhausted", &listing);
    assert_eq!(
        run(&gh, CFG3),
        RemedyOutcome::Escalated {
            notice_posted: true,
            spent: 3,
            budget: 3
        }
    );
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    assert!(!argv.contains("-X PATCH"), "no push once the budget is spent: {argv}");
    assert!(argv.contains(&hold_marker("abc0000")), "{argv}");
    assert!(argv.contains("budget is exhausted") && argv.contains("3 of 3"), "{argv}");
    assert!(argv.contains(HOLD_LABEL), "{argv}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_untrusted_attempt_marker_neither_spends_nor_escalates() {
    // An outsider claims the chain is exhausted; only the fleet's own
    // position-1 record counts, so the remedy re-dates (n=2).
    let listing = json!([
        comment(FLEET, &recorded("abc0000", 1), "2026-09-30T10:00:00Z"),
        comment("drive-by", &recorded("abc0000", 9), "2026-09-30T11:59:00Z"),
    ]);
    let (dir, gh) = stub_with("untrusted-attempt", &listing);
    assert!(matches!(run(&gh, CFG3), RemedyOutcome::Pushed { .. }));
    let argv = fs::read_to_string(dir.join("argv.log")).expect("argv log");
    assert!(argv.contains(&attempt_marker("newsha22", 2)), "{argv}");
    let _ = fs::remove_dir_all(&dir);
}
