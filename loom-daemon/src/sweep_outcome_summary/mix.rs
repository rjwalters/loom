//! `--group-by mix`: the role-model-mix read-out (Issue #14067, split from
//! #13956), plus the review-bounce fold that makes it comparable. Split out of
//! `sweep_outcome_summary.rs` for the same file-size reason as `first_pass`.
//!
//! A **mix** is the configuration an operator is experimenting with: which
//! model ran each role, on which runtime/provider, at which reasoning effort.
//! No record carries a mix label, so the label is *derived* from fields the
//! `sweep.outcome` record already has — no new telemetry, no new store:
//!
//! ```text
//! curator=claude-opus-5 builder=claude-sonnet-5 judge=claude-opus-5 | claude/- | effort=high
//! ```
//!
//! * **Per-role models** come from each phase's own `tokens_by_model` (what the
//!   phase actually ran, not what was dispatched): the model with the most
//!   tokens in that role. Only `curator`, `builder`
//!   and `judge` are part of the key: `doctor` runs only when a review
//!   bounced, so keying on it would split one mix into "needed a Doctor" and
//!   "did not" — an outcome, reported in the bounce columns instead. A role
//!   whose phase carries no per-model usage is omitted, never guessed; a
//!   record with no per-phase usage at all falls back to `model=<dispatched>`.
//! * **Runtime/provider** read `runtime`/`provider` (the launch record), then
//!   `config["runtime"]`. An absent half renders `-` — a Claude launch writes
//!   no provider, and none is invented for it.
//! * **Effort** prefers each role's phases' own `effort` stamps (#14242),
//!   falling back to the sweep-level `effort`; `default` when none was set.
//!   Agreeing values render one `effort=<v>` (so history keeps grouping);
//!   differing values render per role, `effort=curator:high builder:medium`.
//!
//! A sweep that died before a role ran has fewer `role=` parts and so forms its
//! own row; `--exclude-spawn-deaths` removes the common case.

use std::collections::BTreeMap;

use super::{first_pass, fmt_opt_f64, Accum, SummaryReport};
use crate::telemetry::SweepOutcomeRecord;

/// Roles whose model is part of the mix key, in key order. `doctor` and
/// `merge` are deliberately absent — see the module docs.
const MIX_ROLES: [&str; 3] = ["curator", "builder", "judge"];

/// The model each of [`MIX_ROLES`] mainly ran, from the phases' own per-model
/// usage: the model with the most tokens across the role's attempts (ties break
/// to the lexicographically first). One dominant model rather than the union,
/// so a one-off escalation inside a role does not split a mix into its own row.
fn role_models(record: &SweepOutcomeRecord) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for role in MIX_ROLES {
        let mut by_model: BTreeMap<&str, i64> = BTreeMap::new();
        for t in record
            .phase_durations
            .iter()
            .filter(|p| p.phase.eq_ignore_ascii_case(role))
            .filter_map(|p| p.tokens_by_model.as_deref())
            .flatten()
        {
            *by_model.entry(t.model.as_str()).or_default() +=
                t.input + t.output + t.cache_read + t.cache_write_5m + t.cache_write_1h;
        }
        // BTreeMap iterates in key order; `max_by_key` keeps the LAST maximum,
        // so reverse to make a tie resolve to the first key.
        if let Some((model, _)) = by_model.iter().rev().max_by_key(|(_, tokens)| **tokens) {
            out.push((role, (*model).to_string()));
        }
    }
    out
}

fn nonblank(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|v| !v.is_empty())
}

