//! `loom-daemon check-renovate-labels` — the Renovate-side
//! `loom:review-requested` routing contract (Issue #9418), the counterpart of
//! `defaults/scripts/check-dependabot-labels.sh` (#7577).
//!
//! A subcommand rather than more lines in that script because the script is
//! `contract`-category shell, which epic #7810's `shell-budget` gate ratchets
//! down, never up (`.loom/docs/shell-language-policy.md`). The logic lives in
//! [`loom_daemon::renovate_labels`].
//!
//! Runs in CI's `Installer Integration Tests` job, next to the Dependabot
//! check, which already downloads the shared debug binary.
//!
//! Exit codes: `0` = ok (or no Renovate config); `1` = at least one violation.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::renovate_labels;

#[derive(clap::Args)]
pub(crate) struct CheckRenovateLabelsArgs {
    /// Repository root to check. Defaults to the current directory — CI runs
    /// it from the checkout root.
    #[arg(long, value_name = "PATH")]
    repo_root: Option<PathBuf>,
}

impl CheckRenovateLabelsArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        let report = renovate_labels::check(&root);
        if report.violations.is_empty() {
            println!("{}", renovate_labels::ok_message(&report.checked));
            return Ok(());
        }
        for v in &report.violations {
            eprintln!("{}", v.render());
        }
        eprintln!("{}", renovate_labels::failure_trailer(report.violations.len()));
        std::process::exit(1);
    }
}
