//! `loom-daemon forge rerun <RUN_ID> [--failed]` / `forge rerun --job <JOB_ID>` — re-run a
//! workflow run (or one job) in place, and say plainly why GitHub refused
//! when it does (#10633).
//!
//! # Why
//!
//! Doctor re-ran a job cancelled by a runner shutdown with the plain CLI and
//! the REST endpoint by hand; both came back `HTTP 403`, and all it could
//! report was "my token got 403". It could not tell a missing App permission
//! (re-running needs **Actions: write**, which the fleet's writer App
//! `loom-fleet-dispatch` does not hold) from a secondary rate limit that
//! clears by itself, so it rebased and pushed to trigger a fresh run — a full
//! CI cycle and a new conflict window instead of one rerun.
//!
//! # Credential
//!
//! A rerun is a write, so it runs on the **writer** identity only
//! ([`AccessIntent::Write`] + [`GhInvocation::writer_identity`]); it is never
//! routed to a reader App, which holds no write permission at all.
//!
//! # Contract
//!
//! Exactly one sentinel line on stdout. Branch on it, not on the exit code.
//!
//! | Sentinel | Exit |
//! |---|---|
//! | `LOOM-RERUN-OK <run\|job> <id>` | 0 |
//! | `LOOM-RERUN-DENIED <class> <detail>` — `permission`, `credential` or `forbidden` | 1 |
//! | `LOOM-RERUN-DENIED <class> <detail>` — `secondary-rate-limit` or `rate-limit` | 2 |
//! | `LOOM-RERUN-ERROR <reason>` | 3 |
//!
//! `<class>` is [`crate::forge_denial::Denial::as_str`]. A `permission`
//! detail names the grant (`needs actions:write`) and GitHub's message. A
//! rate-limited rerun is not retried here: a rerun is not idempotent
//! (`ci.rerun`: `retry = "no-auto-retry"`), so the caller decides.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::forge_denial::{self as denial, Denial};
use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
use crate::proc_exec::Completion;

/// Deadline for the one POST.
const TIMEOUT: Duration = Duration::from_secs(60);

/// What to re-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    /// Every job of run `id` (`POST …/actions/runs/{id}/rerun`).
    Run(u64),
    /// Only the failed (and cancelled) jobs of run `id`
    /// (`POST …/actions/runs/{id}/rerun-failed-jobs`).
    Failed(u64),
    /// One job (`POST …/actions/jobs/{id}/rerun`).
    Job(u64),
}

impl What {
    /// The REST path for `nwo`.
    #[must_use]
    pub fn endpoint(self, nwo: &str) -> String {
        match self {
            Self::Run(id) => format!("repos/{nwo}/actions/runs/{id}/rerun"),
            Self::Failed(id) => format!("repos/{nwo}/actions/runs/{id}/rerun-failed-jobs"),
            Self::Job(id) => format!("repos/{nwo}/actions/jobs/{id}/rerun"),
        }
    }

    fn label(self) -> String {
        match self {
            Self::Run(id) | Self::Failed(id) => format!("run {id}"),
            Self::Job(id) => format!("job {id}"),
        }
    }
}

/// The terminal answer — exactly one sentinel line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok(String),
    Denied { denial: Denial, detail: String },
    Error(String),
}

impl Outcome {
    /// The single stdout line callers branch on.
    #[must_use]
    pub fn sentinel(&self) -> String {
        let line = match self {
            Self::Ok(what) => format!("LOOM-RERUN-OK {what}"),
            Self::Denied { denial, detail } => {
                format!("LOOM-RERUN-DENIED {} {detail}", denial.as_str())
            }
            Self::Error(why) => format!("LOOM-RERUN-ERROR {why}"),
        };
        line.replace(['\n', '\r'], " ")
    }

    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Ok(_) => 0,
            Self::Denied { denial, .. } if denial.is_transient() => 2,
            Self::Denied { .. } => 1,
            Self::Error(_) => 3,
        }
    }
}

