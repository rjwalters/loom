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
use serde::{Deserialize, Serialize};

use loom_daemon::cmd_out::Query;
use loom_daemon::eta::backtest::{self, BacktestReport, Bucket, Comparison, Filter};
use loom_daemon::eta::config::HistoryScopeMode;
use loom_daemon::eta::explanation::{Explanation, Features};
use loom_daemon::eta::fleet;
use loom_daemon::eta::history::StageSamples;
use loom_daemon::eta::journal::{self, censored_from_pr_history, entries_from_pr_history};
use loom_daemon::eta::labels as eta_labels;
use loom_daemon::eta::shadow;
use loom_daemon::eta::{
    AgeSource, CurrentStage, CurrentState, EstimateInput, Kind, NoEstimateReason, Provenance,
    Registry, Stage, Subject,
};
use loom_daemon::script_helpers::{gh_query, run_gh};
use loom_daemon::telemetry::TelemetryEnvelope;
use loom_daemon::worktree_ops::gh::{
    open_linked_pr_args, parse_open_linked_pr_trusted, OpenPrProbe,
};

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
    /// The current estimate(s) for one issue (#9327, Phase 4 of #9289):
    /// `loom-daemon eta view owner/repo#123 [--explain] [--json]`.
    View(EtaViewArgs),
    /// The landing-next list for a repo, soonest first (#9327):
    /// `loom-daemon eta list --repo owner/repo`.
    List(EtaListArgs),
    /// Evaluate the two-gate promotion rule for a shadow candidate (#9328),
    /// and with `--apply`, flip `autonomous.eta.current.<kind>` when — and
    /// only when — both gates pass.
    Promote(EtaPromoteArgs),
    /// Build, top up and inspect the fleet-wide forge-derived history
    /// snapshot (#9343): `loom-daemon eta fleet backfill|refresh|show`.
    Fleet {
        #[command(subcommand)]
        command: super::eta_fleet_cmd::FleetCommand,
    },
    /// Point-in-time walk-forward evaluation on logged estimate/outcome
    /// pairs (#10193): a heuristic's logged estimates vs a fitted model.
    Offline(super::eta_offline_cmd::EtaOfflineArgs),
}

impl EtaCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            EtaCommand::Backfill(args) => args.run(),
            EtaCommand::Backtest(args) => args.run(),
            EtaCommand::View(args) => args.run(),
            EtaCommand::List(args) => args.run(),
            EtaCommand::Promote(args) => args.run(),
            EtaCommand::Fleet { command } => command.run(),
            EtaCommand::Offline(args) => args.run(),
        }
    }
}

/// `loom-daemon eta promote` — the promotion switch's operator surface.
///
/// Assembles both gates from what already exists: the phase-2 backtest over
/// this host's journals (gate 1) and the shadow ledger the ETA tracker has
/// been accumulating (gate 2). The gates themselves live in
/// [`shadow::evaluate`] / [`shadow::promote_if_ready`] — this command cannot
/// relax them, and `--apply` on a failing candidate is a refusal, not an
/// override.
#[derive(clap::Args)]
pub(crate) struct EtaPromoteArgs {
    /// Candidate heuristic id to promote (e.g. `land-v2`).
    #[arg(long, value_name = "ID")]
    pub candidate: String,

    /// Directory whose journals, ledger and config to use. Defaults to the
    /// current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Only replay this repo's backtest cases.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Only replay backtest cases at or after this RFC 3339 instant.
    #[arg(long, value_name = "RFC3339")]
    pub since: Option<String>,

    /// Write the flip when both gates pass. Without it this is a dry
    /// evaluation that changes nothing.
    #[arg(long)]
    pub apply: bool,

    /// Emit the decision record as JSON instead of the human report.
    #[arg(long)]
    pub json: bool,
}

