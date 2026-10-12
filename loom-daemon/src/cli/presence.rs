//! `loom-daemon presence` (#11563): operator-session facts for ETA.
//!
//! A thin entry point over `loom_daemon::presence`. `beat` is what the opt-in
//! Claude Code hook (`defaults/hooks/presence-beat.sh`) calls; it reads the
//! hook JSON from stdin for `session_id` and `hook_event_name` only. Every
//! subcommand on the hook path is fail-open: an error goes to stderr and the
//! exit status stays `0`, so a presence problem can never block a prompt.

use std::io::{IsTerminal, Read};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use loom_daemon::presence::beat::{self, Ctx};
use loom_daemon::presence::journal;
use loom_daemon::telemetry::SweepResult;

/// Opt-in switch for the automatic in-session `sweep.started` that
/// `lease ensure` emits.
pub(crate) const ENABLE_ENV: &str = "LOOM_OPERATOR_PRESENCE";

/// Operator-session presence and in-session sweep facts (#11563).
#[derive(clap::Args)]
pub(crate) struct PresenceArgs {
    #[command(subcommand)]
    action: PresenceCommand,
}

impl PresenceArgs {
    pub(crate) fn run(self) -> Result<()> {
        self.action.run()
    }
}

#[derive(clap::Subcommand)]
enum PresenceCommand {
    /// Record one hook event: journal an `operator.presence` fact (always on
    /// `UserPromptSubmit`, otherwise at most once per 5 minutes). Reads the
    /// hook payload from stdin; only `session_id` and `hook_event_name` are
    /// kept. Always exits 0.
    Beat(BeatArgs),
    /// Journal `sweep.started` (`dispatch_origin=operator`) for an in-session
    /// sweep, carrying the lease comment's sweep id.
    SweepStart(SweepStartArgs),
    /// Journal `sweep.outcome` (`dispatch_origin=operator`) for an in-session
    /// sweep and stop reporting it as driven.
    SweepOutcome(SweepOutcomeArgs),
    /// Hand every journaled fact to the transport. No transport exists yet
    /// (#11525), so this reports the pending count and leaves the journal intact.
    Export(CommonArgs),
    /// Print how many facts are waiting in the journal.
    Status(CommonArgs),
}

#[derive(clap::Args, Clone)]
pub(crate) struct CommonArgs {
    /// Workspace (any directory inside the Loom checkout).
    #[arg(long, default_value = ".")]
    workspace: String,
    /// The session id. Defaults to `$CLAUDE_SESSION_ID`, then `unknown`.
    #[arg(long)]
    session_id: Option<String>,
    /// The operator's forge login. Defaults to `$LOOM_OPERATOR`.
    #[arg(long)]
    operator: Option<String>,
}

#[derive(clap::Args)]
pub(crate) struct BeatArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// The hook event. Defaults to the payload's `hook_event_name`.
    #[arg(long)]
    event: Option<String>,
}

#[derive(clap::Args)]
pub(crate) struct SweepStartArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// The issue (or PR with `--pr`) number.
    number: u32,
    /// The sweep id from the in-session lease comment.
    #[arg(long)]
    sweep_id: String,
    /// `owner/repo`. Defaults to `$LOOM_REPO`, then the `origin` remote.
    #[arg(long)]
    repo: Option<String>,
    /// The number is a PR, not an issue.
    #[arg(long)]
    pr: bool,
}

#[derive(clap::Args)]
pub(crate) struct SweepOutcomeArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// The issue number.
    number: u32,
    /// The sweep id from the in-session lease comment.
    #[arg(long)]
    sweep_id: String,
    /// `success`, `failure`, `cancelled` or `blocked`.
    #[arg(long)]
    result: String,
    /// `owner/repo`; defaults to the one recorded at start.
    #[arg(long)]
    repo: Option<String>,
    /// The PR the sweep produced, if any.
    #[arg(long)]
    pr_number: Option<u32>,
}

impl PresenceCommand {
    fn run(self) -> Result<()> {
        let outcome = match self {
            Self::Beat(a) => run_beat(&a),
            Self::SweepStart(a) => run_sweep_start(&a),
            Self::SweepOutcome(a) => run_sweep_outcome(&a),
            Self::Export(a) => run_export(&a),
            Self::Status(a) => run_status(&a),
        };
        if let Err(e) = outcome {
            eprintln!("presence: {e:#}");
        }
        Ok(())
    }
}