/// The mix key's effort term (Issue #14242): each [`MIX_ROLES`] role's
/// phases' own `effort` stamps first, a role without one falling back to the
/// sweep-level `effort` — so a record with no stamps at all renders exactly
/// the pre-#14242 label. All values agreeing renders the one `effort=<v>`
/// the sweep level always rendered, keeping a homogeneous sweep in the same
/// row as its own history; disagreeing values render per role
/// (`effort=curator:high builder:medium`, a role's own attempts disagreeing
/// joined by `/`) so two mixes that differ only in a role's effort stay
/// separate rows. `doctor`/`merge` efforts are deliberately not read — the
/// same exclusion rationale as their models.
fn effort_term(record: &SweepOutcomeRecord) -> String {
    let sweep = nonblank(record.effort.as_deref()).map(str::to_string);
    let mut per_role: Vec<(&'static str, Vec<String>)> = Vec::new();
    let mut distinct: Vec<String> = Vec::new();
    for role in MIX_ROLES {
        let role_phases: Vec<_> = record
            .phase_durations
            .iter()
            .filter(|p| p.phase.eq_ignore_ascii_case(role))
            .collect();
        let mut efforts: Vec<String> = role_phases
            .iter()
            .filter_map(|p| nonblank(p.effort.as_deref()))
            .map(str::to_string)
            .collect();
        efforts.sort();
        efforts.dedup();
        if efforts.is_empty() && !role_phases.is_empty() {
            // Fall back per role, not per record, so one stamped role never
            // masks another's different value. A role that never ran is
            // omitted, never guessed — same contract as its model.
            if let Some(e) = &sweep {
                efforts.push(e.clone());
            }
        }
        for e in &efforts {
            if !distinct.contains(e) {
                distinct.push(e.clone());
            }
        }
        if !efforts.is_empty() {
            per_role.push((role, efforts));
        }
    }
    if per_role.is_empty() {
        // No MIX_ROLES phase at all (none ran, or only doctor/merge did):
        // exactly the pre-#14242 sweep-level rendering.
        return sweep.unwrap_or_else(|| "default".to_string());
    }
    match distinct.len() {
        0 => unreachable!("per_role non-empty implies at least one effort"),
        1 => distinct.remove(0),
        _ => per_role
            .iter()
            .map(|(role, efforts)| format!("{role}:{}", efforts.join("/")))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// A record's derived mix label — see the module docs for the grammar.
#[must_use]
pub fn resolve_mix(record: &SweepOutcomeRecord) -> String {
    let roles = role_models(record);
    let models = if roles.is_empty() {
        format!("model={}", record.model.as_deref().unwrap_or("default"))
    } else {
        roles
            .iter()
            .map(|(role, model)| format!("{role}={model}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let runtime = nonblank(record.runtime.as_deref())
        .or_else(|| nonblank(record.config.get("runtime").map(String::as_str)))
        .unwrap_or("-");
    let provider = nonblank(record.provider.as_deref()).unwrap_or("-");
    format!("{models} | {runtime}/{provider} | effort={}", effort_term(record))
}

/// Fold one record's review bounces into `acc`. Same "unknown != zero"
/// contract as `judge_verdicts`/`doctor_cycles` themselves, applied to each
/// field on its own: `judge_verdicts` feeds `judge_rejections` and the
/// `bounce_records` (`BNC_N`) count; `doctor_cycles` is summed on its own,
/// outside that gate. A record carrying neither adds to nothing. The two fields
/// are paired in every record the current writer emits, but a pre-#8222 record
/// carries `doctor_cycles` (the retired phase-history proxy) with no
/// `judge_verdicts`; its cycles still count, though it is not in `BNC_N`, so
/// `doctor_cycles` can exceed what `bounce_records` alone would justify.
pub fn accumulate_bounces(acc: &mut Accum, record: &SweepOutcomeRecord) {
    if let Some(verdicts) = &record.judge_verdicts {
        acc.bounce_records += 1;
        acc.judge_rejections += verdicts.iter().filter(|v| v.verdict == "fail").count();
    }
    if let Some(cycles) = record.doctor_cycles {
        acc.doctor_cycles += cycles as usize;
    }
}

/// Render the mix table: each mix key on its own line (it names the models,
/// provider and effort, so it is wider than any column), then the comparison
/// columns beneath it. `render_text` prints the shared header before this.
#[must_use]
pub fn render_rows(report: &SummaryReport) -> String {
    let mut out = format!(
        "{:>6} {:>6} {:>9} {:>9} {:>7} {:>6} {:>5} {:>6} {:>6} {:>8}\n",
        "SWEEPS",
        "MERGED",
        "WTOK_USD",
        "MERGE/USD",
        "JDG1%",
        "JDG_N",
        "REJ",
        "DR_CYC",
        "BNC_N",
        "DOCTOR%"
    );
    for row in &report.rows {
        let (first_pass, first_pass_judged) = first_pass::render_first_pass_tail(row);
        out.push_str(&format!(
            "\n{}\n{:>6} {:>6} {:>9} {:>9} {:>7} {:>6} {:>5} {:>6} {:>6} {:>7.1}%\n",
            row.group,
            row.sweeps,
            row.merged_prs
                .map_or_else(|| "n/a".to_string(), |m| m.to_string()),
            fmt_opt_f64(row.weighted_tokens_usd, 2),
            fmt_opt_f64(row.merges_per_weighted_token, 3),
            first_pass,
            first_pass_judged,
            row.judge_rejections,
            row.doctor_cycles,
            row.bounce_records,
            row.doctor_phase_rate * 100.0,
        ));
    }
    out.push('\n');
    out.push_str(&format!("Rate card: {}\n", report.rate_card.caveat));
    out.push_str(first_pass::FIRST_PASS_NOTE);
    out.push_str(MIX_NOTE);
    for note in &report.notes {
        out.push_str(&format!("Note: {note}\n"));
    }
    out
}

/// The `render_text` footnote for the mix table.
pub const MIX_NOTE: &str = "REJ = Judge `fail` verdicts, summed over the BNC_N sweeps whose PR \
    timeline was read (an unread timeline is unknown, not zero). DR_CYC = completed Doctor cycles, \
    summed over every sweep carrying `doctor_cycles`: that is BNC_N plus any pre-#8222 sweep that \
    has the cycle count but no Judge verdicts, so DR_CYC / BNC_N overstates the per-sweep rate on \
    old history. The mix key omits `doctor` on purpose, and a sweep that died before a role ran \
    forms its own row (#14067).\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script_helpers::sweep_experiment::ModelUsageTotals;
    use crate::telemetry::{JudgeVerdict, PhaseDuration, SweepOutcomeRecord};

    fn usage(model: &str) -> Vec<ModelUsageTotals> {
        vec![ModelUsageTotals {
            model: model.into(),
            speed: "standard".into(),
            service_tier: "standard".into(),
            input: 1,
            cache_read: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            output: 1,
        }]
    }

    fn phase(name: &str, model: &str) -> PhaseDuration {
        PhaseDuration {
            tokens_by_model: Some(usage(model)),
            ..PhaseDuration::new(name, 10)
        }
    }

    fn rec() -> SweepOutcomeRecord {
        serde_json::from_value(serde_json::json!({
            "issue": 1, "sweep_id": "s", "total_duration_sec": 1,
            "result": "success"
        }))
        .expect("minimal record")
    }

    #[test]
    fn label_names_per_role_models_runtime_provider_and_effort() {
        let mut r = rec();
        r.effort = Some("high".into());
        r.config.insert("runtime".into(), "claude".into());
        r.phase_durations = vec![
            phase("curator", "opus"),
            phase("builder", "sonnet"),
            phase("judge", "opus"),
            // Not part of the key.
            phase("doctor", "opus"),
        ];
        assert_eq!(
            resolve_mix(&r),
            "curator=opus builder=sonnet judge=opus | claude/- | effort=high"
        );
    }

    #[test]
    fn doctor_phase_does_not_split_a_mix() {
        let mut with = rec();
        with.phase_durations = vec![phase("builder", "sonnet"), phase("doctor", "opus")];
        let mut without = rec();
        without.phase_durations = vec![phase("builder", "sonnet")];
        assert_eq!(resolve_mix(&with), resolve_mix(&without));
    }

    #[test]
    fn falls_back_to_dispatched_model_without_per_phase_usage() {
        let mut r = rec();
        r.model = Some("sonnet".into());
        r.runtime = Some("pi".into());
        r.provider = Some("zai".into());
        assert_eq!(resolve_mix(&r), "model=sonnet | pi/zai | effort=default");
        assert_eq!(resolve_mix(&rec()), "model=default | -/- | effort=default");
    }

    #[test]
    fn a_role_keys_on_its_dominant_model_so_escalation_does_not_split_a_mix() {
        let mut r = rec();
        let mut rescue = phase("judge", "opus");
        rescue.tokens_by_model.as_mut().unwrap()[0].input = 5;
        let mut main = phase("judge", "haiku");
        main.tokens_by_model.as_mut().unwrap()[0].input = 500;
        r.phase_durations = vec![rescue, main];
        assert!(resolve_mix(&r).starts_with("judge=haiku |"));
    }

    fn phase_at(name: &str, model: &str, effort: Option<&str>) -> PhaseDuration {
        PhaseDuration {
            tokens_by_model: Some(usage(model)),
            effort: effort.map(str::to_string),
            ..PhaseDuration::new(name, 10)
        }
    }

    #[test]
    fn a_homogeneous_per_phase_effort_keeps_the_sweep_level_label() {
        // Back-compat (#14242): a record whose phases all carry one effort
        // renders exactly the label the sweep-level value always rendered, so
        // it keeps grouping with its own pre-stamp history.
        let mut unstamped = rec();
        unstamped.effort = Some("high".into());
        unstamped.phase_durations = vec![
            phase_at("curator", "opus", None),
            phase_at("builder", "sonnet", None),
        ];
        let mut stamped = unstamped.clone();
        for p in &mut stamped.phase_durations {
            p.effort = Some("high".into());
        }
        assert_eq!(resolve_mix(&unstamped), resolve_mix(&stamped));
        assert!(resolve_mix(&stamped).ends_with("| effort=high"));
    }

    #[test]
    fn per_phase_effort_distinguishes_mixes_differing_only_in_a_role() {
        // AC #14242: two mixes identical except one role's effort are
        // separate rows — whether the difference is against the sweep level
        // or between two roles.
        let mut all_high = rec();
        all_high.effort = Some("high".into());
        all_high.phase_durations = vec![
            phase_at("curator", "opus", Some("high")),
            phase_at("builder", "sonnet", Some("high")),
            phase_at("judge", "opus", Some("high")),
        ];
        let mut builder_medium = all_high.clone();
        builder_medium.phase_durations[1] = phase_at("builder", "sonnet", Some("medium"));
        assert_ne!(resolve_mix(&all_high), resolve_mix(&builder_medium));
        assert_eq!(
            resolve_mix(&all_high),
            "curator=opus builder=sonnet judge=opus | -/- | effort=high"
        );
        assert_eq!(
            resolve_mix(&builder_medium),
            "curator=opus builder=sonnet judge=opus | -/- | effort=curator:high builder:medium judge:high"
        );
    }

    #[test]
    fn a_role_without_a_stamp_falls_back_to_the_sweep_level_effort() {
        let mut r = rec();
        r.effort = Some("high".into());
        r.phase_durations = vec![
            phase_at("curator", "opus", None),
            phase_at("builder", "sonnet", Some("medium")),
        ];
        assert_eq!(
            resolve_mix(&r),
            "curator=opus builder=sonnet | -/- | effort=curator:high builder:medium"
        );
    }

    #[test]
    fn an_unstamped_effort_field_is_absent_not_serialized() {
        let p = phase_at("builder", "sonnet", None);
        let value = serde_json::to_value(&p).unwrap();
        assert!(value.get("effort").is_none(), "absent key, not null: {value}");
        let p = phase_at("builder", "sonnet", Some("high"));
        assert_eq!(serde_json::to_value(&p).unwrap()["effort"], "high");
    }

    #[test]
    fn bounces_skip_records_whose_timeline_was_never_read() {
        let mut acc = Accum::default();
        let mut judged = rec();
        judged.judge_verdicts = Some(vec![
            JudgeVerdict {
                attempt: 1,
                verdict: "fail".into(),
            },
            JudgeVerdict {
                attempt: 2,
                verdict: "pass".into(),
            },
        ]);
        judged.doctor_cycles = Some(1);
        accumulate_bounces(&mut acc, &judged);
        accumulate_bounces(&mut acc, &rec());
        assert_eq!((acc.bounce_records, acc.judge_rejections, acc.doctor_cycles), (1, 1, 1));
    }
}
