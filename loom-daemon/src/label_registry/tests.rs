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
fn validation_rejects_bad_retired_entries() {
    let good: serde_json::Value = serde_json::from_str(REGISTRY_JSON).unwrap();
    let mutate = |f: &dyn Fn(&mut serde_json::Value)| {
        let mut v = good.clone();
        f(&mut v);
        Registry::parse(&v.to_string())
    };
    // A retired name that is still declared.
    assert!(mutate(&|v| v["retired"][0]["name"] = "loom:pr".into()).is_err());
    // A duplicate retired name.
    assert!(mutate(&|v| v["retired"][1]["name"] = v["retired"][0]["name"].clone()).is_err());
    // replaced_by naming an unknown label, and a paused (non-active) one.
    assert!(mutate(&|v| v["retired"][0]["replaced_by"] = "loom:nope".into()).is_err());
    assert!(mutate(&|v| {
        let labels = v["labels"].as_array_mut().unwrap();
        let paused = labels.iter_mut().find(|l| l["name"] == "points:1").unwrap();
        paused["lifecycle"] = "paused".into();
        v["retired"][0]["replaced_by"] = "points:1".into();
    })
    .is_err());
    // A retired entry must name its retiring issue, and has a fixed shape.
    assert!(mutate(&|v| v["retired"][0]["retired_in"] = "".into()).is_err());
    assert!(mutate(&|v| v["retired"][0]["extra"] = true.into()).is_err());
    // `lifecycle: retired` is gone: a retired label lives in `retired`.
    assert!(mutate(&|v| v["labels"][0]["lifecycle"] = "retired".into()).is_err());
    // An absent `retired` array is an empty one.
    assert!(mutate(&|v| {
        v.as_object_mut().unwrap().remove("retired");
    })
    .is_ok());
}

/// #11513: every label retired by the priority rollout (#11103) and the label
/// cleanup (#11105) has a mapping entry (a replacement or an explicit none),
/// with the agreed replacement, and none of them is still declared.
#[test]
fn every_retired_label_has_its_mapping_entry() {
    let want: &[(&str, Option<&str>, bool, &str)] = &[
        ("loom:operator-priority", Some("loom:important"), true, "#11103"),
        ("loom:operator-high-priority", Some("loom:very-important"), true, "#11103"),
        ("loom:high-priority-inherited", None, true, "#11103"),
        ("tier:goal-advancing", None, true, "#11103"),
        ("tier:goal-supporting", None, true, "#11103"),
        ("tier:maintenance", None, true, "#11103"),
        // Operator ruling (#11675): owned, relabelled then deleted.
        ("priority:high", Some("loom:important"), true, "#11103"),
        ("loom:urgent", None, true, "#11105"),
        ("loom:heavy", None, true, "#11105"),
        ("loom:operator-objective", None, true, "#11105"),
        // #11818: legacy undeclared names the fleet label-drift check found
        // live; open items lose them (`--keep-labels`), closed keep them.
        ("loom:ready", None, true, "#11818"),
        ("loom:approved", None, true, "#11818"),
        ("loom:in-progress", None, true, "#11818"),
        ("loom:pr-feedback", None, true, "#11818"),
        ("loom:worked-example", None, true, "#11818"),
        // #11795: the hold taxonomy collapses to loom:blocked /
        // loom:operator / loom:frozen. operator-blocked is a human hold
        // (operator, 2026-10-14), so it folds into loom:operator.
        ("loom:operator-only", Some("loom:operator"), true, "#11795"),
        ("loom:operator-decision", Some("loom:operator"), true, "#11795"),
        ("loom:operator-mechanical", Some("loom:operator"), true, "#11795"),
        ("loom:operator-blocked", Some("loom:operator"), true, "#11795"),
        ("loom:needs-capability", Some("loom:blocked"), true, "#11795"),
    ];
    let got: Vec<(&str, Option<&str>, bool, &str)> = reg()
        .retired
        .iter()
        .map(|r| (r.name.as_str(), r.replaced_by.as_deref(), r.owned, r.retired_in.as_str()))
        .collect();
    assert_eq!(got, want);
    for (name, ..) in want {
        assert!(reg().get(name).is_none(), "{name} is retired but still declared");
        assert!(reg().get_retired(name).is_some(), "{name}");
    }
    assert!(reg().get_retired("loom:pr").is_none());
    for name in [
        crate::priority_pick::IMPORTANT_LABEL,
        crate::priority_pick::VERY_IMPORTANT_LABEL,
    ] {
        assert_eq!(reg().get(name).unwrap().lifecycle, "active", "{name}");
    }
}

