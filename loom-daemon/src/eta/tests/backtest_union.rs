//! The multi-fold backtest (#10233, Slice B PR 2): the paired comparison on
//! the union of cases counting refusals, the walk-forward daily folds and
//! their deterministic interval, stability on the predicted landing instant,
//! and convergence.

use super::{as_of, history_a, history_a_envelopes, provenance, subject};
use crate::eta::backtest::{self, Comparison, Filter, ReplayCase};
use crate::eta::heuristics::{FinishV1, LandV1};
use crate::eta::history::{SampleSource, StageSample, StageSamples};
use crate::eta::score::OutcomeKind;
use crate::eta::shadow::{self, wilson, GateStatus, MIN_FOLDS};
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind, NoEstimateReason, Stage};
use chrono::{DateTime, Duration, Utc};

/// `land-v1`, except that it refuses every case it expects to take an hour
/// or more — the slow, hard ones. On the cases it does answer it says exactly
/// what `land-v1` says.
#[derive(Debug, Clone, Copy)]
struct RefusesSlow;

const SLOW_SEC: i64 = 3600;

impl Heuristic for RefusesSlow {
    fn id(&self) -> &'static str {
        "refuses-slow-test"
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut e = LandV1.estimate(input, history);
        if e.result.as_ref().is_some_and(|r| r.p50_sec >= SLOW_SEC) {
            e.result = None;
            e.no_estimate_reason = Some(NoEstimateReason::BeyondHistory);
        }
        e
    }
}

fn compare_on_history_a(a: &dyn Heuristic, b: &dyn Heuristic) -> Comparison {
    let envelopes = history_a_envelopes();
    let cases = backtest::cases_from_envelopes(&envelopes);
    backtest::compare(a, b, &history_a(), &cases, Filter::default(), &provenance())
        .expect("both predict `land`")
}

#[test]
fn a_heuristic_that_refuses_the_slowest_cases_no_longer_wins() {
    let c = compare_on_history_a(&LandV1, &RefusesSlow);

    // The defect, reproduced: on the cases each answered on its own, the
    // refuser's mean loss is lower — which is what `compare` used to rank on.
    let own_land_v1 = c.a.overall.mean_pinball_loss_sec.unwrap();
    let own_refuser = c.b.overall.mean_pinball_loss_sec.unwrap();
    assert!(
        own_refuser < own_land_v1,
        "fixture must make refusing look better on own subsets: {own_refuser} vs {own_land_v1}"
    );
    assert!(c.b.overall.scored < c.a.overall.scored, "the refuser refused something");

    // On the union, counting refusals, it does not win: on the cases both
    // answered the two say the same thing, and it answered less.
    let p = &c.paired;
    assert_eq!(p.cases, c.a.overall.n, "the union is every case of the kind");
    assert_eq!(p.common, c.b.overall.scored, "common = the refuser's answered cases");
    assert_eq!(p.a_mean_pinball4_loss_sec, p.b_mean_pinball4_loss_sec);
    assert!(p.b_answer_rate.unwrap() < p.a_answer_rate.unwrap());
    assert_eq!(c.better.as_deref(), Some("land-v1"));

    // Argument order does not matter.
    let swapped = compare_on_history_a(&RefusesSlow, &LandV1);
    assert_eq!(swapped.better.as_deref(), Some("land-v1"));

    // And the promotion gate refuses it as the candidate.
    let decision = shadow::evaluate(
        Kind::Land,
        "land-v1",
        "refuses-slow-test",
        Some(&c),
        &shadow::ShadowLedger::default().stats(Kind::Land, "land-v1", "refuses-slow-test"),
        as_of(),
    );
    assert_eq!(decision.backtest.status, GateStatus::Failed);
    assert!(
        decision.backtest.detail.contains("does not win"),
        "{}",
        decision.backtest.detail
    );
    assert!(
        decision.backtest.candidate_answer_rate < decision.backtest.current_answer_rate,
        "the record carries the refusals that decided it"
    );
}

/// `land-v1` with every quantile shifted later by `shift` seconds: better
/// than `land-v1` on history A wherever it runs early, and refusing nothing.
#[derive(Debug, Clone, Copy)]
struct Shifted(i64);

