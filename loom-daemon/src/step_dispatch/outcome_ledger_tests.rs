//! #11900: the step-outcome ledger. One test per outcome class's effect,
//! plus the outcome line, the post-state classifier and the repeat cap.

use super::*;
use chrono::TimeZone;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn mins(m: i64) -> chrono::Duration {
    chrono::Duration::minutes(m)
}

fn out(class: OutcomeClass, reason: &str) -> StepOutcome {
    StepOutcome::new(class, reason)
}

const SLOW: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------- the line

#[test]
fn the_last_outcome_line_for_this_role_and_target_wins() {
    let output = "\
LOOM_STEP_OUTCOME role=judge target=5 class=noop reason=ci_pending
noise
LOOM_STEP_OUTCOME role=judge target=6 class=verdict reason=approved
LOOM_STEP_OUTCOME role=builder target=5 class=verdict reason=pr_opened
  LOOM_STEP_OUTCOME role=judge target=5 class=refused reason=claimed
";
    assert_eq!(
        parse_outcome_line(output, StepRole::Judge, 5),
        Some(out(OutcomeClass::Refused, "claimed"))
    );
    assert_eq!(
        parse_outcome_line(output, StepRole::Builder, 5),
        Some(out(OutcomeClass::Verdict, "pr_opened"))
    );
    assert_eq!(parse_outcome_line(output, StepRole::Curator, 5), None);
    assert_eq!(
        parse_outcome_line(
            "LOOM_STEP_OUTCOME role=curator target=9 class=noop",
            StepRole::Curator,
            9
        ),
        Some(out(OutcomeClass::Noop, UNCLASSIFIED)),
        "a line without a reason is unclassified"
    );
    assert_eq!(
        parse_outcome_line(
            "LOOM_STEP_OUTCOME role=curator target=9 class=done reason=x",
            StepRole::Curator,
            9
        ),
        None,
        "an unknown class is not an outcome"
    );
}

#[test]
fn champion_lines_are_read_per_mode_and_folded_onto_the_classes() {
    let pr = "CHAMPION-STEP mode=pr item=30 outcome=head-moved";
    assert_eq!(
        parse_outcome_line(pr, StepRole::ChampionMerge, 30),
        Some(out(OutcomeClass::Refused, "head_moved"))
    );
    assert_eq!(parse_outcome_line(pr, StepRole::ChampionPromote, 30), None, "wrong mode");
    assert_eq!(parse_outcome_line(pr, StepRole::ChampionMerge, 31), None, "wrong item");
    let issue = "CHAMPION-STEP mode=issue item=8 outcome=blocked";
    assert_eq!(
        parse_outcome_line(issue, StepRole::ChampionPromote, 8),
        Some(out(OutcomeClass::Noop, "blocked"))
    );

    let table = [
        (true, "merged", OutcomeClass::Verdict),
        (true, "rejected", OutcomeClass::Verdict),
        (true, "held", OutcomeClass::Noop),
        (true, "requeued", OutcomeClass::Noop),
        (true, "head-moved", OutcomeClass::Refused),
        (true, "not-eligible", OutcomeClass::Refused),
        (true, "failed", OutcomeClass::Failed),
        (false, "promoted", OutcomeClass::Verdict),
        (false, "revise", OutcomeClass::Verdict),
        (false, "operator", OutcomeClass::Verdict),
        (false, "closed", OutcomeClass::Verdict),
        (false, "blocked", OutcomeClass::Noop),
        (false, "not-eligible", OutcomeClass::Refused),
    ];
    for (on_pr, word, class) in table {
        assert_eq!(champion_outcome(on_pr, word).unwrap().class, class, "{word}");
    }
    assert_eq!(champion_outcome(false, "merged"), None, "not an --issue outcome");
    assert_eq!(champion_outcome(true, "deferred"), None, "never invented");
}

// ---------------------------------------------------------- the post-state

