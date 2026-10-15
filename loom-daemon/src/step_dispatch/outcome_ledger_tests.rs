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

    assert_eq!(classify(Some(&verdict), Some(moved)), verdict);
    assert_eq!(
        classify(Some(&verdict), Some(unchanged)),
        out(OutcomeClass::Failed, UNCLASSIFIED),
        "a verdict the forge does not show is failed"
    );
    assert_eq!(classify(Some(&noop), Some(unchanged)), noop);
    assert_eq!(classify(None, Some(unchanged)), out(OutcomeClass::Failed, UNCLASSIFIED));
    assert_eq!(
        classify(Some(&verdict), Some(held)).class,
        OutcomeClass::Failed,
        "claim left held"
    );
    assert_eq!(classify(None, Some(closed)), out(OutcomeClass::Verdict, "closed"));
    assert_eq!(classify(None, Some(moved)), out(OutcomeClass::Verdict, UNCLASSIFIED));
    assert_eq!(
        classify(Some(&noop), None),
        out(OutcomeClass::Failed, PROBE_FAILED),
        "a failed forge read never trusts the line"
    );
    assert_eq!(classify(Some(&verdict), None).class, OutcomeClass::Failed);
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
        (
            StepRole::Judge,
            out(OutcomeClass::Failed, "crashed"),
            fast,
            Effect::FastFailWindow,
        ),
        (
            StepRole::ChampionMerge,
            out(OutcomeClass::Failed, "failed"),
            fast,
            Effect::FastFailWindow,
        ),
        // #11969: only Builder charges the issue ladder.
        (
            StepRole::Curator,
            out(OutcomeClass::Failed, "crashed"),
            fast,
            Effect::FastFailWindow,
        ),
        (
            StepRole::ChampionPromote,
            out(OutcomeClass::Failed, "crashed"),
            fast,
            Effect::FastFailWindow,
        ),
        (StepRole::Curator, out(OutcomeClass::Failed, "crashed"), SLOW, Effect::Failed),
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

/// #12042: a Judge step that stood down for pending CI cools down for the
/// short refusal window, not the no-op hour, so a PR whose CI concludes in
/// minutes gets its Judge back in minutes.
#[test]
fn a_ci_pending_noop_takes_the_short_window() {
    let cfg = LedgerConfig::default();
    let noop = out(OutcomeClass::Noop, "ci_pending");
    assert_eq!(
        effect_of(StepRole::Judge, &noop, SLOW, &cfg),
        Effect::Cooldown(cfg.refusal_cooldown)
    );
    assert!(cfg.refusal_cooldown < cfg.noop_cooldown);
    let mut l = OutcomeLedger::default();
    let now = t0();
    let rec = l.record(9, StepRole::Judge, &noop, SLOW, now);
    assert_eq!(rec.cooling_until, Some(now + mins(10)));
    assert!(!l
        .snapshot(now)
        .is_cooling(9, StepRole::Judge, now + mins(11)));
    // Every other no-op keeps the hour.
    let rec =
        l.record(9, StepRole::Builder, &out(OutcomeClass::Noop, "no_changes_needed"), SLOW, now);
    assert_eq!(rec.cooling_until, Some(now + chrono::Duration::hours(1)));
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
    for k in 0..5 {
        let rec =
            l.record(6, StepRole::Judge, &out(OutcomeClass::Refused, "claim_lost"), SLOW, now);
        assert_eq!(rec.effect, Effect::Redraw);
        assert_ne!(
            l.cooling_cause(6, StepRole::Judge, now),
            Some(CooldownCause::RepeatCap),
            "lost claim {k} is not a run"
        );
    }
    assert_eq!(l.counters().claim_lost, 5);
    assert_eq!(l.counters().by_class[&OutcomeClass::Refused], 5);
    assert_eq!(l.counters().repeat_capped, 0);
}

/// #11969: one lost claim is a race and only redraws; a second in a row arms
/// the refusal cooldown (10 min, doubling, 24 h cap); a verdict or any other
/// outcome on the pair resets the streak.
#[test]
fn a_second_consecutive_claim_lost_arms_the_refusal_cooldown() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    let lost = out(OutcomeClass::Refused, "claim_lost");
    let step = StepRole::Doctor;

    let first = l.record(80, step, &lost, SLOW, now);
    assert_eq!((first.effect, first.cooling_until), (Effect::Redraw, None), "a race is free");
    let mut lengths = Vec::new();
    for k in 1..12 {
        let at = now + chrono::Duration::days(k);
        let rec = l.record(80, step, &lost, SLOW, at);
        assert_eq!(rec.effect, Effect::Redraw, "still a redraw");
        lengths.push((rec.cooling_until.expect("cooling") - at).num_seconds());
    }
    assert_eq!(&lengths[..3], &[600, 1200, 2400], "10 min, doubling");
    assert_eq!(*lengths.last().unwrap(), 24 * 3600, "capped at 24 h");
    assert_eq!(
        l.cooling_cause(80, step, now + chrono::Duration::days(11)),
        Some(CooldownCause::Refused)
    );

    // Per (item, step): another step's lost claim is its own streak.
    assert_eq!(
        l.record(80, StepRole::Judge, &lost, SLOW, now)
            .cooling_until,
        None
    );

    // Another outcome in between resets it.
    l.record(81, step, &lost, SLOW, now);
    l.record(81, step, &out(OutcomeClass::Refused, "not_eligible"), SLOW, now);
    assert_eq!(l.record(81, step, &lost, SLOW, now).cooling_until, None);
    l.record(82, step, &lost, SLOW, now);
    l.record(82, step, &out(OutcomeClass::Failed, "crashed"), SLOW, now);
    assert_eq!(l.record(82, step, &lost, SLOW, now).cooling_until, None);

    // So does a verdict.
    l.record(83, step, &lost, SLOW, now);
    l.record(83, step, &out(OutcomeClass::Verdict, "fixed"), SLOW, now);
    assert_eq!(l.record(83, step, &lost, SLOW, now).cooling_until, None);
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
    let noop = out(OutcomeClass::Noop, "already_judged");
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