/// A retired label must never reach `labels.yml`: `sync-labels.sh` is
/// additive and would re-create it on every sync.
#[test]
fn no_retired_label_is_rendered_into_labels_yml() {
    let block = generate::loom_block(reg());
    let names = generate::block_names(&block);
    for r in &reg().retired {
        assert!(!names.contains(&r.name), "{} rendered into labels.yml", r.name);
    }
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
    assert_eq!(
        reg().with_property("park").unwrap(),
        vec!["loom:blocked", "loom:operator", "loom:frozen"]
    );
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

/// The retired hold names a set picks up for the migration window (#11795),
/// in retired order.
const FOLDED: [&str; 5] = [
    "loom:operator-only",
    "loom:operator-decision",
    "loom:operator-mechanical",
    "loom:operator-blocked",
    "loom:needs-capability",
];

#[test]
fn derived_park_labels_equal_the_previous_literal_in_order() {
    // Order matters: the park guards report the first match in this order.
    // #11795: loom:operator is an issue-dispatch park; the retired names
    // follow the active ones so an unmigrated item stays parked.
    let mut want = vec!["loom:blocked", "loom:operator", "loom:frozen"];
    want.extend(FOLDED);
    assert_eq!(*crate::work_finder::PARK_LABELS, want);
}

#[test]
fn derived_pr_lane_park_labels_leave_loom_operator_out() {
    // #11795: on a PR loom:operator is not a park (#7660); the retired
    // loom:operator-only keeps parking an unmigrated PR exactly as before.
    let mut want = vec!["loom:blocked", "loom:frozen"];
    want.extend(FOLDED);
    assert_eq!(*crate::work_finder::PR_LANE_PARK_LABELS, want);
}

#[test]
fn derived_skip_labels_equal_the_previous_literal_in_order() {
    let mut want = vec![
        "loom:building",
        "loom:blocked",
        "loom:operator",
        "loom:frozen",
    ];
    want.extend(FOLDED);
    // #11480: a bounced Decision issue is never build work. #11399: an epic
    // is a tracking container. Skips, not parks.
    want.extend(["loom:decision-malformed", "loom:epic"]);
    assert_eq!(*crate::work_finder::SKIP_LABELS, want);
}

#[test]
fn hold_name_sets_fold_each_retired_name_into_its_replacement() {
    assert_eq!(
        *crate::work_finder::OPERATOR_HOLD_NAMES,
        [
            "loom:operator",
            "loom:operator-only",
            "loom:operator-decision",
            "loom:operator-mechanical",
            "loom:operator-blocked",
        ]
    );
    assert_eq!(
        *crate::work_finder::BLOCKED_HOLD_NAMES,
        ["loom:blocked", "loom:needs-capability"]
    );
    let mut holds = vec!["loom:blocked", "loom:operator", "loom:frozen"];
    holds.extend(FOLDED);
    assert_eq!(*crate::work_finder::HOLD_NAMES, holds);
}

#[test]
fn embedded_set_with_retired_skips_retirements_without_the_property() {
    // loom:operator-priority -> loom:important: not a hold, so not picked up.
    assert!(!embedded_set_with_retired("hold").contains(&"loom:operator-priority"));
    // A retired label with no replacement never joins any set.
    assert!(!embedded_set_with_retired("skip").contains(&"loom:urgent"));
    // contradicts_approval is never read with retired names (a report order).
    assert_eq!(embedded_set("contradicts_approval").len(), 5);
}

#[test]
fn derived_hard_exclusion_labels_equal_the_previous_literal() {
    assert_eq!(*crate::hard_exclusion::HARD_EXCLUSION_LABELS, ["loom:frozen", "external"]);
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
    use crate::work_finder::{BLOCKED_HOLD_LABEL, BUILDING_LABEL, OPERATOR_HOLD_LABEL};
    let get = |n: &str| {
        reg()
            .get(n)
            .unwrap_or_else(|| panic!("{n} not in registry"))
    };
    // The claim: skipped, never a park.
    let building = get(BUILDING_LABEL);
    assert!(building.kind == "claim" && building.skip && !building.park);
    // The operator hold: an issue-dispatch park since #11795.
    let hold = get(OPERATOR_HOLD_LABEL);
    assert!(hold.hold && hold.skip && hold.park && hold.requires_base.is_none());
    assert!(hold.human_gated && hold.operator_hold);
    let blocked = get(BLOCKED_HOLD_LABEL);
    assert!(blocked.hold && blocked.park && !blocked.human_gated);
    // #11795: no label requires a base or pairs a removal any more.
    assert!(reg()
        .labels
        .iter()
        .all(|l| l.requires_base.is_none() && l.remove_with.is_empty()));
}

// --- slice 2b: more derived tables, pinned to their previous literals --------

/// #11087: `human_gated` is the notifier contract ("a human must act"). The
/// self-clearing sub-kind, the capability gate and the decision bounce are
/// not human asks; the curation workflow labels are not either.
#[test]
fn human_gated_is_exactly_the_human_must_act_labels() {
    assert_eq!(
        set(embedded_set("human_gated")),
        // #11795: the operator hold and the external marker; frozen stays
        // false (nothing is pending).
        set(["loom:operator", "external"])
    );
}

#[test]
fn derived_operator_gate_labels_equal_the_previous_literal() {
    assert_eq!(
        set(crate::pr_latency::OPERATOR_GATE_LABELS.iter().copied()),
        // #11795: loom:operator and the retired names that fold into it.
        set(crate::work_finder::OPERATOR_HOLD_NAMES.iter().copied())
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
        // #11743: a frozen PR is held, not waiting on a reviewer.
        set(["loom:blocked", "loom:operator", "loom:frozen"])
    );
}

#[test]
fn derived_queue_blocked_hold_labels() {
    assert_eq!(
        set(crate::observability::queue_blocked::HOLD_LABELS
            .iter()
            .copied()),
        set(crate::work_finder::OPERATOR_HOLD_NAMES.iter().copied())
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
fn dep_classify_reads_the_operator_hold_under_any_name() {
    use crate::dep_classify::consts::{held_label, other_hold_names, OPERATOR_LABEL};
    let l = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    assert_eq!(OPERATOR_LABEL, "loom:operator");
    assert_eq!(held_label(&l(&["loom:operator"])), "loom:operator");
    let old = l(&["loom:operator-only", "loom:operator-blocked"]);
    assert_eq!(held_label(&old), "loom:operator-only");
    assert_eq!(other_hold_names(&old, "loom:operator-only"), ["loom:operator-blocked"]);
    // Absent: the hold name the deciders compare against, so they refuse.
    assert_eq!(held_label(&l(&["loom:issue"])), "loom:operator");
}
