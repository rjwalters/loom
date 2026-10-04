//! `loom-daemon eta fleet events backfill|refresh` and
//! `loom-daemon eta fleet state --as-of` (#10197).
//!
//! - `events backfill` walks each endpoint (`--endpoint`, default both: the
//!   issue-events listing, then the pulls listing) from where its cursor left
//!   off to the end of the listing, checkpointing after every page. A rate-limit stop, the reserve floor or `--max-pages` ends the run
//!   cleanly with exit `75` (`EX_TEMPFAIL`); re-running resumes at the next
//!   unread page.
//! - `events refresh` reads from the head (page 1 conditional on the cached
//!   ETag, so a quiet repo costs one `304`) until it meets rows already cached.
//! - `state --as-of T` replays the cached events strictly before `T`
//!   ([`loom_daemon::eta::fleet_state::fleet_state`]) and makes no forge call.

use std::path::PathBuf;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};

use loom_daemon::eta::fleet_agreement;
use loom_daemon::eta::fleet_events::{self, EventLog, EventsCursor, SyncMode, SyncOutcome};
use loom_daemon::eta::fleet_events_forge::{ForgeEndpoint, ForgeEventSource};
use loom_daemon::eta::fleet_state::{fleet_state, FleetState};

/// `EX_TEMPFAIL`: the run stopped early and is resumable.
const EXIT_RESUMABLE: i32 = 75;

/// Pages per run, absent `--max-pages`: 100 rows each, so a 50k-event backfill
/// spans a few runs rather than draining an hour's budget in one.
const DEFAULT_MAX_PAGES: u64 = 200;

/// Core calls left untouched, absent `--reserve`: the pool the fleet's daemons
/// share for dispatch must not be spent on history.
const DEFAULT_RESERVE: u64 = 1000;

#[derive(clap::Subcommand)]
pub(crate) enum EventsCommand {
    /// Continue (or start) the historical walk of the raw event cache.
    Backfill(EventsSyncArgs),
    /// Top the raw event cache up from the head of the listing.
    Refresh(EventsSyncArgs),
}

impl EventsCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            EventsCommand::Backfill(args) => args.run(SyncMode::Backfill),
            EventsCommand::Refresh(args) => args.run(SyncMode::Refresh),
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct EventsSyncArgs {
    /// Repository, as `owner/name`. Defaults to whatever `gh` resolves from
    /// `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory to run `gh` from, and whose `.loom/state/eta/fleet/` to write.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Maximum page requests this run, per endpoint (100 rows each).
    #[arg(long, value_name = "N")]
    pub max_pages: Option<u64>,

    /// Read only this listing: `issues-events` or `pulls`. Default: both, in
    /// that order.
    #[arg(long, value_name = "ENDPOINT", value_parser = parse_endpoint)]
    pub endpoint: Option<ForgeEndpoint>,

    /// Stop once the forge reports fewer than this many core calls remaining.
    #[arg(long, value_name = "CALLS")]
    pub reserve: Option<u64>,
}

impl EventsSyncArgs {
    fn run(self, mode: SyncMode) -> Result<()> {
        let root = resolve_root(self.repo_root);
        let repo = resolve_repo(self.repo, &root)?;
        let events = fleet_events::events_path(&root, &repo);
        let cursor_file = fleet_events::cursor_path(&root, &repo);
        let mut log = EventLog::open(&events)?;
        let mut cursor = EventsCursor::read(&cursor_file, &repo);
        let endpoints: Vec<ForgeEndpoint> = match self.endpoint {
            Some(e) => vec![e],
            None => ForgeEndpoint::ALL.to_vec(),
        };
        let mut resumable = false;
        for endpoint in endpoints {
            let mut source = ForgeEventSource::new(
                endpoint,
                &repo,
                &root,
                self.reserve.unwrap_or(DEFAULT_RESERVE),
            );
            let report = fleet_events::sync(
                &mut source,
                &mut log,
                &mut cursor,
                &cursor_file,
                mode,
                self.max_pages.unwrap_or(DEFAULT_MAX_PAGES),
            )?;
            let name = endpoint.name();
            println!(
                "[eta fleet events] {repo} {name}: {} page(s) read, {} event(s) appended, {} cached ({})",
                report.pages,
                report.appended,
                log.len(),
                events.display(),
            );
            match report.outcome {
                SyncOutcome::Complete => {
                    if mode == SyncMode::Backfill {
                        println!(
                            "[eta fleet events] {name}: backfill complete; use `refresh` from now on"
                        );
                    }
                }
                SyncOutcome::PageBudget => {
                    eprintln!("[eta fleet events] {name}: page budget reached; re-run to continue");
                    resumable = true;
                }
                SyncOutcome::Stopped(why) => {
                    // A rate limit or the reserve floor applies to every
                    // endpoint alike: stop here rather than spend the next.
                    eprintln!("[eta fleet events] {name}: stopped: {why}; re-run to resume");
                    std::process::exit(EXIT_RESUMABLE);
                }
            }
        }
        if resumable {
            std::process::exit(EXIT_RESUMABLE);
        }
        Ok(())
    }
}