#[test]
fn the_forge_post_state_decides_and_the_line_only_chooses() {
    let verdict = out(OutcomeClass::Verdict, "approved");
    let noop = out(OutcomeClass::Noop, "ci_pending");
    let moved = PostState {
        moved: true,
        ..PostState::default()
    };
    let held = PostState {
        claim_held: true,
        ..PostState::default()
    };
    let closed = PostState {
        closed: true,
        ..PostState::default()
    };
    let unchanged = PostState::default();

    assert_eq!(classify(Some(&verdict), moved), verdict);
    assert_eq!(
        classify(Some(&verdict), unchanged),
        out(OutcomeClass::Failed, UNCLASSIFIED),
        "a verdict the forge does not show is failed"
    );
    assert_eq!(classify(Some(&noop), unchanged), noop);
    assert_eq!(classify(None, unchanged), out(OutcomeClass::Failed, UNCLASSIFIED));
    assert_eq!(classify(Some(&verdict), held).class, OutcomeClass::Failed, "claim left held");
    assert_eq!(classify(None, closed), out(OutcomeClass::Verdict, "closed"));
    assert_eq!(classify(None, moved), out(OutcomeClass::Verdict, UNCLASSIFIED));
}

// -------------------------------------------------------- class → effect

#[test]
fn each_class_maps_to_one_effect() {
    let cfg = LedgerConfig::default();
    let fast = Duration::from_secs(30);
    let cases = [
        (StepRole::Builder, out(OutcomeClass::Verdict, "closed"), SLOW, Effect::Landed),
        (StepRole::Curator, out(OutcomeClass::Verdict, "closed"), SLOW, Effect::Landed),
        (StepRole::Judge, out(OutcomeClass::Verdict, "closed"), SLOW, Effect::Advance),
        (StepRole::Judge, out(OutcomeClass::Verdict, "approved"), SLOW, Effect::Advance),
        (
            StepRole::Builder,
            out(OutcomeClass::Noop, "no_changes_needed"),
            SLOW,
            Effect::Cooldown(cfg.noop_cooldown),
        ),
        (
            StepRole::Curator,
            out(OutcomeClass::Refused, "pool_gate"),
            SLOW,
            Effect::Cooldown(cfg.refusal_cooldown),
        ),
        (
            StepRole::Judge,
            out(OutcomeClass::Refused, "claimed"),
            SLOW,
            Effect::Cooldown(cfg.refusal_cooldown),
        ),
        (StepRole::Judge, out(OutcomeClass::Refused, "claim_lost"), SLOW, Effect::Redraw),
        (
            StepRole::Builder,
            out(OutcomeClass::Refused, "open_pr"),
            SLOW,
            Effect::Reclassify,
        ),
        (
            StepRole::Builder,
            out(OutcomeClass::Failed, "crashed"),
            fast,
            Effect::FastFailLadder,
        ),
        (StepRole::Builder, out(OutcomeClass::Failed, "crashed"), SLOW, Effect::Failed),
        (
            StepRole::ChampionMerge,
            out(OutcomeClass::Noop, "held"),
            SLOW,
            Effect::ForgeLadder,
        ),
        (
            StepRole::ChampionMerge,
            out(OutcomeClass::Verdict, "held"),
            SLOW,
            Effect::ForgeLadder,
        ),
        (
            StepRole::ChampionPromote,
            out(OutcomeClass::Noop, "blocked"),
            SLOW,
            Effect::ForgeLadder,
        ),
        (
            StepRole::ChampionMerge,
            out(OutcomeClass::Noop, "requeued"),
            SLOW,
            Effect::Cooldown(cfg.noop_cooldown),
        ),
        (
            StepRole::ChampionMerge,
            out(OutcomeClass::Verdict, "merged"),
            SLOW,
            Effect::Advance,
        ),
    ];
    for (step, outcome, ran_for, want) in cases {
        assert_eq!(effect_of(step, &outcome, ran_for, &cfg), want, "{step} {outcome:?}");
    }
}

// ------------------------------------------------------------- the ledger

