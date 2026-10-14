//! Claim-unwind helpers for the dispatch path, extracted from `dispatch.rs` to
//! hold that file at its file-size ratchet (Issue #11304).
//!
//! - [`SweepRegistry::unwind_local_claim`]: undo this dispatch attempt's own
//!   purely local side effects (peer-claim advertisement, claim lock,
//!   in-flight idempotency key) — shared by every abandon branch whose unwind
//!   is exactly those three steps in that order.
//! - [`SweepRegistry::release_claim_if_closed_after_flip`]: step 4e, the
//!   post-claim closed-issue re-verification.

use super::*;

impl SweepRegistry {
    /// Retract this host's peer-claim advertisement (3a), release the claim
    /// lock (3) it owns, and — #9572 — release the in-flight idempotency key
    /// claimed at step 3.05 so a later same-key dispatch is not wedged behind
    /// a sweep that never spawned.
    pub(super) fn unwind_local_claim(
        &mut self,
        issue_number: u32,
        sweep_id: &str,
        idempotency_key: Option<&String>,
    ) {
        self.publish_peer_claim(peer_claims::ClaimKind::Retract, issue_number);
        let _ = self.release_lock_owned(issue_number, sweep_id);
        if let Some(key) = idempotency_key {
            self.inflight_idempotency.remove(key);
        }
    }

    /// Remove `loom:building` and nothing else — no probes, never `--add-label`.
    fn remove_building_claim(&self, issue_number: u32) -> Result<()> {
        let mut args = vec![
            "issue".to_string(),
            "edit".to_string(),
            issue_number.to_string(),
            "--remove-label".to_string(),
            "loom:building".to_string(),
        ];
        args.extend(crate::claim_reconciliation::gh_call::loom_repo_flag());
        match self.gh_write("restore.label", &args)? {
            None => anyhow::bail!("removing `loom:building` from #{issue_number} timed out"),
            Some(out) if !out.status.success() => anyhow::bail!(
                "removing `loom:building` from #{issue_number} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Some(_) => Ok(()),
        }
    }

    /// 4e. Post-claim closed-issue re-verification (Issue #11304).
    ///
    /// The 2.5 guard is deliberately fail-open — a forge lookup error, breaker
    /// or timeout returns `None` and dispatch proceeds — and `gh issue edit`
    /// succeeds on a closed issue, so a closed issue can still get claimed.
    /// Re-probe through the 2.5 reader now that the flip has landed (the
    /// own-write pin makes that read unconditional, so it is never served
    /// stale from a `304`). Only a positive CLOSED/PR answer releases the
    /// claim; a `None` (probe error) stays fail-open and returns `Ok(())`.
    ///
    /// The release is remove-only ([`Self::remove_building_claim`]): the
    /// positive CLOSED/PR answer is already in hand, so it must not go through
    /// `restore_label_to_ready`, whose independent state probes can fail and
    /// fall back to re-adding `loom:issue` to the known-closed issue. The
    /// caller must not have spawned a child yet.
    pub(super) fn release_claim_if_closed_after_flip(
        &mut self,
        issue_number: u32,
        sweep_id: &str,
        idempotency_key: Option<&String>,
    ) -> Result<()> {
        if self.guard_closed_or_pr(issue_number) != Some(true) {
            return Ok(());
        }
        log::warn!(
            "sweep_registry: issue #{issue_number} sweep_id={sweep_id} was found CLOSED (or a \
             PR) right after the claim flip (#11304) - the pre-flip guard failed open. \
             Releasing the claim instead of spawning a builder."
        );
        let restore = self.remove_building_claim(issue_number);
        if restore.is_ok() {
            self.note_label_flip(issue_number); // #4485 flap detection
        }
        self.unwind_local_claim(issue_number, sweep_id, idempotency_key);
        match restore {
            Ok(()) => Err(anyhow!(
                "refusing to dispatch issue #{issue_number}: it was found closed on the forge \
                 right after the claim flip (#11304 post-claim re-verification); the \
                 `loom:building` claim was released and no builder was spawned."
            )),
            Err(e) => Err(e.context(format!(
                "refusing to dispatch issue #{issue_number}: it was found closed on the forge \
                 right after the claim flip (#11304 post-claim re-verification), but removing \
                 the `loom:building` claim FAILED - the forge may still carry it; no builder \
                 was spawned and local claim state was unwound"
            ))),
        }
    }
}
