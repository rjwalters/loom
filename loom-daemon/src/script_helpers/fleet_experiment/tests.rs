//! Fixture tests for #8055 phases 1–2: plan determinism and the start/stop
//! round trip.
//!
//! Nothing here touches the real registry, the real `~/.loom/experiments`, or
//! the real token pool: workspace roots are temp dirs, the state dir is a temp
//! dir, and the pool probe is injected (see [`super::lifecycle::PoolProbe`]).
//! No test mutates the environment, so none of them need `#[serial]` under the
//! crate's test-isolation convention.

#![allow(clippy::unwrap_used)]

use super::lifecycle::{
    marker_id, overlay_path, plan_overlay_edit, pointer_overlay, read_marker, remove_pointer,
    revert_overlay_edit, start, state_path, stop, Revert, StartOptions,
};
use super::{
    build_plan, merges_labels, repo_kind, shuffle_key, stratum_labels, Plan, WorkspaceInput,
    DISPATCH_MODEL_POINTER, MARKER_KEY, ROLE_RUNNER_MODEL_POINTER,
};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::{tempdir, TempDir};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 17, 12, 0, 0).unwrap()
}

fn arms() -> Vec<String> {
    vec!["opus".to_string(), "sonnet".to_string()]
}

fn dims(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

/// `n` measured workspaces under one temp root, alternating `kind` and with
/// descending merge counts, sorted by path the way `collect_inputs` returns
/// them.
fn inputs(root: &Path, n: usize) -> Vec<WorkspaceInput> {
    (0..n)
        .map(|i| WorkspaceInput {
            path: root.join(format!("repo-{i:02}")),
            repo: format!("acme/repo-{i:02}"),
            merges14d: Some(u32::try_from(n - i).unwrap() * 3),
            kind: if i % 2 == 0 {
                "rust".to_string()
            } else {
                "docs".to_string()
            },
        })
        .collect()
}

// -----------------------------------------------------------------------
// Phase 1 — plan
// -----------------------------------------------------------------------

#[test]
fn same_seed_yields_a_byte_identical_plan() {
    let dir = tempdir().unwrap();
    let ws = inputs(dir.path(), 8);
    let a = build_plan(&ws, &arms(), &dims(&["merges14d", "kind"]), 1234, now()).unwrap();
    let b = build_plan(&ws, &arms(), &dims(&["merges14d", "kind"]), 1234, now()).unwrap();
    assert_eq!(
        serde_json::to_string_pretty(&a).unwrap(),
        serde_json::to_string_pretty(&b).unwrap()
    );
}

#[test]
fn a_different_seed_yields_a_different_assignment() {
    let dir = tempdir().unwrap();
    let ws = inputs(dir.path(), 12);
    let a = build_plan(&ws, &arms(), &dims(&["kind"]), 1, now()).unwrap();
    let b = build_plan(&ws, &arms(), &dims(&["kind"]), 2, now()).unwrap();
    let arms_a: Vec<&str> = a.workspaces.iter().map(|w| w.arm.as_str()).collect();
    let arms_b: Vec<&str> = b.workspaces.iter().map(|w| w.arm.as_str()).collect();
    assert_ne!(arms_a, arms_b, "seed must change the assignment");
    assert_ne!(a.experiment_id, b.experiment_id);
}

#[test]
fn arm_sizes_are_balanced_within_each_stratum_and_fleet_wide() {
    let dir = tempdir().unwrap();
    for n in [2usize, 5, 8, 13, 24] {
        for seed in [7u64, 99, 2026] {
            let plan = build_plan(
                &inputs(dir.path(), n),
                &arms(),
                &dims(&["merges14d", "kind"]),
                seed,
                now(),
            )
            .unwrap();
            let mut per_stratum: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
            for w in &plan.workspaces {
                *per_stratum
                    .entry(w.stratum.as_str())
                    .or_default()
                    .entry(w.arm.as_str())
                    .or_default() += 1;
            }
            for (stratum, counts) in &per_stratum {
                let total: usize = counts.values().sum();
                let min = if counts.len() < 2 {
                    0
                } else {
                    *counts.values().min().unwrap()
                };
                let max = *counts.values().max().unwrap();
                assert!(
                    max - min <= 1,
                    "n={n} seed={seed} stratum {stratum} unbalanced: {counts:?} (total {total})"
                );
            }
            // ... and overall, which is what the cross-stratum deal counter buys:
            // restarting the deal in each stratum would give every singleton
            // stratum's member the same arm.
            let mut overall: BTreeMap<&str, usize> = BTreeMap::new();
            for w in &plan.workspaces {
                *overall.entry(w.arm.as_str()).or_default() += 1;
            }
            let min = if overall.len() < 2 {
                0
            } else {
                *overall.values().min().unwrap()
            };
            let max = *overall.values().max().unwrap();
            assert!(max - min <= 1, "n={n} seed={seed} fleet-wide unbalanced: {overall:?}");
        }
    }
}

#[test]
fn plan_assigns_every_registered_workspace_exactly_once() {
    let dir = tempdir().unwrap();
    let ws = inputs(dir.path(), 9);
    let plan = build_plan(&ws, &arms(), &dims(&["kind"]), 5, now()).unwrap();
    assert_eq!(plan.workspaces.len(), ws.len());
    for w in &plan.workspaces {
        assert!(arms().contains(&w.arm), "unassigned or unknown arm: {w:?}");
    }
    let paths: Vec<&str> = plan.workspaces.iter().map(|w| w.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted, "plan rows must be ordered by path");
}

#[test]
fn merges_labels_median_split_when_every_count_is_known() {
    let dir = tempdir().unwrap();
    let ws = inputs(dir.path(), 4); // counts 12, 9, 6, 3
    assert_eq!(merges_labels(&ws), vec!["high", "high", "low", "low"]);
}

#[test]
fn one_unmeasurable_repo_degrades_the_whole_dimension_to_the_alphabetical_fallback() {
    let dir = tempdir().unwrap();
    let mut ws = inputs(dir.path(), 4);
    ws[2].merges14d = None;
    assert_eq!(merges_labels(&ws), vec!["alpha-a", "alpha-a", "alpha-b", "alpha-b"]);
    // And the dimension still stratifies — it does not collapse to one bucket.
    let plan = build_plan(&ws, &arms(), &dims(&["merges14d"]), 3, now()).unwrap();
    let strata: Vec<&str> = plan.workspaces.iter().map(|w| w.stratum.as_str()).collect();
    assert_eq!(
        strata,
        vec![
            "merges14d=alpha-a",
            "merges14d=alpha-a",
            "merges14d=alpha-b",
            "merges14d=alpha-b"
        ]
    );
}

#[test]
fn unknown_stratify_dimension_is_an_error_not_a_silent_no_op() {
    let dir = tempdir().unwrap();
    let err = stratum_labels(&inputs(dir.path(), 2), &dims(&["stars"])).unwrap_err();
    assert!(err.to_string().contains("stars"), "{err}");
}

#[test]
fn repo_kind_is_first_match_wins_over_the_documented_order() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    assert_eq!(repo_kind(root), "docs");
    std::fs::create_dir(root.join("scripts")).unwrap();
    assert_eq!(repo_kind(root), "shell");
    std::fs::write(root.join("package.json"), "{}").unwrap();
    assert_eq!(repo_kind(root), "node");
    std::fs::write(root.join("Cargo.toml"), "").unwrap();
    assert_eq!(repo_kind(root), "rust");
}

