//! `loom-daemon presence` (#11563): operator-session facts for ETA.
//!
//! A thin entry point over `loom_daemon::presence`. `beat` is what the opt-in
//! Claude Code hook (`defaults/hooks/presence-beat.sh`) calls; it reads the
//! hook JSON from stdin for `session_id` and `hook_event_name` only. Every
//! subcommand on the hook path is fail-open: an error goes to stderr and the
//! exit status stays `0`, so a presence problem can never block a prompt.
//! Because the hook discards that stderr, every error is also appended to the
//! breadcrumb file beside the journal (`journal::record_error`), and
//! `presence status` reports how many there are and the last one.

use std::io::{IsTerminal, Read};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use loom_daemon::presence::beat::{self, Ctx};
use loom_daemon::presence::{flush, journal};
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
    /// One-shot, daemon-less drain (#11713): POST every journaled fact to the
    /// OTLP ingest the workspace's `observability` config (or
    /// `$LOOM_PRESENCE_OTLP_ENDPOINT`) names, release what the collector
    /// acknowledged, and exit. `beat` runs this itself on `Stop`.
    Flush(CommonArgs),
    /// Alias of `flush` (kept for the pre-#11713 spelling).
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
    fn workspace(&self) -> &str {
        match self {
            Self::Beat(a) => &a.common.workspace,
            Self::SweepStart(a) => &a.common.workspace,
            Self::SweepOutcome(a) => &a.common.workspace,
            Self::Flush(a) | Self::Export(a) | Self::Status(a) => &a.workspace,
        }
    }

    fn run(self) -> Result<()> {
        let workspace = self.workspace().to_string();
        let outcome = match self {
            Self::Beat(a) => run_beat(&a),
            Self::SweepStart(a) => run_sweep_start(&a),
            Self::SweepOutcome(a) => run_sweep_outcome(&a),
            Self::Flush(a) | Self::Export(a) => run_export(&a),
            Self::Status(a) => run_status(&a),
        };
        if let Err(e) = outcome {
            report_failure(&workspace, &format!("{e:#}"));
        }
        Ok(())
    }
}

