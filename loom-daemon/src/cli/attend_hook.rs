//! Attended live output from a role that never claims its issue (#10120).
//!
//! `lease ensure` starts an attended `session.output` tailer at every Builder
//! and Doctor claim step (#10116). Curator and Judge act on an issue without
//! claiming it, so they never reach that call. This module hooks the code
//! paths each of them already runs on its target, instead of adding a step
//! to their prompts (frozen by the prompt budgets, and prose steps get
//! skipped, #7672's lesson):
//!
//! - **Curator**: `premise-check --issue N` (its first per-issue call).
//! - **Judge**: `pr-worktree.sh <PR>`, which runs `live-output-attend --pr`.
//!   The issue is the PR's one closing issue; a PR that closes none or
//!   several stays unscoped rather than guessed. A Judge that reuses a
//!   builder worktree runs neither script; one that checks out with
//!   `worktree.sh <N>` is already covered by `lease ensure`.
//!
//! Both scripts are also run by other roles (Hermit, Architect and Auditor
//! verify their own filed proposal with `premise-check`; Doctor checks out
//! external PRs with `pr-worktree.sh`). So no hook names a role: the run is
//! labelled with the subagent's own `loom-<role>` type.
//!
//! Every refusal (a daemon-launched agent, a top-level session, #10129) is
//! [`attended::start`]'s own; nothing here re-decides it. A hook never fails
//! its host command and, with live output unconfigured, prints nothing.

use std::path::{Path, PathBuf};

use loom_daemon::observability::session_output::attended::{
    self, AttendEnv, Outcome, StartRequest, DEFAULT_IDLE_EXIT_SECS, DEFAULT_MAX_AGE_SECS,
};

/// A request with the defaults every hook shares: no explicit role (the
/// transcript's `loom-<role>` type names it) and the session pid from the
/// environment.
pub(crate) fn request(issue: u32, workspace: PathBuf) -> StartRequest {
    StartRequest {
        issue,
        role: None,
        watch_pid: attended::session_pid_from_env(),
        workspace,
        transcript: None,
        from_offset: None,
        max_age_secs: DEFAULT_MAX_AGE_SECS,
        idle_exit_secs: DEFAULT_IDLE_EXIT_SECS,
    }
}

/// Start `request`'s tailer, then report in at most one stderr line.
pub(crate) fn attend(label: &str, request: &StartRequest) -> Outcome {
    let outcome = attended::start(request, &AttendEnv::from_process());
    if let Some(line) = report(label, request.issue, &outcome) {
        eprintln!("{line}");
    }
    outcome
}

/// The diagnostic for `outcome`, or `None` when nothing was ever going to
/// start: a daemon-launched agent (its own producer covers it) or live output
/// not configured. Those are the common cases, and a hook in someone else's
/// command must stay silent for them.
pub(crate) fn report(label: &str, issue: u32, outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::DaemonLaunched | Outcome::NotConfigured(_) => None,
        other => Some(format!("{label}: live output: {}", other.describe(issue))),
    }
}

/// What a PR-scoped hook does next.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrStep {
    /// Nothing can start; say nothing (see [`report`]).
    Quiet,
    /// Nothing starts, for this reason.
    Unscoped(String),
    /// Start for this issue.
    Attend(u32),
}

/// Decide a PR-scoped hook without touching the network until it has to.
///
/// The cheap local refusals come first, so that with live output unconfigured
/// (or in a daemon-launched agent) the forge is never asked. `closing` lists
/// the PR's closing issues; only exactly one scopes the run.
pub(crate) fn decide_pr(
    pr: u32,
    env: &AttendEnv,
    export: impl FnOnce() -> Result<(), String>,
    closing: impl FnOnce() -> Result<Vec<i64>, String>,
) -> PrStep {
    if env.daemon_launched || export().is_err() {
        return PrStep::Quiet;
    }
    if env.session_id.is_none() {
        return PrStep::Unscoped(format!(
            "PR #{pr}: not publishing live output, no Claude Code session ({} unset)",
            attended::SESSION_ID_ENV
        ));
    }
    let issues = match closing() {
        Ok(issues) => issues,
        Err(why) => {
            return PrStep::Unscoped(format!(
                "PR #{pr}: not publishing live output, its closing issue is unknown: {why}"
            ))
        }
    };
    match issues.as_slice() {
        [one] => match u32::try_from(*one) {
            Ok(issue) => PrStep::Attend(issue),
            Err(_) => {
                PrStep::Unscoped(format!("PR #{pr}: closing issue {one} is not an issue number"))
            }
        },
        [] => PrStep::Unscoped(format!(
            "PR #{pr}: not publishing live output, it closes no issue, so its output has no \
             issue to belong to"
        )),
        many => PrStep::Unscoped(format!(
            "PR #{pr}: not publishing live output, it closes {} issues ({}); one is not \
             guessed",
            many.len(),
            many.iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// `live-output-attend --pr`: attend the PR's one closing issue.
pub(crate) fn attend_pr(label: &str, pr: u32, workspace: &Path) -> Option<Outcome> {
    let env = AttendEnv::from_process();
    let root = loom_daemon::repo_root::resolve_repo_root(&workspace.to_string_lossy()).ok();
    let export = || {
        let root = root.as_deref().ok_or("not inside a Loom checkout")?;
        attended::export_plan(root).map(drop)
    };
    let closing = || {
        let root = root.as_deref().ok_or("not inside a Loom checkout")?;
        super::notify_cleared_blockers::pr_close_targets(i64::from(pr), None, root)
    };
    match decide_pr(pr, &env, export, closing) {
        PrStep::Quiet => None,
        PrStep::Unscoped(why) => {
            eprintln!("{label}: live output: {why}");
            None
        }
        PrStep::Attend(issue) => Some(attend(label, &request(issue, workspace.to_path_buf()))),
    }
}

#[cfg(test)]
#[path = "attend_hook_tests.rs"]
mod tests;
