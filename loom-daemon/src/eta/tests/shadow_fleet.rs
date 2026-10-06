//! Shadow fleet management (#10525): tiers, the shadow budget, and the
//! candidate-only promotion gate.

use super::as_of;
use super::shadow::{comparison, ledger_with};
use crate::eta::config::resolve;
use crate::eta::heuristics::{LAND_V1, LAND_V2, LAND_V3, LAND_V4, LITTLE_V0};
use crate::eta::shadow::{self, GateStatus, PromotionDecision, MIN_LIVE_PAIRS};
use crate::eta::shadow_fleet::{
    builtin_tier, check_budget, BudgetExceeded, DEFAULT_MAX_ACTIVE, RETIRED,
};
use crate::eta::{Kind, Registry, Tier};
use crate::telemetry::kinds::eta_snapshot::MAX_ALTERNATES;
use serde_json::json;

const AMBER_HERON: &str = "land-2026-10-04-amber-heron";
const FRESH_TIDE: &str = "land-2026-10-04-fresh-tide";

/// Every built-in id and its declared tier. A new registration fails this
/// test until its tier is written down here: the tier is a decision, not a
/// default someone forgot to change.
const BUILTIN_TIERS: &[(&str, Tier)] = &[
    ("start-v1", Tier::Baseline),
    ("finish-v1", Tier::Baseline),
    ("land-v1", Tier::Baseline),
    ("land-v2", Tier::Candidate),
    ("land-2026-10-06-calm-plover", Tier::Candidate),
    ("land-v4", Tier::Candidate),
    ("little-v0", Tier::Baseline),
    ("land-2026-10-06-quick-tern", Tier::Candidate),
    ("land-2026-10-06-held-heron", Tier::Candidate),
    ("land-2026-10-06-keen-wren", Tier::Candidate),
    ("land-2026-10-04-twin-otter", Tier::Candidate),
    ("land-2026-10-04-twin-otter-b", Tier::Candidate),
];

#[test]
fn every_builtin_heuristic_declares_the_pinned_tier() {
    let registry = Registry::builtin();
    let ids = registry.ids();
    let pinned: Vec<&str> = BUILTIN_TIERS.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, pinned, "registration order and the pinned table agree");
    for (id, tier) in BUILTIN_TIERS {
        assert_eq!(registry.tier_of(id), Some(*tier), "{id}");
        assert_eq!(builtin_tier(id), Some(*tier), "{id}");
    }
}

#[test]
fn retired_ids_are_unregistered_and_answer_retired() {
    let registry = Registry::builtin();
    assert_eq!(
        RETIRED.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [LAND_V3, AMBER_HERON, FRESH_TIDE]
    );
    for (id, kind) in RETIRED {
        assert!(!registry.registers(*kind, id), "{id} is retired, so not registered");
        assert_eq!(registry.tier_of(id), Some(Tier::Retired));
        assert_eq!(builtin_tier(id), Some(Tier::Retired));
    }
    // No registered heuristic declares itself retired.
    for id in registry.ids() {
        assert_ne!(registry.tier_of(id), Some(Tier::Retired), "{id}");
    }
    assert_eq!(registry.tier_of("land-never-shipped"), None);
    assert_eq!(builtin_tier("land-never-shipped"), None);
}

#[test]
fn tiers_serialise_as_snake_case_wire_names() {
    for tier in [Tier::Baseline, Tier::Candidate, Tier::Retired] {
        assert_eq!(serde_json::to_value(tier).unwrap(), tier.as_str());
        assert_eq!(tier.to_string(), tier.as_str());
    }
}

/// #10549: `land-2026-10-04-fresh-tide` lost its backtest and was retired.
/// It answers `retired`, is not registered, and cannot be promoted.
#[test]
fn fresh_tide_is_retired() {
    assert_eq!(builtin_tier(FRESH_TIDE), Some(Tier::Retired));
    assert!(!Registry::builtin().registers(Kind::Land, FRESH_TIDE));
    let decision = decide_with_passing_evidence(LAND_V1, FRESH_TIDE);
    assert!(!decision.promote);
    assert_eq!(decision.candidate_tier, Some(Tier::Retired));
}

#[test]
fn the_builtin_registry_fits_the_default_budget() {
    assert_eq!(DEFAULT_MAX_ACTIVE, 13);
    Registry::builtin()
        .check_budget(DEFAULT_MAX_ACTIVE)
        .expect("the shipped registry must fit the default shadow budget");
}

/// #10549: the default budget is the kind's `current` plus every alternate
/// one `eta.snapshot` row carries, so a registry within the default budget
/// never has a shadow the snapshot silently drops from the chooser.
#[test]
fn the_default_budget_is_current_plus_the_alternates_cap() {
    assert_eq!(MAX_ALTERNATES, 12);
    assert_eq!(DEFAULT_MAX_ACTIVE, MAX_ALTERNATES + 1);
}

