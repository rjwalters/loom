//! `loom-daemon eta fleet …` — build, top up and inspect the fleet-wide,
//! forge-derived ETA history snapshot (#9343).
//!
//! # Why this is a command and not a tick
//!
//! Deriving a repo's stage history costs one `gh pr list` plus one
//! `issues/<n>/timeline` read per PR. Paying that on every daemon tick, on
//! every host, is exactly the API cost #9343 names as the objection to
//! forge-sourced history — so the derivation lives here, behind an explicit
//! backfill, and the daemon only ever *reads* the cached file
//! ([`loom_daemon::eta::fleet::load_all`]).
//!
//! - `backfill` — the once-per-repo full derivation.
//! - `refresh` — the incremental top-up: only PRs the forge says moved since
//!   the cached snapshot's cursor.
//! - `show` — what is cached, and whether it is enough evidence to estimate.
//! - `events backfill|refresh`, `state --as-of` — the raw event cache and
//!   `fleet_state(t)` (#10197); see `eta_fleet_events_cmd.rs`.
//!
//! # Sharing a snapshot between hosts
//!
//! The snapshot file is a portable artifact: the determinism guarantee is over
//! its *contents*, and nothing host-shaped is in it. Two hosts agree byte for
//! byte either by pointing `LOOM_ETA_FLEET_SNAPSHOT_DIR` at one shared
//! directory, or by copying the file, or by backfilling with the same
//! `--as-of` against the same forge state. `show --json` prints the whole
//! snapshot for exactly that purpose.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};

use loom_daemon::eta::fleet::{self, FleetSnapshot};

use super::pr_latency_cmd::fetch_histories_matching;

/// PRs examined per full backfill, absent `--limit`. Matches `eta backfill`'s
/// own default: enough to seed a whole repo's baseline in one pass.
const DEFAULT_BACKFILL_LIMIT: u32 = 300;

/// PRs examined per incremental refresh, absent `--limit`. A refresh covers
/// only what moved since the cursor; this is a ceiling, not an expectation.
const DEFAULT_REFRESH_LIMIT: u32 = 100;

/// Slack subtracted from the cursor when building the `updated:>=` search.
///
/// The cursor is the newest forge *event* the snapshot holds, and a PR whose
/// event is that old may still have been updated a moment later by something
/// the snapshot does not record. Re-reading a day's worth of PRs is a handful
/// of timeline calls and makes the incremental path idempotent rather than
/// merely cheap.
const REFRESH_OVERLAP_DAYS: i64 = 1;

#[derive(clap::Subcommand)]
pub(crate) enum FleetCommand {
    /// Derive the full snapshot for a repo from its PR label timelines.
    Backfill(FleetBuildArgs),
    /// Top the cached snapshot up with only the PRs that moved since its
    /// cursor.
    Refresh(FleetBuildArgs),
    /// Print what is cached: id, as-of, cursor, PR census, per-stage sample
    /// and stage-episode counts.
    Show(FleetShowArgs),
    /// The raw, resumable per-repo event cache (#10197):
    /// `eta fleet events backfill|refresh`.
    Events {
        #[command(subcommand)]
        command: super::eta_fleet_events_cmd::EventsCommand,
    },
    /// Reconstruct the repo's whole fleet at an instant from the raw event
    /// cache (#10197): `eta fleet state --as-of RFC3339 [--json]`.
    State(super::eta_fleet_events_cmd::FleetStateArgs),
    /// Score `fleet_state(as_of)` against logged `eta.estimate` features
    /// (#10197): `eta fleet agreement --estimates export.jsonl`.
    Agreement(super::eta_fleet_events_cmd::AgreementArgs),
}

impl FleetCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            FleetCommand::Backfill(args) => args.run(false),
            FleetCommand::Refresh(args) => args.run(true),
            FleetCommand::Show(args) => args.run(),
            FleetCommand::Events { command } => command.run(),
            FleetCommand::State(args) => args.run(),
            FleetCommand::Agreement(args) => args.run(),
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct FleetBuildArgs {
    /// Repository, as `owner/name`. Defaults to whatever `gh` resolves from
    /// `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory to run `gh` from, and whose `.loom/state/eta/fleet/` to write.
    /// Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Maximum number of PRs to examine.
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,

    /// The instant the snapshot describes. Defaults to now; pin it to make
    /// two hosts' snapshots directly comparable.
    #[arg(long, value_name = "RFC3339")]
    pub as_of: Option<String>,

    /// Report progress to stderr while the timelines are fetched.
    #[arg(long)]
    pub progress: bool,

    /// Derive and print, but do not write the cache file.
    #[arg(long)]
    pub dry_run: bool,

    /// Print the resulting snapshot as JSON.
    #[arg(long)]
    pub json: bool,
}

