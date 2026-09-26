//! Curator-vs-Jev complexity-tier agreement for `--group-by complexity` /
//! `--group-by model-complexity` (Issue #8608).
//!
//! The Curator's tier lives on the telemetry record
//! ([`SweepOutcomeRecord::complexity`], resolved by
//! [`super::complexity::resolve_complexity`]); Jev's shadow-mode tier lives
//! only on the narrower sibling `sweep-outcomes.jsonl` journal
//! ([`crate::sweep_outcomes::OutcomeRecord::jev_tier`], issue #8543). Both key
//! on `sweep_id`, so the join rides on [`SpawnDeathIndex`] — the `sweep_id`
//! index this report already builds from that sibling journal — rather than
//! changing either journal's schema.
//!
//! Each complexity row gains an agree / disagree / unknown count, and the
//! existing first-pass Judge approval rate
//! ([`super::complexity::first_pass_verdict`], reused as-is) is additionally
//! split by agree vs disagree. Split out of `sweep_outcome_summary.rs` to stay
//! under the file-size ratchet threshold (`scripts/check-file-size-budget.sh`).

use serde::Serialize;

use super::complexity::first_pass_verdict;
use super::{GroupBy, GroupRow, SpawnDeathIndex, SummaryReport};
use crate::telemetry::SweepOutcomeRecord;

/// One sweep's Curator-vs-Jev agreement label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Agreement {
    /// Both tiers observed and equal.
    Agree,
    /// Both tiers observed and different.
    Disagree,
    /// Either side absent — no Curator marker observed, or no Jev tier in the
    /// sibling journal for this `sweep_id` (Jev only runs when
    /// `TYPESAFE_API_KEY` is set). Mirrors the `unknown`-bucket convention of
    /// [`super::complexity::unknown_group_note`]: counted, never dropped.
    Unknown,
}

impl SpawnDeathIndex {
    /// Jev's tier for `sweep_id`, when the sibling journal recorded one.
    #[must_use]
    pub fn jev_tier_for(&self, sweep_id: &str) -> Option<&str> {
        self.jev_tiers.get(sweep_id).map(String::as_str)
    }
}

/// Resolve `record`'s agreement label by joining its `sweep_id` against the
/// sibling journal's Jev tier. Tiers compare case-insensitively after
/// trimming, so a stray capital or whitespace never manufactures a
/// disagreement.
#[must_use]
pub fn resolve_agreement(record: &SweepOutcomeRecord, index: &SpawnDeathIndex) -> Agreement {
    match (record.complexity.as_deref(), index.jev_tier_for(&record.sweep_id)) {
        (Some(curator), Some(jev)) if curator.trim().eq_ignore_ascii_case(jev.trim()) => {
            Agreement::Agree
        }
        (Some(_), Some(_)) => Agreement::Disagree,
        _ => Agreement::Unknown,
    }
}

/// One row's (or the report's) agreement split. Only populated for the
/// `complexity` / `model-complexity` groupings.
///
/// **Invariant:** `agree + disagree + unknown` equals the row's `sweeps`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AgreementSplit {
    /// Sweeps whose Curator tier equals Jev's.
    pub agree: usize,
    /// Sweeps whose Curator tier differs from Jev's.
    pub disagree: usize,
    /// Sweeps missing either tier.
    pub unknown: usize,
    /// Judged sweeps among `agree` — the denominator for
    /// `agree_first_pass_approval_rate`.
    pub agree_first_pass_judged: usize,
    /// Of `agree_first_pass_judged`, first-pass approvals.
    pub agree_first_pass_approved: usize,
    /// First-pass Judge approval rate over `agree` sweeps. `None` when none
    /// was judged — never a fabricated `0.0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agree_first_pass_approval_rate: Option<f64>,
    /// Judged sweeps among `disagree`.
    pub disagree_first_pass_judged: usize,
    /// Of `disagree_first_pass_judged`, first-pass approvals.
    pub disagree_first_pass_approved: usize,
    /// First-pass Judge approval rate over `disagree` sweeps. `None` when
    /// none was judged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disagree_first_pass_approval_rate: Option<f64>,
}

impl AgreementSplit {
    /// Fold one record in.
    pub fn accumulate(&mut self, record: &SweepOutcomeRecord, index: &SpawnDeathIndex) {
        let verdict = first_pass_verdict(record);
        let (judged, approved) = match resolve_agreement(record, index) {
            Agreement::Agree => {
                self.agree += 1;
                (&mut self.agree_first_pass_judged, &mut self.agree_first_pass_approved)
            }
            Agreement::Disagree => {
                self.disagree += 1;
                (&mut self.disagree_first_pass_judged, &mut self.disagree_first_pass_approved)
            }
            Agreement::Unknown => {
                self.unknown += 1;
                return;
            }
        };
        if let Some(passed) = verdict {
            *judged += 1;
            if passed {
                *approved += 1;
            }
        }
    }

