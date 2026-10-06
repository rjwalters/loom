//! OTLP mapping for `eta.estimate` / `eta.outcome` (#9289),
//! `eta.fleet_refresh` (#10263), `eta.fit` (#10391) and `pr.resolved`
//! (#10519).
//!
//! Each is one log record. The **body** is the record's JSON — for an
//! estimate that is the whole `eta-explanation/v1` explanation — so ClickHouse
//! can `JSONExtract` any field of it, and no attribute policy bounds it. The
//! scalar summary rides as `loom.eta.*` attributes for cheap dashboards. The
//! record time is the estimate's `as_of`, or the outcome's `actual_at`.
//!
//! `loom.eta.version` / `loom.eta.revision` / `loom.eta.tree_state` are always
//! the **estimating** build, on both kinds, so accuracy groups by
//! `(heuristic, revision)` with one key. An outcome adds the observing build
//! as `loom.eta.outcome_version` / `_revision` / `_tree_state`. Each build's
//! `complete` flag rides as `loom.eta.provenance_complete` /
//! `loom.eta.outcome_provenance_complete`; accuracy queries keep only rows
//! where both are true.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv, kv_int, kv_string, nanos};
use crate::eta::Provenance;
use crate::telemetry::TelemetryRecord;

fn kv_bool(key: &str, value: bool) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        },
    )
}

fn kv_double(key: &str, value: f64) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::DoubleValue(value)),
        },
    )
}

fn provenance(attributes: &mut Vec<KeyValue>, prefix: &str, loom: &Provenance) {
    attributes.push(kv_string(&format!("{prefix}version"), loom.version.clone()));
    attributes.push(kv_string(&format!("{prefix}revision"), loom.revision.clone()));
    attributes.push(kv_string(&format!("{prefix}tree_state"), loom.tree_state.clone()));
    attributes.push(kv_bool(&format!("{prefix}provenance_complete"), loom.complete));
}

fn opt_int(attributes: &mut Vec<KeyValue>, key: &str, value: Option<i64>) {
    if let Some(value) = value {
        attributes.push(kv_int(key, value));
    }
}

/// #10498: only the fleet's ETA authority emits estimates and outcomes, so
/// the emitting host *is* the authority. Pushes `loom.eta.authority` for those
/// two kinds; a no-op for every other record. Kept here, not in the caller,
/// because this file is the one place `loom.eta.*` attributes are produced
/// (`tests/eta_artifacts.rs` scans it).
pub(super) fn push_authority(
    attributes: &mut Vec<KeyValue>,
    record: &TelemetryRecord,
    host_id: &str,
) {
    if matches!(record, TelemetryRecord::EtaEstimate(_) | TelemetryRecord::EtaOutcome(_)) {
        attributes.push(kv_string("loom.eta.authority", host_id.to_string()));
    }
}

