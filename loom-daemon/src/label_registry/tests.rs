//! Schema, generator-drift and per-table lockstep tests (#10013 slice 1).
//!
//! Each lockstep test asserts one hand-listed daemon label table equals the
//! registry query for the matching property, so converting the table to a
//! registry lookup (slice 2) is a pure swap.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::*;

fn reg() -> &'static Registry {
    Registry::embedded()
}

fn set<'a>(it: impl IntoIterator<Item = &'a str>) -> BTreeSet<&'a str> {
    it.into_iter().collect()
}

fn prop(p: &str) -> BTreeSet<&'static str> {
    set(reg().with_property(p).expect("known property"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

// --- schema -----------------------------------------------------------------

#[test]
fn embedded_registry_parses_and_validates() {
    Registry::parse(REGISTRY_JSON).unwrap();
}

#[test]
fn validation_rejects_bad_registries() {
    let good: serde_json::Value = serde_json::from_str(REGISTRY_JSON).unwrap();
    let mutate = |f: &dyn Fn(&mut serde_json::Value)| {
        let mut v = good.clone();
        f(&mut v);
        Registry::parse(&v.to_string())
    };
    assert!(mutate(&|v| v["labels"][0]["color"] = "zz".into()).is_err());
    assert!(mutate(&|v| v["labels"][0]["kind"] = "bogus".into()).is_err());
    assert!(mutate(&|v| v["labels"][0]["extra"] = true.into()).is_err());
    assert!(mutate(&|v| v["labels"][1]["name"] = v["labels"][0]["name"].clone()).is_err());
    assert!(mutate(&|v| v["labels"][0]["description"] = "x".repeat(101).into()).is_err());
    assert!(mutate(&|v| v["labels"][0]["requires_base"] = "loom:nope".into()).is_err());
}

#[test]
fn registry_covers_every_label_in_both_labels_yml_copies() {
    for rel in [".github/labels.yml", "defaults/.github/labels.yml"] {
        let yml = std::fs::read_to_string(repo_root().join(rel)).unwrap();
        let yml_names: BTreeSet<String> = generate::block_names(&yml).into_iter().collect();
        let reg_names: BTreeSet<String> = reg().labels.iter().map(|l| l.name.clone()).collect();
        assert_eq!(
            yml_names, reg_names,
            "{rel}: a label exists only in one of labels.yml / labels.json"
        );
    }
}

#[test]
fn on_disk_registry_equals_embedded() {
    let disk = std::fs::read_to_string(repo_root().join("defaults/labels.json")).unwrap();
    assert_eq!(disk, REGISTRY_JSON);
}

// --- generator drift --------------------------------------------------------

#[test]
fn both_full_labels_yml_copies_equal_the_generated_block() {
    let block = generate::loom_block(reg());
    for rel in [".github/labels.yml", "defaults/.github/labels.yml"] {
        let yml = std::fs::read_to_string(repo_root().join(rel)).unwrap();
        assert!(
            yml == block,
            "{rel} drifted from defaults/labels.json; run `loom-daemon labels generate --write`"
        );
    }
}

#[test]
fn generator_output_is_a_marker_block() {
    let block = generate::loom_block(reg());
    assert!(block.starts_with("# BEGIN LOOM LABELS\n"));
    assert!(block.ends_with("# END LOOM LABELS\n"));
}

// --- query API --------------------------------------------------------------

#[test]
fn queries_answer_and_reject_unknowns() {
    assert_eq!(reg().with_property("park").unwrap(), vec!["loom:blocked", "loom:operator-only"]);
    assert!(reg().with_property("nonsense").is_none());
    assert!(reg().get("loom:nope").is_none());
    assert_eq!(reg().get("loom:pr").unwrap().kind, "pr-lane");
}

// --- lockstep: one test per existing hand-listed table -----------------------

#[test]
fn lockstep_park_labels() {
    assert_eq!(set(crate::work_finder::PARK_LABELS.iter().copied()), prop("park"));
}

#[test]
fn lockstep_skip_labels_and_hold_decision_labels() {
    use crate::work_finder::{OPERATOR_HOLD_LABEL, SKIP_LABELS};
    assert_eq!(set(SKIP_LABELS.iter().copied()), prop("skip"));
    // OPERATOR_HOLD_LABEL is a skip label but never a park.
    assert!(prop("skip").contains(OPERATOR_HOLD_LABEL));
    assert!(!prop("park").contains(OPERATOR_HOLD_LABEL));
    // OPERATOR_DECISION_LABEL (private const): the one skip label that is
    // neither claim, park nor the generic hold.
    let rest: BTreeSet<&str> = prop("skip")
        .into_iter()
        .filter(|n| {
            reg().get(n).unwrap().kind != "claim"
                && !prop("park").contains(n)
                && *n != OPERATOR_HOLD_LABEL
        })
        .collect();
    assert_eq!(rest, set(["loom:operator-decision"]));
    assert_eq!(
        set(reg()
            .with_kind("claim")
            .into_iter()
            .filter(|n| prop("skip").contains(n))),
        set([crate::work_finder::BUILDING_LABEL])
    );
}

#[test]
fn lockstep_hard_exclusion_labels() {
    assert_eq!(
        set(crate::hard_exclusion::HARD_EXCLUSION_LABELS.iter().copied()),
        prop("hard_exclusion")
    );
}

#[test]
fn lockstep_operator_gate_labels() {
    assert_eq!(
        set(crate::pr_latency::OPERATOR_GATE_LABELS.iter().copied()),
        prop("operator_gate")
    );
}

#[test]
fn lockstep_pr_latency_hold_labels() {
    let all: Vec<&str> = reg().labels.iter().map(|l| l.name.as_str()).collect();
    let got = crate::pr_latency::hold_labels(&all);
    assert_eq!(set(got.iter().map(String::as_str)), prop("hold"));
}

#[test]
fn lockstep_eta_hold_labels() {
    // eta::labels::hold_labels = blocked + park + skip (minus the claim) +
    // queue_blocked's co-labels.
    let mut want: BTreeSet<&str> = BTreeSet::new();
    want.insert(crate::observability::queue_blocked::BLOCKED_LABEL);
    want.extend(prop("park"));
    want.extend(
        prop("skip")
            .into_iter()
            .filter(|n| reg().get(n).unwrap().kind != "claim"),
    );
    want.extend(prop("blocked_colabel"));
    assert_eq!(set(crate::eta::labels::hold_labels()), want);
    assert_eq!(
        set(crate::observability::queue_blocked::HOLD_LABELS
            .iter()
            .copied()),
        prop("blocked_colabel")
    );
}

#[test]
fn lockstep_merge_pr_blocking_in_order() {
    assert_eq!(
        crate::merge_pr::labels::BLOCKING.to_vec(),
        reg().with_property("contradicts_approval").unwrap()
    );
}

#[test]
fn lockstep_human_gated_labels() {
    assert_eq!(set(crate::eta::labels::HUMAN_GATED_LABELS.iter().copied()), prop("human_gated"));
}

#[test]
fn lockstep_dep_classify_operator_only_kinds() {
    use crate::dep_classify::consts::{
        OPERATOR_BLOCKED_LABEL, OPERATOR_DECISION_LABEL, OPERATOR_ONLY_LABEL,
    };
    let base = reg().get(OPERATOR_ONLY_LABEL).unwrap();
    assert!(base.park && !base.remove_with.is_empty());
    for sub in [OPERATOR_BLOCKED_LABEL, OPERATOR_DECISION_LABEL] {
        assert!(base.remove_with.iter().any(|r| r == sub), "{sub} not paired for removal");
        assert_eq!(reg().get(sub).unwrap().requires_base.as_deref(), Some(OPERATOR_ONLY_LABEL));
    }
    // Every label that requires the base is paired for removal.
    let requiring: BTreeSet<&str> = reg()
        .labels
        .iter()
        .filter(|l| l.requires_base.as_deref() == Some(OPERATOR_ONLY_LABEL))
        .map(|l| l.name.as_str())
        .collect();
    assert_eq!(requiring, set(base.remove_with.iter().map(String::as_str)));
}

#[test]
fn lockstep_champion_path_labels() {
    assert_eq!(
        set(crate::work_finder::operator_priority::CHAMPION_PATH_LABELS
            .iter()
            .copied()),
        prop("champion_path")
    );
}