/// Only steps that worked the item count toward the cap: refusals and a
/// CI-queue wait never arm it, however many there are. (Each has its own
/// brake: the refusal cooldown, the repeated-refusal guard, the noop window.)
#[test]
fn refusals_and_ci_waits_are_not_runs() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    for (i, (class, reason)) in [
        (OutcomeClass::Refused, "pool_gate"),
        (OutcomeClass::Refused, "claimed"),
        (OutcomeClass::Refused, "open_pr"),
        (OutcomeClass::Noop, "ci_pending"),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(!counts_as_run(&out(class, reason)), "{reason}");
        for k in 0..5 {
            let at = now + mins(i64::try_from(i * 100 + k * 61).unwrap());
            let rec = l.record(
                20 + u32::try_from(i).unwrap(),
                StepRole::Judge,
                &out(class, reason),
                SLOW,
                at,
            );
            assert!(!rec.repeat_capped, "{reason} #{k}");
        }
    }
    assert_eq!(l.counters().repeat_capped, 0);
    // Not a run, but not free either: the repeated identical state refusal
    // (item 22, `open_pr` x5) has its own brake.
    let last = now + mins(200 + 4 * 61);
    assert_eq!(l.cooling_cause(22, StepRole::Judge, last), Some(CooldownCause::Refused));
    assert!(counts_as_run(&out(OutcomeClass::Noop, "already_judged")));
    assert!(counts_as_run(&out(OutcomeClass::Failed, "crashed")));
}

/// A PR step's fast failures double a ledger-local window, capped at 24 h.
#[test]
fn a_pr_step_fast_failure_doubles_a_local_window_capped_at_24h() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    let fail = out(OutcomeClass::Failed, "crashed");
    let quick = Duration::from_secs(20);
    let mut lengths = Vec::new();
    for k in 0..14 {
        let at = now + chrono::Duration::days(k);
        let rec = l.record(40, StepRole::Doctor, &fail, quick, at);
        assert_eq!(rec.effect, Effect::FastFailWindow);
        lengths.push((rec.cooling_until.unwrap() - at).num_seconds());
    }
    assert_eq!(&lengths[..3], &[60, 120, 240], "doubling from the #4485 base");
    assert!(lengths.iter().all(|s| *s <= 24 * 3600), "{lengths:?}");
    assert_eq!(l.cooling_cause(40, StepRole::Doctor, now), Some(CooldownCause::FastFail));
    l.record(
        40,
        StepRole::Doctor,
        &out(OutcomeClass::Verdict, "pr_opened"),
        SLOW,
        now + chrono::Duration::days(15),
    );
    let at = now + chrono::Duration::days(15);
    let rec = l.record(40, StepRole::Doctor, &fail, quick, at);
    assert_eq!(
        (rec.cooling_until.unwrap() - at).num_seconds(),
        60,
        "a verdict resets the streak"
    );
}

