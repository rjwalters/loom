//! The test-only seam that records a bare, process-less registry entry
//! (Issue #11123), split out of `sweep_registry/mod.rs` because that file is
//! over the file-size ratchet's threshold (`.loom/docs/file-size-policy.md`).
//!
//! Only compiled under `#[cfg(test)]` — `test_support`'s own `mod`
//! declaration in `sweep_registry/mod.rs` carries the gate.

use super::super::SweepRegistry;
use crate::types::{SweepInfo, SweepKind, SweepState};
use chrono::Utc;
use std::path::PathBuf;

impl SweepRegistry {
    /// Test seam: record a bare entry of `kind` in `state`, with no process.
    pub(crate) fn seed_entry_for_test(&mut self, kind: SweepKind, state: SweepState) {
        let sweep_id = format!("seed-{}", self.entries.len());
        self.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                pgid: None,
                sweep_id,
                kind,
                pid: 2_147_483_640,
                token_name: String::new(),
                runtime: "claude".into(),
                runtime_source: None,
                log_path: PathBuf::new(),
                idempotency_key: None,
                started_at: Utc::now(),
                state,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
                overflow: false,
            },
        );
    }
}
