//! `loom-daemon worker run --role builder|doctor` (#11285, Slice 1): launch one
//! sweep phase out of process on the runtime its `rolePreference` resolves to.
//!
//! Only Builder and Doctor are launchable here; Judge and Curator stay Claude
//! Task subagents of the orchestrator. The command is blocking and reuses the
//! guarded native launch (`spawn-worker.sh` -> `spawn-worker`) rather than a
//! second launcher, so containment and credential admission are unchanged.
//!
//! Exit codes: 0 finished with its artifact, or `delegate-to-claude`; 1 ran
//! without producing the artifact; 2 usage (bad role); 75 no eligible
//! runtime/pool seat (caller falls back to Claude); 78 config unresolvable;
//! 124 timeout.
use super::LaunchError;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const EXIT_NO_ARTIFACT: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NO_SEAT: i32 = 75;
pub const EXIT_TIMEOUT: i32 = 124;

#[derive(clap::Args)]
pub struct RunArgs {
    /// Phase role to launch: `builder` or `doctor` (Judge/Curator are refused).
    #[arg(long, value_name = "ROLE")]
    role: String,
    /// Issue number (builder).
    #[arg(long, conflicts_with = "pr")]
    issue: Option<u64>,
    /// Pull request number (doctor).
    #[arg(long)]
    pr: Option<u64>,
    /// Print one JSON result object on stdout.
    #[arg(long)]
    json: bool,
    /// Wall-clock budget in seconds; exits 124 when exceeded.
    #[arg(long, default_value_t = 3600)]
    timeout: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Builder,
    Doctor,
}
impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builder => "builder",
            Self::Doctor => "doctor",
        }
    }
}

/// Only Builder and Doctor may be launched out of process.
pub fn parse_role(role: &str) -> Result<Role, String> {
    match role {
        "builder" => Ok(Role::Builder),
        "doctor" => Ok(Role::Doctor),
        other => Err(format!(
            "worker run supports --role builder|doctor only; {other:?} stays a Claude Task subagent"
        )),
    }
}

/// Map a resolution rejection to an exit code: an exhausted preference list is
/// "no seat" (75, the caller falls back to Claude); anything else is config (78).
pub fn rejection_exit_code(reason: &str) -> i32 {
    if reason.contains("No runtime in the preference list can serve") {
        EXIT_NO_SEAT
    } else {
        78
    }
}

#[derive(Debug, serde::Serialize, PartialEq, Eq)]
pub struct Report {
    pub role: String,
    pub issue: Option<u64>,
    pub pr: Option<u64>,
    pub runtime: String,
    pub model_profile: Option<String>,
    pub outcome: String,
    pub log_path: Option<String>,
}

