//! ETA core tests (#9289). Pure: no daemon, no network, no clock.

mod estimate;
mod explanation;
mod primitives;
mod score;

use super::explanation::Features;
use super::history::StageSamples;
use super::{AgeSource, CurrentStage, CurrentState, EstimateInput, Provenance, Stage, Subject};
use crate::telemetry::TelemetryEnvelope;
use chrono::{DateTime, TimeZone, Utc};

/// The fixture history.
pub(super) const HISTORY_A: &str = include_str!("../fixtures/history-a.jsonl");

/// The golden explanation (`land-v1`, `review_wait`, age 0, over history-a).
pub(super) const EXPLANATION_GOLDEN: &str = include_str!("../fixtures/explanation-golden.json");

/// The instant every fixture estimate is made at.
pub(super) fn as_of() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).unwrap()
}

/// History parsed from [`HISTORY_A`].
pub(super) fn history_a() -> StageSamples {
    let envelopes: Vec<TelemetryEnvelope> = HISTORY_A
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line parses"))
        .collect();
    let mut samples = StageSamples::default();
    samples.push_envelopes(&envelopes);
    samples
}

/// A fixed build, so goldens do not move with the binary under test.
pub(super) fn provenance() -> Provenance {
    Provenance {
        version: "0.19.475".to_string(),
        revision: "bf2fb67c3a1d4e5f60718293a4b5c6d7e8f90123".to_string(),
        tree_state: "clean".to_string(),
    }
}

pub(super) fn subject() -> Subject {
    let mut subject = Subject::new("rjwalters/loom", Some(1_073_994_527), 9289);
    subject.pr_number = Some(9301);
    subject.sweep_id = Some("sweep-issue-9289-1790000000".to_string());
    subject
}

/// An input at `stage` with `age_sec` already spent.
pub(super) fn input_at(stage: Stage, age_sec: i64, rework_rounds: u32) -> EstimateInput {
    EstimateInput {
        subject: subject(),
        as_of: as_of(),
        current: CurrentState::At(CurrentStage {
            stage,
            entered_at: Some(as_of() - chrono::Duration::seconds(age_sec)),
            age_sec,
            age_source: AgeSource::LabelEvent,
            rework_rounds,
        }),
        features: Features {
            labels: Some(vec!["loom:review-requested".to_string()]),
            complexity_marker: Some("complex".to_string()),
            points_marker: Some(13),
            hour_utc: Some(12),
            weekday_utc: Some(6),
            ..Features::default()
        },
        features_omitted: Vec::new(),
        provenance: provenance(),
    }
}