#[test]
fn shuffle_key_is_stable_and_seed_sensitive() {
    assert_eq!(shuffle_key(1, "/a"), shuffle_key(1, "/a"));
    assert_ne!(shuffle_key(1, "/a"), shuffle_key(2, "/a"));
    assert_ne!(shuffle_key(1, "/a"), shuffle_key(1, "/b"));
}

#[test]
fn plan_refuses_a_single_arm_and_an_empty_fleet() {
    let dir = tempdir().unwrap();
    assert!(build_plan(&inputs(dir.path(), 4), &[String::from("opus")], &[], 1, now()).is_err());
    assert!(build_plan(&[], &arms(), &[], 1, now()).is_err());
}

// -----------------------------------------------------------------------
// Phase 2 — overlay edit (pure)
// -----------------------------------------------------------------------

#[test]
fn pointer_overlay_builds_the_nested_object() {
    assert_eq!(
        pointer_overlay(ROLE_RUNNER_MODEL_POINTER, json!("opus")),
        json!({"autonomous": {"roleRunner": {"model": "opus"}}})
    );
}

#[test]
fn remove_pointer_prunes_the_containers_it_empties_but_keeps_siblings() {
    let mut doc = json!({"autonomous": {"roleRunner": {"model": "opus"}, "model": "opus"}});
    remove_pointer(&mut doc, ROLE_RUNNER_MODEL_POINTER);
    assert_eq!(doc, json!({"autonomous": {"model": "opus"}}));
    remove_pointer(&mut doc, DISPATCH_MODEL_POINTER);
    assert_eq!(doc, json!({}));

    let mut kept = json!({"autonomous": {"model": "opus", "workFinder": {"maxConcurrent": 3}}});
    remove_pointer(&mut kept, DISPATCH_MODEL_POINTER);
    assert_eq!(kept, json!({"autonomous": {"workFinder": {"maxConcurrent": 3}}}));
}