/// Judge re-review on #11962: a lasting classifier/role disagreement must not
/// loop. The first state refusal only reclassifies; a second identical one
/// arms the refusal cooldown, doubling to 24 h; a different reason, another
/// outcome or a verdict resets the streak.
#[test]
fn a_repeated_identical_state_refusal_arms_a_doubling_cooldown() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    let refuse = |reason: &str| out(OutcomeClass::Refused, reason);
    let step = StepRole::ChampionMerge;

    let first = l.record(60, step, &refuse("not_eligible"), SLOW, now);
    assert_eq!(
        (first.effect, first.cooling_until),
        (Effect::Reclassify, None),
        "a stale listing costs nothing"
    );
    let mut lengths = Vec::new();
    for k in 1..12 {
        let at = now + chrono::Duration::days(k);
        let rec = l.record(60, step, &refuse("not_eligible"), SLOW, at);
        assert_eq!(rec.effect, Effect::Reclassify);
        lengths.push((rec.cooling_until.expect("cooling") - at).num_seconds());
    }
    assert_eq!(&lengths[..3], &[600, 1200, 2400], "10 min, doubling");
    assert!(lengths.iter().all(|s| *s <= 24 * 3600), "{lengths:?}");
    assert_eq!(*lengths.last().unwrap(), 24 * 3600, "capped at 24 h");
    assert_eq!(
        l.cooling_cause(60, step, now + chrono::Duration::days(11)),
        Some(CooldownCause::Refused)
    );

    // Two different reasons in turn: never two identical in a row.
    for (k, reason) in ["open_pr", "head_moved", "open_pr", "head_moved"]
        .into_iter()
        .enumerate()
    {
        let at = now + mins(i64::try_from(k).unwrap());
        assert_eq!(l.record(61, step, &refuse(reason), SLOW, at).cooling_until, None, "{reason}");
    }

    // Another outcome in between resets the streak.
    l.record(62, step, &refuse("not_eligible"), SLOW, now);
    l.record(62, step, &refuse("claim_lost"), SLOW, now);
    assert_eq!(
        l.record(62, step, &refuse("not_eligible"), SLOW, now)
            .cooling_until,
        None
    );

    // So does a verdict.
    l.record(63, step, &refuse("not_eligible"), SLOW, now);
    l.record(63, step, &out(OutcomeClass::Verdict, "rejected"), SLOW, now);
    assert_eq!(
        l.record(63, step, &refuse("not_eligible"), SLOW, now)
            .cooling_until,
        None
    );

    // The streak survives a prune between ticks.
    l.record(64, step, &refuse("not_eligible"), SLOW, now);
    l.prune(now + mins(5));
    assert!(l
        .record(64, step, &refuse("not_eligible"), SLOW, now + mins(5))
        .cooling_until
        .is_some());
}

/// #11909: the reaper prunes every tick. A prune just before each draw must
/// not drop a refusal streak whose cooldown reached the 24 h cap (the same
/// length as the prune window): the next identical refusal keeps the 24 h
/// cooldown instead of restarting at 10 min.
#[test]
fn a_prune_inside_the_refusal_loop_keeps_the_streak_capped() {
    let mut l = OutcomeLedger::default();
    let step = StepRole::ChampionMerge;
    let refuse = out(OutcomeClass::Refused, "not_eligible");
    let mut at = t0();
    let mut lengths = Vec::new();
    for _ in 0..16 {
        let rec = l.record(70, step, &refuse, SLOW, at);
        if let Some(until) = rec.cooling_until {
            lengths.push((until - at).num_seconds());
            // The next draw is just after the cooldown ends, with a reaper
            // prune in between.
            at = until + mins(1);
        } else {
            at += mins(1);
        }
        l.prune(at);
    }
    let capped: Vec<i64> = lengths
        .iter()
        .copied()
        .skip_while(|s| *s < 24 * 3600)
        .collect();
    assert!(capped.len() >= 4, "{lengths:?}");
    assert!(capped.iter().all(|s| *s == 24 * 3600), "the streak restarted: {lengths:?}");
    // A full window past the cooldown with no refusal: the streak is gone.
    l.prune(at + chrono::Duration::days(2));
    assert_eq!(
        l.record(70, step, &refuse, SLOW, at + chrono::Duration::days(2))
            .cooling_until,
        None
    );
}

/// #11969: a Curator or Champion-promote fast failure takes the ledger-local
/// window, never the issue ladder (whose release would restore `loom:issue`).
#[test]
fn a_non_builder_issue_step_fast_failure_takes_the_local_window() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    let fail = out(OutcomeClass::Failed, "crashed");
    let quick = Duration::from_secs(20);
    for step in [StepRole::Curator, StepRole::ChampionPromote] {
        let lengths: Vec<i64> = (0..3)
            .map(|k| {
                let at = now + chrono::Duration::days(k);
                let rec = l.record(90, step, &fail, quick, at);
                assert_eq!(rec.effect, Effect::FastFailWindow, "{step}");
                (rec.cooling_until.unwrap() - at).num_seconds()
            })
            .collect();
        assert_eq!(lengths, [60, 120, 240], "{step}");
        assert_eq!(l.cooling_cause(90, step, now), Some(CooldownCause::FastFail));
    }
    let builder = l.record(91, StepRole::Builder, &fail, quick, now);
    assert_eq!((builder.effect, builder.cooling_until), (Effect::FastFailLadder, None));
}

/// Any other outcome on the pair resets the PR fast-fail streak, so the
/// window doubles only over consecutive fast failures.
#[test]
fn the_pr_fast_fail_streak_is_consecutive() {
    let mut l = OutcomeLedger::default();
    let now = t0();
    let quick = Duration::from_secs(20);
    let fail = out(OutcomeClass::Failed, "crashed");
    l.record(70, StepRole::Judge, &fail, quick, now);
    l.record(70, StepRole::Judge, &fail, quick, now + chrono::Duration::hours(2));
    l.record(
        70,
        StepRole::Judge,
        &out(OutcomeClass::Failed, "crashed"),
        SLOW,
        now + chrono::Duration::hours(4),
    );
    let at = now + chrono::Duration::hours(30);
    let rec = l.record(70, StepRole::Judge, &fail, quick, at);
    assert_eq!((rec.cooling_until.unwrap() - at).num_seconds(), 60, "back to the base");
}
