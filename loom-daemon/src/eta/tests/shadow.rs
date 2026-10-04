//! Shadow mode, the live paired ledger and the promotion switch (#9328).
//!
//! Pure: no daemon, no network, no clock. The only I/O is a `tempfile` config
//! written and read back, which is the point of the persistence test.

use super::{as_of, history_a, input_at, provenance, subject};
use crate::eta::backtest::{BacktestReport, Bucket, Comparison};
use crate::eta::config::{promote, resolve};
use crate::eta::heuristics::{LandV1, LandV2, LAND_V1, LAND_V2, LAND_V3};
use crate::eta::score::{score, EstimateSummary, OutcomeKind, Score};
use crate::eta::shadow::{
    self, GateStatus, PairKey, PairedStats, ShadowLedger, COVERAGE_MAX, COVERAGE_MIN,
    MIN_LIVE_PAIRS,
};
use crate::eta::tracker::Resolved;
use crate::eta::{explanation::EstimateResult, Heuristic, Kind, Stage};
use chrono::{DateTime, Duration, Utc};

// ---------------------------------------------------------------- fixtures

/// An estimate summary for `heuristic` at `as_of() + offset` with the given
/// quartiles — the slim form the tracker keeps pending and scoring reads.
fn summary(heuristic: &str, offset: i64, quartiles: (i64, i64, i64)) -> EstimateSummary {
    let mut explanation = LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history_a());
    explanation.heuristic = heuristic.to_string();
    explanation.as_of = as_of() + Duration::seconds(offset);
    explanation.result = Some(EstimateResult {
        p25_sec: quartiles.0,
        p50_sec: quartiles.1,
        p75_sec: quartiles.2,
        eta_p50_at: explanation.as_of + Duration::seconds(quartiles.1),
        samples_min: 9,
        stage_marks: Vec::new(),
    });
    EstimateSummary::of(&explanation)
}

fn resolved(estimate: EstimateSummary, actual_at: DateTime<Utc>) -> Resolved {
    Resolved {
        score: score(&estimate, OutcomeKind::Landed, actual_at, &[]),
        estimate,
        outcome_source: "pulls_read".to_string(),
        outcome_resolution_sec: Some(0),
        result: None,
    }
}

/// One live pair: `current` and `candidate` estimated the same subject at the
/// same instant, and one landing at `actual_offset` scores both.
fn pair(
    offset: i64,
    current: (i64, i64, i64),
    candidate: (i64, i64, i64),
    actual: i64,
) -> Vec<Resolved> {
    let at = as_of() + Duration::seconds(offset + actual);
    vec![
        resolved(summary(LAND_V1, offset, current), at),
        resolved(summary(LAND_V2, offset, candidate), at),
    ]
}

fn current_land(_: Kind) -> String {
    LAND_V1.to_string()
}

/// A backtest comparison where `better` names `winner` (or nobody).
fn comparison(current_mean: f64, candidate_mean: f64, scored: usize) -> Comparison {
    let report = |heuristic: &str, mean: f64| BacktestReport {
        heuristic: heuristic.to_string(),
        kind: Kind::Land,
        overall: Bucket {
            n: scored,
            scored,
            refused: 0,
            mean_pinball_loss_sec: (scored > 0).then_some(mean),
            coverage: Some(0.5),
            bias_sec: Some(0.0),
        },
        by_repo: Default::default(),
        by_horizon: Default::default(),
    };
    let better = if scored == 0 {
        None
    } else if candidate_mean < current_mean {
        Some(LAND_V2.to_string())
    } else if current_mean < candidate_mean {
        Some(LAND_V1.to_string())
    } else {
        None
    };
    Comparison {
        a: report(LAND_V1, current_mean),
        b: report(LAND_V2, candidate_mean),
        better,
    }
}

