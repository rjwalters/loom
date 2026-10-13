//! Back-compat for the retired `sweep.outcome` `complexity` field (Issue
//! #8542, removed by #11665 / issue-sizing design §6.3).
//!
//! Historical journal lines still carry `"complexity"` (and the Jev
//! shadow's `"jev_tier"`). `SweepOutcomeRecord` does not
//! `deny_unknown_fields`, so those lines must keep decoding, and a freshly
//! serialized record must never emit either key.

use super::super::*;

#[test]
fn a_pre_cutover_line_with_complexity_and_jev_tier_still_decodes() {
    let json = r#"{
        "kind": "sweep.outcome",
        "repo": "rjwalters/loom",
        "visibility": "public",
        "issue": 8542,
        "sweep_id": "sweep-issue-8542-0",
        "model": "sonnet",
        "total_duration_sec": 512,
        "result": "success",
        "pr_number": 8600,
        "complexity": "routine",
        "jev_tier": "complex",
        "jev_confidence": 0.7
    }"#;
    let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
    match decoded {
        TelemetryRecord::SweepOutcome(r) => {
            assert_eq!(r.pr_number, Some(8600));
            let reserialized = serde_json::to_value(&r).unwrap();
            assert!(reserialized.get("complexity").is_none(), "{reserialized}");
            assert!(reserialized.get("jev_tier").is_none(), "{reserialized}");
        }
        other => panic!("expected SweepOutcome, got {other:?}"),
    }
}
