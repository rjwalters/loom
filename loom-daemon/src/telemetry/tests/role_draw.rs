//! `sweep.outcome`'s `role_draw` (2AMLogic/2am#4393): the weighted agent
//! draw's pick and whether the sweep pinned its model. Additive and optional:
//! absent when no `roleDraw` table decided the sweep, and a record written
//! before the field existed still decodes.

use super::super::*;
use super::sweep_outcome;

fn outcome(role_draw: Option<SweepRoleDraw>) -> SweepOutcomeRecord {
    let TelemetryRecord::SweepOutcome(mut record) = sweep_outcome() else {
        panic!("fixture is a sweep.outcome record")
    };
    record.role_draw = role_draw;
    record
}

#[test]
fn a_drawn_sweep_serialises_its_pick_and_unpinned_model() {
    let record = outcome(Some(SweepRoleDraw {
        pick: "claude/opus".into(),
        model_pinned: false,
    }));
    let json = serde_json::to_value(&record).unwrap();
    assert_eq!(json["role_draw"]["pick"], "claude/opus");
    assert_eq!(json["role_draw"]["model_pinned"], false);
    let back: SweepOutcomeRecord = serde_json::from_value(json).unwrap();
    assert_eq!(back, record);
}

#[test]
fn an_undrawn_sweep_omits_the_field() {
    let json = serde_json::to_value(outcome(None)).unwrap();
    assert!(json.get("role_draw").is_none(), "absent, never null: {json}");
}

#[test]
fn a_record_from_before_the_field_still_decodes() {
    let mut json = serde_json::to_value(outcome(None)).unwrap();
    json.as_object_mut().unwrap().remove("role_draw");
    let back: SweepOutcomeRecord = serde_json::from_value(json).unwrap();
    assert_eq!(back.role_draw, None);
}

#[test]
fn the_summary_takes_the_pick_and_never_claims_a_pinned_model() {
    use crate::runtime_preference::role_draw::{Layer, RoleDrawRecord};
    let mut draw = RoleDrawRecord {
        role: "builder".into(),
        attempt: 1,
        table_attempt: 1,
        layer: Layer::Project,
        table: Vec::new(),
        off: Vec::new(),
        seed: "0000000000000001".into(),
        seed_inputs: None,
        steps: Vec::new(),
        passed_over: Vec::new(),
        pick: Some("claude/opus".into()),
        model: Some("opus".into()),
    };
    let summary = SweepRoleDraw::from_draw(&draw);
    assert_eq!(summary.pick, "claude/opus");
    assert!(!summary.model_pinned, "sweeps do not pin /model (loom#11782 item 2)");
    draw.pick = None;
    assert_eq!(SweepRoleDraw::from_draw(&draw).pick, "none");
}
