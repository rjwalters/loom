//! Stable log paths shared by dispatch entry points.
use super::SweepRegistry;
use std::path::PathBuf;

impl SweepRegistry {
    /// Delegates to [`crate::launch_record::sweep_log_path`] (Issue #8507) so
    /// this registry method and safehouse's registry-free completion path can
    /// never disagree about where a sweep's log lives.
    pub(crate) fn compute_log_path(&self, issue: u32) -> PathBuf {
        crate::launch_record::sweep_log_path(&self.config.workspace_root, issue)
    }

    /// The `PrSet` counterpart of [`Self::compute_log_path`] (Issue #5342):
    /// one log file per PR set, named after every member so an operator can
    /// tell two overlapping-but-distinct sets apart at a glance.
    pub(crate) fn compute_prset_log_path(&self, prs: &[u32]) -> PathBuf {
        let joined = prs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("-");
        self.config
            .logs_dir()
            .join(format!("sweep-prs-{joined}.log"))
    }
}
