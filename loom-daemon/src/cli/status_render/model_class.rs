//! `loom-daemon status` per-model-class token-capacity numbers (#8058 Phase 3).
//!
//! Phase 1 (#8090) and Phase 2 (#8241) made a per-model-class credit
//! exhaustion stop starving an account for every *other* class. What they did
//! not change is what this command prints: one account-wide healthy count,
//! which by construction reads a class-scoped hold as a whole-account outage.
//! `2/20 accounts healthy` on a pool where eighteen accounts can still serve
//! Sonnet is indistinguishable here from a genuinely dead pool.
//!
//! This module supplies the breakdown for both surfaces — the human capacity
//! line and `--json`'s `capacity.healthy_accounts_by_class` — from the one
//! shared computation in [`loom_daemon::capacity::model_class`], which
//! `loom-daemon health`'s `tokens` section also uses. The two commands
//! therefore cannot disagree about a pool.
//!
//! # Scoped to the daemon's pool, and silent without one
//!
//! The pool directory is the one the DAEMON resolved
//! ([`DaemonStatusReport::token_pool_dir`], #4292), never one re-derived from
//! this process's cwd — the same rule the healthy/exhausted counts printed
//! beside it already follow. A pre-#4292 daemon (no field), an unreadable
//! `.ranking`, or a pool with no class-scoped `.bad_tokens` state all produce
//! **nothing**: an empty suffix and an empty JSON object, so the rendered
//! output is byte-identical to its pre-#8058 form. Per-class numbers appear
//! only once there is per-class state to report.
//!
//! Split into its own file (following `holds`) rather than added to
//! `status_render.rs`: that file sits at its `.loom/docs/file-size-policy.md`
//! ratchet, so new rendering logic goes in a sibling module and the parent
//! keeps only the call.

use loom_daemon::capacity::model_class::{self, ClassCapacity};
use loom_daemon::types::DaemonStatusReport;

/// Read the per-class snapshot for the pool this report names, or `None` when
/// there is no pool directory on the wire or no readable `.ranking` in it.
fn snapshot(report: &DaemonStatusReport) -> Option<ClassCapacity> {
    let dir = report.token_pool_dir.as_ref()?;
    model_class::read_class_capacity_at(dir)
}

/// The ` (per class: opus 2/20, sonnet 20/20)` fragment to splice into the
/// human `N/M accounts healthy` line, or `""` when there is no class-scoped
/// state to report.
pub(crate) fn capacity_suffix(report: &DaemonStatusReport) -> String {
    model_class::summary_suffix_of(snapshot(report).as_ref())
}

/// The `class -> healthy` object for `--json`'s
/// `capacity.healthy_accounts_by_class`, `{}` when there is no class-scoped
/// state.
pub(crate) fn capacity_detail(report: &DaemonStatusReport) -> serde_json::Value {
    model_class::detail_of(snapshot(report).as_ref())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{capacity_detail, capacity_suffix};
    use crate::cli::status::sample_report::sample_report;
    use loom_daemon::types::DaemonStatusReport;
    use std::path::Path;

    /// A report pointing at `dir` as the daemon-resolved token pool.
    fn report_for(dir: &Path) -> DaemonStatusReport {
        let mut report = sample_report();
        report.token_pool_dir = Some(dir.to_path_buf());
        report
    }

    fn write_pool(dir: &Path, ranking: &str, bad_tokens: &str) {
        std::fs::write(dir.join(".ranking"), ranking).unwrap();
        if !bad_tokens.is_empty() {
            std::fs::write(dir.join(".bad_tokens"), bad_tokens).unwrap();
        }
    }

    fn fresh_mark(name: &str, reason: &str) -> String {
        format!("{} {name} {reason}\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"))
    }

    /// AC2's degradation clause on the `status` side: a pool with no
    /// class-scoped mark contributes nothing, so the capacity line renders
    /// exactly as it did before #8058.
    #[test]
    fn a_pool_without_class_marks_renders_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_pool(dir.path(), "a|available\nb|available\n", "");
        let report = report_for(dir.path());
        assert_eq!(capacity_suffix(&report), "");
        assert_eq!(capacity_detail(&report), serde_json::json!({}));
    }

    /// The observability gap this phase closes: the account-wide count is
    /// 1/4, but only Opus is actually down.
    #[test]
    fn a_class_scoped_mark_renders_the_breakdown() {
        let dir = tempfile::tempdir().unwrap();
        let mut marks = String::new();
        for name in ["a", "b", "c"] {
            marks.push_str(&fresh_mark(name, "exhausted: credits [model-class:opus]"));
        }
        write_pool(dir.path(), "a|available\nb|available\nc|available\nd|available\n", &marks);
        let report = report_for(dir.path());
        assert_eq!(
            capacity_suffix(&report),
            " (per class: fable 4/4, haiku 4/4, opus 1/4, sonnet 4/4)"
        );
        assert_eq!(
            capacity_detail(&report),
            serde_json::json!({"fable": 4, "haiku": 4, "opus": 1, "sonnet": 4})
        );
    }

    /// A pre-#4292 daemon sends no pool directory. Rendering must stay silent
    /// rather than re-resolve a pool from this process's cwd, which could
    /// describe a different one than the counts beside it.
    #[test]
    fn no_pool_dir_on_the_wire_renders_nothing() {
        let mut report = sample_report();
        report.token_pool_dir = None;
        assert_eq!(capacity_suffix(&report), "");
        assert_eq!(capacity_detail(&report), serde_json::json!({}));
    }

    /// A pool directory that exists but has no `.ranking` yet (never probed)
    /// is the same silent case, not a crash or a fabricated zero.
    #[test]
    fn a_pool_without_a_ranking_renders_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let report = report_for(dir.path());
        assert_eq!(capacity_suffix(&report), "");
        assert_eq!(capacity_detail(&report), serde_json::json!({}));
    }
}
