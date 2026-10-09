//! `loom-daemon release-stale-blocked` — the deterministic, no-LLM
//! `loom:blocked` release pass (#10556), by hand.
//!
//! The same pass the daemon tick runs when `LOOM_RELEASE_STALE_BLOCKED` is on
//! ([`loom_daemon::stale_blocked::release_gh::maybe_run`]), for one workspace,
//! without the cadence, shard and env gates. `--dry-run` prints the plan and
//! the counts with zero writes. Unlike `check-stale-blocked`, which stays
//! read-only, this WRITES: an audit comment, then a label or body edit.
//!
//! Exit 0 always: an unevaluated artifact, a refused write scope (#9548,
//! enforced per write) or a failed write is reported, never a failed exit.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::stale_blocked::budget::{self, Floor};
use loom_daemon::stale_blocked::release::{Config, Report};
use loom_daemon::stale_blocked::release_gh::{run_for_root, DEFAULT_MAX_WRITES};

#[derive(clap::Args)]
pub(crate) struct ReleaseStaleBlockedArgs {
    /// Plan and report; write nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit the report as one JSON object on stdout.
    #[arg(long)]
    pub json: bool,

    /// Repository, as `owner/name`. Defaults to whatever `gh` resolves from
    /// `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory to run `gh` from. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Artifacts released or re-parked at most.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_WRITES)]
    pub max_writes: usize,

    /// GraphQL points that must remain (#10480). `0` disables the check.
    #[arg(long, value_name = "N", default_value_t = budget::DEFAULT_MIN_GRAPHQL_REMAINING)]
    pub min_graphql_remaining: u64,

    /// Core (REST) requests that must remain (#10480). `0` disables the check.
    #[arg(long, value_name = "N", default_value_t = budget::DEFAULT_MIN_CORE_REMAINING)]
    pub min_core_remaining: u64,
}

impl ReleaseStaleBlockedArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = match &self.repo_root {
            Some(r) => r.clone(),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let report = run_for_root(
            &root,
            self.repo.as_deref(),
            &Config {
                dry_run: self.dry_run,
                max_writes: self.max_writes,
                floor: Floor {
                    graphql: self.min_graphql_remaining,
                    core: self.min_core_remaining,
                },
            },
        );
        if self.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_human(&report);
        }
        Ok(())
    }
}

fn print_human(r: &Report) {
    println!("[release-stale-blocked] {}", r.summary());
    let verb = if r.dry_run { "would " } else { "" };
    for a in &r.released {
        let restored = a
            .restored
            .as_deref()
            .map(|l| format!(", restore {l}"))
            .unwrap_or_default();
        println!("  {verb}release {} #{} (resolved {:?}{restored})", a.kind, a.number, a.resolved);
    }
    for a in &r.reparked {
        println!(
            "  {verb}re-park {} #{} (resolved {:?}, still open {:?})",
            a.kind, a.number, a.resolved, a.still_open
        );
    }
    for u in &r.unevaluated {
        eprintln!("  not evaluated #{}: {}", u.number, u.why);
    }
    for u in &r.failed {
        eprintln!("  write failed #{}: {}", u.number, u.why);
    }
    eprintln!("{}", r.cost.summary());
}
