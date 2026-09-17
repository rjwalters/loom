//! `loom-daemon shell-budget` — how far epic #7810 actually is.
//!
//! A gate that only prevents growth gives no answer to "are we getting
//! anywhere". This prints the number the epic is driving down, what it started
//! at, and which way it has moved — so progress is something anyone can look
//! at rather than something inferred from merged PR titles.

use anyhow::{Context, Result};
use loom_daemon::shell_budget;

#[derive(clap::Args)]
pub(crate) struct ShellBudgetArgs {
    /// Emit the measurement as JSON for scripting.
    #[arg(long)]
    pub json: bool,

    /// Repo root to measure. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub root: Option<std::path::PathBuf>,
}

impl ShellBudgetArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = match self.root {
            Some(r) => r,
            None => std::env::current_dir().context("could not resolve the current directory")?,
        };
        let budget = shell_budget::measure(&root).map_err(anyhow::Error::msg)?;
        let origin = shell_budget::read_origin_portable(&root).unwrap_or(0);

        if self.json {
            let by_cat: serde_json::Map<String, serde_json::Value> = budget
                .by_category
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "portable": budget.portable(),
                    "floor": budget.floor(),
                    "stubbed": budget.stubbed(),
                    "total": budget.total(),
                    "files": budget.file_count(),
                    "origin_portable": origin,
                    "net_vs_epic_start": i128::from(budget.portable()) - i128::from(origin),
                    "by_category": by_cat,
                    "unlisted": budget.unlisted.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                })
            );
        } else {
            print!("{}", shell_budget::render_report(&budget, origin));
        }
        Ok(())
    }
}