fn ctx(common: &CommonArgs, session_hint: Option<String>) -> Result<Ctx> {
    let root = loom_daemon::repo_root::resolve_repo_root(&common.workspace)?;
    let raw = common
        .session_id
        .clone()
        .or(session_hint)
        .or_else(|| std::env::var("CLAUDE_SESSION_ID").ok())
        .unwrap_or_default();
    Ok(Ctx {
        journal: journal::default_journal_path(&root),
        root,
        session_id: beat::sanitize_session(&raw),
        host: loom_daemon::host_identity::host_identity(),
        operator: common
            .operator
            .clone()
            .or_else(|| std::env::var("LOOM_OPERATOR").ok())
            .filter(|s| !s.trim().is_empty()),
        now: Utc::now(),
    })
}

fn run_beat(args: &BeatArgs) -> Result<()> {
    let mut stdin = String::new();
    if !std::io::stdin().is_terminal() {
        // Bounded read: the payload can carry a long prompt we never keep.
        std::io::stdin()
            .take(1 << 20)
            .read_to_string(&mut stdin)
            .ok();
    }
    let fields = beat::parse_hook_fields(&stdin);
    drop(stdin);
    let event = args
        .event
        .clone()
        .or(fields.hook_event_name)
        .unwrap_or_else(|| "PostToolUse".to_string());
    let ctx = ctx(&args.common, fields.session_id)?;
    beat::beat(&ctx, &event)?;
    Ok(())
}

fn run_sweep_start(args: &SweepStartArgs) -> Result<()> {
    let ctx = ctx(&args.common, None)?;
    let repo = resolve_repo(args.repo.as_deref(), &ctx.root)
        .context("repo unresolved: pass --repo owner/name or set LOOM_REPO")?;
    let kind = if args.pr { "pr" } else { "issue" };
    beat::sweep_started(&ctx, &repo, kind, args.number, &args.sweep_id)
}

fn run_sweep_outcome(args: &SweepOutcomeArgs) -> Result<()> {
    let ctx = ctx(&args.common, None)?;
    let result = match args.result.as_str() {
        "success" => SweepResult::Success,
        "failure" => SweepResult::Failure,
        "cancelled" => SweepResult::Cancelled,
        "blocked" => SweepResult::Blocked,
        other => anyhow::bail!("unknown --result `{other}`"),
    };
    let repo = resolve_repo(args.repo.as_deref(), &ctx.root);
    beat::sweep_outcome(&ctx, repo.as_deref(), args.number, &args.sweep_id, result, args.pr_number)
}

fn run_export(args: &CommonArgs) -> Result<()> {
    let ctx = ctx(args, None)?;
    match journal::export(&ctx.journal, &mut journal::NoTransport) {
        Ok(r) => {
            println!("presence export: sent {} record(s), {} rejected", r.exported, r.rejected)
        }
        Err(e) => println!(
            "presence export: {} record(s) pending, kept in {}: {e:#}",
            journal::pending_count(&ctx.journal),
            ctx.journal.display()
        ),
    }
    Ok(())
}

fn run_status(args: &CommonArgs) -> Result<()> {
    let ctx = ctx(args, None)?;
    println!(
        "presence: {} record(s) pending in {}",
        journal::pending_count(&ctx.journal),
        ctx.journal.display()
    );
    Ok(())
}

/// `--repo`, else `$LOOM_REPO`, else the `origin` remote's `owner/name`.
fn resolve_repo(flag: Option<&str>, root: &Path) -> Option<String> {
    flag.map(str::to_string)
        .or_else(|| std::env::var("LOOM_REPO").ok())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["config", "--get", "remote.origin.url"])
                .output()
                .ok()?;
            parse_remote(String::from_utf8_lossy(&out.stdout).trim())
        })
}

/// `owner/name` out of a GitHub-style remote URL.
pub(crate) fn parse_remote(url: &str) -> Option<String> {
    let tail = url.rsplit_once("github.com").map(|(_, t)| t)?;
    let tail = tail.trim_start_matches([':', '/']).trim_end_matches(".git");
    let mut parts = tail.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !name.is_empty() && parts.next().is_none())
        .then(|| format!("{owner}/{name}"))
}

/// Called by `lease ensure` once a lease is published and renewing. Opt-in
/// (`LOOM_OPERATOR_PRESENCE=1`) and fail-open: an in-session sweep is reported
/// as `sweep.started` with the lease's own sweep id.
pub(crate) fn on_lease_published(workspace: &str, issue: u64, sweep_id: &str) {
    if std::env::var(ENABLE_ENV).ok().as_deref() != Some("1") {
        return;
    }
    let Ok(number) = u32::try_from(issue) else {
        return;
    };
    let common = CommonArgs {
        workspace: workspace.to_string(),
        session_id: None,
        operator: None,
    };
    let result = ctx(&common, None).and_then(|ctx| {
        let repo = resolve_repo(None, &ctx.root).context("repo unresolved")?;
        beat::sweep_started(&ctx, &repo, "issue", number, sweep_id)
    });
    if let Err(e) = result {
        eprintln!("presence: sweep.started not journaled: {e:#}");
    }
}