impl EtaPromoteArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let registry = Registry::builtin();
        let Some(candidate) = registry.get(&self.candidate) else {
            bail!(
                "unknown heuristic id {:?} (known: {})",
                self.candidate,
                registry.ids().join(", ")
            );
        };
        let kind = candidate.kind();
        let config = loom_daemon::eta::config::read(&root);
        let current = registry.current(kind, config.current(kind));
        if current.id() == candidate.id() {
            bail!("{} is already current for {kind}", candidate.id());
        }

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
        super::eta_replay_cmd::with_replay_calibration(&registry, &mut history, &cases, &loom);
        let comparison =
            backtest::compare(current, candidate, &history, &cases, filter, &loom).ok();

        let ledger_path = shadow::ledger_path(&root);
        let mut ledger = shadow::read_ledger(&ledger_path)?;
        let now = Utc::now();
        let config_path = loom_daemon::eta::config::promotion_config_path(&root);
        let decision = if self.apply {
            let decision = shadow::promote_if_ready(
                &mut ledger,
                kind,
                current.id(),
                candidate.id(),
                comparison.as_ref(),
                &config_path,
                now,
            )?;
            if decision.promote {
                shadow::write_ledger(&ledger_path, &ledger)?;
            }
            decision
        } else {
            let stats = ledger.stats(kind, current.id(), candidate.id());
            shadow::evaluate(kind, current.id(), candidate.id(), comparison.as_ref(), &stats, now)
        };

        // Every evaluation is recorded, promoting or not: "why has this not
        // flipped yet?" is the question an operator actually asks.
        let log_path = shadow::decision_log_path(&root);
        if let Err(error) = shadow::append_decision(&log_path, &decision) {
            eprintln!("[eta promote] WARNING: could not record the decision: {error}");
        }

        if self.json {
            println!("{}", serde_json::to_string_pretty(&decision)?);
        } else {
            print!("{}", render_decision(&decision, self.apply));
        }
        Ok(())
    }
}

