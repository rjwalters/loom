//! ETA core tests (#9289). Pure: no daemon, no network, no clock.

mod backtest;
mod backtest_adaptation;
mod backtest_censored;
mod backtest_pr;
mod backtest_priority;
mod backtest_subsets;
mod backtest_union;
mod censoring;
mod conditioning;
mod config;
mod conformal;
mod conformal_ipcw;
mod conformal_ipcw_drift;
mod conformal_seconds;
mod conformal_wrap;
mod dependency;
mod emit;
mod episodes;
mod estimate;
mod explain;
mod explanation;
mod features_v3;
mod fit;
mod fit_leak;
mod fit_newton;
mod fit_parity;
mod fit_publish;
mod fit_publish_v2;
mod fit_rows;
mod flag_timeline;
mod fleet;
mod fleet_refresh;
mod fleet_signoz;
mod fleet_signoz_history;
mod fleet_signoz_timeline;
mod fleet_signoz_timeline_sql;
mod friction;
mod held_heron;
mod hold_parity;
mod hold_serving;
mod item_features;
mod journal;
mod keen_wren;
pub(crate) mod land_twin_otter;
mod land_v3;
mod little_v0;
mod loop_features;
mod loop_kite;
mod merge_hold;
mod offline;
mod planner_sim;
mod planner_version;
mod point_in_time;
mod pr_file_log;
mod primitives;
mod priority_features;
mod priority_inputs;
mod queue_features;
mod ready;
mod ready_order;
mod recalibrate;
mod recency;
mod regime;
mod regime_serving;
mod roster_history;
mod score;
mod serve_parity;
mod shadow;
mod shadow_fleet;
mod shadow_gate;
mod shadow_lifecycle;
mod shadow_stats;
mod stage_forecast;
mod stall;
mod star_parity;
mod tracker;
mod tracker_hold;
mod twin_otter_parity;
mod walk_forward;

use super::explanation::Features;
use super::history::StageSamples;
use super::{AgeSource, CurrentStage, CurrentState, EstimateInput, Provenance, Stage, Subject};
use crate::telemetry::TelemetryEnvelope;
use chrono::{DateTime, TimeZone, Utc};

/// The fixture history.
pub(crate) const HISTORY_A: &str = include_str!("../fixtures/history-a.jsonl");

/// The golden explanation (`land-v1`, `review_wait`, age 0, over history-a).
pub(crate) const EXPLANATION_GOLDEN: &str = include_str!("../fixtures/explanation-golden.json");

/// The golden `start-v1` and unstarted `land-v1` explanations (#9326).
pub(crate) const READY_GOLDEN: &str = include_str!("../fixtures/ready-golden.json");

/// The golden backtest report (`land-v1` replayed over history-a, #9325).
pub(crate) const BACKTEST_GOLDEN: &str = include_str!("../fixtures/backtest-golden.json");

/// The twin-otter parity fixture (#10223): the generator spec, the fitted
/// reference coefficients and the evaluation rows for #10221 and #10222.
pub(crate) const TWIN_OTTER_PARITY: &str = include_str!("../fixtures/twin_otter_parity.json");

/// The leakage fixture (#9325): a history whose LAST record, if the replay
/// could see it, would change the answer — and which every replay instant in
/// the fixture predates.
pub(crate) const LEAKAGE: &str = include_str!("../fixtures/leakage.jsonl");

/// No stall signals (#10210): the `stalls` of a test [`super::tracker::EstimateContext`]
/// that has to outlive a temporary (a helper returning the context).
pub(crate) static NO_STALLS: super::stall::StallSnapshot = super::stall::StallSnapshot {
    host: Vec::new(),
    locked_repos: std::collections::BTreeSet::new(),
};

/// The instant every fixture estimate is made at.
pub(crate) fn as_of() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).unwrap()
}

/// [`HISTORY_A`]'s envelopes, parsed but not yet flattened — what
/// [`crate::eta::backtest::cases_from_envelopes`] needs, and what
/// [`history_a`] itself flattens via [`StageSamples::push_envelopes`].
pub(crate) fn history_a_envelopes() -> Vec<TelemetryEnvelope> {
    HISTORY_A
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line parses"))
        .collect()
}

/// History parsed from [`HISTORY_A`].
pub(crate) fn history_a() -> StageSamples {
    let mut samples = StageSamples::default();
    samples.push_envelopes(&history_a_envelopes());
    samples
}

/// A fixed build, so goldens do not move with the binary under test.
pub(crate) fn provenance() -> Provenance {
    Provenance {
        version: "0.19.475".to_string(),
        revision: "bf2fb67c3a1d4e5f60718293a4b5c6d7e8f90123".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

pub(crate) fn subject() -> Subject {
    let mut subject = Subject::new("rjwalters/loom", Some(1_073_994_527), 9289);
    subject.pr_number = Some(9301);
    subject.sweep_id = Some("sweep-issue-9289-1790000000".to_string());
    subject
}

/// An input at `stage` with `age_sec` already spent.
pub(crate) fn input_at(stage: Stage, age_sec: i64, rework_rounds: u32) -> EstimateInput {
    EstimateInput {
        subject: subject(),
        as_of: as_of(),
        current: CurrentState::At(CurrentStage {
            stage,
            entered_at: Some(as_of() - chrono::Duration::seconds(age_sec)),
            age_sec,
            age_source: AgeSource::LabelEvent,
            rework_rounds,
            episode_entered_at: None,
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
        dispatch: None,
        stalls: Vec::new(),
        held: None,
        queue: Vec::new(),
        dependencies: None,
    }
}