impl Heuristic for Shifted {
    fn id(&self) -> &'static str {
        "shifted-test"
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut e = LandV1.estimate(input, history);
        if let Some(r) = &mut e.result {
            r.p25_sec += self.0;
            r.p50_sec += self.0;
            r.p75_sec += self.0;
            r.p90_sec = r.p90_sec.map(|p| p + self.0);
        }
        e
    }
}

#[test]
fn walk_forward_folds_partition_the_union_by_utc_day_and_are_deterministic() {
    let c = compare_on_history_a(&LandV1, &Shifted(1800));
    let p = &c.paired;
    assert!(p.folds.len() > 1, "history A spans several days");
    assert_eq!(p.folds.iter().map(|f| f.cases).sum::<usize>(), p.cases);
    assert_eq!(p.folds.iter().map(|f| f.loss4_pairs).sum::<usize>(), p.loss4_pairs);
    let days: Vec<&str> = p.folds.iter().map(|f| f.day.as_str()).collect();
    let mut sorted = days.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(days, sorted, "one fold per day, oldest first");

    // A decided day is a fold whose two means differ; ties are left out.
    let decided = p
        .folds
        .iter()
        .filter(|f| f.loss4_pairs > 0 && f.a_mean_pinball4_loss_sec != f.b_mean_pinball4_loss_sec)
        .count();
    assert_eq!(p.day_wins.days, decided);
    let b_won = p
        .folds
        .iter()
        .filter(|f| f.b_mean_pinball4_loss_sec < f.a_mean_pinball4_loss_sec)
        .count();
    assert_eq!(p.day_wins.wins, b_won, "the wins are `b`'s");

    // The interval is the closed-form Wilson bound — no resampling, so the
    // same replay always yields the same CI.
    let round = |x: f64| (x * 1e4).round() / 1e4;
    let (low, high) = wilson(p.day_wins.wins, p.day_wins.days);
    assert_eq!(p.day_wins.ci_low, Some(round(low)));
    assert_eq!(p.day_wins.ci_high, Some(round(high)));
    let again = compare_on_history_a(&LandV1, &Shifted(1800));
    assert_eq!(again, c, "a replay is a pure function of its inputs");
}

fn merge_wait_history(t: DateTime<Utc>) -> StageSamples {
    let mut h = StageSamples::default();
    for i in 0..8_i64 {
        h.stages.push(StageSample {
            repo: "rjwalters/loom".to_string(),
            stage: Stage::MergeWait,
            duration_sec: 600 + i * 60,
            observed_at: t - Duration::days(30) - Duration::hours(i + 1),
            source: SampleSource::SweepOutcome,
            host: "host-a".to_string(),
            worked: None,
        });
    }
    h
}

/// A case on day `day` (0-based from `as_of()`), landing 900 s later.
fn case_on(day: i64, offset_sec: i64) -> ReplayCase {
    let t = as_of() + Duration::days(day) + Duration::seconds(offset_sec);
    ReplayCase {
        subject: subject(),
        as_of: t,
        stage: Stage::MergeWait,
        rework_rounds: 0,
        kind: Kind::Land,
        outcome: OutcomeKind::Landed,
        actual_at: t + Duration::seconds(900),
        dispatch: None,
        age_sec: 0,
        queue: Vec::new(),
        pr_flags: None,
    }
}

/// `land-v1`'s shape, but predicting `p50 = remaining` exactly on even days
/// and `remaining × 3` on odd ones: wins only on the even days.
#[derive(Debug, Clone, Copy)]
struct GoodOnEvenDays;

impl Heuristic for GoodOnEvenDays {
    fn id(&self) -> &'static str {
        "good-on-even-days-test"
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut e = LandV1.estimate(input, history);
        let day = (input.as_of - as_of()).num_days();
        let p50 = if day % 2 == 0 { 900 } else { 2700 };
        if let Some(r) = &mut e.result {
            (r.p25_sec, r.p50_sec, r.p75_sec, r.p90_sec) = (p50, p50, p50, Some(p50));
        }
        e
    }
}

/// The same, but bad on every day: always `remaining × 3`.
#[derive(Debug, Clone, Copy)]
struct AlwaysLate;

impl Heuristic for AlwaysLate {
    fn id(&self) -> &'static str {
        "always-late-test"
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut e = LandV1.estimate(input, history);
        if let Some(r) = &mut e.result {
            (r.p25_sec, r.p50_sec, r.p75_sec, r.p90_sec) = (2700, 2700, 2700, Some(2700));
        }
        e
    }
}