/// A ledger with `pairs` identical observations: the candidate's paired loss
/// is `candidate_loss`, `current`'s is `current_loss`, and the candidate
/// covers `covered` of them.
fn ledger_with(
    pairs: usize,
    current_loss: f64,
    candidate_loss: f64,
    covered: usize,
) -> ShadowLedger {
    let mut ledger = ShadowLedger::default();
    for i in 0..pairs {
        // Each pair is one resolved group at its own `as_of`, so nothing is
        // accidentally merged across pairs.
        let offset = i as i64 * 10;
        let mut group = pair(offset, (0, 0, 0), (0, 0, 0), 0);
        // Overwrite the scores directly: this test is about the ledger's
        // arithmetic, not about re-deriving pinball loss.
        set_score(&mut group[0], current_loss, true);
        set_score(&mut group[1], candidate_loss, i < covered);
        ledger.record(&current_land, &group);
    }
    ledger
}

fn set_score(r: &mut Resolved, loss: f64, covered: bool) {
    r.score.pinball_loss_sec = Some(loss);
    r.score.covered = Some(covered);
}

// ---------------------------------------------------------- shadow mode

#[test]
fn shadow_estimates_every_registered_heuristic_without_moving_the_primary() {
    use crate::eta::tracker::{EstimateContext, Tracker};
    use crate::eta::Registry;
    use std::collections::BTreeMap;

    const REPO: &str = "rjwalters/loom";
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
    let mut tracker = Tracker::new(provenance());
    tracker.on_dispatch(REPO, 9289, "sweep-issue-9289-1", as_of());
    let emissions = tracker.estimate(None, &ctx, as_of());

    // Every registered heuristic of every estimable kind is computed.
    let ids: Vec<&str> = emissions
        .iter()
        .map(|e| e.explanation.heuristic.as_str())
        .collect();
    assert_eq!(ids, vec!["finish-v1", LAND_V1, LAND_V2, LAND_V3]);

    // Exactly one primary per kind, and it is `current`.
    let primaries: Vec<&str> = emissions
        .iter()
        .filter(|e| e.primary)
        .map(|e| e.explanation.heuristic.as_str())
        .collect();
    assert_eq!(primaries, vec!["finish-v1", LAND_V1]);
    // The primary is emitted before its shadows, so a consumer reading the
    // first estimate of a kind never reads a candidate.
    let land_order: Vec<bool> = emissions
        .iter()
        .filter(|e| e.explanation.kind == Kind::Land)
        .map(|e| e.primary)
        .collect();
    assert_eq!(land_order, vec![true, false, false]);

    // The primary's own number is byte-identical to what a registry with no
    // candidate at all would produce: shadow mode is additive, not a change.
    let alone = LandV1.estimate(&input_at(Stage::SweepCurator, 0, 0), &history);
    let primary = emissions
        .iter()
        .find(|e| e.primary && e.explanation.kind == Kind::Land)
        .unwrap();
    assert_eq!(primary.explanation.quantiles(), alone.quantiles());

    // Both sides are pending, so one outcome scores both — the live pair.
    let pending: Vec<&str> = tracker
        .pending()
        .iter()
        .filter(|p| p.kind == Kind::Land)
        .map(|p| p.heuristic.as_str())
        .collect();
    assert_eq!(pending, vec![LAND_V1, LAND_V2, LAND_V3]);
}

// ------------------------------------------------------ the paired ledger

