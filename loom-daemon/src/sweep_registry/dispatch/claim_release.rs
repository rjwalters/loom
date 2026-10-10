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
    /// The release goes through `restore_label_to_ready`, whose #9463
    /// carve-out removes `loom:building` but never re-adds `loom:issue` to a
    /// closed issue. The caller must not have spawned a child yet.
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
        if let Err(e) = self.restore_label_to_ready(issue_number) {
            log::warn!(
                "sweep_registry: post-claim release of closed issue #{issue_number} failed: {e}"
            );
        }
        self.note_label_flip(issue_number); // #4485 flap detection
        self.unwind_local_claim(issue_number, sweep_id, idempotency_key);
        Err(anyhow!(
            "refusing to dispatch issue #{issue_number}: it was found closed on the forge \
             right after the claim flip (#11304 post-claim re-verification); the \
             `loom:building` claim was released and no builder was spawned."
        ))
    }
}
