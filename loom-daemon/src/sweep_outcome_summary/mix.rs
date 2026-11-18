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
//! * **Effort** is the sweep-level `effort`; `default` when none was set. It
//!   is not recorded per phase.
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
    let effort = nonblank(record.effort.as_deref()).unwrap_or("default");
    format!("{models} | {runtime}/{provider} | effort={effort}")
}

/// Fold one record's review bounces into `acc`. Same "unknown != zero"
/// contract as `judge_verdicts`/`doctor_cycles` themselves: a record whose
/// forge timeline was never read adds to neither the count nor the denominator.
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
pub const MIX_NOTE: &str = "REJ = Judge `fail` verdicts and DR_CYC = completed Doctor cycles, \
    both summed over the BNC_N sweeps whose PR timeline was read (an unread timeline is unknown, \
    not zero). The mix key omits `doctor` on purpose, and a sweep that died before a role ran \
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