#[test]
fn landed_reclassifies_and_blocks_the_builder_step_for_24h() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    l.record(7, StepRole::Curator, &out(OutcomeClass::Noop, "nothing_to_add"), SLOW, now);
    assert!(l.cooling_until(7, StepRole::Curator, now).is_some());

    let rec =
        l.record(7, StepRole::Curator, &out(OutcomeClass::Verdict, "closed"), SLOW, now + mins(1));
    assert_eq!(rec.effect, Effect::Landed);
    assert_eq!(rec.lifted, vec![StepRole::Curator], "the verdict lifts the noop cooldown");
    let until = l
        .cooling_until(7, StepRole::Builder, now + mins(1))
        .unwrap();
    assert_eq!(until, now + mins(1) + chrono::Duration::hours(24));
    assert!(l.snapshot(now + chrono::Duration::hours(23)).is_cooling(
        7,
        StepRole::Builder,
        now + chrono::Duration::hours(23)
    ));
    assert!(!l
        .snapshot(now)
        .is_cooling(7, StepRole::Curator, now + mins(2)));
}

#[test]
fn a_verdict_clears_the_items_history_and_cooldowns() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    l.record(4, StepRole::Judge, &out(OutcomeClass::Noop, "ci_pending"), SLOW, now);
    l.record(4, StepRole::Judge, &out(OutcomeClass::Failed, "crashed"), SLOW, now);
    l.record(5, StepRole::Judge, &out(OutcomeClass::Noop, "ci_pending"), SLOW, now);
    let rec = l.record(4, StepRole::Doctor, &out(OutcomeClass::Verdict, "pr_opened"), SLOW, now);
    assert_eq!(rec.effect, Effect::Advance);
    assert_eq!(rec.lifted, vec![StepRole::Judge]);
    assert!(l.cooling_until(4, StepRole::Judge, now).is_none());
    assert!(l.cooling_until(5, StepRole::Judge, now).is_some(), "another item is untouched");
}

#[test]
fn noop_cools_the_item_step_for_the_noop_window() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    let rec =
        l.record(9, StepRole::Builder, &out(OutcomeClass::Noop, "no_changes_needed"), SLOW, now);
    assert_eq!(rec.cooling_until, Some(now + chrono::Duration::hours(1)));
    assert!(l.cooling_until(9, StepRole::Judge, now).is_none(), "per (item, step)");
    assert_eq!(l.cooling_cause(9, StepRole::Builder, now), Some(CooldownCause::Noop));
}

/// A `pool_gate` or `claimed` refusal is in the draw's query the moment it
/// is recorded, so the very next tick does not redraw the step; a state
/// refusal arms nothing.
#[test]
fn a_pool_or_claim_refusal_cools_before_the_next_tick_and_a_state_refusal_does_not() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    for (item, reason) in [(1, "pool_gate"), (2, "claimed")] {
        let rec = l.record(item, StepRole::Curator, &out(OutcomeClass::Refused, reason), SLOW, now);
        assert_eq!(rec.effect, Effect::Cooldown(DEFAULT_REFUSAL_COOLDOWN));
        let next_tick = now + chrono::Duration::seconds(30);
        assert!(
            l.snapshot(next_tick)
                .is_cooling(item, StepRole::Curator, next_tick),
            "{reason}"
        );
        assert_eq!(l.cooling_cause(item, StepRole::Curator, now), Some(CooldownCause::Refused));
        let after = now + mins(11);
        assert!(!l.snapshot(now).is_cooling(item, StepRole::Curator, after), "short");
    }
    let rec = l.record(3, StepRole::Builder, &out(OutcomeClass::Refused, "open_pr"), SLOW, now);
    assert_eq!(rec.effect, Effect::Reclassify);
    assert_eq!(rec.cooling_until, None);
}

#[test]
fn claim_lost_redraws_and_is_counted_but_is_not_a_run() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    for _ in 0..5 {
        let rec =
            l.record(6, StepRole::Judge, &out(OutcomeClass::Refused, "claim_lost"), SLOW, now);
        assert_eq!(rec.effect, Effect::Redraw);
        assert_eq!(rec.cooling_until, None, "five lost claims never trip the repeat cap");
    }
    assert_eq!(l.counters().claim_lost, 5);
    assert_eq!(l.counters().by_class[&OutcomeClass::Refused], 5);
}

