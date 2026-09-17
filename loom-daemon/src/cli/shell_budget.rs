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

    /// Enforce the ratchet: exit 1 when THIS change grows the portable pool.
    ///
    /// Compares against the merge-base with `--base` (default `origin/main`),
    /// so it measures what your change did and needs no committed number to
    /// stay in sync. A baseline file goes stale every time `main` moves; this
    /// cannot.
    ///
    /// The report prints either way — a gate that only speaks up on failure
    /// teaches nobody which way the number is moving, which is how the portable
    /// pool grew +317 across four merged ports without anyone noticing.
    #[arg(long)]
    pub check: bool,

    /// The ref `--check` compares against. Its merge-base with `HEAD` is used,
    /// so an un-rebased branch is measured on its own contribution rather than
    /// being blamed for everything that landed while it was open.
    #[arg(long, value_name = "REF", default_value = "origin/main")]
    pub base: String,
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

        if self.check {
            // The unlisted check does not depend on a comparison: a production
            // script with no allowlist entry makes every figure an undercount,
            // whichever revision you measure.
            if !budget.unlisted.is_empty() {
                eprintln!(
                    "\nshell-budget: {} production script(s) carry no allowlist entry, so every \
                     figure above is an undercount — add them to scripts/shell-allowlist.txt:\n{}",
                    budget.unlisted.len(),
                    budget
                        .unlisted
                        .iter()
                        .map(|p| format!("  {}", p.display()))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                std::process::exit(1);
            }

            let cmp = shell_budget::comparison(&root, &self.base).map_err(anyhow::Error::msg)?;
            let before =
                shell_budget::measure_at_rev(&root, &cmp.rev).map_err(anyhow::Error::msg)?;
            let desc = cmp.desc;

            if let Err(why) = shell_budget::check_against_rev(&budget, &before, &desc) {
                eprintln!("\nshell-budget: PORTABLE SHELL GREW\n\n{why}");
                std::process::exit(1);
            }
            if !self.json {
                let delta = i128::from(budget.portable()) - i128::from(before.portable());
                println!(
                    "\nshell-budget: this change moves portable shell by {delta:+} vs {desc}."
                );
            }
        }
        Ok(())
    }
}