fn render_decision(d: &shadow::PromotionDecision, applied: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "eta promote {} → {} ({})", d.current, d.candidate, d.kind);
    let _ =
        writeln!(out, "  gate 1 backtest: {} — {}", d.backtest.status.as_str(), d.backtest.detail);
    let _ = writeln!(out, "  gate 2 live:     {} — {}", d.live.status.as_str(), d.live.detail);
    let _ = writeln!(out, "  decision: {}", d.reason);
    match (&d.config_path, d.promote, applied) {
        (Some(path), _, _) => {
            let _ =
                writeln!(out, "  PROMOTED: autonomous.eta.current.{} written to {path}", d.kind);
        }
        (None, true, false) => {
            let _ = writeln!(out, "  both gates pass; re-run with --apply to flip the config");
        }
        _ => {
            let _ = writeln!(out, "  current stands: {}", d.current);
        }
    }
    out
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
        let now = Utc::now();
        let mut entries = Vec::new();
        let mut censored = 0_usize;
        for h in &histories {
            entries.extend(entries_from_pr_history(h, &repo, &loom));
            // #9328: the open segments — a PR still in review, a rejection not
            // yet answered, an approval not yet merged — as right-censored
            // lower bounds. Only `land-v2` reads them; every v1 distribution
            // is built from the completed rows above, unchanged.
            let open = censored_from_pr_history(h, &repo, now, &loom);
            censored += open.len();
            entries.extend(open);
        }
        println!(
            "[eta backfill] {} PR(s) examined for {repo}, {} stage sample(s) derived \
             ({censored} of them right-censored)",
            histories.len(),
            entries.len()
        );

        if self.dry_run {
            println!("{}", serde_json::to_string_pretty(&entries)?);
        } else if !entries.is_empty() {
            let path = journal::journal_path(&root);
            let written = journal::append_dedup(&path, &entries)?;
            println!(
                "[eta backfill] appended {written} new row(s) to {} ({} already journaled)",
                path.display(),
                entries.len() - written
            );
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
pub(crate) fn resolve_repo(root: &Path) -> Option<String> {
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
        super::eta_replay_cmd::with_replay_calibration(&registry, &mut history, &cases, &loom);

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

/// [`StageSamples`] read the same way `eta backtest` builds it
/// ([`load_outcome_envelopes`] plus the stage-sample journal), for `eta
/// view`/`eta list`: the `sweep.outcome` telemetry journal merged with
/// `.loom/logs/eta-stage-samples.jsonl` transitions this host observed, with
/// `scope` applied on top (#9343).
///
/// `scope` reads the cached fleet snapshot only — it makes no forge call, so
/// `eta view` costs the same whichever scope it runs at. Building that cache
/// is `eta fleet backfill`'s job.
fn load_history(root: &Path, scope: HistoryScopeMode) -> StageSamples {
    let envelopes = load_outcome_envelopes(root);
    let mut history = StageSamples::default();
    history.push_envelopes(&envelopes);
    let journal_entries = journal::read(&journal::journal_path(root));
    history.push_journal(&journal_entries, "local");
    let mut history = fleet::apply_scope(scope, root, history);
    // #10207: the daemon's calibration log and pending store, so `eta view`
    // shows the recalibrated interval the daemon would.
    history.calibration = loom_daemon::eta::calibration_log::load(root);
    history
}

/// The scope a `--scope` flag asks for: the flag when given, else the
/// configured `autonomous.eta.historyScope`.
fn resolve_scope(flag: Option<&str>, root: &Path) -> Result<HistoryScopeMode> {
    match flag {
        Some(raw) => HistoryScopeMode::parse(raw)
            .ok_or_else(|| anyhow::anyhow!("invalid --scope {raw:?} (local | augment | fleet)")),
        None => Ok(loom_daemon::eta::config::read(root).history_scope),
    }
}

/// Parse `owner/repo#issue` (the `eta view` positional argument) into its
/// `owner/repo` slug and issue number.
fn parse_story(story: &str) -> Result<(String, u32)> {
    let (repo, issue) = story
        .rsplit_once('#')
        .ok_or_else(|| anyhow::anyhow!("expected OWNER/NAME#ISSUE, got {story:?}"))?;
    if repo.split('/').count() != 2 || repo.is_empty() {
        bail!("expected OWNER/NAME#ISSUE, got {story:?}");
    }
    let issue: u32 = issue
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid issue number in {story:?}"))?;
    Ok((repo.to_string(), issue))
}

/// One label, as `gh issue view --json labels` / `gh pr view --json labels`
/// shapes it.
#[derive(Debug, Clone, Deserialize)]
struct LabelRef {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct IssueLabelsRow {
    #[serde(default)]
    labels: Vec<LabelRef>,
}

/// `gh issue view <issue> --repo <repo> --json labels`. `None` when the read
/// did not answer (a caller must not guess a state from an unanswered read).
fn fetch_issue_labels(root: &Path, repo: &str, issue: u32) -> Option<Vec<String>> {
    let issue_s = issue.to_string();
    let args = [
        "issue", "view", &issue_s, "--repo", repo, "--json", "labels",
    ];
    let q: Query<IssueLabelsRow> = gh_query(&args, root, false, |_: &IssueLabelsRow| false);
    match q {
        Query::Populated(row) => Some(row.labels.into_iter().map(|l| l.name).collect()),
        Query::Empty => Some(Vec::new()),
        Query::Malformed { .. } | Query::Failed { .. } | Query::Unavailable(_) => None,
    }
}

#[derive(Debug, Clone, Deserialize)]
struct PrLabelsRow {
    #[serde(default)]
    labels: Vec<LabelRef>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<DateTime<Utc>>,
}

/// The open PR linked to `issue`, if any: `(pr_number, labels, updated_at)`.
///
/// Uses the same closes-graph query [`crate::forge_check_open_pr`]'s guard
/// does ([`open_linked_pr_args`] / [`parse_open_linked_pr_trusted`]) so this
/// never grows a second "is there an open PR" implementation to drift from
/// that one — see `worktree_ops::gh`'s own module docs. Unlike that guard this
/// does not also consult the REST timeline leg (the `Part of #N` phase-PR
/// case): a missed phase PR here means one issue falls back to the checkpoint
/// or refusal path, not a wrongly-reset claim, so the cheaper single-leg probe
/// is an acceptable trade for a read-only estimate.
fn fetch_open_pr(
    root: &Path,
    repo: &str,
    issue: u32,
) -> Option<(u32, Vec<String>, Option<DateTime<Utc>>)> {
    let (owner, name) = repo.split_once('/')?;
    let args = open_linked_pr_args(owner, name, issue);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let outcome = run_gh(&arg_refs, root, false);
    let stdout = outcome.stdout_lossy();
    let OpenPrProbe::Open(pr) = parse_open_linked_pr_trusted(&stdout, root) else {
        return None;
    };
    let pr_s = pr.to_string();
    let pr_args = [
        "pr",
        "view",
        &pr_s,
        "--repo",
        repo,
        "--json",
        "labels,updatedAt",
    ];
    let q: Query<PrLabelsRow> = gh_query(&pr_args, root, false, |_: &PrLabelsRow| false);
    match q {
        Query::Populated(row) => {
            Some((pr, row.labels.into_iter().map(|l| l.name).collect(), row.updated_at))
        }
        // The PR exists (the closes-graph verified it), but its labels could
        // not be read: report it with no labels rather than dropping it, so
        // `stage_from_pr_labels` refuses with `UnknownStage` instead of the
        // item silently vanishing from the view/list output.
        Query::Empty | Query::Malformed { .. } | Query::Failed { .. } | Query::Unavailable(_) => {
            Some((pr, Vec::new(), None))
        }
    }
}

/// Best-effort numeric GitHub repo id for `repo`, for [`Subject::repo_id`]'s
/// rename-stable `repo_key`. `None` on any failure — never blocks an estimate.
fn resolve_repo_id(root: &Path, repo: &str) -> Option<u64> {
    let path = format!("repos/{repo}");
    let outcome = run_gh(&["api", &path, "--jq", ".id"], root, false);
    outcome.ok_stdout_trimmed()?.parse().ok()
}

/// The `/loom:sweep` checkpoint phase this host recorded for `issue`
/// (`<root>/.loom/sweep-checkpoint/issue-<issue>.json`, schema owned by
/// `defaults/scripts/sweep-checkpoint.sh`), mapped to the [`Stage`] it implies
/// and the instant that phase completed (a lower bound on the stage's entry).
///
/// Host-local by construction, like [`StageSamples`]'s own history (#9343):
/// this reads one host's file. A host that never ran the issue's sweep has no
/// checkpoint and returns `None`, which [`resolve_current`] turns into
/// [`NoEstimateReason::UnknownStage`] rather than a guess.
fn checkpoint_stage(root: &Path, issue: u32) -> Option<(Stage, Option<DateTime<Utc>>)> {
    let path = root
        .join(".loom")
        .join("sweep-checkpoint")
        .join(format!("issue-{issue}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let phase = v.get("phase").and_then(serde_json::Value::as_str)?;
    let stage = match phase {
        "curator-done" => Stage::SweepBuilder,
        "builder-done" | "doctor-done" => Stage::ReviewWait,
        "judge-rejected" => Stage::Doctor,
        "judge-done" => Stage::MergeWait,
        // "merge-done" means the work already landed; nothing current to
        // report from the checkpoint alone.
        _ => return None,
    };
    let entered_at = v
        .get("timestamp")
        .and_then(serde_json::Value::as_str)
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.with_timezone(&Utc));
    Some((stage, entered_at))
}

/// Resolve [`CurrentState`] for one issue: the shared state-resolution step
/// `eta view` and `eta list` both call, per the curated design (#9327).
///
/// Precedence, exactly as documented on the issue: an open PR's labels first
/// ([`eta_labels::stage_from_pr_labels`]); else this host's own sweep
/// checkpoint ([`checkpoint_stage`]); else the issue's own labels decide the
/// refusal ([`eta_labels::unstarted_issue_reason`]).
///
/// The `plan` argument of that last call is always `None` here: the dispatch
/// plan a `ready_wait` `start` estimate reads (#9326) is the work finder's
/// last in-process tick, which a separate CLI process cannot see, so a ready
/// (`loom:issue`) item is refused `no_dispatch_plan` rather than given a
/// start time. Surfacing `start` from the CLI needs a published plan; it is
/// not in this phase's scope.
fn resolve_current(
    root: &Path,
    issue: u32,
    pr_labels: Option<&[String]>,
    pr_updated_at: Option<DateTime<Utc>>,
    issue_labels: &[String],
    now: DateTime<Utc>,
) -> CurrentState {
    if let Some(pr_labels) = pr_labels {
        return match eta_labels::stage_from_pr_labels(pr_labels) {
            Ok(stage) => {
                let entered_at = pr_updated_at.map(|t| t.min(now));
                let age_sec = entered_at.map_or(0, |t| (now - t).num_seconds().max(0));
                CurrentState::At(CurrentStage {
                    stage,
                    entered_at,
                    age_sec,
                    age_source: AgeSource::UpdatedAtLowerBound,
                    // A single label read cannot say how many rework rounds
                    // preceded it; `estimate_path` floors `doctor` to at
                    // least one on its own, so 0 is the honest "unknown"
                    // value everywhere else.
                    rework_rounds: 0,
                    episode_entered_at: None,
                })
            }
            Err(reason) => CurrentState::Refused(reason),
        };
    }
    if let Some((stage, entered_at)) = checkpoint_stage(root, issue) {
        let age_sec = entered_at.map_or(0, |t| (now - t).num_seconds().max(0));
        return CurrentState::At(CurrentStage {
            stage,
            entered_at,
            age_sec,
            age_source: AgeSource::Checkpoint,
            rework_rounds: u32::from(stage == Stage::Doctor),
            episode_entered_at: None,
        });
    }
    CurrentState::Refused(
        eta_labels::unstarted_issue_reason(issue_labels, None)
            .unwrap_or(NoEstimateReason::NoDispatchPlan),
    )
}

/// Which [`Kind`]s apply to `current`, per the model's own rule
/// ("finish: every item with a running sweep"; "land: from `sweep.curator` on,
/// or any open PR under review"): both kinds for a running sweep (the
/// checkpoint path) or a refusal (so a hold/gate is reported for either kind a
/// caller asks about), `land` only for an open PR with no known running sweep.
/// A held PR (`merge_hold`, #10218) is reported like the refusal it was
/// before the stage existed: every shipped heuristic refuses it `blocked`.
#[must_use]
fn eligible_kinds(current: &CurrentState, has_open_pr: bool) -> &'static [Kind] {
    match current {
        CurrentState::Refused(_) => &[Kind::Finish, Kind::Land],
        CurrentState::At(c) if has_open_pr && c.stage != Stage::MergeHold => &[Kind::Land],
        CurrentState::At(_) => &[Kind::Finish, Kind::Land],
    }
}

/// Build the [`EstimateInput`] a heuristic reads for `kind`. `labels` feeds
/// [`Features::labels`] only — no v1 heuristic reads features, so everything
/// else is left `None` and [`Features::complete_omissions`] fills in the
/// omission reasons. `dispatch` is likewise always `None`: see
/// [`resolve_current`] for why the CLI never has a dispatch plan to pass.
fn build_input(
    subject: Subject,
    now: DateTime<Utc>,
    current: CurrentState,
    labels: Vec<String>,
) -> EstimateInput {
    let features = Features {
        labels: (!labels.is_empty()).then_some(labels),
        ..Features::default()
    };
    EstimateInput {
        subject,
        as_of: now,
        current,
        features,
        features_omitted: Vec::new(),
        provenance: Provenance::current(),
        dispatch: None,
    }
}

/// One kind's one-line summary, text or JSON.
#[derive(Debug, Clone, Serialize)]
struct KindSummary {
    kind: Kind,
    heuristic: String,
    p25_sec: Option<i64>,
    p50_sec: Option<i64>,
    p75_sec: Option<i64>,
    eta_p50_at: Option<DateTime<Utc>>,
    no_estimate_reason: Option<NoEstimateReason>,
}

fn summarize(e: &Explanation) -> KindSummary {
    KindSummary {
        kind: e.kind,
        heuristic: e.heuristic.clone(),
        p25_sec: e.result.as_ref().map(|r| r.p25_sec),
        p50_sec: e.result.as_ref().map(|r| r.p50_sec),
        p75_sec: e.result.as_ref().map(|r| r.p75_sec),
        eta_p50_at: e.result.as_ref().map(|r| r.eta_p50_at),
        no_estimate_reason: e.no_estimate_reason,
    }
}

fn render_kind_line(s: &KindSummary) -> String {
    match (s.p50_sec, s.no_estimate_reason) {
        (Some(p50), _) => format!(
            "{:<6} p25={}s p50={}s p75={}s eta_p50_at={} ({})\n",
            s.kind.as_str(),
            s.p25_sec.unwrap_or(0),
            p50,
            s.p75_sec.unwrap_or(0),
            s.eta_p50_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
            s.heuristic,
        ),
        (None, Some(reason)) => {
            format!("{:<6} no_estimate_reason={reason} ({})\n", s.kind.as_str(), s.heuristic)
        }
        (None, None) => format!("{:<6} no estimate\n", s.kind.as_str()),
    }
}

/// Every current-heuristic explanation for `subject`/`current`, one per
/// [`eligible_kinds`].
fn explain_current(
    root: &Path,
    subject: &Subject,
    current: &CurrentState,
    issue_labels: &[String],
    has_open_pr: bool,
    now: DateTime<Utc>,
    scope: HistoryScopeMode,
) -> Vec<Explanation> {
    let config = loom_daemon::eta::config::read(root);
    let registry = Registry::builtin();
    let history = load_history(root, scope);
    eligible_kinds(current, has_open_pr)
        .iter()
        .map(|&kind| {
            let input = build_input(subject.clone(), now, current.clone(), issue_labels.to_vec());
            let heuristic = registry.current(kind, config.current(kind));
            heuristic.estimate(&input, &history)
        })
        .collect()
}

#[derive(clap::Args)]
pub(crate) struct EtaViewArgs {
    /// `owner/repo#issue`, e.g. `rjwalters/loom#9327`.
    pub story: String,

    /// Print the full `eta-explanation/v1` JSON for every applicable kind
    /// instead of the one-line summary.
    #[arg(long)]
    pub explain: bool,

    /// Emit the summary as JSON instead of text (ignored with `--explain`,
    /// which is already JSON).
    #[arg(long)]
    pub json: bool,

    /// Directory to run `gh` from, and whose `.loom/` journals to read.
    /// Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Which history to estimate from (#9343): `local` (this host's journals),
    /// `augment` (local plus the cached fleet snapshot) or `fleet` (the cached
    /// snapshot alone). Defaults to `autonomous.eta.historyScope`.
    #[arg(long, value_name = "SCOPE")]
    pub scope: Option<String>,
}

impl EtaViewArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let (repo, issue) = parse_story(&self.story)?;
        let now = Utc::now();

        let Some(issue_labels) = fetch_issue_labels(&root, &repo, issue) else {
            bail!("could not read issue {repo}#{issue} (gh issue view did not answer)");
        };
        let pr = fetch_open_pr(&root, &repo, issue);
        let pr_labels: Option<Vec<String>> = pr.as_ref().map(|(_, labels, _)| labels.clone());
        let pr_updated_at = pr.as_ref().and_then(|(_, _, at)| *at);
        let pr_number = pr.as_ref().map(|(n, _, _)| *n);

        let current =
            resolve_current(&root, issue, pr_labels.as_deref(), pr_updated_at, &issue_labels, now);

        let mut subject = Subject::new(&repo, resolve_repo_id(&root, &repo), issue);
        subject.pr_number = pr_number;

        let scope = resolve_scope(self.scope.as_deref(), &root)?;
        let explanations = explain_current(
            &root,
            &subject,
            &current,
            &issue_labels,
            pr_number.is_some(),
            now,
            scope,
        );

        if self.explain {
            let value = if explanations.len() == 1 {
                serde_json::to_value(&explanations[0])?
            } else {
                serde_json::to_value(&explanations)?
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
            return Ok(());
        }

        let summaries: Vec<KindSummary> = explanations.iter().map(summarize).collect();
        if self.json {
            println!("{}", serde_json::to_string_pretty(&summaries)?);
        } else {
            println!("{repo}#{issue}");
            for s in &summaries {
                print!("{}", render_kind_line(s));
            }
        }
        Ok(())
    }
}

/// Default number of open issues `eta list` examines — the same order of
/// magnitude as `pr-latency`'s default, generous for an on-demand read that is
/// not on a tick.
const DEFAULT_LIST_LIMIT: u32 = 60;

#[derive(clap::Args)]
pub(crate) struct EtaListArgs {
    /// Repository to list, as `owner/name`. Defaults to whatever `gh`
    /// resolves from `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory to run `gh` from. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Maximum number of open issues to examine.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_LIST_LIMIT)]
    pub limit: u32,

    /// Emit the list as JSON instead of text.
    #[arg(long)]
    pub json: bool,

    /// Which history to estimate from (#9343): `local`, `augment` or `fleet`.
    /// Defaults to `autonomous.eta.historyScope`.
    #[arg(long, value_name = "SCOPE")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct IssueRow {
    number: u32,
    #[serde(default)]
    labels: Vec<LabelRef>,
}

#[derive(Debug, Clone, Serialize)]
struct ListRow {
    issue: u32,
    #[serde(flatten)]
    land: KindSummary,
}

impl EtaListArgs {
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
        let now = Utc::now();
        let limit_s = self.limit.to_string();
        let args = [
            "issue",
            "list",
            "--repo",
            &repo,
            "--state",
            "open",
            "--json",
            "number,labels",
            "--limit",
            &limit_s,
        ];
        let q: Query<Vec<IssueRow>> =
            gh_query(&args, &root, false, |v: &Vec<IssueRow>| v.is_empty());
        let rows = match q {
            Query::Populated(rows) => rows,
            Query::Empty => Vec::new(),
            Query::Malformed { error, .. } => {
                bail!("gh issue list returned unreadable JSON: {error}")
            }
            Query::Failed { status, .. } => bail!("gh issue list exited {status}"),
            Query::Unavailable(u) => bail!("gh issue list could not be run: {u:?}"),
        };

        let config = loom_daemon::eta::config::read(&root);
        let registry = Registry::builtin();
        let history = load_history(&root, resolve_scope(self.scope.as_deref(), &root)?);
        let repo_id = resolve_repo_id(&root, &repo);

        let mut out: Vec<ListRow> = Vec::with_capacity(rows.len());
        for row in rows {
            let issue = row.number;
            let issue_labels: Vec<String> = row.labels.into_iter().map(|l| l.name).collect();
            let pr = fetch_open_pr(&root, &repo, issue);
            let pr_labels: Option<Vec<String>> = pr.as_ref().map(|(_, l, _)| l.clone());
            let pr_updated_at = pr.as_ref().and_then(|(_, _, at)| *at);
            let pr_number = pr.as_ref().map(|(n, _, _)| *n);
            let current = resolve_current(
                &root,
                issue,
                pr_labels.as_deref(),
                pr_updated_at,
                &issue_labels,
                now,
            );
            let mut subject = Subject::new(&repo, repo_id, issue);
            subject.pr_number = pr_number;
            let input = build_input(subject, now, current, issue_labels);
            let heuristic = registry.current(Kind::Land, config.current(Kind::Land));
            let explanation = heuristic.estimate(&input, &history);
            out.push(ListRow {
                issue,
                land: summarize(&explanation),
            });
        }

        // Estimable rows first, ascending by land p50; no-estimate rows last,
        // stably ordered by issue number.
        out.sort_by(|a, b| match (a.land.p50_sec, b.land.p50_sec) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.issue.cmp(&b.issue),
        });

        if self.json {
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            for row in &out {
                print!("#{:<6} {}", row.issue, render_kind_line(&row.land));
            }
        }
        Ok(())
    }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    // -- parse_story -----------------------------------------------------

    #[test]
    fn parse_story_splits_owner_repo_and_issue() {
        let (repo, issue) = parse_story("rjwalters/loom#9327").unwrap();
        assert_eq!(repo, "rjwalters/loom");
        assert_eq!(issue, 9327);
    }

    #[test]
    fn parse_story_rejects_missing_hash() {
        assert!(parse_story("rjwalters/loom").is_err());
    }

    #[test]
    fn parse_story_rejects_missing_owner() {
        assert!(parse_story("loom#9327").is_err());
        assert!(parse_story("#9327").is_err());
    }

    #[test]
    fn parse_story_rejects_non_numeric_issue() {
        assert!(parse_story("rjwalters/loom#abc").is_err());
    }

    // -- PR-label stage resolution reuse ----------------------------------

    #[test]
    fn resolve_current_reuses_stage_from_pr_labels_for_review_requested() {
        let root = tempfile::tempdir().unwrap();
        let labels = vec!["loom:review-requested".to_string()];
        let current = resolve_current(root.path(), 42, Some(&labels), Some(now()), &[], now());
        match current {
            CurrentState::At(stage) => {
                assert_eq!(stage.stage, Stage::ReviewWait);
                assert_eq!(stage.age_source, AgeSource::UpdatedAtLowerBound);
            }
            other => panic!("expected At(ReviewWait), got {other:?}"),
        }
    }

    #[test]
    fn resolve_current_reuses_stage_from_pr_labels_for_changes_requested() {
        let root = tempfile::tempdir().unwrap();
        let labels = vec!["loom:changes-requested".to_string()];
        let current = resolve_current(root.path(), 42, Some(&labels), Some(now()), &[], now());
        assert_eq!(
            current,
            CurrentState::At(CurrentStage {
                stage: Stage::Doctor,
                entered_at: Some(now()),
                age_sec: 0,
                age_source: AgeSource::UpdatedAtLowerBound,
                rework_rounds: 0,
                episode_entered_at: None,
            })
        );
    }

    #[test]
    fn resolve_current_refuses_a_held_pr_via_labels() {
        let root = tempfile::tempdir().unwrap();
        let labels = vec![
            "loom:review-requested".to_string(),
            "loom:blocked".to_string(),
        ];
        let current = resolve_current(root.path(), 42, Some(&labels), Some(now()), &[], now());
        assert_eq!(current, CurrentState::Refused(NoEstimateReason::Blocked));
    }

    // -- Local-checkpoint fallback for a running sweep --------------------

    #[test]
    fn checkpoint_stage_maps_every_known_phase() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".loom").join("sweep-checkpoint");
        std::fs::create_dir_all(&dir).unwrap();
        let cases = [
            ("curator-done", Stage::SweepBuilder),
            ("builder-done", Stage::ReviewWait),
            ("doctor-done", Stage::ReviewWait),
            ("judge-rejected", Stage::Doctor),
            ("judge-done", Stage::MergeWait),
        ];
        for (phase, expected) in cases {
            std::fs::write(
                dir.join("issue-42.json"),
                format!(r#"{{"phase":"{phase}","timestamp":"2026-09-30T10:00:00Z"}}"#),
            )
            .unwrap();
            let (stage, entered_at) = checkpoint_stage(root.path(), 42)
                .unwrap_or_else(|| panic!("expected a stage for phase {phase:?}"));
            assert_eq!(stage, expected, "phase {phase:?}");
            assert_eq!(
                entered_at,
                Some(
                    DateTime::parse_from_rfc3339("2026-09-30T10:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc)
                )
            );
        }
    }

    #[test]
    fn checkpoint_stage_is_none_for_a_landed_or_missing_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        // No checkpoint at all.
        assert_eq!(checkpoint_stage(root.path(), 42), None);

        let dir = root.path().join(".loom").join("sweep-checkpoint");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("issue-42.json"), r#"{"phase":"merge-done"}"#).unwrap();
        assert_eq!(checkpoint_stage(root.path(), 42), None);
    }

    #[test]
    fn resolve_current_falls_back_to_the_local_checkpoint_for_a_running_sweep() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".loom").join("sweep-checkpoint");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("42.json").with_file_name("issue-42.json"),
            r#"{"phase":"curator-done","timestamp":"2026-09-30T11:00:00Z"}"#,
        )
        .unwrap();

        // No PR yet; the issue still carries the Builder's claim label.
        let issue_labels = vec!["loom:building".to_string()];
        let current = resolve_current(root.path(), 42, None, None, &issue_labels, now());
        match &current {
            CurrentState::At(stage) => {
                assert_eq!(stage.stage, Stage::SweepBuilder);
                assert_eq!(stage.age_source, AgeSource::Checkpoint);
                assert_eq!(stage.age_sec, Duration::hours(1).num_seconds());
            }
            other => panic!("expected At(SweepBuilder) from the checkpoint, got {other:?}"),
        }
        // A running sweep with no PR yet is eligible for both kinds.
        assert_eq!(eligible_kinds(&current, false), &[Kind::Finish, Kind::Land]);
    }

    // -- Host-local refusal when neither signal is present ----------------

    #[test]
    fn resolve_current_refuses_unknown_stage_with_no_pr_and_no_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        // The issue is claimed (`loom:building`) but this host never ran its
        // sweep, so it has no checkpoint of its own (#9343: host-local
        // history). Must refuse, never guess.
        let issue_labels = vec!["loom:building".to_string()];
        let current = resolve_current(root.path(), 42, None, None, &issue_labels, now());
        assert_eq!(current, CurrentState::Refused(NoEstimateReason::UnknownStage));
        // A refusal is reported for both kinds.
        assert_eq!(eligible_kinds(&current, false), &[Kind::Finish, Kind::Land]);
    }

    #[test]
    fn resolve_current_reports_human_gated_for_an_uncurated_issue() {
        let root = tempfile::tempdir().unwrap();
        let issue_labels = vec!["loom:triage".to_string()];
        let current = resolve_current(root.path(), 42, None, None, &issue_labels, now());
        assert_eq!(current, CurrentState::Refused(NoEstimateReason::HumanGated));
    }

    #[test]
    fn resolve_current_reports_blocked_for_a_held_issue() {
        let root = tempfile::tempdir().unwrap();
        let issue_labels = vec!["loom:blocked".to_string()];
        let current = resolve_current(root.path(), 42, None, None, &issue_labels, now());
        assert_eq!(current, CurrentState::Refused(NoEstimateReason::Blocked));
    }

    // -- eligible_kinds -----------------------------------------------------

    #[test]
    fn eligible_kinds_is_land_only_for_an_open_pr_with_no_known_running_sweep() {
        let current = CurrentState::At(CurrentStage {
            stage: Stage::ReviewWait,
            entered_at: None,
            age_sec: 0,
            age_source: AgeSource::UpdatedAtLowerBound,
            rework_rounds: 0,
            episode_entered_at: None,
        });
        assert_eq!(eligible_kinds(&current, true), &[Kind::Land]);
    }

    // -- rendering ------------------------------------------------------

    #[test]
    fn render_kind_line_shows_the_refusal_reason_when_there_is_no_estimate() {
        let summary = KindSummary {
            kind: Kind::Land,
            heuristic: "land-v1".to_string(),
            p25_sec: None,
            p50_sec: None,
            p75_sec: None,
            eta_p50_at: None,
            no_estimate_reason: Some(NoEstimateReason::Blocked),
        };
        let line = render_kind_line(&summary);
        assert!(line.contains("no_estimate_reason=blocked"), "{line}");
        assert!(line.contains("land-v1"), "{line}");
    }

    #[test]
    fn render_kind_line_shows_quantiles_when_there_is_an_estimate() {
        let summary = KindSummary {
            kind: Kind::Finish,
            heuristic: "finish-v1".to_string(),
            p25_sec: Some(100),
            p50_sec: Some(200),
            p75_sec: Some(300),
            eta_p50_at: Some(now()),
            no_estimate_reason: None,
        };
        let line = render_kind_line(&summary);
        assert!(line.contains("p25=100s"), "{line}");
        assert!(line.contains("p50=200s"), "{line}");
        assert!(line.contains("p75=300s"), "{line}");
    }

    #[test]
    fn kind_summary_json_round_trips_the_no_estimate_reason() {
        let summary = KindSummary {
            kind: Kind::Land,
            heuristic: "land-v1".to_string(),
            p25_sec: None,
            p50_sec: None,
            p75_sec: None,
            eta_p50_at: None,
            no_estimate_reason: Some(NoEstimateReason::HumanGated),
        };
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["kind"], "land");
        assert_eq!(value["no_estimate_reason"], "human_gated");
        assert!(value["p50_sec"].is_null());
    }
}