#[test]
fn a_fast_failure_goes_to_the_ladder_and_a_slow_one_is_only_counted() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    let fast = l.record(
        2,
        StepRole::Builder,
        &out(OutcomeClass::Failed, "crashed"),
        Duration::from_secs(45),
        now,
    );
    assert_eq!(fast.effect, Effect::FastFailLadder);
    assert_eq!(fast.cooling_until, None, "the registry mirrors the ladder's window");
    let slow = l.record(3, StepRole::Builder, &out(OutcomeClass::Failed, "crashed"), SLOW, now);
    assert_eq!(slow.effect, Effect::Failed);
    assert_eq!(slow.cooling_until, None);
}

#[test]
fn champion_held_or_blocked_goes_to_the_forge_ladder() {
    let mut l = OutcomeLedger::default();
    let rec = l.record(30, StepRole::ChampionMerge, &out(OutcomeClass::Noop, "held"), SLOW, t0());
    assert_eq!(rec.effect, Effect::ForgeLadder);
}

/// Acceptance: no `(item, step)` runs more than 3 times in 24 h without a
/// state change. A daemon that redraws whenever the query allows, for two
/// days, with every run failing slowly.
#[test]
fn no_item_step_runs_more_than_three_times_in_24h_without_a_state_change() {
    let mut l = OutcomeLedger::default();
    let start = t0();
    let mut runs = Vec::new();
    for minute in (0..48 * 60).step_by(5) {
        let now = start + mins(minute);
        if l.snapshot(now).is_cooling(11, StepRole::Judge, now) {
            continue;
        }
        runs.push(now);
        l.record(11, StepRole::Judge, &out(OutcomeClass::Failed, "crashed"), SLOW, now);
    }
    assert!(runs.len() >= 4, "it does run again after the window: {}", runs.len());
    for (i, at) in runs.iter().enumerate() {
        let in_window = runs[i..]
            .iter()
            .take_while(|r| **r - *at < chrono::Duration::hours(24))
            .count();
        assert!(in_window <= 3, "{in_window} runs within 24 h of {at}");
    }
    assert!(l.counters().repeat_capped >= 1);
}

#[test]
fn the_repeat_cap_counts_noops_too_and_arms_until_the_oldest_leaves() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    let noop = out(OutcomeClass::Noop, "ci_pending");
    l.record(5, StepRole::Judge, &noop, SLOW, now);
    l.record(5, StepRole::Judge, &noop, SLOW, now + mins(61));
    let rec = l.record(5, StepRole::Judge, &noop, SLOW, now + mins(122));
    assert!(rec.repeat_capped);
    assert_eq!(rec.cooling_until, Some(now + chrono::Duration::hours(24)));
    assert_eq!(
        l.cooling_cause(5, StepRole::Judge, now + mins(122)),
        Some(CooldownCause::RepeatCap)
    );
}

#[test]
fn a_local_window_is_capped_at_24h_and_extend_only() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    let week = now + chrono::Duration::days(7);
    let got = l.arm(1, StepRole::Doctor, week, CooldownCause::ForgeLadder, now);
    assert_eq!(got, now + chrono::Duration::hours(24));
    assert_eq!(MAX_SHORT_COOLDOWN, Duration::from_secs(24 * 3600));
    let got = l.arm(1, StepRole::Doctor, now + mins(5), CooldownCause::Refused, now);
    assert_eq!(got, now + chrono::Duration::hours(24), "never shortened");

    l.prune(now + chrono::Duration::hours(25));
    assert!(l.snapshot(now).is_empty(), "pruned once lapsed");
}

#[test]
fn the_query_merges_extend_only() {
    let mut q = StepCooldowns::default();
    let now = t0();
    q.insert(3, StepRole::ChampionPromote, now + mins(10));
    q.insert(3, StepRole::ChampionPromote, now + mins(5));
    assert_eq!(q.until(3, StepRole::ChampionPromote), Some(now + mins(10)));
    assert!(q.is_cooling(3, StepRole::ChampionPromote, now + mins(9)));
    assert!(!q.is_cooling(3, StepRole::ChampionPromote, now + mins(10)));
    assert!(!q.is_cooling(3, StepRole::Builder, now));
}