impl FleetBuildArgs {
    /// `incremental = false` rebuilds from a full listing; `true` enumerates
    /// only what moved since the cached cursor and folds it into what is
    /// already there.
    fn run(self, incremental: bool) -> Result<()> {
        let root = resolve_root(self.repo_root.clone());
        let repo = match &self.repo {
            Some(r) => r.clone(),
            None => super::eta_cmd::resolve_repo(&root)
                .ok_or_else(|| anyhow::anyhow!("could not resolve owner/repo; pass --repo"))?,
        };
        let as_of = match &self.as_of {
            Some(raw) => DateTime::parse_from_rfc3339(raw)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| anyhow::anyhow!("invalid --as-of {raw:?}: {e}"))?,
            None => Utc::now(),
        };

        let path = fleet::snapshot_path(&root, &repo);
        let cached = fleet::read(&path);
        let mut snapshot = match (incremental, cached) {
            (true, Some(existing)) => existing,
            // A refresh with nothing cached is a backfill: refusing would make
            // the incremental path an ordering trap, and a full derivation is
            // the correct answer to "top up an empty cache".
            (true, None) => {
                eprintln!(
                    "[eta fleet] no snapshot cached at {}; deriving a full one",
                    path.display()
                );
                FleetSnapshot::empty(&repo)
            }
            (false, _) => FleetSnapshot::empty(&repo),
        };
        let before = snapshot.samples.len();

        let search = incremental.then(|| refresh_search(&snapshot)).flatten();
        let limit = self.limit.unwrap_or(if incremental {
            DEFAULT_REFRESH_LIMIT
        } else {
            DEFAULT_BACKFILL_LIMIT
        });
        if let Some(expr) = &search {
            eprintln!("[eta fleet] incremental: {expr}");
        }
        let (histories, list_error) = fetch_histories_matching(
            &root,
            Some(repo.as_str()),
            limit,
            false,
            search.as_deref(),
            self.progress,
        );
        if let Some(err) = &list_error {
            eprintln!("[eta fleet] WARNING: enumerating PRs did not fully answer: {err}");
        }
        let unreadable = histories.iter().filter(|h| !h.timeline_complete).count();
        if unreadable > 0 {
            eprintln!(
                "[eta fleet] WARNING: {unreadable} PR timeline(s) did not fully answer; \
                 they contribute nothing and evict nothing"
            );
        }

        snapshot.merge(&histories, as_of);
        println!(
            "[eta fleet] {repo}: {} PR(s) read, {} sample(s) cached ({} before), \
             snapshot {} as of {}",
            histories.len(),
            snapshot.samples.len(),
            before,
            snapshot.snapshot_id,
            snapshot.as_of.to_rfc3339(),
        );

        if self.dry_run {
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        } else {
            fleet::write(&path, &snapshot)?;
            println!("[eta fleet] wrote {}", path.display());
        }
        if self.json && !self.dry_run {
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        }

        if histories.is_empty() && list_error.is_some() {
            std::process::exit(1);
        }
        Ok(())
    }
}

/// The `gh pr list --search` expression that covers everything the cached
/// snapshot may have missed, or `None` when there is no cursor to start from
/// (an empty cache, which enumerates everything).
fn refresh_search(snapshot: &FleetSnapshot) -> Option<String> {
    let cursor = snapshot.cursor?;
    let from = cursor - chrono::Duration::days(REFRESH_OVERLAP_DAYS);
    Some(format!("updated:>={}", from.format("%Y-%m-%d")))
}

#[derive(clap::Args)]
pub(crate) struct FleetShowArgs {
    /// Repository, as `owner/name`. Defaults to whatever `gh` resolves from
    /// `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory whose `.loom/state/eta/fleet/` to read. Defaults to the current
    /// directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Print the whole snapshot as JSON — the portable form to hand another
    /// host.
    #[arg(long)]
    pub json: bool,
}

impl FleetShowArgs {
    fn run(self) -> Result<()> {
        let root = resolve_root(self.repo_root.clone());
        let repo = match &self.repo {
            Some(r) => r.clone(),
            None => super::eta_cmd::resolve_repo(&root)
                .ok_or_else(|| anyhow::anyhow!("could not resolve owner/repo; pass --repo"))?,
        };
        let path = fleet::snapshot_path(&root, &repo);
        let Some(snapshot) = fleet::read(&path) else {
            bail!(
                "no fleet snapshot for {repo} at {} — run `loom-daemon eta fleet backfill --repo {repo}`",
                path.display()
            );
        };
        if self.json {
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
            return Ok(());
        }
        print!("{}", render(&snapshot, &path));
        Ok(())
    }
}

fn render(snapshot: &FleetSnapshot, path: &Path) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "eta fleet snapshot {} ({})", snapshot.repo, path.display());
    let _ = writeln!(out, "  id:      {}", snapshot.snapshot_id);
    let _ = writeln!(out, "  as_of:   {}", snapshot.as_of.to_rfc3339());
    let _ = writeln!(
        out,
        "  cursor:  {}",
        snapshot
            .cursor
            .map(|c| c.to_rfc3339())
            .unwrap_or_else(|| "-".to_string())
    );
    let _ = writeln!(out, "  prs:     {}", snapshot.prs.len());
    let _ = writeln!(out, "  samples: {}", snapshot.samples.len());
    for (stage, (completed, censored)) in snapshot.counts_by_stage() {
        let _ = writeln!(out, "    {stage:<14} n={completed:<5} censored={censored}");
    }
    let (verdicts, rejected) = snapshot.verdict_counts();
    let _ = writeln!(out, "  verdicts: {verdicts} ({rejected} changes-requested)");
    // #10218: the split stage record, `merge_hold` included.
    let _ = writeln!(out, "  episodes: {}", snapshot.episodes.len());
    for (stage, (completed, censored)) in snapshot.episode_counts_by_stage() {
        let _ = writeln!(out, "    {stage:<14} n={completed:<5} censored={censored}");
    }
    out
}