fn parse_endpoint(name: &str) -> Result<ForgeEndpoint, String> {
    ForgeEndpoint::from_name(name).ok_or_else(|| {
        let known: Vec<&str> = ForgeEndpoint::ALL.iter().map(|e| e.name()).collect();
        format!("unknown endpoint {name:?}; expected one of {}", known.join(", "))
    })
}

#[derive(clap::Args)]
pub(crate) struct FleetStateArgs {
    /// The instant to reconstruct. Only cached events strictly before it are
    /// read.
    #[arg(long, value_name = "RFC3339")]
    pub as_of: String,

    /// Repository, as `owner/name`. Defaults to whatever `gh` resolves from
    /// `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory whose `.loom/state/eta/fleet/` to read.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Print the whole state as JSON.
    #[arg(long)]
    pub json: bool,
}

impl FleetStateArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = resolve_root(self.repo_root);
        let repo = resolve_repo(self.repo, &root)?;
        let as_of = DateTime::parse_from_rfc3339(&self.as_of)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| anyhow::anyhow!("invalid --as-of {:?}: {e}", self.as_of))?;
        let path = fleet_events::events_path(&root, &repo);
        if !path.exists() {
            bail!(
                "no raw event cache for {repo} at {} — run `loom-daemon eta fleet events backfill --repo {repo}`",
                path.display()
            );
        }
        let events = fleet_events::load_events(&path);
        let state = fleet_state(&events, &repo, as_of);
        if self.json {
            println!("{}", serde_json::to_string_pretty(&state)?);
        } else {
            print!("{}", render(&state));
        }
        Ok(())
    }
}

#[derive(clap::Args)]
pub(crate) struct AgreementArgs {
    /// JSONL export of logged `eta.estimate` explanations (one
    /// `eta-explanation/v1` object per line, e.g. from SigNoz).
    #[arg(long, value_name = "FILE")]
    pub estimates: PathBuf,

    /// Repository, as `owner/name`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory whose `.loom/state/eta/fleet/` to read.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

impl AgreementArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = resolve_root(self.repo_root);
        let repo = resolve_repo(self.repo, &root)?;
        let path = fleet_events::events_path(&root, &repo);
        if !path.exists() {
            bail!("no raw event cache for {repo} at {}", path.display());
        }
        let text = std::fs::read_to_string(&self.estimates)?;
        let (explanations, skipped) = fleet_agreement::parse_explanations(&text);
        if skipped > 0 {
            eprintln!("[eta fleet agreement] skipped {skipped} unparseable line(s)");
        }
        let events = fleet_events::load_events(&path);
        let report = fleet_agreement::agreement(&events, &repo, &explanations);
        if self.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", fleet_agreement::render(&report));
        }
        Ok(())
    }
}

fn render(state: &FleetState) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "fleet state {} as of {}", state.repo, state.as_of.to_rfc3339());
    let _ = writeln!(out, "  events read:     {}", state.events_read);
    let _ = writeln!(out, "  open issues:     {}", state.open_issues);
    let _ = writeln!(out, "  open PRs:        {}", state.open_prs);
    let _ = writeln!(out, "  building:        {}", state.building);
    let _ = writeln!(out, "  operator holds:  {}", state.operator_holds);
    let _ = writeln!(out, "  held for human:  {}", state.held_for_human);
    let lockout = state
        .pr_open_skip_lockout
        .map_or("unknown (no closing refs cached)", |l| if l { "yes" } else { "no" });
    let _ = writeln!(out, "  open-PR lockout: {lockout}");
    for (stage, n) in &state.stage_counts {
        let _ = writeln!(out, "    {stage:<15} {n}");
    }
    out
}

fn resolve_root(repo_root: Option<PathBuf>) -> PathBuf {
    repo_root
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn resolve_repo(repo: Option<String>, root: &std::path::Path) -> Result<String> {
    match repo {
        Some(r) => Ok(r),
        None => super::eta_cmd::resolve_repo(root)
            .ok_or_else(|| anyhow::anyhow!("could not resolve owner/repo; pass --repo")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn render_names_the_counts_and_every_stage() {
        let state = fleet_state(&[], "o/r", Utc::now());
        let text = render(&state);
        assert!(text.contains("fleet state o/r"), "{text}");
        assert!(text.contains("open PRs:        0"), "{text}");
        assert!(text.contains("open-PR lockout: unknown"), "{text}");
    }

    #[test]
    fn endpoint_names_parse_and_unknown_ones_are_refused() {
        assert_eq!(parse_endpoint("pulls"), Ok(ForgeEndpoint::Pulls));
        assert_eq!(parse_endpoint("issues-events"), Ok(ForgeEndpoint::IssuesEvents));
        assert!(parse_endpoint("reviews")
            .unwrap_err()
            .contains("issues-events, pulls"));
    }
}
