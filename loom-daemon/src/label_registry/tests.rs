//! Schema, generator-drift and per-table lockstep tests (#10013 slice 1).
//!
//! Each lockstep test asserts one hand-listed daemon label table equals the
//! registry query for the matching property, so converting the table to a
//! registry lookup (slice 2b) is a pure swap. Tables already derived from the
//! registry (slice 2a) are pinned to the literal they replaced instead.

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
    // A park label that is not also a skip label (#4444).
    let park_only = |v: &mut serde_json::Value| {
        v["labels"][0]["park"] = true.into();
        v["labels"][0]["skip"] = false.into();
    };
    assert!(mutate(&park_only).is_err());
}

#[test]
fn validation_rejects_bad_propagate() {
    let good: serde_json::Value = serde_json::from_str(REGISTRY_JSON).unwrap();
    let idx = |name: &str| reg().labels.iter().position(|l| l.name == name).unwrap();
    let (ext, tier) = (idx("external"), idx("tier:maintenance"));
    let mutate = |i: usize, key: &str, val: serde_json::Value| {
        let mut v = good.clone();
        v["labels"][i]["propagate"][key] = val;
        Registry::parse(&v.to_string())
    };
    assert!(mutate(ext, "direction", "child-to-parent".into()).is_err());
    assert!(mutate(ext, "add", "sometimes".into()).is_err());
    assert!(mutate(ext, "bogus", true.into()).is_err());
    // A rank gap, and two rules sharing a rank.
    assert!(mutate(ext, "rank", 9.into()).is_err());
    assert!(mutate(ext, "rank", 1.into()).is_err());
    // A family default without a family; a family that is not the prefix.
    assert!(mutate(tier, "family", serde_json::Value::Null).is_err());
    assert!(mutate(ext, "family", "tier:".into()).is_err());
    // Family members must agree.
    assert!(mutate(tier, "to_prs", true.into()).is_err());
}