#[test]
fn overlay_edit_preserves_unrelated_keys_and_records_the_prior_value() {
    let before = json!({
        "autonomous": {"workFinder": {"maxConcurrent": 2}, "model": "sonnet"},
        "operatorNote": "hands off"
    });
    let (after, marker) =
        plan_overlay_edit(&before, "exp-1", "opus", "2026-09-17T12:00:00Z", Some("2026-09-28"))
            .unwrap();

    assert_eq!(after["operatorNote"], json!("hands off"));
    assert_eq!(after["autonomous"]["workFinder"]["maxConcurrent"], json!(2));
    assert_eq!(after["autonomous"]["model"], json!("opus"));
    assert_eq!(after["autonomous"]["roleRunner"]["model"], json!("opus"));
    assert_eq!(marker_id(&after).as_deref(), Some("exp-1"));
    assert_eq!(marker.until.as_deref(), Some("2026-09-28"));
    assert_eq!(marker.prior[DISPATCH_MODEL_POINTER].value, Some(json!("sonnet")));
    assert!(!marker.prior[ROLE_RUNNER_MODEL_POINTER].present);
}

#[test]
fn revert_restores_a_pre_existing_pin_rather_than_deleting_it() {
    let before = json!({"autonomous": {"model": "sonnet"}});
    let (after, _) =
        plan_overlay_edit(&before, "exp-1", "opus", "2026-09-17T12:00:00Z", None).unwrap();
    match revert_overlay_edit(&after, "exp-1", false) {
        Revert::Reverted(Some(doc)) => assert_eq!(doc, before),
        other => panic!("expected a revert, got {other:?}"),
    }
}

#[test]
fn revert_leaves_another_experiments_overlay_alone() {
    let (after, _) =
        plan_overlay_edit(&json!({}), "exp-other", "opus", "2026-09-17T12:00:00Z", None).unwrap();
    assert!(matches!(
        revert_overlay_edit(&after, "exp-mine", false),
        Revert::OtherExperiment(id) if id == "exp-other"
    ));
    assert!(matches!(revert_overlay_edit(&json!({}), "exp-mine", false), Revert::NotOurs));
}

// -----------------------------------------------------------------------
// Phase 2 — start / stop against a temp registry
// -----------------------------------------------------------------------

struct Fleet {
    _root: TempDir,
    state: TempDir,
    roots: Vec<PathBuf>,
    plan: Plan,
}

fn never_held(_root: &Path, _now: DateTime<Utc>) -> bool {
    false
}

fn always_held(_root: &Path, _now: DateTime<Utc>) -> bool {
    true
}

fn fleet(n: usize) -> Fleet {
    let root = tempdir().unwrap();
    let state = tempdir().unwrap();
    let ws = inputs(root.path(), n);
    for w in &ws {
        std::fs::create_dir_all(&w.path).unwrap();
    }
    let plan = build_plan(&ws, &arms(), &dims(&["kind"]), 42, now()).unwrap();
    let roots = ws.into_iter().map(|w| w.path).collect();
    Fleet {
        _root: root,
        state,
        roots,
        plan,
    }
}

