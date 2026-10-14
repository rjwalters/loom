//! `loom-daemon check-dependabot-labels` — the Dependabot-side
//! `loom:review-requested` routing contract (#7577), behind
//! `defaults/scripts/check-dependabot-labels.sh`. The logic lives in
//! [`loom_daemon::dependabot_labels`]; the script is a thin `contract` wrapper
//! (epic #7810's `shell-budget` ratchet).
//!
//! Exit codes: `0` = ok (or no `.github/dependabot.yml`); `1` = a problem.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::dependabot_labels::{self, CONFIG, REQUIRED_LABEL};

#[derive(clap::Args)]
pub(crate) struct CheckDependabotLabelsArgs {
    /// Repository root to check. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    repo_root: Option<PathBuf>,
}

impl CheckDependabotLabelsArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        let report = dependabot_labels::check(&root);
        if !report.config_present {
            println!(
                "check-dependabot-labels: no {CONFIG} under {} — nothing to check (ok).",
                root.display()
            );
            return Ok(());
        }
        for n in &report.notes {
            println!("{n}");
        }
        if report.errors.is_empty() {
            println!(
                "check-dependabot-labels: all dependabot.yml entries carry '{REQUIRED_LABEL}' and every lockfile-bearing manifest is covered (ok)."
            );
            return Ok(());
        }
        for e in &report.errors {
            eprintln!("{e}");
        }
        eprintln!(
            "\ncheck-dependabot-labels: {} problem(s) found in {CONFIG}.",
            report.errors.len()
        );
        std::process::exit(1);
    }
}