#[test]
fn the_ledger_pairs_only_same_subject_same_kind_same_instant() {
    let mut ledger = ShadowLedger::default();
    ledger.record(&current_land, &pair(0, (100, 200, 300), (150, 250, 350), 240));
    let stats = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    assert_eq!(stats.pairs, 1);
    assert!(stats.current_mean_pinball_loss_sec.is_some());
    assert!(stats.candidate_mean_pinball_loss_sec.is_some());

    // Two estimates of the same subject at DIFFERENT instants are two
    // separate observations, never one pair.
    let mut split = ShadowLedger::default();
    let mut group = pair(0, (100, 200, 300), (150, 250, 350), 240);
    group[1].estimate.as_of += Duration::seconds(1);
    split.record(&current_land, &group);
    assert_eq!(split.stats(Kind::Land, LAND_V1, LAND_V2).pairs, 0, "no pair");

    // A group with no `current` side records nothing: no baseline.
    let mut orphan = ShadowLedger::default();
    let group = vec![resolved(
        summary(LAND_V2, 0, (100, 200, 300)),
        as_of() + Duration::seconds(240),
    )];
    orphan.record(&current_land, &group);
    assert!(orphan.all().is_empty());

    // An unscored side (an `abandoned` outcome) is never counted as a win.
    let mut abandoned = ShadowLedger::default();
    let mut group = pair(0, (100, 200, 300), (150, 250, 350), 240);
    group[1].score = Score {
        pinball_loss_sec: None,
        covered: None,
        ..group[1].score.clone()
    };
    abandoned.record(&current_land, &group);
    assert_eq!(abandoned.stats(Kind::Land, LAND_V1, LAND_V2).pairs, 0);
}

#[test]
fn the_ledger_reports_paired_means_and_the_candidates_coverage() {
    let ledger = ledger_with(4, 100.0, 60.0, 3);
    let stats = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    assert_eq!(stats.pairs, 4);
    assert_eq!(stats.current_mean_pinball_loss_sec, Some(100.0));
    assert_eq!(stats.candidate_mean_pinball_loss_sec, Some(60.0));
    assert_eq!(stats.candidate_coverage, Some(0.75));
    assert_eq!(stats.current_coverage, Some(1.0));
    // An unknown comparison is all-zero, never absent.
    let none = ledger.stats(Kind::Land, LAND_V1, "land-v9");
    assert_eq!(none.pairs, 0);
    assert_eq!(none.candidate_coverage, None);
}

#[test]
fn the_ledger_round_trips_through_its_persisted_form() {
    let ledger = ledger_with(3, 10.0, 9.0, 2);
    let dir = tempfile::tempdir().unwrap();
    let path = shadow::ledger_path(dir.path());
    shadow::write_ledger(&path, &ledger).unwrap();
    assert_eq!(shadow::read_ledger(&path), ledger, "a restart keeps the count");
    // An absent or malformed file is an empty ledger, never a failure.
    assert_eq!(shadow::read_ledger(&dir.path().join("nope.json")), ShadowLedger::default());
    std::fs::write(&path, "{ not json").unwrap();
    assert_eq!(shadow::read_ledger(&path), ShadowLedger::default());
}

// ------------------------------------------------------- the gate, in order

/// Live evidence that would pass the live gate on its own.
fn passing_live() -> PairedStats {
    PairedStats::of(
        PairKey {
            kind: Kind::Land,
            current: LAND_V1.to_string(),
            candidate: LAND_V2.to_string(),
        },
        crate::eta::shadow::PairSums {
            pairs: MIN_LIVE_PAIRS,
            current_loss_sec: 100.0 * MIN_LIVE_PAIRS as f64,
            candidate_loss_sec: 60.0 * MIN_LIVE_PAIRS as f64,
            current_covered: MIN_LIVE_PAIRS / 2,
            candidate_covered: MIN_LIVE_PAIRS / 2,
        },
    )
}

#[test]
fn both_gates_passing_promotes() {
    let decision = shadow::evaluate(
        Kind::Land,
        LAND_V1,
        LAND_V2,
        Some(&comparison(1000.0, 800.0, 40)),
        &passing_live(),
        as_of(),
    );
    assert_eq!(decision.backtest.status, GateStatus::Passed);
    assert_eq!(decision.live.status, GateStatus::Passed);
    assert!(decision.promote, "{}", decision.reason);
    assert_eq!(decision.backtest.candidate_scored, 40);
    assert_eq!(decision.live.stats.pairs, MIN_LIVE_PAIRS);
}