#[test]
fn propagate_holds_the_10012_table() {
    let named: Vec<(&str, u32)> = reg()
        .labels
        .iter()
        .filter_map(|l| l.propagate.as_ref().map(|p| (l.name.as_str(), p.rank)))
        .collect();
    assert_eq!(
        named,
        vec![
            ("loom:operator-priority", 1),
            ("tier:goal-advancing", 3),
            ("tier:goal-supporting", 3),
            ("tier:maintenance", 3),
            ("external", 2),
        ]
    );
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

// --- slice 2a: tables derived from the registry ------------------------------
//
// These sets are no longer hand-listed, so a lockstep test against the
// registry would be a tautology. Instead each derived set is pinned to the
// literal it replaced (#10013 slice 2a: no behavior change). Changing one of
// these sets is a semantic change: edit `defaults/labels.json` and the pin in
// the same PR.

#[test]
fn derived_park_labels_equal_the_previous_literal_in_order() {
    // Order matters: the park guards report the first match in this order.
    assert_eq!(*crate::work_finder::PARK_LABELS, ["loom:blocked", "loom:operator-only"]);
}

#[test]
fn derived_skip_labels_equal_the_previous_literal_in_order() {
    assert_eq!(
        *crate::work_finder::SKIP_LABELS,
        [
            "loom:building",
            "loom:blocked",
            "loom:operator-only",
            "loom:operator",
            "loom:operator-decision",
        ]
    );
}

#[test]
fn derived_hard_exclusion_labels_equal_the_previous_literal() {
    assert_eq!(*crate::hard_exclusion::HARD_EXCLUSION_LABELS, ["external"]);
}

#[test]
fn derived_champion_path_labels_equal_the_previous_literal_set() {
    // The order was never load-bearing (membership only), so compare sets.
    assert_eq!(
        set(crate::work_finder::operator_priority::CHAMPION_PATH_LABELS
            .iter()
            .copied()),
        set(["loom:epic", "loom:architect", "loom:hermit", "loom:auditor"])
    );
}

/// The work finder's single-label name constants are identifiers, not sets,
/// so they stay `const` (usable in const contexts and patterns). Check each
/// names a registry label with the properties its doc comment claims.
#[test]
fn work_finder_name_constants_match_their_registry_properties() {
    use crate::dep_classify::consts::OPERATOR_DECISION_LABEL;
    use crate::work_finder::{BUILDING_LABEL, OPERATOR_HOLD_LABEL};
    let get = |n: &str| {
        reg()
            .get(n)
            .unwrap_or_else(|| panic!("{n} not in registry"))
    };
    // The claim: skipped, never a park.
    let building = get(BUILDING_LABEL);
    assert!(building.kind == "claim" && building.skip && !building.park);
    // The generic hold: skipped, never a park (vibesql#6664).
    let hold = get(OPERATOR_HOLD_LABEL);
    assert!(hold.hold && hold.skip && !hold.park && hold.requires_base.is_none());
    // The decision sub-kind (work_finder's private copy is pinned through
    // SKIP_LABELS by `work_finder::tests`): skipped, not a park, rides on
    // loom:operator-only.
    let decision = get(OPERATOR_DECISION_LABEL);
    assert!(decision.skip && !decision.park);
    assert_eq!(decision.requires_base.as_deref(), Some("loom:operator-only"));
}

// --- slice 2b: more derived tables, pinned to their previous literals --------

#[test]
fn derived_operator_gate_labels_equal_the_previous_literal() {
    assert_eq!(
        set(crate::pr_latency::OPERATOR_GATE_LABELS.iter().copied()),
        set([
            "loom:operator",
            "loom:operator-only",
            "loom:needs-capability"
        ])
    );
}

#[test]
fn derived_pr_latency_hold_labels_equal_the_previous_set() {
    // Previously: gates + parks + every `loom:operator-` label except the
    // priority levels. Over the registry's labels that is exactly this set.
    let all: Vec<&str> = reg().labels.iter().map(|l| l.name.as_str()).collect();
    let got = crate::pr_latency::hold_labels(&all);
    assert_eq!(
        set(got.iter().map(String::as_str)),
        set([
            "loom:blocked",
            "loom:operator",
            "loom:operator-only",
            "loom:needs-capability",
            "loom:operator-blocked",
            "loom:operator-mechanical",
            "loom:operator-decision",
            "loom:operator-objective",
        ])
    );
}

#[test]
fn derived_eta_hold_labels_and_queue_blocked_colabels() {
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
        set([
            "loom:operator",
            "loom:operator-only",
            "loom:operator-mechanical",
            "loom:needs-capability"
        ])
    );
}

#[test]
fn derived_merge_pr_blocking_equals_the_previous_literal_in_order() {
    // Order matters: the guard reports the first match in this order.
    assert_eq!(
        *crate::merge_pr::labels::BLOCKING,
        [
            "loom:changes-requested",
            "loom:blocked",
            "loom:operator",
            "loom:sequenced",
            "loom:review-requested"
        ]
    );
}

#[test]
fn derived_human_gated_labels_equal_the_previous_literal() {
    assert_eq!(
        *crate::eta::labels::HUMAN_GATED_LABELS,
        ["loom:triage", "loom:curating", "loom:curated"]
    );
}

// dep_classify's operator-only base/sub-kind labels are single named labels
// (each with its own writer and marker), not a set, so they stay consts; this
// test keeps them and the registry's requires_base/remove_with pairing (#5671)
// in lockstep.

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
fn derived_eta_operator_hold_sets_match_the_previous_literals() {
    use crate::eta::labels::{
        MERGE_HOLD_COMPANION_LABELS, MERGE_HOLD_LABELS, OPERATOR_HOLD_LABELS,
    };
    assert_eq!(
        set(MERGE_HOLD_LABELS.iter().copied()),
        set([
            "loom:operator",
            "loom:operator-only",
            "loom:operator-decision"
        ])
    );
    assert_eq!(*MERGE_HOLD_COMPANION_LABELS, ["loom:operator-mechanical"]);
    // Order is load-bearing: `operator_hold_label` reports the first match.
    assert_eq!(
        *OPERATOR_HOLD_LABELS,
        [
            "loom:operator",
            "loom:operator-only",
            "loom:operator-decision",
            "loom:operator-mechanical"
        ]
    );
}
