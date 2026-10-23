//! A refused Builder claim's rollback (#12822).
//!
//! `pre_claim_builder_step` writes before it can know it has won: the label
//! flip, then its lease comment, whose forge order is the cross-host mutex
//! (#12133). A refusal after those writes (`claim_lost`, `claim_unverified`)
//! used to leave the lease behind. It sat live on the issue with no Builder
//! behind it, so `forge check-claim` and a peer that read it stood down: both
//! yielded, nobody ran (loom-ui#5377, #5408).
//!
//! The rollback undoes only what this step wrote, and never a peer's:
//!
//! - **Lease**: a `loom:lease-yield` for this step's own `host`/`sweep` pair.
//!   A yield rather than a delete keeps the audit trail, and `check-claim`
//!   already excludes a lease its owner has yielded.
//! - **Label**: restored `loom:building` -> `loom:issue` only when a fresh read
//!   shows no peer lease live. Behind a peer's lease (`claim_lost`) the label
//!   is the winner's claim and stays. If the order cannot be read, the label
//!   stays too: fail closed, as the claim itself does.
//!
//! The outcome token lands on the `loom.dispatch.step` span as
//! `loom.dispatch.claim_rollback`.
//!
//! The other refusals that reach `unwind_step_claim(.., claimed = false)` after
//! a Builder pre-claim write nothing of their own: `label_flip_failed` fails
//! before any write, and `roll_paused` / `claimed` / `no_token_pool` happen
//! before the pre-claim or (`roll_paused` in the unlocked path) before the
//! Builder's pre-claim runs. A spawn failure after a won claim already
//! restores the label through `unwind_step_claim(.., true)`.

use super::*;
use crate::gh_invocation::AccessIntent;

/// Why a Builder claim was refused after its lease write.
pub(super) enum Loss {
    /// A peer's earlier lease holds the issue.
    Peer { host: String, sweep: String },
    /// The lease could not be written, or the order could not be read.
    Unverified,
}

/// The rollback outcome token for what was done. Pure.
pub(super) fn outcome(yielded: Option<bool>, restored: Option<bool>) -> &'static str {
    match (yielded, restored) {
        (Some(false), _) | (_, Some(false)) => "failed",
        (Some(true), Some(true)) => "both",
        (Some(true), None) => "lease_yielded",
        (None, Some(true)) => "label_restored",
        (None, None) => "none",
    }
}

impl SweepRegistry {
    /// Undo this step's pre-claim writes, then return `refusal` carrying the
    /// rollback's outcome. Best effort: a rollback that fails still returns the
    /// original refusal, and the outcome says `failed`.
    pub(super) fn roll_back_builder_claim(
        &mut self,
        item: u32,
        sweep_id: &str,
        wrote_lease: bool,
        loss: &Loss,
        episode_start: DateTime<Utc>,
        mut refusal: StepRefusal,
    ) -> StepRefusal {
        let yielded = wrote_lease.then(|| self.post_own_lease_yield(item, sweep_id, loss));
        let restored = match loss {
            Loss::Peer { .. } => None,
            Loss::Unverified => self
                .no_peer_lease_live(item, sweep_id, episode_start)
                .then(|| {
                    let ok = self.restore_label_to_ready(item).is_ok();
                    self.note_label_flip(item);
                    ok
                }),
        };
        let token = outcome(yielded, restored);
        log::info!(
            "sweep_registry: Builder claim on #{item} refused ({}): rollback {token} (#12822)",
            refusal.reason
        );
        refusal.claim = Some(Box::new(lease::ClaimFacts {
            outcome: "lease_order",
            exit_code: None,
            claim_ms: u64::try_from((Utc::now() - episode_start).num_milliseconds().max(0))
                .unwrap_or(0),
            stderr_tail: None,
            rollback: Some(token),
        }));
        refusal
    }

    /// Whether a fresh read shows no lease but this step's own live on the
    /// issue. `false` when the read fails: unknown is not "none".
    fn no_peer_lease_live(&self, item: u32, sweep_id: &str, episode_start: DateTime<Utc>) -> bool {
        let Some(comments) = self.read_lease_comments(item) else {
            return false;
        };
        let host = self.published_host_id();
        let live = lease_episode::in_episode(
            self,
            &comments,
            lease_episode::created_cutoff(episode_start),
            lease_episode::renewal_cutoff(episode_start),
        );
        live.iter()
            .all(|c| c.host == host && c.sweep_id == sweep_id)
    }

    /// Post a `loom:lease-yield` for this step's own lease. `true` when the
    /// forge took it. The peer's lease is never touched.
    fn post_own_lease_yield(&self, item: u32, sweep_id: &str, loss: &Loss) -> bool {
        let host = self.published_host_id();
        let (earliest_host, earliest_sweep, why) = match loss {
            Loss::Peer { host, sweep } => (
                host.as_str(),
                sweep.as_str(),
                format!(
                    "a lease from sweep `{sweep}` on host `{host}` is earlier in the forge's \
                     comment order. The `loom:building` label is left alone: it is that \
                     claimant's."
                ),
            ),
            Loss::Unverified => (
                "unverified",
                "unverified",
                "its claim could not be verified, so no Builder was launched.".to_string(),
            ),
        };
        let body = format!(
            "<!-- loom:lease-yield host={host} sweep={sweep_id} earliest_host={earliest_host} \
             earliest_sweep={earliest_sweep} -->\n\
             This Builder step's lease (sweep `{sweep_id}` on host `{host}`) is released and no \
             Builder runs for it: {why} (#12822)",
        );
        let gh = self.resolved_gh();
        let comment = self
            .gh_inv("guard.step_lease_yield", AccessIntent::Write, &gh)
            .arg("issue")
            .arg("comment")
            .arg(item.to_string())
            .args(crate::claim_reconciliation::gh_call::loom_repo_flag())
            .arg("--body")
            .arg(self.dashboard_comment_body(item, &body));
        match self.run_counted(comment) {
            Ok(Some(output)) if output.status.success() => true,
            Ok(Some(output)) => {
                log::warn!(
                    "step lease-yield for #{item} exited {:?}: {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                false
            }
            Ok(None) => {
                log::warn!("step lease-yield for #{item} timed out, killed");
                false
            }
            Err(e) => {
                log::warn!("step lease-yield for #{item} failed: {e}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::outcome;

    #[test]
    fn the_outcome_names_what_was_undone() {
        assert_eq!(outcome(None, None), "none");
        assert_eq!(outcome(Some(true), None), "lease_yielded");
        assert_eq!(outcome(None, Some(true)), "label_restored");
        assert_eq!(outcome(Some(true), Some(true)), "both");
        assert_eq!(outcome(Some(false), None), "failed");
        assert_eq!(outcome(Some(true), Some(false)), "failed");
    }
}
