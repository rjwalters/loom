//! A fast-fail park that survives a restart (Issue #11930).
//!
//! The park was memory-only: the startup reconciliation (#4110) released every
//! marked `loom:blocked` on daemon start, so each worker restart in a roll
//! undid every park. It now keeps a park younger than its TTL on the forge and
//! hands it here, where the reaper rebuilds the in-memory quarantine from the
//! marker's timestamp ([`SweepRegistry::adopt_restored_parks`]); the ordinary
//! TTL expiry then releases it on schedule.
//!
//! Fail-open in the old direction: a park the startup pass could not date is
//! released as before. The fleet-wide streak half of #11930 rode the retired
//! peer-claim channel (#11112) and is not carried over.

use super::*;

impl SweepRegistry {
    /// Rebuild the in-memory quarantine for every park the startup
    /// reconciliation kept on the forge for this workspace (Issue #11930).
    /// Called at the top of each reaper tick, before the TTL expiry, so a park
    /// whose TTL has since run out is released by the ordinary path on the
    /// same tick. An issue already quarantined here keeps its own entry.
    /// Returns how many parks were adopted.
    pub fn adopt_restored_parks(&mut self) -> usize {
        let restored = quarantine_reconciliation::restored::take(&self.config.workspace_root);
        let mut adopted = 0;
        for (issue, quarantined_at) in restored {
            if self.quarantined.contains_key(&issue) {
                continue;
            }
            // Served as a first-generation park: the forge does not record
            // the generation, and the startup pass dated it against the base
            // TTL.
            self.quarantine_generations.entry(issue).or_insert(1);
            self.quarantined.insert(issue, quarantined_at);
            adopted += 1;
            log::info!(
                "sweep_registry: issue #{issue} park restored from the forge (marked {}), held \
                 until its TTL instead of released on restart (#11930)",
                quarantined_at.to_rfc3339()
            );
        }
        adopted
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "fast_fail_fleet_tests.rs"]
mod tests;
