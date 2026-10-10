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
    /// Tail of the worker's stderr on a nonzero exit (preflight rejections
    /// otherwise reach the caller with no reason).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

/// Bytes of worker stderr kept as the `diagnostic` on a failed launch.
const DIAGNOSTIC_TAIL_BYTES: usize = 2000;

/// The last [`DIAGNOSTIC_TAIL_BYTES`] of `stderr`, trimmed; `None` when empty.
pub fn stderr_tail(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut start = text.len().saturating_sub(DIAGNOSTIC_TAIL_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    Some(text[start..].to_string())
}

/// Only a non-claude runtime is launched here; `claude` keeps today's Task
/// subagent path byte-identical and the caller is told `delegate-to-claude`.
pub fn delegates_to_claude(runtime: &str) -> bool {
    runtime == "claude"
}

/// One read-only `gh` call through the managed facade (#9985/#9987), parsed
/// as JSON. `None` on any failure: the callers treat "unknown" as "no artifact".
fn gh_json(root: &Path, op: &'static str, args: &[&str]) -> Option<serde_json::Value> {
    use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    let outcome = GhInvocation::new(
        Operation::new(op),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(60),
    )
    .args(args)
    .current_dir(root)
    .run();
    match outcome {
        crate::cmd_out::CmdOutcome::Ran(out) if out.status.success() => {
            serde_json::from_slice(&out.stdout).ok()
        }
        _ => None,
    }
}

fn has_label(value: &serde_json::Value, label: &str) -> bool {
    value["labels"]
        .as_array()
        .is_some_and(|l| l.iter().any(|x| x["name"] == label))
}

/// Open PRs on `feature/issue-N` (number, head SHA, labels); `None` when gh
/// could not answer.
fn open_issue_prs(root: &Path, issue: u64) -> Option<serde_json::Value> {
    let head = format!("feature/issue-{issue}");
    gh_json(
        root,
        "worker_run.pr_list",
        &[
            "pr",
            "list",
            "--head",
            &head,
            "--state",
            "open",
            "--json",
            "number,headRefOid,labels",
        ],
    )
}

/// Builder artifact: an open `feature/issue-N` PR labelled
/// `loom:review-requested` that is new since `before` (the pre-launch
/// snapshot) or whose head moved, so a pre-existing PR is not credited to a
/// worker that did nothing.
pub fn builder_artifact(before: &serde_json::Value, after: &serde_json::Value) -> bool {
    let prior = |number: &serde_json::Value| {
        before
            .as_array()
            .and_then(|prs| prs.iter().find(|p| &p["number"] == number))
    };
    after.as_array().is_some_and(|prs| {
        prs.iter().any(|p| {
            has_label(p, "loom:review-requested")
                && prior(&p["number"]).is_none_or(|b| b["headRefOid"] != p["headRefOid"])
        })
    })
}

fn pr_view(root: &Path, pr: u64) -> Option<serde_json::Value> {
    gh_json(
        root,
        "worker_run.pr_view",
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

/// Resolve the role's runtime the way a dispatch does (#8554). `Err` carries
/// the exit code: 75 no seat, 78 config.
fn admit(
    root: &Path,
    role: Role,
) -> Result<
    (
        crate::runtime_admission::ResolvedRuntime,
        Option<crate::runtime_preference::Reservation>,
    ),
    LaunchError,
> {
    let mut admission = crate::runtime_preference::resolve_for_dispatch(root, role.as_str(), None)
        .map_err(|r| LaunchError {
            code: rejection_exit_code(&r.reason),
            message: r.diagnostic(),
        })?;
    let backstop = admission.backstop.take();
    let admitted = admission
        .admitted
        .ok_or_else(|| LaunchError::config("no runtime resolved for role"))?;
    Ok((admitted, backstop))
}

fn run(args: &RunArgs, role: Role) -> Result<i32, LaunchError> {
    let root = super::workspace(None)?;
    let (admitted, backstop) = admit(&root, role)?;
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
        diagnostic: None,
    };
    if delegates_to_claude(&admitted.runtime) {
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
    // Pre-launch snapshot so a PR that already existed is not credited to this
    // worker. An unreadable snapshot degrades to "no prior PRs".
    let builder_before = match role {
        Role::Builder => open_issue_prs(&root, issue).unwrap_or_else(|| serde_json::json!([])),
        Role::Doctor => serde_json::Value::Null,
    };
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
            report.diagnostic = stderr_tail(&out.stderr);
            if let Some(tail) = &report.diagnostic {
                eprintln!("worker exited {:?}; stderr tail:\n{tail}", out.status.code());
            }
            // Preflight rejections (78/75/126) pass through; a plain failure is 1.
            match out.status.code() {
                Some(c @ (75 | 78 | 126 | 127)) => c,
                _ => EXIT_NO_ARTIFACT,
            }
        }
        crate::proc_exec::Completion::Exited(_) => {
            let produced = match role {
                Role::Builder => open_issue_prs(&root, issue)
                    .is_some_and(|after| builder_artifact(&builder_before, &after)),
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
    #[serial_test::serial]
    fn claude_resolution_delegates_without_launching() {
        // Isolate from a dispatched session's runtime pins (#4739), restoring after.
        let pins = [
            "LOOM_RUNTIME",
            "LOOM_RUNTIME_BUILDER",
            "LOOM_RUNTIME_DOCTOR",
        ];
        let prior: Vec<_> = pins.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for k in pins {
            std::env::remove_var(k);
        }
        // Unconfigured repo with the claude adapter present: the static path.
        let dir = tempfile::tempdir().unwrap();
        let adapter = dir.path().join("defaults/scripts/spawn-claude.sh");
        std::fs::create_dir_all(adapter.parent().unwrap()).unwrap();
        std::fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for rel in [
            "defaults/roles/builder.json",
            "defaults/runtimes/claude.json",
        ] {
            let to = dir.path().join(rel);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::copy(repo.join(rel), to).unwrap();
        }
        let result = admit(dir.path(), Role::Builder);
        for (k, v) in prior {
            if let Some(v) = v {
                std::env::set_var(k, v);
            }
        }
        let (admitted, backstop) = result.unwrap();
        assert_eq!(admitted.runtime, "claude");
        assert!(backstop.is_none());
        assert!(delegates_to_claude(&admitted.runtime));
        assert!(!delegates_to_claude("codex"));
        assert!(!delegates_to_claude("opencode"));
    }

    #[test]
    fn builder_artifact_ignores_a_preexisting_unchanged_pr() {
        let rr = serde_json::json!([{"name": "loom:review-requested"}]);
        let pr =
            |n: u64, sha: &str| serde_json::json!({"number": n, "headRefOid": sha, "labels": rr});
        let before = serde_json::json!([pr(7, "a")]);
        assert!(!builder_artifact(&before, &serde_json::json!([pr(7, "a")])));
        assert!(builder_artifact(&before, &serde_json::json!([pr(7, "b")])));
        assert!(builder_artifact(&serde_json::json!([]), &serde_json::json!([pr(8, "a")])));
        let unlabelled = serde_json::json!([{"number": 9, "headRefOid": "c", "labels": []}]);
        assert!(!builder_artifact(&serde_json::json!([]), &unlabelled));
    }

    #[test]
    fn stderr_tail_keeps_the_end_and_skips_empty() {
        assert_eq!(stderr_tail(b"  \n"), None);
        assert_eq!(stderr_tail(b"no seat\n").as_deref(), Some("no seat"));
        let long = format!("{}END", "x".repeat(5000));
        let tail = stderr_tail(long.as_bytes()).unwrap();
        assert_eq!(tail.len(), DIAGNOSTIC_TAIL_BYTES);
        assert!(tail.ends_with("END"));
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