/// `(event_name, severity, record time, attributes, body)` for an ETA
/// record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    match record {
        TelemetryRecord::EtaEstimate(r) => {
            let e = &r.explanation;
            let mut attributes = vec![
                kv_string("loom.repo", e.subject.repo.clone()),
                kv_int("loom.issue", i64::from(e.subject.issue)),
                kv_string("loom.story", e.subject.story.clone()),
                kv_string("loom.eta.estimate_id", e.estimate_id.clone()),
                kv_string("loom.eta.kind", e.kind.as_str()),
                kv_string("loom.eta.heuristic", e.heuristic.clone()),
                kv_bool("loom.eta.primary", r.primary),
                kv_string("loom.eta.trigger", r.trigger.as_str()),
            ];
            provenance(&mut attributes, "loom.eta.", &e.loom);
            opt_int(&mut attributes, "loom.pr_number", e.subject.pr_number.map(i64::from));
            if let Some(current) = &e.current_stage {
                attributes.push(kv_string("loom.eta.stage", current.stage.as_str()));
                attributes.push(kv_int("loom.eta.age_sec", current.age_sec));
            }
            if let Some(result) = &e.result {
                attributes.push(kv_int("loom.eta.p25_sec", result.p25_sec));
                attributes.push(kv_int("loom.eta.p50_sec", result.p50_sec));
                attributes.push(kv_int("loom.eta.p75_sec", result.p75_sec));
                opt_int(&mut attributes, "loom.eta.p90_sec", result.p90_sec);
                attributes.push(kv_int("loom.eta.samples_min", result.samples_min as i64));
                attributes.push(kv_string(
                    "loom.eta.horizon_bucket",
                    crate::eta::score::bucket(result.p50_sec),
                ));
            }
            if let Some(reason) = e.no_estimate_reason {
                attributes.push(kv_string("loom.eta.no_estimate_reason", reason.as_str()));
            }
            let body = serde_json::to_string(e).unwrap_or_default();
            Some(("eta.estimate", SeverityNumber::Info, nanos(e.as_of), attributes, body))
        }
        TelemetryRecord::EtaOutcome(r) => {
            let e = &r.estimate;
            let s = &r.score;
            let mut attributes = vec![
                kv_string("loom.repo", e.repo.clone()),
                kv_int("loom.issue", i64::from(e.issue)),
                kv_string("loom.eta.estimate_id", e.estimate_id.clone()),
                kv_string("loom.eta.kind", e.kind.as_str()),
                kv_string("loom.eta.heuristic", e.heuristic.clone()),
                kv_string(
                    "loom.eta.outcome",
                    serde_json::to_value(s.outcome)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default(),
                ),
                kv_string("loom.eta.outcome_source", r.outcome_source.clone()),
                kv_int("loom.eta.lead_sec", s.lead_sec),
                kv_int("loom.eta.rework_rounds_actual", i64::from(s.rework_rounds_actual)),
            ];
            provenance(&mut attributes, "loom.eta.", &e.loom);
            provenance(&mut attributes, "loom.eta.outcome_", &r.loom);
            opt_int(&mut attributes, "loom.pr_number", e.pr_number.map(i64::from));
            // All four quantiles ride on the outcome row (#10211), so the
            // accuracy views read an interval without joining the estimate.
            opt_int(&mut attributes, "loom.eta.p25_sec", e.p25_sec);
            opt_int(&mut attributes, "loom.eta.p50_sec", e.p50_sec);
            opt_int(&mut attributes, "loom.eta.p75_sec", e.p75_sec);
            opt_int(&mut attributes, "loom.eta.p90_sec", e.p90_sec);
            opt_int(&mut attributes, "loom.eta.error_sec", s.error_sec);
            opt_int(&mut attributes, "loom.eta.abs_error_sec", s.abs_error_sec);
            opt_int(&mut attributes, "loom.eta.outcome_resolution_sec", r.outcome_resolution_sec);
            opt_int(&mut attributes, "loom.eta.samples_min", s.samples_min.map(|n| n as i64));
            if let Some(covered) = s.covered {
                attributes.push(kv_bool("loom.eta.covered", covered));
            }
            if let Some(loss) = s.pinball_loss_sec {
                attributes.push(kv_double("loom.eta.pinball_loss_sec", loss));
            }
            if let Some(late) = s.above_p90 {
                attributes.push(kv_bool("loom.eta.above_p90", late));
            }
            if let Some(loss) = s.pinball4_loss_sec {
                attributes.push(kv_double("loom.eta.pinball4_loss_sec", loss));
            }
            for (key, value) in [
                ("loom.eta.horizon_bucket", s.horizon_bucket.as_deref()),
                ("loom.eta.age_bucket", s.age_bucket.as_deref()),
                ("loom.eta.stage", s.stage_at_estimate.map(|st| st.as_str())),
                ("loom.eta.result", r.result.as_deref()),
            ] {
                if let Some(value) = value {
                    attributes.push(kv_string(key, value));
                }
            }
            let body = serde_json::to_string(r).unwrap_or_default();
            Some(("eta.outcome", SeverityNumber::Info, nanos(s.actual_at), attributes, body))
        }
        TelemetryRecord::EtaFleetRefresh(r) => {
            // #10263: one repo, one cycle; stamped at the cycle's start so
            // every repo of a cycle sorts together.
            let to_i64 = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
            let mut attributes = vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_string("loom.eta.fleet.cycle_id", r.cycle_id.clone()),
                kv_string("loom.eta.fleet.pass", r.pass.clone()),
                kv_string("loom.eta.fleet.stop_reason", r.stop_reason.clone()),
                kv_bool("loom.eta.fleet.promoted", r.promoted),
                kv_int("loom.eta.fleet.prs_read", to_i64(r.prs_read)),
                kv_int("loom.eta.fleet.pass_done", to_i64(r.pass_done)),
                kv_int("loom.eta.fleet.timelines_incomplete", to_i64(r.timelines_incomplete)),
                kv_int("loom.eta.fleet.samples_added", r.samples_added),
                kv_int("loom.eta.fleet.forge_calls", to_i64(r.forge_calls)),
                kv_int("loom.eta.fleet.not_modified_calls", to_i64(r.not_modified_calls)),
                kv_int("loom.eta.fleet.duration_ms", to_i64(r.duration_ms)),
            ];
            provenance(&mut attributes, "loom.eta.", &r.loom);
            opt_int(
                &mut attributes,
                "loom.eta.fleet.raw_events_added",
                r.raw_events_added.map(to_i64),
            );
            opt_int(
                &mut attributes,
                "loom.eta.fleet.ratelimit_remaining_min",
                r.ratelimit_remaining_min.map(to_i64),
            );
            for (key, value) in [
                ("loom.eta.fleet.reader_app", r.reader_app.clone()),
                ("loom.eta.fleet.snapshot_id", r.snapshot_id.clone()),
                ("loom.eta.fleet.as_of", r.as_of.map(crate::telemetry::trace::instant)),
            ] {
                if let Some(value) = value {
                    attributes.push(kv_string(key, value));
                }
            }
            let body = serde_json::to_string(r).unwrap_or_default();
            Some(("eta.fleet_refresh", SeverityNumber::Info, nanos(r.started_at), attributes, body))
        }
        TelemetryRecord::EtaFit(r) => {
            // #10391: one fit check; stamped at its start.
            let to_i64 = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
            let instant = crate::telemetry::trace::instant;
            let mut attributes = vec![
                kv_string("loom.eta.fit.check_id", r.check_id.clone()),
                kv_string("loom.eta.fit.trigger", r.trigger.clone()),
                kv_string("loom.eta.fit.outcome", r.outcome.clone()),
                kv_int("loom.eta.fit.snapshots", to_i64(r.snapshots)),
                kv_int("loom.eta.fit.duration_ms", to_i64(r.duration_ms)),
            ];
            provenance(&mut attributes, "loom.eta.", &r.loom);
            for (key, value) in [
                ("loom.eta.fit.skip_reason", r.skip_reason.clone()),
                ("loom.eta.fit.error", r.error.clone()),
                ("loom.eta.fit.fit_id", r.fit_id.clone()),
                ("loom.eta.fit.cutoff", r.cutoff.map(instant)),
                ("loom.eta.fit.window_start", r.window_start.map(instant)),
                ("loom.eta.fit.data_through", r.data_through.map(instant)),
                ("loom.eta.fit.snapshot_oldest_as_of", r.snapshot_oldest_as_of.map(instant)),
                ("loom.eta.fit.snapshot_newest_as_of", r.snapshot_newest_as_of.map(instant)),
                ("loom.eta.fit.coeff_file", r.coeff_file.clone()),
                ("loom.eta.fit.coeff_sha256", r.coeff_sha256.clone()),
            ] {
                if let Some(value) = value {
                    attributes.push(kv_string(key, value));
                }
            }
            for (key, value) in [
                ("loom.eta.fit.window_days", r.window_days),
                ("loom.eta.fit.rows_total", r.rows_total.map(to_i64)),
                ("loom.eta.fit.rows_censored", r.rows_censored.map(to_i64)),
                ("loom.eta.fit.rows_dropped_missing", r.rows_dropped_missing.map(to_i64)),
                ("loom.eta.fit.rows_dropped_no_flags", r.rows_dropped_no_flags.map(to_i64)),
                ("loom.eta.fit.rows_star_unknown", r.rows_star_unknown.map(to_i64)),
                ("loom.eta.fit.dwells", r.dwells.map(to_i64)),
                ("loom.eta.fit.pruned", r.pruned.map(to_i64)),
                ("loom.eta.fit.coeff_bytes", r.coeff_bytes.map(to_i64)),
            ] {
                opt_int(&mut attributes, key, value);
            }
            let body = serde_json::to_string(r).unwrap_or_default();
            Some(("eta.fit", SeverityNumber::Info, nanos(r.started_at), attributes, body))
        }
        TelemetryRecord::PrResolved(r) => {
            // #10519: stamped at the merge/close instant; the caller sets the
            // observed timestamp to `observed_at` (the knowable-at time).
            let instant = crate::telemetry::trace::instant;
            let mut attributes = vec![
                kv_string("loom.repo", r.repo.clone()),
                kv_int("loom.pr_number", i64::from(r.pr_number)),
                kv_string("loom.eta.pr.state", r.state.as_str()),
                kv_string("loom.eta.pr.resolved_at", instant(r.resolved_at)),
                kv_string("loom.eta.pr.observed_at", instant(r.observed_at)),
                kv_int("loom.eta.pr.resolution_sec", r.resolution_sec),
            ];
            provenance(&mut attributes, "loom.eta.", &r.loom);
            opt_int(&mut attributes, "loom.issue", r.issue.map(i64::from));
            let body = serde_json::to_string(r).unwrap_or_default();
            Some(("pr.resolved", SeverityNumber::Info, nanos(r.resolved_at), attributes, body))
        }
        TelemetryRecord::EtaBacktestFold(r) => {
            // #10492: one heuristic's fold for one day; stamped at its cutoff.
            let to_i64 = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
            let mut attributes = vec![
                kv_string("loom.eta.backtest.fold.fold_id", r.fold_id.clone()),
                kv_string("loom.eta.backtest.fold.heuristic", r.heuristic.clone()),
                kv_string("loom.eta.backtest.fold.kind", r.kind.clone()),
                kv_string("loom.eta.backtest.fold.day", r.day.clone()),
                kv_string("loom.eta.backtest.fold.compared_to", r.compared_to.clone()),
                kv_bool("loom.eta.backtest.fold.is_current", r.is_current),
                kv_int("loom.eta.backtest.fold.n_cases", to_i64(r.n_cases)),
                kv_int("loom.eta.backtest.fold.n_answered", to_i64(r.n_answered)),
                kv_int("loom.eta.backtest.fold.paired_pairs", to_i64(r.paired_pairs)),
            ];
            provenance(&mut attributes, "loom.eta.", &r.loom);
            for (key, value) in [
                ("loom.eta.backtest.fold.answer_rate", r.answer_rate),
                ("loom.eta.backtest.fold.pinball4_loss_sec", r.pinball4_loss_sec),
                ("loom.eta.backtest.fold.cov_25_75", r.cov_25_75),
                ("loom.eta.backtest.fold.late_surprise", r.late_surprise),
                ("loom.eta.backtest.fold.delta_pinball4_loss_sec", r.delta_pinball4_loss_sec),
                ("loom.eta.backtest.fold.delta_answer_rate", r.delta_answer_rate),
                ("loom.eta.backtest.fold.delta_late_surprise", r.delta_late_surprise),
            ] {
                if let Some(value) = value {
                    attributes.push(kv_double(key, value));
                }
            }
            if let Some(win) = r.win {
                attributes.push(kv_bool("loom.eta.backtest.fold.win", win));
            }
            if let Some(fit_id) = &r.fit_id {
                attributes.push(kv_string("loom.eta.backtest.fold.fit_id", fit_id.clone()));
            }
            let body = serde_json::to_string(r).unwrap_or_default();
            Some(("eta.backtest.fold", SeverityNumber::Info, nanos(r.cutoff), attributes, body))
        }
        TelemetryRecord::EtaBacktestSummary(r) => {
            // #10492: one challenger's rolling standing; stamped at its cutoff.
            let to_i64 = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
            let mut attributes = vec![
                kv_string("loom.eta.backtest.summary.summary_id", r.summary_id.clone()),
                kv_string("loom.eta.backtest.summary.heuristic", r.heuristic.clone()),
                kv_string("loom.eta.backtest.summary.kind", r.kind.clone()),
                kv_string("loom.eta.backtest.summary.compared_to", r.compared_to.clone()),
                kv_string("loom.eta.backtest.summary.as_of_day", r.as_of_day.clone()),
                kv_int("loom.eta.backtest.summary.cases", to_i64(r.cases)),
                kv_int("loom.eta.backtest.summary.days", to_i64(r.days)),
                kv_int("loom.eta.backtest.summary.wins", to_i64(r.wins)),
                kv_int("loom.eta.backtest.summary.ties", to_i64(r.ties)),
                kv_int("loom.eta.backtest.summary.min_folds", to_i64(r.min_folds)),
                kv_bool("loom.eta.backtest.summary.gate_ready", r.gate_ready),
                kv_string("loom.eta.backtest.summary.gate_detail", r.gate_detail.clone()),
                kv_int("loom.eta.backtest.summary.cases_before_fit", to_i64(r.cases_before_fit)),
            ];
            provenance(&mut attributes, "loom.eta.", &r.loom);
            for (key, value) in [
                ("loom.eta.backtest.summary.win_rate", r.win_rate),
                ("loom.eta.backtest.summary.ci_low", r.ci_low),
                ("loom.eta.backtest.summary.ci_high", r.ci_high),
            ] {
                if let Some(value) = value {
                    attributes.push(kv_double(key, value));
                }
            }
            if let Some(day) = &r.fitted_from {
                attributes.push(kv_string("loom.eta.backtest.summary.fitted_from", day.clone()));
            }
            if let Some(fit_id) = &r.fit_id {
                attributes.push(kv_string("loom.eta.backtest.summary.fit_id", fit_id.clone()));
            }
            let body = serde_json::to_string(r).unwrap_or_default();
            Some(("eta.backtest.summary", SeverityNumber::Info, nanos(r.cutoff), attributes, body))
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::log_record_for;
    use crate::eta::emit::Trigger;
    use crate::eta::heuristics::LandV1;
    use crate::eta::{
        AgeSource, CurrentStage, CurrentState, EstimateInput, Heuristic, Provenance, Stage, Subject,
    };
    use crate::telemetry::kinds::eta::EtaEstimateRecord;
    use crate::telemetry::trace::story_context;
    use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
    use chrono::{TimeZone, Utc};
    use opentelemetry_proto::tonic::common::v1::any_value::Value;

    fn record() -> EtaEstimateRecord {
        let as_of = Utc.with_ymd_and_hms(2026, 9, 28, 12, 0, 0).unwrap();
        let input = EstimateInput {
            subject: Subject::new("rjwalters/loom", Some(1_073_994_527), 9289),
            as_of,
            current: CurrentState::At(CurrentStage {
                stage: Stage::ReviewWait,
                entered_at: Some(as_of),
                age_sec: 0,
                age_source: AgeSource::Bus,
                rework_rounds: 0,
                episode_entered_at: None,
            }),
            features: Default::default(),
            features_omitted: Vec::new(),
            provenance: Provenance {
                version: "0.19.476".to_string(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
                tree_state: "clean".to_string(),
                complete: true,
            },
            dispatch: None,
            stalls: Vec::new(),
            held: None,
            queue: Vec::new(),
            dependencies: None,
        };
        EtaEstimateRecord {
            trigger: Trigger::Transition,
            primary: true,
            explanation: Box::new(LandV1.estimate(&input, &Default::default())),
        }
    }

    fn attr(log: &opentelemetry_proto::tonic::logs::v1::LogRecord, key: &str) -> Option<Value> {
        log.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone())
    }

    #[test]
    fn estimate_log_body_is_the_explanation_in_the_story_trace() {
        let record = record();
        let mut envelope =
            TelemetryEnvelope::new("host", TelemetryRecord::EtaEstimate(record.clone()));
        let story = story_context(1_073_994_527, 9289).unwrap();
        envelope.trace_context = Some(story.clone());
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "eta.estimate");
        let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
            panic!("string body");
        };
        let parsed: crate::eta::Explanation = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, *record.explanation);
        assert_eq!(log.trace_id, story.trace_id.bytes());
        for key in [
            "loom.eta.estimate_id",
            "loom.eta.kind",
            "loom.eta.heuristic",
            "loom.eta.version",
            "loom.eta.revision",
            "loom.eta.tree_state",
            "loom.eta.stage",
            "loom.eta.age_sec",
            "loom.eta.no_estimate_reason",
            "loom.repo",
            "loom.issue",
        ] {
            assert!(attr(&log, key).is_some(), "{key}");
        }
        assert_eq!(
            attr(&log, "loom.eta.revision"),
            Some(Value::StringValue("9d8e226ce0123456789abcdef0123456789abcde".to_string()))
        );
        assert_eq!(log.time_unix_nano, super::nanos(record.explanation.as_of));
    }

    #[test]
    fn every_eta_attribute_is_allowlisted() {
        use crate::eta::explanation::EstimateResult;
        use crate::eta::score::{score, EstimateSummary, OutcomeKind};
        use crate::telemetry::kinds::eta::{EtaOutcomeRecord, ETA_LOG_ATTRIBUTE_KEYS};
        // The fixture estimate is a refusal (no history); a copy with a
        // result exercises the quantile keys an answer adds.
        let estimate = record();
        let mut answered = estimate.clone();
        answered.explanation.result = Some(EstimateResult {
            p25_sec: 60,
            p50_sec: 120,
            p75_sec: 240,
            p90_sec: Some(300),
            eta_p50_at: answered.explanation.as_of + chrono::Duration::seconds(120),
            samples_min: 9,
            stage_marks: Vec::new(),
            tail_extrapolated: false,
        });
        let summary = EstimateSummary::of(&estimate.explanation);
        let mut summary_with_numbers = summary.clone();
        summary_with_numbers.p25_sec = Some(60);
        summary_with_numbers.p50_sec = Some(120);
        summary_with_numbers.p75_sec = Some(240);
        summary_with_numbers.p90_sec = Some(300);
        summary_with_numbers.pr_number = Some(9301);
        let at = summary.as_of + chrono::Duration::seconds(100);
        let outcome = EtaOutcomeRecord {
            score: score(&summary_with_numbers, OutcomeKind::Finished, at, &[]),
            estimate: summary_with_numbers,
            loom: estimate.explanation.loom.clone(),
            outcome_source: "sweep_terminal".to_string(),
            outcome_resolution_sec: Some(0),
            result: Some("exited".to_string()),
        };
        // The keys #10211 added are actually emitted, so the allowlist check
        // below is not vacuous for them.
        let expected: [(&TelemetryRecord, &[&str]); 2] = [
            (&TelemetryRecord::EtaEstimate(answered.clone()), &["loom.eta.p90_sec"]),
            (
                &TelemetryRecord::EtaOutcome(outcome.clone()),
                &[
                    "loom.eta.p25_sec",
                    "loom.eta.p75_sec",
                    "loom.eta.p90_sec",
                    "loom.eta.above_p90",
                    "loom.eta.pinball4_loss_sec",
                ],
            ),
        ];
        for (record, keys) in expected {
            let log = log_record_for(&TelemetryEnvelope::new("host", record.clone())).unwrap();
            for key in keys {
                assert!(attr(&log, key).is_some(), "{key} is emitted");
            }
        }
        for record in [
            TelemetryRecord::EtaEstimate(estimate),
            TelemetryRecord::EtaEstimate(answered),
            TelemetryRecord::EtaOutcome(outcome),
        ] {
            let log = log_record_for(&TelemetryEnvelope::new("host", record)).unwrap();
            for kv in &log.attributes {
                assert!(
                    ETA_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                        || [
                            "loom.repo",
                            "loom.issue",
                            "loom.pr_number",
                            "loom.record_id"
                        ]
                        .contains(&kv.key.as_str()),
                    "{} is not allowlisted",
                    kv.key
                );
            }
        }
    }

    #[test]
    fn an_outcome_row_carries_the_late_surprise_and_the_four_quantile_loss() {
        use crate::eta::score::{score, EstimateSummary, OutcomeKind};
        use crate::telemetry::kinds::eta::EtaOutcomeRecord;
        let estimate = record();
        let mut summary = EstimateSummary::of(&estimate.explanation);
        (summary.p25_sec, summary.p50_sec, summary.p75_sec) = (Some(600), Some(1200), Some(2400));
        let outcome = |p90: Option<i64>, actual: i64| {
            let mut summary = summary.clone();
            summary.p90_sec = p90;
            let at = summary.as_of + chrono::Duration::seconds(actual);
            let record = EtaOutcomeRecord {
                score: score(&summary, OutcomeKind::Landed, at, &[]),
                estimate: summary,
                loom: estimate.explanation.loom.clone(),
                outcome_source: "pulls_read".to_string(),
                outcome_resolution_sec: Some(0),
                result: None,
            };
            log_record_for(&TelemetryEnvelope::new("host", TelemetryRecord::EtaOutcome(record)))
                .unwrap()
        };
        // Landed at 3600 s, after p90 = 3000: a late surprise. The
        // three-quantile loss is ρ.25(3000) + ρ.5(2400) + ρ.75(1200)
        // = 750 + 1200 + 900 = 2850, and ρ.9(600) = 540 makes 3390.
        let late = outcome(Some(3000), 3600);
        assert_eq!(attr(&late, "loom.eta.p90_sec"), Some(Value::IntValue(3000)));
        assert_eq!(attr(&late, "loom.eta.above_p90"), Some(Value::BoolValue(true)));
        assert_eq!(attr(&late, "loom.eta.pinball_loss_sec"), Some(Value::DoubleValue(2850.0)));
        assert_eq!(attr(&late, "loom.eta.pinball4_loss_sec"), Some(Value::DoubleValue(3390.0)));
        assert_eq!(attr(&late, "loom.eta.p25_sec"), Some(Value::IntValue(600)));
        assert_eq!(attr(&late, "loom.eta.p75_sec"), Some(Value::IntValue(2400)));
        // A summary from before p90 existed: the three-quantile score still
        // rides, the p90 keys are absent (never `false`, never 0).
        let old = outcome(None, 3600);
        assert_eq!(attr(&old, "loom.eta.pinball_loss_sec"), Some(Value::DoubleValue(2850.0)));
        for key in [
            "loom.eta.p90_sec",
            "loom.eta.above_p90",
            "loom.eta.pinball4_loss_sec",
        ] {
            assert_eq!(attr(&old, key), None, "{key} is absent without a p90");
        }
    }

    #[test]
    fn a_fleet_refresh_record_emits_every_fleet_key_and_only_allowlisted_ones() {
        use crate::telemetry::kinds::eta::ETA_LOG_ATTRIBUTE_KEYS;
        use crate::telemetry::kinds::eta_fleet_refresh::EtaFleetRefreshRecord;
        let at = Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap();
        let fleet = EtaFleetRefreshRecord {
            repo: "rjwalters/loom".to_string(),
            cycle_id: "0123456789abcdef".to_string(),
            started_at: at,
            pass: "backfill".to_string(),
            stop_reason: "complete".to_string(),
            promoted: true,
            prs_read: 3,
            pass_done: 3,
            timelines_incomplete: 1,
            samples_added: 9,
            raw_events_added: Some(4),
            forge_calls: 5,
            not_modified_calls: 0,
            ratelimit_remaining_min: Some(4000),
            reader_app: Some("7".to_string()),
            snapshot_id: Some("feedface".to_string()),
            as_of: Some(at),
            duration_ms: 12,
            loom: record().explanation.loom.clone(),
        };
        let envelope =
            TelemetryEnvelope::new("host", TelemetryRecord::EtaFleetRefresh(fleet.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "eta.fleet_refresh");
        assert_eq!(log.time_unix_nano, super::nanos(at));
        for kv in &log.attributes {
            assert!(
                ETA_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                    || ["loom.repo", "loom.record_id"].contains(&kv.key.as_str()),
                "{} is not allowlisted",
                kv.key
            );
        }
        for key in ETA_LOG_ATTRIBUTE_KEYS
            .iter()
            .filter(|k| k.starts_with("loom.eta.fleet."))
        {
            assert!(attr(&log, key).is_some(), "{key} is emitted");
        }
        assert_eq!(
            attr(&log, "loom.eta.fleet.stop_reason"),
            Some(Value::StringValue("complete".into()))
        );
        assert_eq!(attr(&log, "loom.eta.fleet.samples_added"), Some(Value::IntValue(9)));
        let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
            panic!("string body");
        };
        let parsed: EtaFleetRefreshRecord = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, fleet);
    }

    #[test]
    fn an_eta_fit_record_emits_every_fit_key_and_only_allowlisted_ones() {
        use crate::telemetry::kinds::eta::ETA_LOG_ATTRIBUTE_KEYS;
        use crate::telemetry::kinds::eta_fit::{EtaFitRecord, EtaFitStage};
        let at = Utc.with_ymd_and_hms(2026, 10, 5, 1, 0, 0).unwrap();
        let fit = EtaFitRecord {
            check_id: "0123456789abcdef".to_string(),
            trigger: "fleet_refresh".to_string(),
            started_at: at,
            outcome: "written".to_string(),
            skip_reason: Some("none-but-present-for-the-test".to_string()),
            error: Some("boom".to_string()),
            fit_id: Some("feedface".to_string()),
            cutoff: Some(at),
            window_start: Some(at),
            window_days: Some(28),
            data_through: Some(at),
            snapshots: 2,
            snapshot_oldest_as_of: Some(at),
            snapshot_newest_as_of: Some(at),
            snapshot_as_of: [("rjwalters/loom".to_string(), at)].into(),
            stages: [(
                "review_wait".to_string(),
                EtaFitStage {
                    rows: 5,
                    exits: 2,
                    exit_censored: 1,
                    merge_events: 3,
                    merge_censored: 2,
                    hazard: true,
                    aft: false,
                },
            )]
            .into(),
            rows_total: Some(5),
            rows_censored: Some(1),
            rows_dropped_missing: Some(0),
            rows_dropped_no_flags: Some(0),
            rows_star_unknown: Some(0),
            dwells: Some(4),
            pruned: Some(0),
            coeff_file: Some("fit-20261005T000000Z.json".to_string()),
            coeff_bytes: Some(1234),
            coeff_sha256: Some("ab".repeat(32)),
            duration_ms: 12,
            loom: record().explanation.loom.clone(),
        };
        let envelope = TelemetryEnvelope::new("host", TelemetryRecord::EtaFit(fit.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "eta.fit");
        assert_eq!(log.time_unix_nano, super::nanos(at));
        for kv in &log.attributes {
            assert!(
                ETA_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                    || ["loom.repo", "loom.record_id"].contains(&kv.key.as_str()),
                "{} is not allowlisted",
                kv.key
            );
        }
        for key in ETA_LOG_ATTRIBUTE_KEYS
            .iter()
            .filter(|k| k.starts_with("loom.eta.fit."))
        {
            assert!(attr(&log, key).is_some(), "{key} is emitted");
        }
        assert_eq!(attr(&log, "loom.eta.fit.rows_total"), Some(Value::IntValue(5)));
        let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
            panic!("string body");
        };
        let parsed: EtaFitRecord = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, fit);
    }

    #[test]
    fn a_pr_resolved_record_is_stamped_at_the_merge_and_observed_at_the_pass() {
        use crate::telemetry::kinds::eta::ETA_LOG_ATTRIBUTE_KEYS;
        use crate::telemetry::kinds::pr_resolved::{PrResolution, PrResolvedRecord};
        let merged_at = Utc.with_ymd_and_hms(2026, 10, 6, 12, 0, 0).unwrap();
        let observed_at = merged_at + chrono::Duration::seconds(240);
        let resolved = PrResolvedRecord {
            repo: "rjwalters/loom".to_string(),
            pr_number: 10547,
            issue: Some(10511),
            state: PrResolution::Merged,
            resolved_at: merged_at,
            observed_at,
            resolution_sec: 0,
            loom: record().explanation.loom.clone(),
        };
        let envelope =
            TelemetryEnvelope::new("host", TelemetryRecord::PrResolved(resolved.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "pr.resolved");
        assert_eq!(log.time_unix_nano, super::nanos(merged_at), "event time");
        assert_eq!(log.observed_time_unix_nano, super::nanos(observed_at), "knowable-at");
        for kv in &log.attributes {
            assert!(
                ETA_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                    || [
                        "loom.repo",
                        "loom.record_id",
                        "loom.pr_number",
                        "loom.issue"
                    ]
                    .contains(&kv.key.as_str()),
                "{} is not allowlisted",
                kv.key
            );
        }
        for key in ETA_LOG_ATTRIBUTE_KEYS
            .iter()
            .filter(|k| k.starts_with("loom.eta.pr."))
        {
            assert!(attr(&log, key).is_some(), "{key} is emitted");
        }
        assert_eq!(attr(&log, "loom.pr_number"), Some(Value::IntValue(10547)));
        assert_eq!(attr(&log, "loom.eta.pr.state"), Some(Value::StringValue("merged".to_string())));
        assert!(attr(&log, "loom.eta.authority").is_none(), "not an estimate");
        let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
            panic!("string body");
        };
        let parsed: PrResolvedRecord = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, resolved);
    }
}