fn gh_json(root: &Path, args: &[&str]) -> Option<serde_json::Value> {
    let mut cmd = Command::new("gh");
    cmd.args(args).current_dir(root);
    let out = crate::proc_exec::run_bounded(cmd, Duration::from_secs(60))
        .ok()?
        .output()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn has_label(value: &serde_json::Value, label: &str) -> bool {
    value["labels"]
        .as_array()
        .is_some_and(|l| l.iter().any(|x| x["name"] == label))
}

/// Builder artifact: an open PR on `feature/issue-N` labelled `loom:review-requested`.
fn builder_artifact(root: &Path, issue: u64) -> bool {
    let head = format!("feature/issue-{issue}");
    gh_json(
        root,
        &[
            "pr",
            "list",
            "--head",
            &head,
            "--state",
            "open",
            "--json",
            "number,labels",
        ],
    )
    .and_then(|v| v.as_array().cloned())
    .is_some_and(|prs| prs.iter().any(|p| has_label(p, "loom:review-requested")))
}

fn pr_view(root: &Path, pr: u64) -> Option<serde_json::Value> {
    gh_json(
        root,
        &[
            "pr",
            "view",
            &pr.to_string(),
            "--json",
            "headRefOid,headRefName,labels",
        ],
    )
}

/// Doctor artifact: head moved and the PR is back at `loom:review-requested`.
pub fn doctor_artifact(before_sha: &str, after: &serde_json::Value) -> bool {
    after["headRefOid"]
        .as_str()
        .is_some_and(|s| s != before_sha)
        && has_label(after, "loom:review-requested")
}

fn emit(args: &RunArgs, report: &Report) {
    if args.json {
        println!("{}", serde_json::to_string(report).unwrap_or_default());
    } else {
        println!("outcome={} runtime={}", report.outcome, report.runtime);
    }
}

fn fail(error: &LaunchError) -> ! {
    eprintln!("{}", error.message);
    std::process::exit(error.code);
}

pub fn cli(args: RunArgs) -> anyhow::Result<()> {
    let role = match parse_role(&args.role) {
        Ok(r) => r,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(EXIT_USAGE);
        }
    };
    match (role, args.issue, args.pr) {
        (Role::Builder, Some(_), None) | (Role::Doctor, None, Some(_)) => {}
        _ => {
            eprintln!("builder requires --issue N; doctor requires --pr N");
            std::process::exit(EXIT_USAGE);
        }
    }
    let code = run(&args, role).unwrap_or_else(|e| fail(&e));
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn run(args: &RunArgs, role: Role) -> Result<i32, LaunchError> {
    let root = super::workspace(None)?;
    let mut admission = crate::runtime_preference::resolve_for_dispatch(&root, role.as_str(), None)
        .map_err(|r| LaunchError {
            code: rejection_exit_code(&r.reason),
            message: r.diagnostic(),
        })?;
    let backstop = admission.backstop.take();
    let admitted = admission
        .admitted
        .ok_or_else(|| LaunchError::config("no runtime resolved for role"))?;
    let model_profile = admitted
        .preference
        .as_ref()
        .and_then(|p| p.model_profile.clone());
    let mut report = Report {
        role: role.as_str().into(),
        issue: args.issue,
        pr: args.pr,
        runtime: admitted.runtime.clone(),
        model_profile,
        outcome: "delegate-to-claude".into(),
        log_path: None,
    };
    if admitted.runtime == "claude" {
        // Today's Task-subagent path stays byte-identical; nothing is launched.
        emit(args, &report);
        return Ok(0);
    }

    let (issue, before_sha) = match (role, args.issue, args.pr) {
        (Role::Builder, Some(n), _) => (n, String::new()),
        (_, _, Some(pr)) => {
            let view = pr_view(&root, pr)
                .ok_or_else(|| LaunchError::config(format!("cannot read PR #{pr} via gh")))?;
            let issue = view["headRefName"]
                .as_str()
                .and_then(|h| h.strip_prefix("feature/issue-"))
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| LaunchError::config("PR head is not feature/issue-N"))?;
            (issue, view["headRefOid"].as_str().unwrap_or_default().to_string())
        }
        _ => return Err(LaunchError::config("missing target")),
    };
    let worktree: PathBuf = root.join(".loom/worktrees").join(format!("issue-{issue}"));
    if !worktree.is_dir() {
        return Err(LaunchError::config(format!(
            "worktree {} missing; create it with ./.loom/scripts/worktree.sh {issue}",
            worktree.display()
        )));
    }
    let log = root.join(".loom/logs").join(format!(
        "worker-run-{}-{}-{}.log",
        role.as_str(),
        issue,
        chrono::Utc::now().timestamp()
    ));
    let target = args.issue.or(args.pr).unwrap_or(issue);
    let mut cmd = Command::new(super::scripts_dir(&root).join("spawn-worker.sh"));
    cmd.current_dir(&worktree)
        .env("LOOM_WORKSPACE", &root)
        .arg("--prompt")
        .arg(format!("/loom:{} {target}", role.as_str()))
        .arg("--log")
        .arg(&log)
        .arg("--dangerously-skip-permissions");
    crate::launch_env::apply_launch_env(&mut cmd, Some(&admitted), "worker_run");
    report.log_path = Some(log.display().to_string());

    let completion = crate::proc_exec::run_bounded_observed(
        cmd,
        Duration::from_secs(args.timeout),
        move |pid| crate::runtime_preference::handoff::attach(backstop, pid),
    )
    .map_err(|e| LaunchError {
        code: 126,
        message: format!("cannot launch worker: {e}"),
    })?;
    let code = match completion {
        crate::proc_exec::Completion::TimedOut { .. } => {
            report.outcome = "timeout".into();
            EXIT_TIMEOUT
        }
        crate::proc_exec::Completion::Exited(out) if !out.status.success() => {
            report.outcome = "worker-failed".into();
            // Preflight rejections (78/75/126) pass through; a plain failure is 1.
            match out.status.code() {
                Some(c @ (75 | 78 | 126 | 127)) => c,
                _ => EXIT_NO_ARTIFACT,
            }
        }
        crate::proc_exec::Completion::Exited(_) => {
            let produced = match role {
                Role::Builder => builder_artifact(&root, issue),
                Role::Doctor => args
                    .pr
                    .and_then(|pr| pr_view(&root, pr))
                    .is_some_and(|v| doctor_artifact(&before_sha, &v)),
            };
            if produced {
                report.outcome = "artifact".into();
                0
            } else {
                report.outcome = "no-artifact".into();
                EXIT_NO_ARTIFACT
            }
        }
    };
    emit(args, &report);
    Ok(code)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn judge_and_curator_are_refused() {
        assert_eq!(parse_role("builder"), Ok(Role::Builder));
        assert_eq!(parse_role("doctor"), Ok(Role::Doctor));
        for r in ["judge", "curator", "sweep-lifecycle", ""] {
            assert!(parse_role(r).is_err(), "{r}");
        }
    }

    #[test]
    fn rejection_maps_to_exit_codes() {
        let exhausted = "No runtime in the preference list can serve role \"builder\" right now";
        assert_eq!(rejection_exit_code(exhausted), EXIT_NO_SEAT);
        assert_eq!(rejection_exit_code("unknown runtime"), 78);
    }

    #[test]
    fn doctor_artifact_needs_new_head_and_label() {
        let v =
            serde_json::json!({"headRefOid": "b", "labels": [{"name": "loom:review-requested"}]});
        assert!(doctor_artifact("a", &v));
        assert!(!doctor_artifact("b", &v));
        let no_label = serde_json::json!({"headRefOid": "b", "labels": []});
        assert!(!doctor_artifact("a", &no_label));
    }

    #[test]
    fn claude_resolution_delegates_without_launching() {
        let dir = tempfile::tempdir().unwrap();
        let admission =
            crate::runtime_preference::resolve_for_dispatch(dir.path(), "builder", None);
        // With no preference configured the static path must not name a non-claude runtime.
        if let Ok(a) = admission {
            if let Some(r) = a.admitted {
                assert_eq!(r.runtime, "claude");
            }
        }
    }

    #[test]
    fn clap_parses_run() {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: RunArgs,
        }
        let c = Cli::try_parse_from(["x", "--role", "doctor", "--pr", "5", "--json"]).unwrap();
        assert_eq!(c.args.pr, Some(5));
        assert!(c.args.json);
        assert!(
            Cli::try_parse_from(["x", "--role", "builder", "--issue", "1", "--pr", "2"]).is_err()
        );
    }
}