#[test]
fn a_failing_backtest_gate_blocks_and_the_live_gate_is_not_even_consulted() {
    // Live evidence alone is overwhelming — and irrelevant, because the
    // backtest gate comes first and the candidate loses it.
    let decision = shadow::evaluate(
        Kind::Land,
        LAND_V1,
        LAND_V2,
        Some(&comparison(800.0, 1000.0, 40)),
        &passing_live(),
        as_of(),
    );
    assert_eq!(decision.backtest.status, GateStatus::Failed);
    assert_eq!(
        decision.live.status,
        GateStatus::NotReached,
        "the order is backtest THEN live, not both at once"
    );
    assert!(!decision.promote);
    assert!(decision.reason.starts_with("backtest gate failed"), "{}", decision.reason);
}

#[test]
fn a_backtest_that_could_not_run_is_a_failure_not_a_pass() {
    // #9579: the `land` kind derives zero replay cases on this fleet today.
    // Nothing to rule on must refuse to promote, not wave the candidate past.
    let zero = shadow::evaluate(
        Kind::Land,
        LAND_V1,
        LAND_V2,
        Some(&comparison(0.0, 0.0, 0)),
        &passing_live(),
        as_of(),
    );
    assert_eq!(zero.backtest.status, GateStatus::Failed);
    assert!(!zero.promote);
    assert!(zero.backtest.detail.contains("nothing to compare"), "{}", zero.backtest.detail);

    // No comparison at all is likewise a failure.
    let none = shadow::evaluate(Kind::Land, LAND_V1, LAND_V2, None, &passing_live(), as_of());
    assert_eq!(none.backtest.status, GateStatus::Failed);
    assert!(!none.promote);

    // A tie on the backtest is not a win.
    let tie = shadow::evaluate(
        Kind::Land,
        LAND_V1,
        LAND_V2,
        Some(&comparison(900.0, 900.0, 40)),
        &passing_live(),
        as_of(),
    );
    assert_eq!(tie.backtest.status, GateStatus::Failed);
    assert!(!tie.promote);
}

#[test]
fn a_passing_backtest_still_needs_fifty_live_pairs() {
    let mut stats = passing_live();
    let sums = crate::eta::shadow::PairSums {
        pairs: MIN_LIVE_PAIRS - 1,
        current_loss_sec: 100.0 * (MIN_LIVE_PAIRS - 1) as f64,
        candidate_loss_sec: 60.0 * (MIN_LIVE_PAIRS - 1) as f64,
        current_covered: (MIN_LIVE_PAIRS - 1) / 2,
        candidate_covered: (MIN_LIVE_PAIRS - 1) / 2,
    };
    stats = PairedStats::of(stats.key.clone(), sums);
    let decision = shadow::evaluate(
        Kind::Land,
        LAND_V1,
        LAND_V2,
        Some(&comparison(1000.0, 800.0, 40)),
        &stats,
        as_of(),
    );
    assert_eq!(decision.backtest.status, GateStatus::Passed, "gate 1 was reached and passed");
    assert_eq!(decision.live.status, GateStatus::Failed);
    assert!(!decision.promote);
    assert!(decision.live.detail.contains("49 live pair"), "{}", decision.live.detail);
}

