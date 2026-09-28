//! The per-repo body of one claim-reconciliation pass (Issue #4348), and the
//! mid-pass rate-limit-breaker re-check Issue #8953 added to it.
//!
//! Lives in its own sibling module rather than inline in
//! `claim_reconciliation.rs`: that file is already over the file-size
//! ratchet's threshold and therefore frozen at its current size (see
//! `.loom/docs/file-size-policy.md`), and the loop is the one piece of
//! `run_reconciliation_pass` worth unit-testing on its own — everything else
//! in that function is env-gating and summary logging.

use std::path::Path;

use super::{forge, review_conflict, VerdictReconcileStats};

/// Aggregated counts from [`run_reconciliation_pass_over_roots`] — the same
/// four scalar accumulators `run_reconciliation_pass` used to keep as loose
/// locals before Issue #8953 extracted the per-repo loop into its own
/// testable function, plus `roots_processed` so callers/tests can see
/// whether a mid-pass breaker trip cut the loop short.
#[derive(Default)]
pub(super) struct ReconciliationPassStats {
    pub(super) total_checked: usize,
    pub(super) total_reclaimed: usize,
    pub(super) total_pr_checked: usize,
    pub(super) total_pr_reclaimed: usize,
    pub(super) verdict_stats: VerdictReconcileStats,
    /// Number of `roots` actually visited before the loop returned. Equal to
    /// `roots.len()` unless `is_suppressed` tripped mid-pass and stopped the
    /// remainder short (Issue #8953).
    pub(super) roots_processed: usize,
}

/// The per-repo body of one reconciliation pass (Issue #4348), split out of
/// [`super::run_reconciliation_pass`] so it can be exercised in tests with an
/// injected `is_suppressed` closure instead of the process-global rate-limit
/// breaker singleton. `rate_limit_breaker`'s `GLOBAL` handle is a `OnceLock`
/// shared by the entire test binary ("first registration wins" — see its own
/// doc comment); registering it from a test here would leak into every other
/// test that happens to run afterward in the same process, exactly the
/// hazard `observability/collector/tests.rs`'s `dispatch_halt_from_breaker`
/// tests document and avoid for the sibling `host_breaker`. Injecting the
/// check as a closure keeps this function pure-testable without touching
/// `GLOBAL` at all.
///
/// Re-checks `is_suppressed()` at the top of **every** iteration, not just
/// once before the loop starts (Issue #8953): a rate-limit trip triggered by
/// repo N's `gh` call (surfaced via `global_observe_failure` inside
/// [`forge::reconcile_workspace`] / [`forge::reconcile_pr_claims`] /
/// [`forge::reconcile_pr_verdicts`]) now stops repos `N+1..roots.len()` from
/// making their own doomed `gh` calls in the *same* pass — mirroring the
/// "protect the rest of the current pass, not just the next one" shape #7619
/// already applied to `work_finder`'s own dispatch-guard chain. Before this,
/// the breaker was consulted only once before the loop
/// ([`super::run_reconciliation_pass`]'s own top-of-function check), so a
/// trip on repo N still let repos N+1..len() each issue their full `gh pr
/// list` / `gh issue list` fan-out against an already-exhausted shared quota.
pub(super) fn run_reconciliation_pass_over_roots(
    roots: &[std::path::PathBuf],
    gh_bin: &Path,
    is_startup: bool,
    is_suppressed: impl Fn() -> bool,
) -> ReconciliationPassStats {
    let mut stats = ReconciliationPassStats::default();
    for root in roots {
        if is_suppressed() {
            log::info!(
                "claim_reconciliation: pass stopping early after {}/{} workspace(s) — shared \
                 GitHub API rate limit exhausted mid-pass (#8953)",
                stats.roots_processed,
                roots.len()
            );
            break;
        }
        let (checked, reclaimed) = forge::reconcile_workspace(gh_bin, root, is_startup);
        stats.total_checked += checked;
        stats.total_reclaimed += reclaimed;
        let (pr_checked, pr_reclaimed) = forge::reconcile_pr_claims(gh_bin, root);
        stats.total_pr_checked += pr_checked;
        stats.total_pr_reclaimed += pr_reclaimed;
        stats
            .verdict_stats
            .merge(forge::reconcile_pr_verdicts(gh_bin, root));
        // #8922: AFTER the verdict pass, so a verdict it just re-queued to
        // `loom:review-requested` is checked for base conflicts on this tick.
        review_conflict::reconcile_review_conflicts(gh_bin, root);
        stats.roots_processed += 1;
    }
    stats
}

#[cfg(test)]
#[path = "pass_loop_tests.rs"]
mod tests;