#[test]
fn the_backtest_gate_needs_min_folds_decided_days_with_a_lower_bound_above_half() {
    let history = merge_wait_history(as_of());
    let run = |days: i64| {
        let cases: Vec<ReplayCase> = (0..days).map(|d| case_on(d, 0)).collect();
        backtest::compare(
            &AlwaysLate,
            &GoodOnEvenDays,
            &history,
            &cases,
            Filter::default(),
            &provenance(),
        )
        .unwrap()
    };
    let gate = |c: &Comparison| {
        shadow::evaluate(
            Kind::Land,
            "always-late-test",
            "good-on-even-days-test",
            Some(c),
            &shadow::ShadowLedger::default().stats(Kind::Land, "x", "y"),
            as_of(),
        )
        .backtest
    };

    // Better on the pooled mean, but every other day is a tie: too few
    // decided days.
    let few = run(2 * MIN_FOLDS as i64 - 2);
    assert_eq!(few.better.as_deref(), Some("good-on-even-days-test"));
    assert_eq!(few.paired.day_wins.days, MIN_FOLDS - 1);
    assert_eq!(few.paired.day_wins.ties, MIN_FOLDS - 1);
    let g = gate(&few);
    assert_eq!(g.status, GateStatus::Failed);
    assert!(g.detail.contains("decided day(s), 7 required"), "{}", g.detail);
    assert_eq!(g.min_folds, MIN_FOLDS);

    // Enough decided days, all won: 7/7 has a Wilson lower bound of ~64.6%.
    let enough = run(2 * MIN_FOLDS as i64);
    assert_eq!(enough.paired.day_wins.days, MIN_FOLDS);
    assert_eq!(enough.paired.day_wins.wins, MIN_FOLDS);
    let g = gate(&enough);
    assert_eq!(g.status, GateStatus::Passed, "{}", g.detail);
    assert!(g.day_wins.ci_low.unwrap() > 0.5);

    // Read from the other side, the same folds are the incumbent's losses:
    // the gate takes the wins of whichever side is the candidate.
    let reversed = shadow::evaluate(
        Kind::Land,
        "good-on-even-days-test",
        "always-late-test",
        Some(&enough),
        &shadow::ShadowLedger::default().stats(Kind::Land, "x", "y"),
        as_of(),
    );
    assert_eq!(reversed.backtest.status, GateStatus::Failed);
    assert_eq!(reversed.backtest.day_wins.wins, 0);
    assert_eq!(reversed.backtest.day_wins.days, MIN_FOLDS);
}

/// Predicts landing at one fixed instant whatever `as_of` is: remaining
/// seconds fall one per second, the promised instant never moves.
#[derive(Debug, Clone, Copy)]
struct FixedInstant(DateTime<Utc>);

impl Heuristic for FixedInstant {
    fn id(&self) -> &'static str {
        "fixed-instant-test"
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut e = LandV1.estimate(input, history);
        let remaining = (self.0 - input.as_of).num_seconds();
        if let Some(r) = &mut e.result {
            (r.p25_sec, r.p50_sec, r.p75_sec) = (remaining - 60, remaining, remaining + 60);
            r.p90_sec = Some(remaining + 120);
        }
        e
    }
}

/// Predicts the same remaining seconds whatever `as_of` is.
#[derive(Debug, Clone, Copy)]
struct FixedRemaining;

impl Heuristic for FixedRemaining {
    fn id(&self) -> &'static str {
        "fixed-remaining-test"
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut e = LandV1.estimate(input, history);
        if let Some(r) = &mut e.result {
            (r.p25_sec, r.p50_sec, r.p75_sec, r.p90_sec) = (600, 900, 1200, Some(1500));
        }
        e
    }
}

