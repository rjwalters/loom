//! `--group-by complexity` / `--group-by model-complexity` support (Issue
//! #8542): group-label resolution, the first-pass Judge approval-rate fold,
//! and the render-time note/footnote helpers. Split out of
//! `sweep_outcome_summary.rs` to stay under the file-size ratchet threshold
//! (`scripts/check-file-size-budget.sh`, #7711) rather than growing that file
//! past it.

use super::{Accum, GroupBy, GroupRow, UNKNOWN_GROUP};
use crate::telemetry::SweepOutcomeRecord;

/// Resolve a record's complexity-tier group label: the tier verbatim when the
/// marker was read, else [`UNKNOWN_GROUP`] — never dropped, matching
/// [`super::resolve_arm`]'s "nothing is dropped for want of a group" rule.
#[must_use]
pub fn resolve_complexity(record: &SweepOutcomeRecord) -> String {
    record
        .complexity
        .clone()
        .unwrap_or_else(|| UNKNOWN_GROUP.to_string())
}

/// Resolve a record's `model` group label — `default` for a record with no
/// explicit model, exactly as `GroupBy::Model` resolves it. Factored out so
/// [`GroupBy::ModelComplexity`]'s compound key reuses the identical fold.
#[must_use]
pub fn resolve_model_label(record: &SweepOutcomeRecord) -> String {
    record
        .model
        .clone()
        .unwrap_or_else(|| "default".to_string())
}

/// `Some(true/false)` if `record.judge_verdicts` has a first entry (judged),
/// else `None`. `Some([])` (an observed-but-unjudged PR — the sweep died
/// before Judge) also maps to `None`: the same "unknown != zero" contract
/// `judge_verdicts` itself uses, so it must not deflate the denominator
/// behind `first_pass_approval_rate`. Only the FIRST verdict (attempt 1)
/// settles the fold.
#[must_use]
pub fn first_pass_verdict(record: &SweepOutcomeRecord) -> Option<bool> {
    record
        .judge_verdicts
        .as_ref()
        .and_then(|verdicts| verdicts.first())
        .map(|first| first.verdict == "pass")
}

/// The report note for a `complexity`/`model-complexity` grouping whose rows
/// include the `unknown` bucket — `None` otherwise.
pub fn unknown_group_note(group_by: GroupBy, rows: &[GroupRow]) -> Option<String> {
    let has_unknown = matches!(group_by, GroupBy::Complexity | GroupBy::ModelComplexity)
        && rows.iter().any(|r| r.group.contains(UNKNOWN_GROUP));
    has_unknown.then(|| {
        "records with no Curator complexity marker observed are bucketed as 'unknown' rather \
         than dropped (issue #8542) — group counts still sum to records_grouped."
            .to_string()
    })
}

/// Accumulate one record's first-pass Judge verdict into `acc` — see
/// [`first_pass_verdict`] for the "unknown != zero" contract.
pub fn accumulate_first_pass(acc: &mut Accum, record: &SweepOutcomeRecord) {
    if let Some(passed) = first_pass_verdict(record) {
        acc.first_pass_judged += 1;
        if passed {
            acc.first_pass_approved += 1;
        }
    }
}

/// Render one row's `JDG1%` / `JDG_N` tail pair for the text table.
#[must_use]
pub fn render_first_pass_tail(row: &GroupRow) -> (String, usize) {
    let pct = row
        .first_pass_approval_rate
        .map_or_else(|| "-".to_string(), |r| format!("{:.1}%", r * 100.0));
    (pct, row.first_pass_judged)
}

/// The `render_text` footnote explaining `JDG1%`/`JDG_N`.
pub const FIRST_PASS_NOTE: &str = "JDG1% is the first-pass Judge approval rate \
    (judge_verdicts[0].verdict == \"pass\") over JDG_N judged sweeps in the group; '-' means no \
    sweep in the group was judged (#8542).\n";
