//! The planner's identity stamp (#10528, slice a): the hash, the serve-side
//! field on `Explanation`, the training-row stamp and the regime restriction.

use super::{EXPLANATION_GOLDEN, HISTORY_A};
use crate::eta::explanation::Explanation;
use crate::eta::fit::rows::{restrict_to_regime, RowKey};
use crate::eta::fit::{
    self, FitMeta, FitStage, FitWindow, Fitter, MergeLabel, ModelInputs, TrainingRow,
};
use crate::eta::planner_version::{planner_version, PlannerConfigView};
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

fn view(config: &Value) -> PlannerConfigView {
    PlannerConfigView::from_config(config)
}

fn base_config() -> Value {
    json!({
        "terminals": [{"id": "a", "role": "builder"}],
        "autonomous": {
            "workFinder": {"maxConcurrent": 4, "maxConcurrentPerRepo": 2},
            "eta": {"refreshSecs": 300}
        }
    })
}

#[test]
fn planner_version_is_deterministic_and_shaped() {
    let v = planner_version("0.19.862", &view(&base_config()));
    assert_eq!(v, planner_version("0.19.862", &view(&base_config())));
    let (version, digest) = v.split_once('+').unwrap();
    assert_eq!(version, "0.19.862");
    assert_eq!(digest.len(), 12);
    assert!(digest
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    let next = planner_version("0.19.863", &view(&base_config()));
    assert_eq!(next.split_once('+').unwrap().1, digest, "the digest is config-only");
    assert_ne!(v, next);
}

#[test]
fn planner_version_ignores_key_order_and_unrelated_config() {
    let want = planner_version("1", &view(&base_config()));
    let reordered = json!({
        "autonomous": {
            "workFinder": {"maxConcurrentPerRepo": 2, "maxConcurrent": 4},
        },
    });
    assert_eq!(want, planner_version("1", &view(&reordered)));
    let mut other = base_config();
    other["terminals"] = json!([{"id": "z", "role": "judge"}]);
    other["autonomous"]["eta"] = json!({"refreshSecs": 60});
    assert_eq!(want, planner_version("1", &view(&other)));
}

#[test]
fn planner_version_changes_with_planner_config() {
    let want = planner_version("1", &view(&base_config()));
    let mut slots = base_config();
    slots["autonomous"]["workFinder"]["maxConcurrent"] = json!(5);
    assert_ne!(want, planner_version("1", &view(&slots)));
    let mut sequencing = base_config();
    sequencing["autonomous"]["mergeSequencing"] = json!({"enabled": true});
    assert_ne!(want, planner_version("1", &view(&sequencing)));
}

#[test]
fn planner_version_field_is_absent_when_none_and_round_trips() {
    let golden: Value = serde_json::from_str(EXPLANATION_GOLDEN).unwrap();
    assert!(golden.get("planner_version").is_none());
    let mut parsed: Explanation = serde_json::from_value(golden.clone()).unwrap();
    assert_eq!(parsed.planner_version, None);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), golden);
    parsed.planner_version = Some("0.19.862+0123456789ab".to_string());
    let stamped = serde_json::to_value(&parsed).unwrap();
    assert_eq!(stamped["planner_version"], "0.19.862+0123456789ab");
    let back: Explanation = serde_json::from_value(stamped).unwrap();
    assert_eq!(back, parsed);
}

fn row(i: usize, stamp: Option<&str>) -> TrainingRow {
    TrainingRow {
        stage: FitStage::ReviewWait,
        group: format!("g#{i}"),
        inputs: ModelInputs {
            age_h: (i % 37) as f64 * 0.7,
            ahead: (i % 5) as u32,
            hour_utc: (i % 24) as f64,
            ..ModelInputs::default()
        },
        starred_any: None,
        star_source: None,
        planner_version: stamp.map(str::to_string),
        exit: Some(i.is_multiple_of(3)),
        merge: MergeLabel {
            dur_h: 1.0 + (i % 11) as f64,
            merged: !i.is_multiple_of(4),
        },
    }
}

fn key(i: usize) -> RowKey {
    RowKey {
        at: Utc
            .timestamp_opt(1_790_000_000 + i as i64 * 1800, 0)
            .unwrap(),
        repo: "rjwalters/loom".to_string(),
        pr: i as u32,
    }
}

#[test]
fn planner_version_stamp_leaves_the_fit_byte_identical() {
    let meta = FitMeta {
        as_of: Utc.with_ymd_and_hms(2026, 10, 4, 0, 0, 0).unwrap(),
        window: FitWindow::standard(Utc.with_ymd_and_hms(2026, 10, 4, 0, 0, 0).unwrap()),
        fitter: Fitter {
            version: "0.19.676".to_string(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
        },
    };
    let plain: Vec<TrainingRow> = (0..400).map(|i| row(i, None)).collect();
    let stamped: Vec<TrainingRow> = (0..400)
        .map(|i| {
            row(
                i,
                Some(if i.is_multiple_of(2) {
                    "1+aaaaaaaaaaaa"
                } else {
                    "1+bbbbbbbbbbbb"
                }),
            )
        })
        .collect();
    let a = serde_json::to_string(&fit::fit(&meta, &plain, &[])).unwrap();
    let b = serde_json::to_string(&fit::fit(&meta, &stamped, &[])).unwrap();
    assert_eq!(a, b);
    // An unstamped row serializes without the key (history-a era rows).
    assert!(!serde_json::to_string(&plain[0])
        .unwrap()
        .contains("planner_version"));
    assert!(!HISTORY_A.is_empty());
}

#[test]
fn planner_version_restrict_to_regime_keeps_only_that_regime_in_order() {
    let stamps = [
        Some("r+A"),
        Some("r+B"),
        None,
        Some("r+B"),
        Some("r+A"),
        Some("r+B"),
    ];
    let rows: Vec<TrainingRow> = stamps.iter().enumerate().map(|(i, s)| row(i, *s)).collect();
    let keys: Vec<RowKey> = (0..rows.len()).map(key).collect();
    let run = || restrict_to_regime(rows.clone(), keys.clone(), "r+B");
    let (kept, kept_keys) = run();
    assert_eq!(kept.iter().map(|r| r.group.as_str()).collect::<Vec<_>>(), ["g#1", "g#3", "g#5"]);
    assert_eq!(kept_keys.iter().map(|k| k.pr).collect::<Vec<_>>(), [1, 3, 5]);
    assert_eq!(run(), (kept, kept_keys));
    assert!(restrict_to_regime(rows, keys, "r+C").0.is_empty());
}