#[test]
fn a_candidate_worse_live_or_outside_the_coverage_band_is_not_promoted() {
    let key = passing_live().key;
    let stats = |candidate_loss: f64, covered: usize| {
        PairedStats::of(
            key.clone(),
            crate::eta::shadow::PairSums {
                pairs: MIN_LIVE_PAIRS,
                current_loss_sec: 100.0 * MIN_LIVE_PAIRS as f64,
                candidate_loss_sec: candidate_loss * MIN_LIVE_PAIRS as f64,
                current_covered: MIN_LIVE_PAIRS / 2,
                candidate_covered: covered,
            },
        )
    };
    let decide = |s: &PairedStats| {
        shadow::evaluate(
            Kind::Land,
            LAND_V1,
            LAND_V2,
            Some(&comparison(1000.0, 800.0, 40)),
            s,
            as_of(),
        )
    };

    // Worse paired pinball loss, whatever the backtest said.
    let worse = decide(&stats(101.0, MIN_LIVE_PAIRS / 2));
    assert_eq!(worse.live.status, GateStatus::Failed);
    assert!(!worse.promote);
    assert!(worse.live.detail.contains("worse"), "{}", worse.live.detail);

    // Equal is "not worse", which the rule accepts.
    assert!(decide(&stats(100.0, MIN_LIVE_PAIRS / 2)).promote);

    // Coverage below the band (over-wide intervals would read as coverage
    // ABOVE it; below means the intervals are too narrow).
    let narrow = decide(&stats(60.0, (MIN_LIVE_PAIRS as f64 * 0.30) as usize));
    assert_eq!(narrow.live.status, GateStatus::Failed);
    assert!(!narrow.promote);
    assert!(narrow.live.detail.contains("outside"), "{}", narrow.live.detail);

    // Coverage above the band.
    let wide = decide(&stats(60.0, (MIN_LIVE_PAIRS as f64 * 0.70) as usize));
    assert_eq!(wide.live.status, GateStatus::Failed);
    assert!(!wide.promote);

    // Both edges of the band inclusive.
    assert!(decide(&stats(60.0, (MIN_LIVE_PAIRS as f64 * COVERAGE_MIN) as usize)).promote);
    assert!(decide(&stats(60.0, (MIN_LIVE_PAIRS as f64 * COVERAGE_MAX) as usize)).promote);
}

#[test]
fn every_decision_records_the_numbers_that_justified_it() {
    let decision = shadow::evaluate(
        Kind::Land,
        LAND_V1,
        LAND_V2,
        Some(&comparison(1000.0, 800.0, 40)),
        &passing_live(),
        as_of(),
    );
    assert_eq!(decision.schema, shadow::DECISION_SCHEMA);
    assert_eq!(decision.at, as_of());
    assert_eq!((decision.current.as_str(), decision.candidate.as_str()), (LAND_V1, LAND_V2));
    assert_eq!(decision.backtest.current_mean_pinball_loss_sec, Some(1000.0));
    assert_eq!(decision.backtest.candidate_mean_pinball_loss_sec, Some(800.0));
    assert_eq!(decision.live.min_pairs, MIN_LIVE_PAIRS);
    assert_eq!(decision.live.coverage_band, (COVERAGE_MIN, COVERAGE_MAX));

    // An operator can read it back out of the log.
    let dir = tempfile::tempdir().unwrap();
    let path = shadow::decision_log_path(dir.path());
    shadow::append_decision(&path, &decision).unwrap();
    shadow::append_decision(&path, &decision).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows.len(), 2, "appended, never rewritten");
    let parsed: shadow::PromotionDecision = serde_json::from_str(rows[0]).unwrap();
    assert_eq!(parsed, decision);
}

// --------------------------------------------------- the persisted flip

#[test]
fn a_flip_is_persisted_to_the_config_and_only_on_a_pass() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = crate::eta::config::promotion_config_path(dir.path());
    let mut ledger = ledger_with(MIN_LIVE_PAIRS, 100.0, 60.0, MIN_LIVE_PAIRS / 2);

    // A failing gate writes nothing at all.
    let held = shadow::promote_if_ready(
        &mut ledger,
        Kind::Land,
        LAND_V1,
        LAND_V2,
        Some(&comparison(800.0, 1000.0, 40)),
        &config_path,
        as_of(),
    )
    .unwrap();
    assert!(!held.promote);
    assert_eq!(held.config_path, None);
    assert!(!config_path.exists(), "a refusal touches nothing");
    assert_eq!(
        ledger.stats(Kind::Land, LAND_V1, LAND_V2).pairs,
        MIN_LIVE_PAIRS,
        "the evidence is kept: the candidate may win later"
    );

    // A passing one writes the key, and the resolver reads it back.
    let flipped = shadow::promote_if_ready(
        &mut ledger,
        Kind::Land,
        LAND_V1,
        LAND_V2,
        Some(&comparison(1000.0, 800.0, 40)),
        &config_path,
        as_of(),
    )
    .unwrap();
    assert!(flipped.promote, "{}", flipped.reason);
    assert_eq!(flipped.config_path.as_deref(), Some(config_path.display().to_string().as_str()));
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(written["autonomous"]["eta"]["current"]["land"], LAND_V2);
    let config = resolve(&written, |_| None);
    assert_eq!(config.current(Kind::Land), Some(LAND_V2));
    // And a registry now resolves the candidate as current.
    assert_eq!(
        crate::eta::Registry::builtin()
            .current(Kind::Land, config.current(Kind::Land))
            .id(),
        LAND_V2
    );
    assert_eq!(
        ledger.stats(Kind::Land, LAND_V1, LAND_V2).pairs,
        0,
        "the new current's comparisons start from zero"
    );
}

