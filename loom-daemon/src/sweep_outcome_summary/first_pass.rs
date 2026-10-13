//! Group-label resolution for `--group-by model` and the first-pass Judge
//! approval-rate fold (Issue #8542), plus the render-time footnote helper.
//! Split out of `sweep_outcome_summary.rs` to stay under the file-size ratchet
//! threshold (`scripts/check-file-size-budget.sh`, #7711) rather than growing
//! that file past it. The `complexity` / `model-complexity` dimensions that
//! once lived here were retired in #11665.

use super::{Accum, GroupRow};
use crate::telemetry::SweepOutcomeRecord;

/// Resolve a record's `model` group label — `default` for a record with no
/// explicit model, exactly as `GroupBy::Model` resolves it.
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