fn start_opts<'a>(
    f: &'a Fleet,
    allow: bool,
    probe: super::lifecycle::PoolProbe,
) -> StartOptions<'a> {
    StartOptions {
        plan: &f.plan,
        until: Some("2026-09-28"),
        allow_exhausted_pool: allow,
        state_dir: f.state.path(),
        now: now(),
        pool_held: probe,
    }
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn start_writes_one_overlay_per_workspace_and_records_the_plan() {
    let f = fleet(4);
    let report = start(start_opts(&f, false, never_held)).unwrap();
    assert_eq!(report.started.len(), 4);
    assert!(report.state_file.is_file());

    for (root, arm) in &report.started {
        let doc = read(&overlay_path(root));
        assert_eq!(doc["autonomous"]["model"], json!(arm));
        assert_eq!(doc["autonomous"]["roleRunner"]["model"], json!(arm));
        let marker = read_marker(&doc).unwrap();
        assert_eq!(marker.id, f.plan.experiment_id);
        assert_eq!(marker.until.as_deref(), Some("2026-09-28"));
        assert_eq!(marker.keys, vec![DISPATCH_MODEL_POINTER, ROLE_RUNNER_MODEL_POINTER]);
    }
    let state = read(&state_path(f.state.path(), &f.plan.experiment_id));
    assert_eq!(state["experiment_id"], json!(f.plan.experiment_id));
    assert_eq!(state["plan"]["workspaces"].as_array().unwrap().len(), 4);
    assert_eq!(state["until"], json!("2026-09-28"));
}

#[test]
fn start_then_stop_restores_every_overlay_byte_for_byte() {
    let f = fleet(3);
    // One workspace already has a hand-written overlay; the other two have none.
    let hand_written = "{\n  \"autonomous\": {\n    \"model\": \"haiku\",\n    \"workFinder\": {\n      \"maxConcurrent\": 1\n    }\n  }\n}\n";
    let touched = overlay_path(&f.roots[0]);
    std::fs::create_dir_all(touched.parent().unwrap()).unwrap();
    std::fs::write(&touched, hand_written).unwrap();

    start(start_opts(&f, false, never_held)).unwrap();
    assert_eq!(read(&touched)["autonomous"]["workFinder"]["maxConcurrent"], json!(1));
    assert!(overlay_path(&f.roots[1]).is_file());

    let (report, state) = stop(f.state.path(), &f.plan.experiment_id, false, now()).unwrap();
    assert!(report.drifted.is_empty(), "{report:?}");
    assert_eq!(report.reverted.len(), 1, "only the pre-existing overlay survives");
    assert_eq!(report.deleted.len(), 2, "overlays created by start are removed");
    assert_eq!(std::fs::read_to_string(&touched).unwrap(), hand_written);
    assert!(!overlay_path(&f.roots[1]).exists());
    assert!(
        !f.roots[1].join(".loom-local").exists(),
        "a directory `start` created is removed with the file"
    );
    assert!(
        touched.parent().unwrap().exists(),
        "a directory that still holds an operator file stays"
    );
    assert_eq!(state.plan.experiment_id, f.plan.experiment_id);

    let stopped = read(&state_path(f.state.path(), &f.plan.experiment_id));
    assert!(stopped["stopped_at"].is_string());
}

#[test]
fn start_twice_on_a_workspace_it_already_owns_preserves_the_true_prior() {
    let f = fleet(1);
    // An operator pin exists before the experiment ever starts.
    let touched = overlay_path(&f.roots[0]);
    let hand_written = json!({"autonomous": {"model": "haiku"}});
    std::fs::create_dir_all(touched.parent().unwrap()).unwrap();
    std::fs::write(&touched, serde_json::to_string_pretty(&hand_written).unwrap()).unwrap();

    start(start_opts(&f, false, never_held)).unwrap();
    let after_first = read_marker(&read(&touched)).unwrap();
    assert_eq!(
        after_first.prior.get(DISPATCH_MODEL_POINTER).unwrap().value,
        Some(json!("haiku")),
        "first start must record the operator's true pre-experiment pin"
    );
    assert!(
        !after_first
            .prior
            .get(ROLE_RUNNER_MODEL_POINTER)
            .unwrap()
            .present,
        "the role-runner pointer never existed before start"
    );

    // Retry/resume: re-run `start` against the same fixture and experiment id
    // without an intervening `stop` (e.g. recovering from a crash partway
    // through a fleet-wide start).
    start(start_opts(&f, false, never_held)).unwrap();
    let after_second = read_marker(&read(&touched)).unwrap();
    assert_eq!(
        after_second.prior.get(DISPATCH_MODEL_POINTER).unwrap().value,
        Some(json!("haiku")),
        "a second start must not overwrite the recorded prior with the arm value the first start wrote"
    );
    assert!(
        !after_second
            .prior
            .get(ROLE_RUNNER_MODEL_POINTER)
            .unwrap()
            .present
    );

    let (report, _) = stop(f.state.path(), &f.plan.experiment_id, false, now()).unwrap();
    assert_eq!(report.reverted.len(), 1, "{report:?}");
    assert_eq!(
        read(&touched)["autonomous"]["model"],
        json!("haiku"),
        "stop must restore the operator's true pin, not the arm value start wrote"
    );
}