#[test]
fn promote_preserves_every_other_config_key_and_refuses_a_non_object() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local.json");
    std::fs::write(
        &path,
        r#"{"terminals": [{"role": "builder"}], "autonomous": {"workFinder": {"enabled": true}, "eta": {"refreshSecs": 600}}}"#,
    )
    .unwrap();
    promote(&path, Kind::Land, LAND_V2).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(written["terminals"][0]["role"], "builder");
    assert_eq!(written["autonomous"]["workFinder"]["enabled"], true);
    assert_eq!(written["autonomous"]["eta"]["refreshSecs"], 600);
    assert_eq!(written["autonomous"]["eta"]["current"]["land"], LAND_V2);

    // A second flip of a different kind is additive too.
    promote(&path, Kind::Finish, "finish-v1").unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(written["autonomous"]["eta"]["current"]["land"], LAND_V2);
    assert_eq!(written["autonomous"]["eta"]["current"]["finish"], "finish-v1");

    // A file that is not a JSON object is never overwritten to land a flip.
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "[1, 2, 3]").unwrap();
    assert!(promote(&bad, Kind::Land, LAND_V2).is_err());
    assert_eq!(std::fs::read_to_string(&bad).unwrap(), "[1, 2, 3]");
    let broken = dir.path().join("broken.json");
    std::fs::write(&broken, "{ not json").unwrap();
    assert!(promote(&broken, Kind::Land, LAND_V2).is_err());
    assert_eq!(std::fs::read_to_string(&broken).unwrap(), "{ not json");
}

#[test]
fn a_missing_config_file_is_created_with_only_the_flipped_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = crate::eta::config::promotion_config_path(dir.path());
    assert!(!path.exists());
    promote(&path, Kind::Start, "start-v1").unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        written,
        serde_json::json!({"autonomous": {"eta": {"current": {"start": "start-v1"}}}})
    );
    // The host-local tier, which is gitignored: a daemon never dirties the
    // tracked config by deciding a promotion.
    assert!(path.ends_with(crate::config_resolver::LOCAL_CONFIG_REL));
}

// --------------------------------------------- land-v2 is not exempt

#[test]
fn land_v2_is_registered_as_a_candidate_and_gated_like_any_other() {
    let registry = crate::eta::Registry::builtin();
    assert_eq!(registry.get(LAND_V2).map(Heuristic::kind), Some(Kind::Land));
    assert_eq!(
        registry.current(Kind::Land, None).id(),
        LAND_V1,
        "land-v2 ships registered, NOT current"
    );
    assert_eq!(LandV2.id(), LAND_V2);
    assert_eq!(LandV2.kind(), Kind::Land);
    // It reaches the promotion switch through the same gate as anything else:
    // with no backtest evidence it does not get promoted.
    let decision = shadow::evaluate(Kind::Land, LAND_V1, LAND_V2, None, &passing_live(), as_of());
    assert!(!decision.promote);
}

#[test]
fn the_subject_fixture_is_the_shared_one() {
    // Guards the fixtures above against drifting from `tests::subject()`.
    assert_eq!(subject().issue, 9289);
}
