//! `loom-daemon eta retire` (#10525): retirement **proposals** from the
//! nightly folds. Prints them; with `--file`, files each new one as an issue
//! through `.loom/scripts/create-issue.sh`. Nothing is ever unregistered:
//! retiring a heuristic stays a code change (#10484).
//!
//! The rule and its evidence are [`loom_daemon::eta::shadow_lifecycle`]; this
//! file parses arguments and prints; reading the folds and filing (the same
//! path the nightly fold task uses) is
//! [`loom_daemon::eta::retire_filing`].

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::eta::retire_filing::{self, Proposed};

#[derive(clap::Args)]
pub(crate) struct EtaRetireArgs {
    /// The Loom workspace whose folds and config to read. Defaults to the
    /// current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// File each proposal not already filed (on this host or on the forge)
    /// as an issue. Only the host that folds files (the explicit
    /// `fleet.etaAuthority`, else `fleet.captain`); any other host refuses. Without it this only prints.
    #[arg(long)]
    pub file: bool,

    /// Print the proposals as JSON.
    #[arg(long)]
    pub json: bool,
}

impl EtaRetireArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = super::eta_fleet_cmd::resolve_root(self.repo_root.clone());
        let Proposed {
            current,
            days,
            proposals,
        } = retire_filing::proposals_for_root(&root).map_err(anyhow::Error::msg)?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&proposals)?);
        } else {
            println!(
                "eta retire: {} proposal(s) from {} fold day(s) against {current}",
                proposals.len(),
                days
            );
            for p in &proposals {
                println!(
                    "  {} (dominated by {}): paired pinball {:+.1}s, 95% CI {:+.1}s..{:+.1}s \
                     over {} day(s); evidence {}",
                    p.heuristic,
                    p.dominated_by,
                    p.mean_delta_pinball4_sec,
                    p.delta_ci95.0,
                    p.delta_ci95.1,
                    p.decided_days,
                    p.evidence_id
                );
            }
        }
        if !self.file || proposals.is_empty() {
            return Ok(());
        }
        // Only the folds' owner files (#8848, #10918); the gate is checked first.
        let report = retire_filing::file_for_root(
            &root,
            &loom_daemon::sweep_registry::host_identity(),
            &proposals,
        )
        .map_err(anyhow::Error::msg)?;
        eprintln!("eta retire: filed {:?}; already filed {:?}", report.filed, report.already);
        Ok(())
    }
}
