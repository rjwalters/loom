//! Grid, generator, ids, labels and provenance.

use super::{as_of, provenance, subject};
use crate::eta::labels::{
    check_holds, hold_labels, stage_from_pr_labels, unstarted_issue_reason, CHANGES_REQUESTED,
    TREATING,
};
use crate::eta::simulate::SplitMix64;
use crate::eta::{
    estimate_id, grid, seed_for, Heuristic, Kind, NoEstimateReason, Provenance, Registry, Stage,
};

fn labels(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn grid_is_nearest_rank_over_observed_values() {
    let sorted: Vec<i64> = (1..=20).map(|i| i * 10).collect();
    let g = grid::grid_of(&sorted);
    assert_eq!(g.len(), grid::GRID_POINTS);
    assert_eq!(g[0], 10);
    assert_eq!(g[20], 200);
    assert_eq!(g[10], 100); // p50: ceil(0.5 * 20) = 10th value
    assert!(g.iter().all(|v| sorted.contains(v)), "every grid value is observed");
    assert_eq!(grid::grid_pct(), (0..=100).step_by(5).map(|p| p as u8).collect::<Vec<_>>());
}

#[test]
fn cdf_inverts_inv_cdf() {
    let sorted: Vec<i64> = (1..=40).map(|i| i * i * 7).collect();
    let g = grid::grid_of(&sorted);
    assert_eq!(grid::cdf(&g, 0), 0.0);
    assert_eq!(grid::cdf(&g, 1_000_000), 1.0);
    for age in [g[0], 100, 1000, 5000, g[19]] {
        let u = grid::cdf(&g, age);
        assert!((grid::inv_cdf(&g, u) - age as f64).abs() < 1e-6, "age {age}");
    }
}

#[test]
fn cdf_takes_the_top_of_a_flat_run() {
    let g = vec![
        0, 0, 0, 0, 10, 10, 10, 10, 20, 20, 20, 20, 30, 30, 30, 30, 40, 40, 40, 40, 50,
    ];
    let u = grid::cdf(&g, 10);
    assert!(grid::inv_cdf(&g, u) >= 10.0);
    assert!(u >= 7.0 / 20.0);
}

#[test]
fn splitmix64_is_the_reference_sequence() {
    // Reference values of SplitMix64 seeded with 0 (Vigna's splitmix64.c).
    let mut rng = SplitMix64::new(0);
    assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
    assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
    let mut rng = SplitMix64::new(42);
    for _ in 0..1000 {
        let u = rng.next_f64();
        assert!((0.0..1.0).contains(&u));
    }
}

#[test]
fn estimate_id_is_derived_and_stable() {
    let at = as_of();
    let a = estimate_id(&subject(), Kind::Land, "land-v1", at);
    let b = estimate_id(&subject(), Kind::Land, "land-v1", at);
    assert_eq!(a, b, "same inputs, same id");
    assert_eq!(a.len(), 16);
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    let later =
        estimate_id(&subject(), Kind::Land, "land-v1", at + chrono::Duration::nanoseconds(1));
    assert_ne!(a, later, "as_of is in the key at nanosecond precision");
    assert_ne!(a, estimate_id(&subject(), Kind::Finish, "land-v1", at));
    assert_ne!(a, estimate_id(&subject(), Kind::Land, "land-v2", at));
    let mut other = subject();
    other.issue += 1;
    assert_ne!(a, estimate_id(&other, Kind::Land, "land-v1", at));
    // The derivation itself, spelled out: a changed key is a new tag, never a
    // silent change of inputs (trace-identity policy).
    let expected = crate::telemetry::trace::derived_hex(
        &[
            "loom.eta.estimate",
            "github:1073994527",
            "9289",
            "land",
            "land-v1",
            "2026-09-20T12:00:00.000000000Z",
        ],
        16,
    );
    assert_eq!(a, expected);
    assert_eq!(seed_for(&a), seed_for(&b));
    assert_ne!(seed_for(&a), seed_for(&later));
}

#[test]
fn estimate_id_without_repo_id_keys_on_the_slug() {
    let mut s = subject();
    s.repo_id = None;
    let with_slug = estimate_id(&s, Kind::Land, "land-v1", as_of());
    assert_ne!(with_slug, estimate_id(&subject(), Kind::Land, "land-v1", as_of()));
    s.repo = "RJWalters/Loom".to_string();
    assert_eq!(with_slug, estimate_id(&s, Kind::Land, "land-v1", as_of()));
}

#[test]
fn blocked_labels_return_no_estimate() {
    let mut required = vec![
        "loom:blocked",
        "loom:operator",
        "loom:operator-only",
        "loom:needs-capability",
    ];
    required.extend(crate::work_finder::PARK_LABELS.iter().copied());
    for hold in required {
        assert!(hold_labels().contains(&hold), "{hold} is a hold label");
        for stage_label in ["loom:review-requested", "loom:changes-requested", "loom:pr"] {
            // #10218: an operator hold on an approved PR is the `merge_hold`
            // stage (which every shipped heuristic still refuses `blocked`).
            let expected = if stage_label == "loom:pr"
                && crate::eta::labels::MERGE_HOLD_LABELS.contains(&hold)
            {
                Ok(Stage::MergeHold)
            } else {
                Err(NoEstimateReason::Blocked)
            };
            assert_eq!(
                stage_from_pr_labels(&labels(&[stage_label, hold])),
                expected,
                "{hold} on {stage_label}"
            );
        }
        assert_eq!(
            unstarted_issue_reason(&labels(&["loom:issue", hold]), None),
            Some(NoEstimateReason::Blocked)
        );
    }
    assert!(!hold_labels().contains(&"loom:building"), "the claim label is not a hold");
    assert_eq!(check_holds(&labels(&["loom:building"])), Ok(()));
}

#[test]
fn unknown_stage_on_contradictory_labels() {
    for pair in [
        ["loom:review-requested", "loom:pr"],
        ["loom:review-requested", "loom:changes-requested"],
        ["loom:changes-requested", "loom:pr"],
    ] {
        assert_eq!(stage_from_pr_labels(&labels(&pair)), Err(NoEstimateReason::UnknownStage));
    }
    assert_eq!(stage_from_pr_labels(&[]), Err(NoEstimateReason::UnknownStage));
}

#[test]
fn pr_labels_resolve_to_stages() {
    assert_eq!(stage_from_pr_labels(&labels(&["loom:review-requested"])), Ok(Stage::ReviewWait));
    assert_eq!(stage_from_pr_labels(&labels(&[CHANGES_REQUESTED, TREATING])), Ok(Stage::Doctor));
    assert_eq!(stage_from_pr_labels(&labels(&[TREATING])), Ok(Stage::Doctor));
    assert_eq!(stage_from_pr_labels(&labels(&["loom:pr"])), Ok(Stage::MergeWait));
    assert_eq!(
        unstarted_issue_reason(&labels(&["loom:curated"]), None),
        Some(NoEstimateReason::HumanGated)
    );
    assert_eq!(
        unstarted_issue_reason(&labels(&["loom:issue"]), None),
        Some(NoEstimateReason::NoDispatchPlan)
    );
}

#[test]
fn provenance_is_the_span_provenance_source() {
    let current = Provenance::current();
    let build = crate::telemetry::trace::provenance::daemon();
    assert_eq!(current.version, build.version);
    assert_eq!(current.revision, build.revision);
    assert_eq!(current.tree_state, build.tree_state);
    assert_eq!(current.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(current.revision, crate::self_update::BUILT_COMMIT_FULL);
    assert!(current.is_valid(), "{current:?}");
    assert_eq!(
        current.complete,
        Provenance::completeness(&current.revision, &current.tree_state)
    );
}

#[test]
fn provenance_validation_requires_full_sha_and_known_state() {
    assert!(provenance().is_valid());
    let mut short = provenance();
    short.revision = "bf2fb67".to_string();
    assert!(!short.is_valid(), "a short sha is not provenance");
    let mut empty = provenance();
    empty.version = String::new();
    assert!(!empty.is_valid());
    let mut state = provenance();
    state.tree_state = "maybe".to_string();
    assert!(!state.is_valid());
    // A tarball build reports `unknown`, like every span: still emitted (no
    // data is lost) but marked incomplete, so accuracy queries exclude it.
    let mut tarball = provenance();
    tarball.revision = "unknown".to_string();
    assert!(!tarball.is_valid(), "`complete` must match the fields");
    tarball.complete = false;
    assert!(tarball.is_valid());
    let mut unknown_tree = provenance();
    unknown_tree.tree_state = "unknown".to_string();
    unknown_tree.complete = false;
    assert!(unknown_tree.is_valid());
    assert!(!Provenance::completeness("unknown", "clean"));
    assert!(!Provenance::completeness(&provenance().revision, "unknown"));
    assert!(Provenance::completeness(&provenance().revision, "dirty"));
    // A record may not claim completeness it does not have.
    let mut claims = provenance();
    claims.revision = "unknown".to_string();
    claims.complete = true;
    assert!(!claims.is_valid());
}

#[test]
fn registry_resolves_current_per_kind() {
    let registry = Registry::builtin();
    assert_eq!(
        registry.ids(),
        vec![
            "start-v1",
            "finish-v1",
            "land-v1",
            "land-v2",
            "land-2026-10-04-fresh-tide",
            "land-v4",
            "land-2026-10-04-twin-otter",
            "land-2026-10-04-twin-otter-b"
        ]
    );
    assert_eq!(registry.current(Kind::Land, None).id(), "land-v1");
    assert_eq!(registry.current(Kind::Finish, None).id(), "finish-v1");
    // A configured id of the wrong kind, or an unknown one, falls back.
    assert_eq!(registry.current(Kind::Land, Some("finish-v1")).id(), "land-v1");
    assert_eq!(registry.current(Kind::Land, Some("land-v9")).id(), "land-v1");
    // A registered candidate IS selectable as current — that is what the
    // promotion switch flips (#9328).
    assert_eq!(registry.current(Kind::Land, Some("land-v2")).id(), "land-v2");
    // `land-v3` (#9970) and `land-2026-10-04-amber-heron` (#10207) were
    // retired from the registry 2026-10-06 (#10484): selecting either falls
    // back to the default.
    assert_eq!(registry.current(Kind::Land, Some("land-v3")).id(), "land-v1");
    assert_eq!(
        registry
            .current(Kind::Land, Some("land-2026-10-04-amber-heron"))
            .id(),
        "land-v1"
    );
    // `land-2026-10-04-fresh-tide` (#10209) likewise: registered, not current.
    assert_eq!(
        registry
            .current(Kind::Land, Some("land-2026-10-04-fresh-tide"))
            .id(),
        "land-2026-10-04-fresh-tide"
    );
    // `land-2026-10-04-twin-otter` (#10243) likewise: registered last, as a
    // shadow, and refusing `no_model` in `builtin()`, which loads no fit.
    assert_eq!(
        registry
            .current(Kind::Land, Some("land-2026-10-04-twin-otter"))
            .id(),
        "land-2026-10-04-twin-otter"
    );
    // `land-v4` (#10210) likewise.
    assert_eq!(registry.current(Kind::Land, Some("land-v4")).id(), "land-v4");
    assert_eq!(Registry::default_current(Kind::Land), "land-v1");
}

#[test]
fn for_kind_enumerates_every_registered_heuristic_of_a_kind() {
    let registry = Registry::builtin();
    // The shadow-mode input (#9328): `current` is one of these, not all of it.
    let land: Vec<&str> = registry.for_kind(Kind::Land).map(Heuristic::id).collect();
    assert_eq!(
        land,
        vec![
            "land-v1",
            "land-v2",
            "land-2026-10-04-fresh-tide",
            "land-v4",
            "land-2026-10-04-twin-otter",
            "land-2026-10-04-twin-otter-b"
        ]
    );
    assert_eq!(
        registry
            .for_kind(Kind::Finish)
            .map(Heuristic::id)
            .collect::<Vec<_>>(),
        vec!["finish-v1"]
    );
    assert_eq!(
        registry
            .for_kind(Kind::Start)
            .map(Heuristic::id)
            .collect::<Vec<_>>(),
        vec!["start-v1"]
    );
    // Every id the registry knows is reachable through exactly one kind.
    let mut all: Vec<&str> = [Kind::Start, Kind::Finish, Kind::Land]
        .into_iter()
        .flat_map(|k| registry.for_kind(k).map(Heuristic::id).collect::<Vec<_>>())
        .collect();
    all.sort_unstable();
    let mut ids = registry.ids();
    ids.sort_unstable();
    assert_eq!(all, ids);
}