fn resolve_root(repo_root: Option<PathBuf>) -> PathBuf {
    repo_root
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use loom_daemon::eta::fleet::samples_from_pr_history;
    use loom_daemon::pr_latency::history::{PrEvent, PrHistory, PrState};
    use loom_daemon::pr_latency::{APPROVED, REVIEW_REQUESTED};

    /// `secs` after a fixed epoch, so every expectation below is exact. The
    /// lib-side fixtures are `#[cfg(test)]` on the library, which the binary
    /// crate's tests cannot see, so this is the local equivalent.
    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .expect("a literal RFC 3339 instant")
            .with_timezone(&Utc)
            + chrono::Duration::seconds(secs)
    }

    fn labeled(label: &str, secs: i64) -> PrEvent {
        PrEvent::Labeled {
            label: label.to_string(),
            at: t(secs),
        }
    }

    /// One merged PR: requested at 1h, approved at 6h, merged at `merged_secs`.
    fn merged_pr(number: u32, merged_secs: i64) -> PrHistory {
        const HOUR: i64 = 3600;
        PrHistory::new(
            number,
            t(0),
            PrState::Merged,
            Some(t(merged_secs)),
            Vec::new(),
            vec![
                labeled(REVIEW_REQUESTED, HOUR),
                labeled(APPROVED, 6 * HOUR),
                PrEvent::Merged { at: t(merged_secs) },
            ],
            true,
        )
    }

    fn snapshot_with_one_pr() -> FleetSnapshot {
        let h = merged_pr(11, 7 * 3600);
        let as_of = t(8 * 3600);
        let mut snapshot = FleetSnapshot::empty("rjwalters/loom");
        snapshot.merge(&[h], as_of);
        snapshot
    }

    #[test]
    fn refresh_search_is_a_day_behind_the_cursor() {
        let snapshot = snapshot_with_one_pr();
        let expr = refresh_search(&snapshot).expect("a snapshot with samples has a cursor");
        assert!(expr.starts_with("updated:>="), "{expr}");
        // The newest event is the merge at 2026-09-01T07:00Z; one day of
        // overlap puts the search at 2026-08-31.
        assert_eq!(expr, "updated:>=2026-08-31");
    }

    #[test]
    fn an_empty_snapshot_has_no_incremental_search_so_refresh_enumerates_all() {
        assert_eq!(refresh_search(&FleetSnapshot::empty("rjwalters/loom")), None);
    }

    #[test]
    fn render_names_the_id_the_census_and_every_stage() {
        let snapshot = snapshot_with_one_pr();
        let text = render(&snapshot, Path::new("/tmp/fleet-x.json"));
        assert!(text.contains(&snapshot.snapshot_id), "{text}");
        assert!(text.contains("prs:     1"), "{text}");
        assert!(text.contains("review_wait"), "{text}");
        assert!(text.contains("merge_wait"), "{text}");
        assert!(text.contains("verdicts: 1"), "{text}");
    }

    #[test]
    fn render_counts_stage_episodes_by_stage_merge_hold_included() {
        const HOUR: i64 = 3600;
        let held = PrHistory::new(
            13,
            t(0),
            PrState::Merged,
            Some(t(9 * HOUR)),
            Vec::new(),
            vec![
                labeled(REVIEW_REQUESTED, HOUR),
                PrEvent::Unlabeled {
                    label: REVIEW_REQUESTED.to_string(),
                    at: t(2 * HOUR),
                },
                labeled(APPROVED, 2 * HOUR),
                labeled("loom:operator", 3 * HOUR),
                PrEvent::Unlabeled {
                    label: "loom:operator".to_string(),
                    at: t(5 * HOUR),
                },
                PrEvent::Merged { at: t(9 * HOUR) },
            ],
            true,
        );
        let mut snapshot = FleetSnapshot::empty("rjwalters/loom");
        snapshot.merge(&[held], t(10 * HOUR));
        let text = render(&snapshot, Path::new("/tmp/fleet-x.json"));
        assert!(text.contains("episodes: 4"), "{text}");
        assert!(text.contains("merge_hold     n=1     censored=0"), "{text}");
        assert!(text.contains("merge_wait     n=2     censored=0"), "{text}");
    }

    #[test]
    fn samples_from_one_pr_carry_the_repo_the_cli_resolved() {
        let h = merged_pr(12, 7 * 3600);
        let samples = samples_from_pr_history(&h, "other/repo", t(7 * 3600));
        assert!(!samples.is_empty());
        assert!(samples.iter().all(|s| s.repo == "other/repo"));
        assert!(samples.iter().all(|s| s.pr_number == 12));
    }
}