/// Classify one finished POST: `stdout` is the `--include` output, `stderr`
/// what `gh` printed. Pure, so the whole table is unit-tested.
#[must_use]
pub fn classify(what: What, endpoint: &str, exit_ok: bool, stdout: &str, stderr: &str) -> Outcome {
    let response = crate::forge_listing::parse_http_response(stdout);
    let status = response
        .as_ref()
        .map(|r| r.status)
        .or_else(|| crate::gh_invocation::accounting::stderr_status(stderr));
    if exit_ok && status.is_none_or(|s| (200..300).contains(&s)) {
        return Outcome::Ok(what.label());
    }
    let body = response
        .as_ref()
        .map(|r| r.body.as_str())
        .unwrap_or_default();
    let text = format!("{body}\n{stderr}");
    let headers = response.as_ref().map(|r| &r.ratelimit);
    let message = denial::body_message(body).or_else(|| {
        stderr
            .lines()
            .next()
            .map(|l| l.trim().trim_start_matches("gh:").trim().to_string())
            .filter(|l| !l.is_empty())
    });
    if let Some(kind) = denial::classify(status, &text, headers) {
        return Outcome::Denied {
            denial: kind,
            detail: format!(
                "HTTP {} for {endpoint}: {}",
                status.map_or_else(|| "?".to_string(), |s| s.to_string()),
                denial::describe(kind, endpoint, true, message.as_deref())
            ),
        };
    }
    let why = match status {
        Some(s) => format!("HTTP {s} for {endpoint}: {}", message.unwrap_or_default()),
        None => format!("{endpoint}: {}", message.unwrap_or_else(|| "no HTTP response".into())),
    };
    Outcome::Error(why.trim_end_matches([':', ' ']).to_string())
}

/// POST the rerun for `what` in `nwo` on the writer credential.
#[must_use]
pub fn rerun(gh_bin: &Path, cwd: Option<&Path>, nwo: &str, what: What) -> Outcome {
    let target = match GhTarget::repo(nwo) {
        Ok(t) => t,
        Err(e) => return Outcome::Error(e),
    };
    let endpoint = what.endpoint(nwo);
    let mut inv =
        GhInvocation::new(Operation::new("ci.rerun"), AccessIntent::Write, target, TIMEOUT)
            .forge_op(crate::forge_call_stats::ops::CI_RERUN)
            .writer_identity()
            .program(gh_bin)
            .args(["api", "-X", "POST", "--include", endpoint.as_str()]);
    if let Some(dir) = cwd {
        inv = inv.current_dir(dir);
    }
    match inv.execute() {
        Ok(GhCompletion::Captured(Completion::Exited(out))) => classify(
            what,
            &endpoint,
            out.status.success(),
            &String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr).trim(),
        ),
        Ok(GhCompletion::Captured(Completion::TimedOut { .. })) => {
            Outcome::Error(format!("{endpoint}: timed out after {}s", TIMEOUT.as_secs()))
        }
        Ok(other) => Outcome::Error(format!("{endpoint}: not sent ({other:?})")),
        Err(e) => Outcome::Error(format!("{endpoint}: {e}")),
    }
}

/// CLI arguments, as parsed by `cli::forge_action`.
#[derive(Debug, Clone)]
pub struct RerunArgs {
    pub run_id: Option<u64>,
    pub failed: bool,
    pub job: Option<u64>,
    pub repo: Option<String>,
}

/// `forge rerun` entry point: print the sentinel, exit.
pub fn cli_entrypoint(args: &RerunArgs) -> ! {
    let outcome = run_cli(args);
    println!("{}", outcome.sentinel());
    std::process::exit(outcome.exit_code())
}

fn run_cli(args: &RerunArgs) -> Outcome {
    if crate::forge_cmd::detect_forge(None) == crate::forge_cmd::ForgeType::Gitea {
        return Outcome::Error("gitea-unsupported".into());
    }
    let what = match (args.job, args.run_id, args.failed) {
        (Some(job), None, false) => What::Job(job),
        (None, Some(run), true) => What::Failed(run),
        (None, Some(run), false) => What::Run(run),
        _ => {
            return Outcome::Error(
                "usage: forge rerun <RUN_ID> [--failed] | forge rerun --job <JOB_ID>".into(),
            )
        }
    };
    let cwd = std::env::current_dir().ok();
    let env_repo = std::env::var("LOOM_REPO").ok().filter(|v| !v.is_empty());
    let target = crate::forge_etag_store::resolve_target(
        cwd.as_deref(),
        args.repo.as_deref().or(env_repo.as_deref()),
    );
    let Some(nwo) = target.repo else {
        return Outcome::Error(
            "cannot resolve owner/repo (pass --repo or run inside a clone)".into(),
        );
    };
    let gh = PathBuf::from(crate::gh_invocation::gh_bin());
    rerun(&gh, cwd.as_deref(), &nwo, what)
}

#[cfg(test)]
#[path = "forge_rerun_tests.rs"]
mod tests;