#[test]
fn start_refuses_when_another_experiment_already_owns_a_workspace_and_writes_nothing() {
    let f = fleet(3);
    let squatted = overlay_path(&f.roots[2]);
    let (doc, _) = plan_overlay_edit(
        &serde_json::json!({}),
        "exp-other",
        "sonnet",
        "2026-09-01T00:00:00Z",
        None,
    )
    .unwrap();
    std::fs::create_dir_all(squatted.parent().unwrap()).unwrap();
    std::fs::write(&squatted, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

    let err = start(start_opts(&f, false, never_held)).unwrap_err();
    assert!(err.to_string().contains("exp-other"), "{err}");
    assert!(!overlay_path(&f.roots[0]).exists(), "no overlay may be written on refusal");
    assert!(!state_path(f.state.path(), &f.plan.experiment_id).exists());
    // The squatter is untouched.
    assert_eq!(marker_id(&read(&squatted)).as_deref(), Some("exp-other"));
}

#[test]
fn start_refuses_on_a_held_token_pool_unless_the_override_is_passed() {
    let f = fleet(2);
    let err = start(start_opts(&f, false, always_held)).unwrap_err();
    assert!(err.to_string().contains("token pool"), "{err}");
    assert!(!overlay_path(&f.roots[0]).exists());
    assert!(!state_path(f.state.path(), &f.plan.experiment_id).exists());

    let report = start(start_opts(&f, true, always_held)).unwrap();
    assert_eq!(report.started.len(), 2);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("--allow-exhausted-pool")),
        "the override must be recorded as a warning: {:?}",
        report.warnings
    );
}

#[test]
fn stop_leaves_a_hand_edited_overlay_in_place_until_forced() {
    let f = fleet(1);
    start(start_opts(&f, false, never_held)).unwrap();
    let overlay = overlay_path(&f.roots[0]);
    let mut doc = read(&overlay);
    doc["autonomous"]["model"] = json!("haiku");
    std::fs::write(&overlay, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

    let (report, _) = stop(f.state.path(), &f.plan.experiment_id, false, now()).unwrap();
    assert_eq!(report.drifted.len(), 1, "{report:?}");
    assert!(report.reverted.is_empty() && report.deleted.is_empty());
    assert_eq!(read(&overlay)["autonomous"]["model"], json!("haiku"));
    // The state file is NOT stamped stopped while a workspace is still drifted.
    assert!(read(&state_path(f.state.path(), &f.plan.experiment_id))["stopped_at"].is_null());

    let (forced, _) = stop(f.state.path(), &f.plan.experiment_id, true, now()).unwrap();
    assert!(forced.drifted.is_empty());
    assert!(!overlay.exists(), "forced stop removes the drifted overlay too");
}

#[test]
fn stop_is_idempotent_and_skips_workspaces_it_does_not_own() {
    let f = fleet(2);
    start(start_opts(&f, false, never_held)).unwrap();
    stop(f.state.path(), &f.plan.experiment_id, false, now()).unwrap();
    let (again, _) = stop(f.state.path(), &f.plan.experiment_id, false, now()).unwrap();
    assert_eq!(again.skipped.len(), 2, "{again:?}");
    assert!(again.skipped.iter().all(|s| s.contains(MARKER_KEY)));
}

#[test]
fn stop_without_a_state_file_is_an_error() {
    let dir = tempdir().unwrap();
    assert!(stop(dir.path(), "exp-nope", false, now()).is_err());
}
