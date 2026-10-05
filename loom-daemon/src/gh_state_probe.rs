//! Read-only `gh` state probes shared by over-threshold modules (#9985).
//!
//! Each helper runs one `gh ... view` through the [`GhInvocation`] facade and
//! returns the raw [`Output`] only when the process actually ran; a spawn
//! failure, collect failure, or timeout is `None`. Callers keep their own
//! exit-status and stdout interpretation. These live here (rather than inline
//! in `stash_retirement` / `sweep_outcome_summary`) so those ratcheted files do
//! not grow — see `.loom/docs/file-size-policy.md`.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use crate::cmd_out::CmdOutcome;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// Deadline for a single probe (previously unbounded at both call sites).
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

fn ran(outcome: CmdOutcome) -> Option<Output> {
    match outcome {
        CmdOutcome::Ran(output) => Some(output),
        _ => None,
    }
}

/// `gh issue view <issue> --json state -q .state`, run from `repo_root`.
pub(crate) fn issue_state_output(repo_root: &Path, issue: u64) -> Option<Output> {
    let issue = issue.to_string();
    ran(GhInvocation::new(
        Operation::new("issue.view"),
        AccessIntent::Read,
        GhTarget::None,
        PROBE_TIMEOUT,
    )
    .args(["issue", "view", &issue, "--json", "state", "-q", ".state"])
    .current_dir(repo_root)
    .run())
}

/// `gh pr view <pr> --repo <repo> --json mergedAt`.
pub(crate) fn pr_merged_at_output(repo: &str, pr: u32) -> Option<Output> {
    let pr = pr.to_string();
    ran(GhInvocation::new(
        Operation::new("pr.view"),
        AccessIntent::Read,
        GhTarget::repo(repo).unwrap_or(GhTarget::None),
        PROBE_TIMEOUT,
    )
    .args(["pr", "view", &pr, "--repo", repo, "--json", "mergedAt"])
    .run())
}

/// `gh run list ...` run from `repo_root` through the facade (#10282), so the
/// main-health-gate CI probe emits an `invoke github` span. `Ok(stdout)` only
/// on a zero exit; anything else is `Err(reason)` (the caller maps it to
/// "unknown", never to a verdict).
pub(crate) fn run_list_stdout(
    repo_root: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    let outcome =
        GhInvocation::new(Operation::new("run.list"), AccessIntent::Read, GhTarget::None, timeout)
            .args(
                std::iter::once("run")
                    .chain(std::iter::once("list"))
                    .chain(args.iter().copied()),
            )
            .current_dir(repo_root)
            .run();
    match ran(outcome) {
        Some(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Some(out) => Err(format!("`gh run list` exited {:?}", out.status.code())),
        None => Err("`gh run list` did not run (spawn failure or timeout)".to_string()),
    }
}