/// Fail-open error reporting: stderr, plus a durable breadcrumb beside the
/// journal so a failure on the hook path (whose output is discarded) is never
/// silent. When the workspace cannot even be resolved, stderr is all there is.
fn report_failure(workspace: &str, message: &str) {
    eprintln!("presence: {message}");
    let Ok(root) = loom_daemon::repo_root::resolve_repo_root(workspace) else {
        return;
    };
    let journal = journal::default_journal_path(&root);
    if !journal::record_error(&journal, Utc::now(), message) {
        eprintln!(
            "presence: could not record the failure in {} either",
            journal::errors_path(&journal).display()
        );
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
    // Session end: drain the journal now, since an operator host has no daemon
    // to do it later (#11713). A failure leaves the facts journaled and is
    // reported through the fail-open path.
    if event == "Stop" {
        flush::flush(&ctx.root, &ctx.journal)?;
    }
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
    match flush::flush(&ctx.root, &ctx.journal) {
        Ok(r) => {
            println!("presence flush: sent {} record(s), {} rejected", r.exported, r.rejected)
        }
        Err(e) => println!(
            "presence flush: {} record(s) pending, kept in {}: {e:#}",
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
    match flush::resolve_target(&ctx.root) {
        Ok(t) => println!("presence: export target {}", t.endpoint),
        Err(e) => println!("presence: no export target ({e:#}); facts stay in the journal"),
    }
    let (failures, last) = journal::error_summary(&ctx.journal);
    if failures > 0 {
        println!(
            "presence: {failures} failed write(s) recorded in {}; last: {}",
            journal::errors_path(&ctx.journal).display(),
            last.unwrap_or_default()
        );
    }
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
        report_failure(workspace, &format!("sweep.started not journaled: {e:#}"));
    }
}

/// Called where an in-session sweep ends: the checkpoint delete after a merge
/// (`success`, with the checkpoint's PR), `lease renewer release` while the
/// sweep is still driven (`failure`), and the renewal loop's issue-closed stop
/// (`success`). Opt-in like [`on_lease_published`] and fail-open: journals
/// `sweep.outcome` only for a lease this host recorded with `sweep.started`,
/// so whichever signal comes first wins and the rest are no-ops.
pub(crate) fn on_sweep_finished(
    workspace: &str,
    issue: u64,
    result: SweepResult,
    pr_number: Option<u32>,
) {
    let enabled = std::env::var(ENABLE_ENV).ok().as_deref() == Some("1");
    finish_if(enabled, workspace, issue, result, pr_number);
}

/// [`on_sweep_finished`] with the opt-in read already made (testable seam).
fn finish_if(
    enabled: bool,
    workspace: &str,
    issue: u64,
    result: SweepResult,
    pr_number: Option<u32>,
) {
    if !enabled {
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
    let result =
        ctx(&common, None).and_then(|ctx| beat::finish_driven(&ctx, number, result, pr_number));
    if let Err(e) = result {
        report_failure(workspace, &format!("sweep.outcome not journaled: {e:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hook path discards stderr, so a failure must reach the breadcrumb
    /// file beside the workspace's journal.
    #[test]
    fn report_failure_writes_the_breadcrumb_beside_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        let ws = dir.path().to_str().unwrap();
        report_failure(ws, "appending to presence journal: No space left on device");
        let root = loom_daemon::repo_root::resolve_repo_root(ws).unwrap();
        let (count, last) = journal::error_summary(&journal::default_journal_path(&root));
        assert_eq!(count, 1);
        assert!(last.unwrap().contains("No space left on device"));
    }

    fn workspace_with_driven_lease(issue: u32) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        let root = loom_daemon::repo_root::resolve_repo_root(dir.path().to_str().unwrap()).unwrap();
        let ctx = Ctx {
            journal: journal::default_journal_path(&root),
            root: root.clone(),
            session_id: "sess-1".into(),
            host: "host-a".into(),
            operator: None,
            now: Utc::now(),
        };
        beat::sweep_started(&ctx, "rjwalters/loom", "issue", issue, "sweep-issue-5-1").unwrap();
        (dir, root)
    }

    fn outcomes(root: &Path) -> Vec<loom_daemon::telemetry::SweepOutcomeRecord> {
        journal::read_pending(&journal::default_journal_path(root))
            .into_iter()
            .filter_map(|e| match e.record {
                loom_daemon::telemetry::TelemetryRecord::SweepOutcome(o) => Some(o),
                _ => None,
            })
            .collect()
    }

    /// #11563: the automatic end-of-sweep outcome carries the operator origin
    /// and the lease id when opted in, and is absent when not.
    #[test]
    fn a_sweep_end_journals_the_outcome_only_when_opted_in() {
        let (dir, root) = workspace_with_driven_lease(5);
        let ws = dir.path().to_str().unwrap();
        finish_if(false, ws, 5, SweepResult::Success, Some(8));
        assert!(outcomes(&root).is_empty(), "opt-in unset must journal nothing");
        assert_eq!(beat::load_state(&root, "sess-1").driving.len(), 1);

        finish_if(true, ws, 5, SweepResult::Success, Some(8));
        let got = outcomes(&root);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].sweep_id, "sweep-issue-5-1");
        assert_eq!(got[0].dispatch_origin.as_deref(), Some("operator"));
        assert_eq!(got[0].pr_number, Some(8));
    }

    /// A failed emit never reaches the caller (the checkpoint delete or the
    /// lease release): it returns normally and leaves a breadcrumb.
    #[test]
    fn a_failed_sweep_end_emit_never_fails_the_caller() {
        let (dir, root) = workspace_with_driven_lease(5);
        let ws = dir.path().to_str().unwrap();
        let journal_path = journal::default_journal_path(&root);
        std::fs::remove_file(&journal_path).unwrap();
        std::fs::create_dir_all(&journal_path).unwrap(); // appends now fail
        finish_if(true, ws, 5, SweepResult::Failure, None);
        let (count, last) = journal::error_summary(&journal_path);
        assert_eq!(count, 1);
        assert!(last.unwrap().contains("sweep.outcome not journaled"));
        // Outside any repository: nothing to write to, still no panic.
        let outside = tempfile::tempdir().unwrap();
        finish_if(true, outside.path().to_str().unwrap(), 5, SweepResult::Success, None);
    }
}