    /// Compute the two rates from the counts.
    #[must_use]
    fn with_rates(mut self) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let rate =
            |approved: usize, judged: usize| (judged > 0).then(|| approved as f64 / judged as f64);
        self.agree_first_pass_approval_rate =
            rate(self.agree_first_pass_approved, self.agree_first_pass_judged);
        self.disagree_first_pass_approval_rate =
            rate(self.disagree_first_pass_approved, self.disagree_first_pass_judged);
        self
    }
}

/// Whether `group_by` carries the agreement split.
fn applies(group_by: GroupBy) -> bool {
    matches!(group_by, GroupBy::Complexity | GroupBy::ModelComplexity)
}

/// Finish one group's accumulated split into its row value — `None` for any
/// grouping other than `complexity` / `model-complexity`, so those reports'
/// JSON is unchanged.
#[must_use]
pub fn finish(acc: &AgreementSplit, group_by: GroupBy) -> Option<AgreementSplit> {
    applies(group_by).then(|| acc.clone().with_rates())
}

/// The report-level split: every row's counts summed, rates recomputed from
/// the summed counts (never averaged across rows).
#[must_use]
pub fn totals(rows: &[GroupRow], group_by: GroupBy) -> Option<AgreementSplit> {
    if !applies(group_by) {
        return None;
    }
    let mut total = AgreementSplit::default();
    for split in rows.iter().filter_map(|r| r.agreement.as_ref()) {
        total.agree += split.agree;
        total.disagree += split.disagree;
        total.unknown += split.unknown;
        total.agree_first_pass_judged += split.agree_first_pass_judged;
        total.agree_first_pass_approved += split.agree_first_pass_approved;
        total.disagree_first_pass_judged += split.disagree_first_pass_judged;
        total.disagree_first_pass_approved += split.disagree_first_pass_approved;
    }
    Some(total.with_rates())
}

/// The report note for a complexity grouping with any `unknown` agreement.
#[must_use]
pub fn unknown_agreement_note(rows: &[GroupRow]) -> Option<String> {
    rows.iter()
        .filter_map(|r| r.agreement.as_ref())
        .any(|s| s.unknown > 0)
        .then(|| {
            "sweeps missing either the Curator tier or a Jev tier (joined by sweep_id from the \
             sibling sweep-outcomes.jsonl; Jev only runs when TYPESAFE_API_KEY is set) count as \
             agreement 'unknown' rather than being dropped — AGREE + DISAGR + UNK_AGR sums to \
             each row's sweeps (issue #8608)."
                .to_string()
        })
}

fn fmt_rate(rate: Option<f64>) -> String {
    rate.map_or_else(|| "-".to_string(), |r| format!("{:.1}%", r * 100.0))
}

fn render_line(out: &mut String, group: &str, s: &AgreementSplit) {
    out.push_str(&format!(
        "{:<26} {:>6} {:>6} {:>7} {:>9} {:>9} {:>9} {:>9}\n",
        group,
        s.agree,
        s.disagree,
        s.unknown,
        fmt_rate(s.agree_first_pass_approval_rate),
        s.agree_first_pass_judged,
        fmt_rate(s.disagree_first_pass_approval_rate),
        s.disagree_first_pass_judged,
    ));
}

/// The Curator-vs-Jev agreement table, rendered below the main table for a
/// complexity grouping — empty for any other grouping. The main table's
/// per-tier `JDG1%`/`JDG_N` is left untouched; this is the additional
/// agree/disagree split beside it.
#[must_use]
pub fn render_text(report: &SummaryReport) -> String {
    let Some(total) = report.agreement_totals.as_ref() else {
        return String::new();
    };
    let mut out = String::from("\nCurator-vs-Jev tier agreement (#8608)\n");
    out.push_str(&format!(
        "{:<26} {:>6} {:>6} {:>7} {:>9} {:>9} {:>9} {:>9}\n",
        "GROUP", "AGREE", "DISAGR", "UNK_AGR", "AGR_JDG1%", "AGR_JDG_N", "DIS_JDG1%", "DIS_JDG_N"
    ));
    for row in &report.rows {
        if let Some(split) = row.agreement.as_ref() {
            render_line(&mut out, &row.group, split);
        }
    }
    render_line(&mut out, "TOTAL", total);
    out.push_str(
        "AGR_/DIS_JDG1% split each group's first-pass Judge approval rate by whether the \
         Curator's tier matched Jev's shadow tier; '-' means no sweep on that side was judged.\n",
    );
    out
}
