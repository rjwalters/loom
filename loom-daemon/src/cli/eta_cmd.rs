//! `loom-daemon eta backfill` / `loom-daemon eta backtest` (#9325, Phase 2 of
//! #9289's ETA-as-a-Loom-primitive effort).
//!
//! `backfill` seeds the ETA stage-sample journal
//! ([`loom_daemon::eta::journal`]) from `pr-latency`'s own forge-derived
//! history (Issue #8923), so a heuristic has a baseline before the ETA
//! tracker has observed anything itself. `backtest` replays a heuristic
//! against real outcomes leak-free: every quantile is recomputed with
//! [`loom_daemon::eta::history::StageSamples::select`]/`select_at` at the
//! replay instant, which by construction excludes the very record the
//! replay case was built from (see [`loom_daemon::eta::backtest`]'s module
//! doc). This is the first gate for promoting a heuristic (operator
//! decision 2 on #9289).
//!
//! Not a script port: brand-new CLI surface, placed under
//! [`super::script_ports::ScriptPortCommand`] purely so `main.rs`'s
//! file-size freeze costs it nothing (same reason as `shell-budget`,
//! `worktree-state`, `premise-check`, …).

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};

use loom_daemon::cmd_out::Query;
use loom_daemon::eta::backtest::{self, BacktestReport, Bucket, Comparison, Filter};
use loom_daemon::eta::history::StageSamples;
use loom_daemon::eta::journal::{self, entries_from_pr_history};
use loom_daemon::eta::{Provenance, Registry};
use loom_daemon::script_helpers::gh_query;
use loom_daemon::telemetry::TelemetryEnvelope;

use super::pr_latency_cmd::fetch_histories;

/// PRs examined per `eta backfill` run, absent `--limit` — generous, since
/// this is meant to seed a whole repo's baseline in one pass rather than
/// track a live queue (`pr-latency`'s own default is much smaller).
const DEFAULT_BACKFILL_LIMIT: u32 = 300;

#[derive(clap::Subcommand)]
pub(crate) enum EtaCommand {
    /// Populate `eta-stage-samples.jsonl` from `pr-latency`'s forge-derived
    /// history (Issue #8923): `review_wait`, `doctor` and `merge_wait`
    /// durations for every PR the listing covers.
    Backfill(EtaBackfillArgs),
    /// Leak-free replay of a heuristic against real `sweep.outcome` history:
    /// mean pinball loss, p25-p75 coverage and bias, optionally paired
    /// against a second heuristic id on the identical replay set.
    Backtest(EtaBacktestArgs),
}

impl EtaCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            EtaCommand::Backfill(args) => args.run(),
            EtaCommand::Backtest(args) => args.run(),
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct EtaBackfillArgs {
    /// Repository to backfill, as `owner/name`. Defaults to whatever `gh`
    /// resolves from `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory to run `gh` from, and whose `.loom/logs/` journal to write.
    /// Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Maximum number of PRs to examine, most recently created first.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_BACKFILL_LIMIT)]
    pub limit: u32,

    /// Report progress to stderr while the timelines are fetched.
    #[arg(long)]
    pub progress: bool,

    /// Print the derived rows instead of appending them to the journal.
    #[arg(long)]
    pub dry_run: bool,
}

impl EtaBackfillArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let repo = match &self.repo {
            Some(r) => r.clone(),
            None => resolve_repo(&root)
                .ok_or_else(|| anyhow::anyhow!("could not resolve owner/repo; pass --repo"))?,
        };

        let (histories, list_error) =
            fetch_histories(&root, Some(repo.as_str()), self.limit, false, self.progress);
        if let Some(err) = &list_error {
            eprintln!("[eta backfill] WARNING: enumerating PRs did not fully answer: {err}");
        }

        let loom = Provenance::current();
        let mut entries = Vec::new();
        for h in &histories {
            entries.extend(entries_from_pr_history(h, &repo, &loom));
        }
        println!(
            "[eta backfill] {} PR(s) examined for {repo}, {} stage sample(s) derived",
            histories.len(),
            entries.len()
        );

        if self.dry_run {
            println!("{}", serde_json::to_string_pretty(&entries)?);
        } else if !entries.is_empty() {
            let path = journal::journal_path(&root);
            journal::append(&path, &entries)?;
            println!("[eta backfill] appended to {}", path.display());
        }

        if histories.is_empty() && list_error.is_some() {
            std::process::exit(1);
        }
        Ok(())
    }
}

