//! A traced write through the agent `gh` front is a span (#13170).
//!
//! The passthrough path ends in an `exec`, which replaces the process: there
//! is no "after", so a `gh pr create` or a label change an agent made left a
//! ledger row (intent) and no record of the outcome. When a trace context is
//! ambient (`LOOM_TRACEPARENT`, which a traced sweep exports to its children)
//! a classified **write** instead runs as a child with the agent's own stdio,
//! wrapped in one `invoke github` span ([`InvocationSpan`], the facade's own
//! emitter — there is no second one).
//!
//! The wrapped call is otherwise the exec path: same argv, same environment
//! (so the same credential), and inherited stdin/stdout/stderr, so its output
//! and exit status reach the agent byte for byte. With no ambient context
//! [`run`] declines and the caller execs, exactly as before. Reads, ETag-served
//! routes and unclassified writes are never touched.

use std::ffi::OsString;
use std::path::Path;

use crate::gh_invocation::billing::{Billing, BillingClass};
use crate::gh_invocation::telemetry::{self, InvocationSpan, Outcome};
use crate::gh_invocation::{
    accounting, transparent, AccessIntent, GhBinSource, GhInvocation, GhTarget, Operation,
    ParentContext,
};

/// The span is advisory; no deadline is applied to the wrapped call.
const NO_DEADLINE: std::time::Duration = std::time::Duration::from_secs(0);

/// Budget for the local `git rev-parse` of the head.
const HEAD_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// `--flag` or `--flag=…` among `args`.
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter()
        .any(|a| a == flag || a.strip_prefix(flag).is_some_and(|v| v.starts_with('=')))
}

/// The `github.operation` of a write `gh` argv the front spans, or `None`
/// for anything else (reads, and writes this slice does not span).
#[must_use]
pub fn operation_of(args: &[String]) -> Option<&'static str> {
    let (command, verb) = (args.first()?, args.get(1)?);
    if has_flag(args, "--help") || args.iter().any(|a| a == "-h" || a == "--web" || a == "-w") {
        return None;
    }
    match (command.as_str(), verb.as_str()) {
        ("pr", "create") => Some("pr.create"),
        ("pr" | "issue", "comment") => Some(if has_flag(args, "--delete-last") {
            "comment.delete"
        } else if has_flag(args, "--edit-last") {
            "comment.edit"
        } else {
            "comment.create"
        }),
        ("pr" | "issue", "edit") => {
            let add = has_flag(args, "--add-label");
            let remove = has_flag(args, "--remove-label");
            Some(match (add, remove, command.as_str()) {
                (true, true, _) => "label.edit",
                (true, false, _) => "label.add",
                (false, true, _) => "label.remove",
                (false, false, "pr") => "pr.edit",
                (false, false, _) => "issue.edit",
            })
        }
        _ => None,
    }
}

/// The head a `pr create` publishes: `HEAD` of `cwd`, which is the branch
/// `gh` itself would use. `None` — not a guess — when the call names its
/// head (`--head`) or `HEAD` cannot be read.
fn head_sha(args: &[String], cwd: Option<&Path>) -> Option<String> {
    if has_flag(args, "--head") || args.iter().any(|a| a == "-H") {
        return None;
    }
    let mut cmd = std::process::Command::new("git");
    cmd.args(["rev-parse", "HEAD"])
        .current_dir(cwd?)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null());
    match crate::proc_exec::run_bounded(cmd, HEAD_BUDGET) {
        Ok(crate::proc_exec::Completion::Exited(out)) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        _ => None,
    }
}

/// Run `raw` as a spanned child of `next` and return its exit code, or `None`
/// when this call is not spanned (not a classified write, or no ambient trace
/// context) and the caller must exec as before.
#[must_use]
pub fn run(
    next: &Path,
    raw: &[OsString],
    sentinel: (&str, OsString),
    cwd: Option<&Path>,
    repo: Option<&str>,
) -> Option<i32> {
    let args = raw
        .iter()
        .map(|a| a.to_str().map(str::to_string))
        .collect::<Option<Vec<String>>>()?;
    let op = operation_of(&args)?;
    // The facade's own gate: an ambient `LOOM_TRACEPARENT`, nothing else.
    let parent = ParentContext::ambient();
    if matches!(parent, ParentContext::Missing) {
        return None;
    }
    let target = repo
        .and_then(|r| GhTarget::repo(r).ok())
        .unwrap_or(GhTarget::None);
    let inv = GhInvocation::new(Operation::new(op), AccessIntent::Write, target, NO_DEADLINE)
        .args(raw)
        .parent(parent);
    let mut span = InvocationSpan::open(&inv);
    if op == "pr.create" {
        span = span.with_head_sha(head_sha(&args, cwd).as_deref());
    }
    let result = transparent::run_inherited(next, raw, &[sentinel]);
    let result = result.map_err(crate::proc_exec::ExecError::Spawn);
    let (outcome, code) = telemetry::classify_passthrough(&result);
    let env_token = ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
    let dir = std::env::var_os("GH_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from);
    let cred = accounting::cred_of_with(dir.as_deref(), env_token);
    let class = match outcome {
        Outcome::Ok => BillingClass::Ok,
        Outcome::SpawnFailed => BillingClass::NotSent,
        _ => BillingClass::Error,
    };
    // A passthrough run never yields a status (its stderr is the agent's).
    let billing = Billing::sent(None, "", None, class, "other", &cred, "writer").with_repo(repo);
    span.finish(&inv, GhBinSource::Path, outcome, code, &billing);
    Some(match (outcome, code) {
        (_, Some(code)) => code,
        (Outcome::SpawnFailed, _) => {
            eprintln!("gh (loom front): failed to exec {}", next.display());
            127
        }
        // Killed by a signal: the shell's convention.
        _ => 128 + signal_of(&result),
    })
}

/// The signal that ended the child, 15 when it cannot be read.
fn signal_of(result: &Result<std::process::ExitStatus, crate::proc_exec::ExecError>) -> i32 {
    #[cfg(unix)]
    if let Ok(status) = result {
        if let Some(sig) = std::os::unix::process::ExitStatusExt::signal(status) {
            return sig;
        }
    }
    let _ = result;
    15
}

#[cfg(test)]
#[path = "write_span_tests.rs"]
mod tests;