#[test]
fn stability_is_measured_on_the_predicted_landing_instant_not_remaining_seconds() {
    let history = merge_wait_history(as_of());
    // Three entries of one series, 10 and then 20 minutes apart.
    let cases = vec![case_on(0, 0), case_on(0, 600), case_on(0, 1800)];
    let landing = as_of() + Duration::seconds(3600);

    let steady =
        backtest::run(&FixedInstant(landing), &history, &cases, Filter::default(), &provenance());
    assert_eq!(steady.stability.steps, 2);
    assert_eq!(steady.stability.median_shift_sec, Some(0.0), "a steady promise does not move");
    assert_eq!(steady.stability.max_shift_sec, Some(0));

    // Positive control: the same remaining seconds at every instant is a
    // promise that slides by exactly the elapsed time.
    let sliding =
        backtest::run(&FixedRemaining, &history, &cases, Filter::default(), &provenance());
    assert_eq!(sliding.stability.steps, 2);
    assert_eq!(sliding.stability.median_shift_sec, Some(900.0));
    assert_eq!(sliding.stability.max_shift_sec, Some(1200));

    // Another series is never paired with this one.
    let mut other = case_on(0, 300);
    other.subject.issue += 1;
    let mut mixed = cases.clone();
    mixed.push(other);
    let r = backtest::run(&FixedRemaining, &history, &mixed, Filter::default(), &provenance());
    assert_eq!(r.stability.steps, 2);
}

#[test]
fn convergence_reports_interval_widths_per_bucket_of_the_actual_lead() {
    let history = merge_wait_history(as_of());
    // Actual lead is 900 s for every case: one `15m_1h` bucket.
    let cases = vec![case_on(0, 0), case_on(0, 600), case_on(1, 0)];
    let r = backtest::run(&FixedRemaining, &history, &cases, Filter::default(), &provenance());
    assert_eq!(r.convergence.len(), 1);
    let c = r.convergence["15m_1h"];
    assert_eq!(c.scored, 3);
    assert_eq!(c.median_p25_p75_sec, Some(600.0));
    assert_eq!(c.with_p90, 3);
    assert_eq!(c.median_p25_p90_sec, Some(900.0));

    // Refused cases are not in it.
    let refused = backtest::run(&RefusesSlow, &history, &cases, Filter::default(), &provenance());
    assert_eq!(
        refused
            .convergence
            .values()
            .map(|c| c.scored)
            .sum::<usize>(),
        refused.overall.scored
    );
}

#[test]
fn a_comparison_of_two_finish_heuristics_still_folds() {
    // `finish` cases (no #9579 dependency) fold the same way.
    let envelopes = history_a_envelopes();
    let cases = backtest::cases_from_envelopes(&envelopes);
    let c = backtest::compare(
        &FinishV1,
        &FinishV1,
        &history_a(),
        &cases,
        Filter::default(),
        &provenance(),
    )
    .unwrap();
    assert!(c.paired.cases > 0);
    assert_eq!(c.paired.day_wins.days, 0, "a heuristic ties itself on every day");
    assert_eq!(c.better, None);
}

#[test]
fn the_paired_loss_deltas_carry_a_deterministic_issue_bootstrap_interval() {
    // #10489: "pinball no worse" is read on the paired `b − a` delta with
    // its issue-bootstrap 95% interval, whole issues resampled.
    let c = compare_on_history_a(&LandV1, &Shifted(1800));
    let p = &c.paired;
    let d = p.delta_pinball_loss_sec.expect("cases in common");
    let d4 = p.delta_pinball4_loss_sec.expect("p90 pairs in common");
    assert_eq!(d.n, p.common);
    assert_eq!(d4.n, p.loss4_pairs);
    let close = |x: f64, y: f64| (x - y).abs() < 1e-6 * (1.0 + y.abs());
    let point = p.b_mean_pinball_loss_sec.unwrap() - p.a_mean_pinball_loss_sec.unwrap();
    assert!(close(d.value.unwrap(), point), "{d:?} vs {point}");
    let point4 = p.b_mean_pinball4_loss_sec.unwrap() - p.a_mean_pinball4_loss_sec.unwrap();
    assert!(close(d4.value.unwrap(), point4), "{d4:?} vs {point4}");
    for e in [d, d4] {
        let (lo, hi) = (e.lo.unwrap(), e.hi.unwrap());
        assert!(lo <= hi, "{e:?}");
    }
    // Seeded: the same replay yields the same interval.
    let again = compare_on_history_a(&LandV1, &Shifted(1800));
    assert_eq!(again.paired.delta_pinball4_loss_sec, p.delta_pinball4_loss_sec);

    // A heuristic against itself: a zero delta with a zero-width interval.
    let same = compare_on_history_a(&LandV1, &LandV1);
    let z = same.paired.delta_pinball4_loss_sec.unwrap();
    assert_eq!((z.value, z.lo, z.hi), (Some(0.0), Some(0.0), Some(0.0)));
}