/// `owner/repo`, from `gh repo view`. Unlike `pr-latency` (which lets `gh`
/// resolve the repo implicitly for every call it makes), backfill needs the
/// slug up front to stamp on every derived row.
fn resolve_repo(root: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct NameWithOwner {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
    }
    let q: Query<NameWithOwner> = gh_query(
        &["repo", "view", "--json", "nameWithOwner"],
        root,
        false,
        |n: &NameWithOwner| n.name_with_owner.is_empty(),
    );
    match q {
        Query::Populated(n) => Some(n.name_with_owner),
        _ => None,
    }
}

#[derive(clap::Args)]
pub(crate) struct EtaBacktestArgs {
    /// The heuristic id to backtest (e.g. `land-v1`, `finish-v1`).
    #[arg(long, value_name = "ID")]
    pub heuristic: String,

    /// Also backtest this second heuristic id and report which one wins
    /// (lower overall mean pinball loss) on the identical replay set.
    #[arg(long, value_name = "ID")]
    pub compare: Option<String>,

    /// Only replay cases at or after this RFC 3339 instant.
    #[arg(long, value_name = "RFC3339")]
    pub since: Option<String>,

    /// Only replay this repo's cases.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory whose `.loom/logs/` journals to replay. Defaults to the
    /// current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Emit one JSON document on stdout instead of the human report.
    #[arg(long)]
    pub json: bool,
}

impl EtaBacktestArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let registry = Registry::builtin();
        let Some(heuristic) = registry.get(&self.heuristic) else {
            bail!(
                "unknown heuristic id {:?} (known: {})",
                self.heuristic,
                registry.ids().join(", ")
            );
        };
        let since = match &self.since {
            Some(raw) => Some(
                DateTime::parse_from_rfc3339(raw)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| anyhow::anyhow!("invalid --since {raw:?}: {e}"))?,
            ),
            None => None,
        };
        let filter = Filter {
            since,
            repo: self.repo.as_deref(),
        };

        let envelopes = load_outcome_envelopes(&root);
        let mut history = StageSamples::default();
        history.push_envelopes(&envelopes);
        let journal_entries = journal::read(&journal::journal_path(&root));
        history.push_journal(&journal_entries, "local");
        let mut cases = backtest::cases_from_envelopes(&envelopes);
        cases.extend(backtest::cases_from_journal(&journal_entries));
        let loom = Provenance::current();

        if let Some(other_id) = &self.compare {
            let Some(other) = registry.get(other_id) else {
                bail!("unknown heuristic id {:?} (known: {})", other_id, registry.ids().join(", "));
            };
            let comparison = backtest::compare(heuristic, other, &history, &cases, filter, &loom)?;
            if self.json {
                println!("{}", serde_json::to_string_pretty(&comparison)?);
            } else {
                print!("{}", render_comparison(&comparison));
            }
            return Ok(());
        }

        let report = backtest::run(heuristic, &history, &cases, filter, &loom);
        if self.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", render_report(&report));
        }
        Ok(())
    }
}

/// Both generations of `sweep-outcome-telemetry.jsonl`, rotated first — the
/// same order [`StageSamples::load_outcome_journal`] reads them in.
fn load_outcome_envelopes(root: &Path) -> Vec<TelemetryEnvelope> {
    let path = root
        .join(".loom")
        .join("logs")
        .join("sweep-outcome-telemetry.jsonl");
    let rotated = path.with_file_name(format!(
        "{}.1",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
    ));
    let mut envelopes = loom_daemon::sweep_outcomes::read_all_outcome_telemetry(&rotated);
    envelopes.extend(loom_daemon::sweep_outcomes::read_all_outcome_telemetry(&path));
    envelopes
}

fn render_bucket(name: &str, b: &Bucket) -> String {
    format!(
        "  {name:<28} n={:<5} scored={:<5} refused={:<5} pinball={:>10} coverage={:>7} bias={:>10}\n",
        b.n,
        b.scored,
        b.refused,
        b.mean_pinball_loss_sec
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "-".to_string()),
        b.coverage
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "-".to_string()),
        b.bias_sec
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "-".to_string()),
    )
}

fn render_report(r: &BacktestReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("ETA backtest: {} ({})\n", r.heuristic, r.kind));
    out.push_str(&render_bucket("overall", &r.overall));
    if !r.by_repo.is_empty() {
        out.push_str("by repo:\n");
        for (repo, b) in &r.by_repo {
            out.push_str(&render_bucket(repo, b));
        }
    }
    if !r.by_horizon.is_empty() {
        out.push_str("by horizon:\n");
        for (h, b) in &r.by_horizon {
            out.push_str(&render_bucket(h, b));
        }
    }
    out
}

fn render_comparison(c: &Comparison) -> String {
    let mut out = String::new();
    out.push_str(&render_report(&c.a));
    out.push_str(&render_report(&c.b));
    out.push_str(&format!("better: {}\n", c.better.as_deref().unwrap_or("tie / neither scored")));
    out
}