#[test]
fn a_registry_over_budget_is_refused_naming_the_excess_in_registration_order() {
    let registry = Registry::builtin();
    let land: Vec<&str> = registry.for_kind(Kind::Land).map(|h| h.id()).collect();
    assert_eq!(land.len(), 10);
    // Exactly at the land count: fine.
    assert!(registry.check_budget(land.len()).is_ok());

    let over = registry.check_budget(3).unwrap_err();
    assert_eq!(
        over,
        BudgetExceeded {
            kind: Kind::Land,
            max_active: 3,
            registered: 10,
            excess: land[3..].to_vec(),
        }
    );
    let message = over.to_string();
    assert!(message.contains("maxActive is 3"), "{message}");
    assert!(message.contains("10 land heuristics"), "{message}");
    for id in &land[3..] {
        assert!(message.contains(id), "{message} names {id}");
    }
    for id in &land[..3] {
        assert!(!message.contains(&format!(" {id},")), "{message} does not name {id}");
    }

    // One over: only the last registration is the excess.
    let one = registry.check_budget(9).unwrap_err();
    assert_eq!(one.excess, ["land-2026-10-04-twin-otter-b"]);
}

#[test]
fn the_first_kind_over_budget_is_the_error() {
    let by_kind = vec![
        (Kind::Start, vec!["a", "b"]),
        (Kind::Land, vec!["c", "d", "e"]),
    ];
    let over = check_budget(by_kind.clone(), 1).unwrap_err();
    assert_eq!((over.kind, over.excess), (Kind::Start, vec!["b"]));
    let over = check_budget(by_kind.clone(), 2).unwrap_err();
    assert_eq!((over.kind, over.excess), (Kind::Land, vec!["e"]));
    assert!(check_budget(by_kind, 3).is_ok());
}

#[test]
fn max_active_follows_env_then_config_then_default_with_a_floor_of_one() {
    let no_env = |_: &str| None;
    assert_eq!(resolve(&json!({}), no_env).shadow_max_active, DEFAULT_MAX_ACTIVE);
    let five = json!({"autonomous": {"eta": {"shadow": {"maxActive": 5}}}});
    assert_eq!(resolve(&five, no_env).shadow_max_active, 5);
    let env = |key: &str| (key == "LOOM_ETA_SHADOW_MAX_ACTIVE").then(|| " 12 ".to_string());
    assert_eq!(resolve(&five, env).shadow_max_active, 12, "env beats config");
    let zero = json!({"autonomous": {"eta": {"shadow": {"maxActive": 0}}}});
    assert_eq!(resolve(&zero, no_env).shadow_max_active, 1, "floor");
    let typo = json!({"autonomous": {"eta": {"shadow": {"maxActive": "ten"}}}});
    assert_eq!(resolve(&typo, no_env).shadow_max_active, DEFAULT_MAX_ACTIVE);
    let bad_env = |key: &str| (key == "LOOM_ETA_SHADOW_MAX_ACTIVE").then(|| "x".to_string());
    assert_eq!(resolve(&five, bad_env).shadow_max_active, 5, "an unparsable env is ignored");
}

/// The same evidence that promotes a candidate, relabelled so `candidate` is
/// the challenger against `current`.
fn decide_with_passing_evidence(current: &str, candidate: &str) -> PromotionDecision {
    let stats = ledger_with(MIN_LIVE_PAIRS, 100.0, 60.0, MIN_LIVE_PAIRS / 2).stats(
        Kind::Land,
        LAND_V1,
        LAND_V2,
    );
    let mut evidence = comparison(1000.0, 800.0, 40);
    evidence.a.heuristic = current.to_string();
    evidence.b.heuristic = candidate.to_string();
    evidence.better = Some(candidate.to_string());
    shadow::evaluate(Kind::Land, current, candidate, Some(&evidence), &stats, as_of())
}

#[test]
fn only_a_candidate_is_promoted() {
    let control = decide_with_passing_evidence(LAND_V1, LAND_V2);
    assert!(control.promote, "{}", control.reason);
    assert_eq!(control.candidate_tier, Some(Tier::Candidate));

    // land-v1 is a baseline: both numerical gates pass, and still no flip.
    for baseline in [LAND_V1, LITTLE_V0] {
        let decision = decide_with_passing_evidence(LAND_V4, baseline);
        assert_eq!(decision.backtest.status, GateStatus::Passed, "{baseline}");
        assert_eq!(decision.live.status, GateStatus::Passed, "{baseline}");
        assert!(!decision.promote, "{baseline} is a baseline");
        assert_eq!(decision.candidate_tier, Some(Tier::Baseline));
        assert!(decision.reason.contains("baseline heuristic"), "{}", decision.reason);
        assert!(decision.reason.contains("#10525"), "{}", decision.reason);
    }

    let retired = decide_with_passing_evidence(LAND_V1, LAND_V3);
    assert!(!retired.promote);
    assert_eq!(retired.candidate_tier, Some(Tier::Retired));
    assert!(retired.reason.contains("retired heuristic"), "{}", retired.reason);
}

#[test]
fn the_tier_is_on_the_decision_record_and_an_older_record_still_parses() {
    let decision = decide_with_passing_evidence(LAND_V4, LAND_V1);
    let wire = serde_json::to_value(&decision).unwrap();
    assert_eq!(wire["candidate_tier"], "baseline");

    let mut old = wire;
    old.as_object_mut().unwrap().remove("candidate_tier");
    let back: PromotionDecision = serde_json::from_value(old).unwrap();
    assert_eq!(back.candidate_tier, None);
}
