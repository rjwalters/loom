//! Journal-path resolvers for [`SweepRegistryConfig`], split out of `mod.rs`
//! (file-size ratchet, #9999).

use super::{sweep_journal, sweep_outcomes, SweepRegistryConfig};
use anyhow::Result;
use std::path::PathBuf;

impl SweepRegistryConfig {
    /// Resolve the sweep journal path: `journal_path` explicit override, else
    /// [`sweep_journal::default_journal_path`].
    pub fn resolve_journal_path(&self) -> Result<PathBuf> {
        if let Some(ref p) = self.journal_path {
            return Ok(p.clone());
        }
        sweep_journal::default_journal_path()
    }

    /// Resolve the durable terminal-outcomes journal path (Issue #4644):
    /// `outcomes_journal_path` explicit override, else
    /// [`sweep_outcomes::default_outcomes_path`].
    #[must_use]
    pub fn resolve_outcomes_journal_path(&self) -> PathBuf {
        self.outcomes_journal_path
            .clone()
            .unwrap_or_else(|| sweep_outcomes::default_outcomes_path(&self.workspace_root))
    }

    /// Resolve the `sweep.outcome` telemetry journal path (Issue #4704):
    /// `outcome_telemetry_path` explicit override, else
    /// [`sweep_outcomes::default_outcome_telemetry_path`].
    #[must_use]
    pub fn resolve_outcome_telemetry_path(&self) -> PathBuf {
        self.outcome_telemetry_path
            .clone()
            .unwrap_or_else(|| sweep_outcomes::default_outcome_telemetry_path(&self.workspace_root))
    }
}
