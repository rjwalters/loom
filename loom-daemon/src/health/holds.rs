//! Health sections for the conditions where the daemon has **stopped doing
//! something and will not resume on its own judgement alone**:
//!
//! | section key | condition |
//! |-------------|-----------|
//! | `worktree_reaper` | worktree removals backed off after a permission-class failure or a retry cap (#7590) |
//! | `pool_hold` | token pools holding ALL sweep dispatch because not one account in them can spawn (#7708) |
//!
//! Both read a plain projection already on the status wire
//! ([`crate::types::StuckWorktreeReclaim`],
//! [`crate::types::PoolExhaustionHoldStatus`]) — this module invents no new
//! probe, consistent with the collector rule in the parent module's doc.
//!
//! Split into its own file (#7990) because `health.rs` sits at its
//! `.loom/docs/file-size-policy.md` ratchet: new assessment logic goes in a
//! sibling module and the parent keeps only the dispatch line.

use super::{
    format_age, no_status_reason, repo_label, unknown_section, HealthInputs, HealthSection, Verdict,
};

// ============================================================================
// Stuck worktree-removal backoff section (Issue #7590)
// ============================================================================

/// Assess [`crate::types::DaemonStatusReport::stuck_worktree_reclaims`]:
/// worktree removals the periodic reaper has backed off after a
/// permission-class failure (or a fixed retry-count cap) — see
/// [`crate::worktree_reaper::stuck_worktree_removals`]'s doc comment for the
/// exact classification. Before #7590 a worktree in this state retried the
/// exact same removal, and failed the exact same way, every single reaper
/// tick forever, with nothing on this surface to notice it — this section
/// closes that gap.
///
/// **Unconditional** (mirrors `stale_sweeps`/`liveness`/`dispatch`), not the
/// anomaly-only `Option`-returning pattern
/// [`super::assess_observability`] uses: a stuck removal is otherwise
/// invisible on every existing surface, so a section that only appears once
/// an operator already suspects trouble would defeat the point.
#[must_use]
pub fn assess_worktree_reaper(inputs: &HealthInputs) -> HealthSection {
    let Some(status) = &inputs.status else {
        return unknown_section("worktree_reaper", &no_status_reason(inputs));
    };

    if status.stuck_worktree_reclaims.is_empty() {
        return HealthSection::new(
            "worktree_reaper",
            Verdict::Green,
            "no worktree removals backed off",
            serde_json::json!({ "count": 0 }),
        );
    }

    let summary = status
        .stuck_worktree_reclaims
        .iter()
        .map(|r| {
            format!(
                "{}-{} @ {} ({} attempt(s), first failed {}): {}",
                r.kind,
                r.number,
                repo_label(&r.repo_root),
                r.attempt_count,
                format_age((inputs.at - r.first_failure_at).num_seconds()),
                r.cause
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    HealthSection::new(
        "worktree_reaper",
        Verdict::Degraded,
        format!(
            "{} worktree removal(s) backed off after repeated/permission-class failures \
             (#7590 — will not self-resolve without operator intervention): {summary}",
            status.stuck_worktree_reclaims.len()
        ),
        serde_json::json!({
            "count": status.stuck_worktree_reclaims.len(),
            "stuck": status.stuck_worktree_reclaims,
        }),
    )
}

// ============================================================================
// Token-pool exhaustion hold section (Issue #7708, surfaced by #7990)
// ============================================================================

/// Assess [`crate::types::DaemonStatusReport::pool_exhaustion_holds`]: token
/// pools this host is holding **all** sweep dispatch for because not one
/// account in them is spawnable (#7708).
///
/// # Why this is its own bucket, not part of `tokens`
///
/// A pool hold is a single *host-level* fact — "nothing can spawn here" —
/// whereas the `tokens` section counts *per-account* health. Folding the
/// hold into it would inflate that section's degraded/failure tallies with
/// one fact that is not an account fault, blurring "6 accounts are cooling
/// down" into "6 account failures". That is the same separation
/// [`super::RoleTickSummary::pool_exhausted`] keeps for #7607's role-tick
/// skips, for the same reason, so this section mirrors it rather than
/// inventing a second convention. `assess_tokens` never reads this field.
///
/// # Verdict
///
/// [`Verdict::Degraded`] while any pool is held: dispatch is stopped
/// host-wide, which is exactly the state an operator running `health` because
/// "nothing is moving" needs named. It is *not* fatal — a pre-flight hold
/// clears on the first tick that finds one spawnable account, with no restart
/// — so the summary states the estimate rather than demanding intervention.
#[must_use]
pub fn assess_pool_hold(inputs: &HealthInputs) -> HealthSection {
    let Some(status) = &inputs.status else {
        return unknown_section("pool_hold", &no_status_reason(inputs));
    };

    if status.pool_exhaustion_holds.is_empty() {
        return HealthSection::new(
            "pool_hold",
            Verdict::Green,
            "no token pool held",
            serde_json::json!({ "count": 0 }),
        );
    }

    let summary = status
        .pool_exhaustion_holds
        .iter()
        .map(|h| {
            format!(
                "{} (0/{} spawnable, held {}, est. clear {}{})",
                h.dir.display(),
                h.total,
                format_age((inputs.at - h.since).num_seconds()),
                h.next_clear_at.to_rfc3339(),
                if h.wrapper_observed {
                    "; armed by a real token-selection death"
                } else {
                    ""
                }
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    HealthSection::new(
        "pool_hold",
        Verdict::Degraded,
        format!(
            "{} token pool(s) EXHAUSTED — all sweep dispatch held for every workspace \
             resolving to them (#7708); run `loom-daemon tokens check --ranking`: {summary}",
            status.pool_exhaustion_holds.len()
        ),
        serde_json::json!({
            "count": status.pool_exhaustion_holds.len(),
            "holds": status.pool_exhaustion_holds,
        }),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{assess_pool_hold, HealthInputs, Verdict};
    use crate::health::{assess, assess_tokens};
    use crate::types::{CapacityReport, DaemonStatusReport, PoolExhaustionHoldStatus};
    use chrono::{Duration, TimeZone, Utc};
    use std::path::PathBuf;

    fn at() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 17, 12, 0, 0).unwrap()
    }

    fn hold(dir: &str, total: usize) -> PoolExhaustionHoldStatus {
        PoolExhaustionHoldStatus {
            dir: PathBuf::from(dir),
            total,
            since: at() - Duration::minutes(42),
            next_clear_at: at() + Duration::minutes(5),
            wrapper_observed: false,
        }
    }

    /// A daemon whose token pool is healthy per-account (6/8 usable, fresh
    /// `.ranking`) and which is nonetheless holding `holds`. That combination
    /// is the point: it is what makes `a_held_pool_does_not_inflate_the_tokens_section`
    /// a real assertion rather than a tautology on an already-degraded pool.
    fn inputs_with(holds: Vec<PoolExhaustionHoldStatus>) -> HealthInputs {
        HealthInputs {
            at: at(),
            ranking_present: true,
            ranking_age_secs: Some(30),
            status: Some(DaemonStatusReport {
                capacity: CapacityReport {
                    ranking_present: true,
                    total_accounts: 8,
                    healthy_accounts: 6,
                    exhausted_accounts: 2,
                    token_axis_limit: 6,
                    token_bound: false,
                },
                pool_exhaustion_holds: holds,
                ..DaemonStatusReport::default()
            }),
            ..HealthInputs::default()
        }
    }

    #[test]
    fn green_with_no_hold() {
        let section = assess_pool_hold(&inputs_with(vec![]));
        assert_eq!(section.key, "pool_hold");
        assert_eq!(section.verdict, Verdict::Green);
        assert_eq!(section.detail["count"], 0);
    }

    #[test]
    fn unknown_without_a_status_report() {
        let section = assess_pool_hold(&HealthInputs {
            at: at(),
            ipc_error: Some("connection refused".to_string()),
            ..HealthInputs::default()
        });
        assert_eq!(section.verdict, Verdict::Unknown);
    }

    /// AC1: every active hold is exposed with `dir`, `total`, `since` and
    /// `next_clear_at` — the four fields an operator needs to answer "which
    /// pool, how dead, how long, until when".
    #[test]
    fn json_detail_carries_dir_total_since_and_next_clear_at() {
        let section = assess_pool_hold(&inputs_with(vec![hold("/home/u/.loom/tokens", 6)]));
        assert_eq!(section.verdict, Verdict::Degraded);
        assert_eq!(section.detail["count"], 1);
        let entry = &section.detail["holds"][0];
        assert_eq!(entry["dir"], "/home/u/.loom/tokens");
        assert_eq!(entry["total"], 6);
        // Serde renders a `DateTime<Utc>` with the `Z` offset form, so compare
        // against the serialized value rather than a hand-built `to_rfc3339`.
        assert_eq!(entry["since"], serde_json::json!(at() - Duration::minutes(42)));
        assert_eq!(entry["next_clear_at"], serde_json::json!(at() + Duration::minutes(5)));
        assert_eq!(entry["wrapper_observed"], false);
    }

    /// Holds arrive sorted by pool directory (`active_holds`) and are rendered
    /// in that order, one entry per held pool.
    #[test]
    fn every_held_pool_is_listed_in_order() {
        let section = assess_pool_hold(&inputs_with(vec![hold("/pool/a", 2), hold("/pool/b", 3)]));
        assert_eq!(section.detail["count"], 2);
        assert_eq!(section.detail["holds"][0]["dir"], "/pool/a");
        assert_eq!(section.detail["holds"][1]["dir"], "/pool/b");
        assert!(section.summary.contains("/pool/a"));
        assert!(section.summary.contains("/pool/b"));
    }

    #[test]
    fn a_wrapper_observed_hold_says_so() {
        let mut h = hold("/pool/a", 4);
        h.wrapper_observed = true;
        let section = assess_pool_hold(&inputs_with(vec![h]));
        assert!(section.summary.contains("token-selection death"));
        assert_eq!(section.detail["holds"][0]["wrapper_observed"], true);
    }

    /// AC2: the hold lives in its OWN bucket. A held pool must not touch the
    /// `tokens` section's verdict or its degraded/failure counts — those
    /// describe per-account health, not a host-level dispatch hold.
    #[test]
    fn a_held_pool_does_not_inflate_the_tokens_section() {
        let healthy = inputs_with(vec![]);
        let held = inputs_with(vec![hold("/pool/a", 6)]);
        let tokens = assess_tokens(&held);
        assert_eq!(tokens, assess_tokens(&healthy));
        assert_eq!(tokens.verdict, Verdict::Green);
        assert!(!tokens.summary.contains("/pool/a"));
    }

    /// The bucket is a section of its own in the full report, so
    /// `health --json` carries it under its own key.
    #[test]
    fn assess_emits_the_bucket_as_its_own_section() {
        let report = assess(&inputs_with(vec![hold("/pool/a", 6)]));
        let section = report
            .sections
            .iter()
            .find(|s| s.key == "pool_hold")
            .expect("pool_hold section present");
        assert_eq!(section.verdict, Verdict::Degraded);
        assert_eq!(section.detail["holds"][0]["total"], 6);
        assert_eq!(
            report
                .sections
                .iter()
                .filter(|s| s.key == "pool_hold")
                .count(),
            1
        );
    }
}
