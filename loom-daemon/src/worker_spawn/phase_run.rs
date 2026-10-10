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
    /// Stacked child (builder): branch a missing worktree off this branch,
    /// e.g. `feature/issue-<parent>` (passed to `worktree.sh --base`).
    #[arg(long, value_name = "BRANCH", requires = "issue")]
    base: Option<String>,
}

/// The Builder's pre-launch PR snapshot. An unreadable one is refused rather
/// than read as "no prior PRs": that would credit an unchanged pre-existing
/// `loom:review-requested` PR to a worker that did nothing.
fn builder_baseline(
    issue: u64,
    snapshot: Option<serde_json::Value>,
) -> Result<serde_json::Value, LaunchError> {
    snapshot.ok_or_else(|| {
        LaunchError::config(format!(
            "cannot read open feature/issue-{issue} PRs via gh before launch; \
             refusing to launch (artifact attribution would be unsound)"
        ))
    })
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
    if is_inherited_dispatch_pin(
        std::env::var("LOOM_RUNTIME").ok().as_deref(),
        std::env::var("LOOM_ROLE").ok().as_deref(),
    ) {
        std::env::remove_var("LOOM_RUNTIME");
    }
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

/// A global `LOOM_RUNTIME` that arrived together with `LOOM_ROLE` was exported
/// by the daemon's own dispatch (`launch_env::apply_launch_env` pins both) for
/// the *parent* phase, not set by an operator for this one. Treating it as an
/// operator pin would short-circuit the role preference of the phase being
/// launched. Per-role (`LOOM_RUNTIME_<ROLE>`) pins are untouched.
pub fn is_inherited_dispatch_pin(runtime: Option<&str>, role: Option<&str>) -> bool {
    let set = |v: Option<&str>| v.is_some_and(|v| !v.trim().is_empty());
    set(runtime) && set(role)
}

/// Resolve (creating if needed) the managed worktree the phase runs in.
///
/// `worker run` is the step that *launches* the Builder, so a fresh issue has
/// no worktree yet. Setup is delegated to the guarded `worktree.sh` (claim
/// guard, lease, configured roots and stacked bases stay its business). A
/// Doctor's open PR makes `worktree.sh` refuse by design, so a missing issue
/// worktree falls back to `pr-worktree.sh`.
///
/// The base directory is the helpers' own effective root
/// (`LOOM_WORKTREE_ROOT` > `worktree.root` > `.loom/worktrees`, with their
/// unreadable-override fallback), so an external root is found and verified
/// where the script actually puts it. `base` stacks a Builder's fresh worktree
/// (`worktree.sh N --base feature/issue-<parent>`).
fn ensure_worktree(
    root: &Path,
    role: Role,
    issue: u64,
    pr: Option<u64>,
    base: Option<&str>,
) -> Result<PathBuf, LaunchError> {
    let dir = crate::worktree_root::worktree_root_readable(root);
    let issue_wt = dir.join(format!("issue-{issue}"));
    if issue_wt.is_dir() {
        return Ok(issue_wt);
    }
    let (script, arg, wt) = match (role, pr) {
        (Role::Doctor, Some(pr)) => ("pr-worktree.sh", pr, dir.join(format!("pr-{pr}"))),
        _ => ("worktree.sh", issue, issue_wt),
    };
    if wt.is_dir() {
        return Ok(wt);
    }
    let mut cmd = Command::new(super::scripts_dir(root).join(script));
    cmd.current_dir(root).arg(arg.to_string());
    if let (Role::Builder, Some(base)) = (role, base) {
        cmd.arg("--base").arg(base);
    }
    let output = crate::proc_exec::run_bounded(cmd, Duration::from_secs(300))
        .map_err(|e| LaunchError::config(format!("cannot run {script}: {e}")))?
        .output();
    if !output.as_ref().is_some_and(|o| o.status.success()) || !wt.is_dir() {
        let detail = output
            .map(|o| {
                String::from_utf8_lossy(&o.stderr)
                    .trim()
                    .chars()
                    .rev()
                    .take(1000)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
            })
            .unwrap_or_default();
        return Err(LaunchError::config(format!(
            "worktree {} missing and ./.loom/scripts/{script} {arg} did not create it: {detail}",
            wt.display()
        )));
    }
    Ok(wt)
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
    // Pre-launch snapshot so a PR that already existed is not credited to this
    // worker. Taken before any setup: an unreadable one refuses the launch.
    let builder_before = match role {
        Role::Builder => builder_baseline(issue, open_issue_prs(&root, issue))?,
        Role::Doctor => serde_json::Value::Null,
    };
    let worktree = ensure_worktree(&root, role, issue, args.pr, args.base.as_deref())?;
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
    // The orchestrator's own marker/profile must not leak into a phase that
    // resolved statically; `apply_launch_env` re-sets them only when this
    // phase's preference walk produced them.
    cmd.env_remove(crate::launch_env::PREFERENCE_MARKER_ENV)
        .env_remove("LOOM_MODEL_PROFILE");
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
    fn global_runtime_with_role_is_an_inherited_dispatch_pin() {
        assert!(is_inherited_dispatch_pin(Some("claude"), Some("sweep-lifecycle")));
        // A bare operator `LOOM_RUNTIME` (no daemon-pinned role) stays a pin.
        assert!(!is_inherited_dispatch_pin(Some("claude"), None));
        assert!(!is_inherited_dispatch_pin(Some("claude"), Some("  ")));
        assert!(!is_inherited_dispatch_pin(None, Some("sweep-lifecycle")));
    }

    #[test]
    #[serial_test::serial]
    fn missing_worktree_is_created_by_script() {
        let _env = EnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        write_exec(
            &dir.path().join(".loom/scripts/worktree.sh"),
            "#!/bin/sh\nmkdir -p \"$(dirname \"$0\")/../worktrees/issue-$1\"\n",
        );
        let wt = ensure_worktree(dir.path(), Role::Builder, 42, None, None).unwrap();
        assert!(wt.ends_with("issue-42") && wt.is_dir());
    }

    /// Saves, clears and restores the env this module reads (serial tests).
    struct EnvGuard(Vec<(&'static str, Option<String>)>);
    impl EnvGuard {
        fn clear() -> Self {
            let keys = [
                "LOOM_RUNTIME",
                "LOOM_ROLE",
                "LOOM_MODEL_PROFILE",
                crate::launch_env::PREFERENCE_MARKER_ENV,
                "LOOM_RUNTIME_BUILDER",
                "LOOM_RUNTIME_DOCTOR",
                "LOOM_WORKTREE_ROOT",
            ];
            let prior = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
            for k in keys {
                std::env::remove_var(k);
            }
            Self(prior)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in self.0.drain(..) {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn write_exec(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// #11303 review (P1 at 3b70a80dc): a daemon-launched Claude orchestrator's
    /// inherited env (`LOOM_RUNTIME`+`LOOM_ROLE`, profile, marker) must not send
    /// Builder/Doctor back to Claude over `rolePreference`; a genuine
    /// `LOOM_RUNTIME_<ROLE>` or a bare operator `LOOM_RUNTIME` still wins.
    #[test]
    #[serial_test::serial]
    fn claude_orchestrator_env_does_not_capture_builder_or_doctor() {
        let _env = EnvGuard::clear();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let dir = tempfile::tempdir().unwrap();
        for kind in ["roles", "runtimes"] {
            let to = dir.path().join(".loom").join(kind);
            std::fs::create_dir_all(&to).unwrap();
            for entry in std::fs::read_dir(repo.join("defaults").join(kind))
                .unwrap()
                .flatten()
            {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("json") {
                    std::fs::copy(&p, to.join(p.file_name().unwrap())).unwrap();
                }
            }
        }
        for rt in ["claude", "opencode"] {
            write_exec(&dir.path().join(format!(".loom/scripts/spawn-{rt}.sh")), "#!/bin/sh\n");
        }
        let flash = serde_json::json!([{"runtime": "opencode", "modelProfile": "zai-flash"}]);
        let config = serde_json::json!({
            "runtimes": {"rolePreference": {"builder": flash, "doctor": flash}}
        });
        std::fs::write(dir.path().join(".loom/config.json"), config.to_string()).unwrap();
        let orchestrator = || {
            std::env::set_var("LOOM_RUNTIME", "claude");
            std::env::set_var("LOOM_ROLE", "sweep-lifecycle");
            std::env::set_var("LOOM_MODEL_PROFILE", "claude-parent");
            std::env::set_var(crate::launch_env::PREFERENCE_MARKER_ENV, "# parent");
        };
        for role in [Role::Builder, Role::Doctor] {
            orchestrator();
            let (admitted, _) = admit(dir.path(), role).unwrap();
            assert_eq!(admitted.runtime, "opencode", "{role:?}");
            let profile = admitted.preference.and_then(|p| p.model_profile);
            assert_eq!(profile.as_deref(), Some("zai-flash"), "{role:?}");
        }
        orchestrator();
        std::env::set_var("LOOM_RUNTIME_BUILDER", "claude");
        assert_eq!(admit(dir.path(), Role::Builder).unwrap().0.runtime, "claude");
        std::env::remove_var("LOOM_RUNTIME_BUILDER");
        std::env::remove_var("LOOM_ROLE");
        std::env::set_var("LOOM_RUNTIME", "claude");
        assert_eq!(admit(dir.path(), Role::Doctor).unwrap().0.runtime, "claude");
    }

    /// #11303 review (P1 at d7fa7f835): setup and reuse resolve the helpers'
    /// effective root, for an env and a config override, Builder and Doctor;
    /// a stacked Builder passes `--base`.
    #[test]
    #[serial_test::serial]
    fn ensure_worktree_honours_the_configured_root() {
        let _env = EnvGuard::clear();
        let ext = tempfile::tempdir().unwrap();
        for via_env in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let over = ext.path().join(if via_env { "env" } else { "cfg" });
            if via_env {
                std::env::set_var("LOOM_WORKTREE_ROOT", &over);
            } else {
                std::env::remove_var("LOOM_WORKTREE_ROOT");
                let cfg = serde_json::json!({"worktree": {"root": over}});
                std::fs::create_dir_all(root.join(".loom")).unwrap();
                std::fs::write(root.join(".loom/config.json"), cfg.to_string()).unwrap();
            }
            let eff = over.join(root.file_name().unwrap());
            for (script, prefix) in [("worktree.sh", "issue"), ("pr-worktree.sh", "pr")] {
                let body = format!(
                    "#!/bin/sh\necho \"{script} $*\" >> calls.txt\nmkdir -p '{}/{prefix}-'\"$1\"\n",
                    eff.display()
                );
                write_exec(&root.join(".loom/scripts").join(script), &body);
            }
            let b = ensure_worktree(root, Role::Builder, 42, None, Some("feature/issue-41"));
            assert_eq!(b.unwrap(), eff.join("issue-42"), "via_env={via_env}");
            let d = ensure_worktree(root, Role::Doctor, 50, Some(77), None).unwrap();
            assert_eq!(d, eff.join("pr-77"));
            // Reuse: existing worktrees under the external root, no new calls.
            assert_eq!(
                ensure_worktree(root, Role::Builder, 42, None, None).unwrap(),
                eff.join("issue-42")
            );
            assert_eq!(
                ensure_worktree(root, Role::Doctor, 42, Some(78), None).unwrap(),
                eff.join("issue-42")
            );
            ensure_worktree(root, Role::Doctor, 50, Some(77), None).unwrap();
            let calls = std::fs::read_to_string(root.join("calls.txt")).unwrap();
            assert_eq!(calls, "worktree.sh 42 --base feature/issue-41\npr-worktree.sh 77\n");
        }
    }

    /// #11303 review (P2 at d7fa7f835): a failed pre-launch read must not
    /// become an empty baseline that credits an unchanged pre-existing PR.
    #[test]
    fn an_unreadable_builder_baseline_refuses_the_launch() {
        let rr = serde_json::json!([{"name": "loom:review-requested"}]);
        let after = serde_json::json!([{"number": 7, "headRefOid": "a", "labels": rr}]);
        // The pre-fix fallback (`[]`) would have credited the unchanged PR.
        assert!(builder_artifact(&serde_json::json!([]), &after));
        let err = builder_baseline(7, None).unwrap_err();
        assert_eq!(err.code, 78);
        assert!(err.message.contains("refusing to launch"), "{}", err.message);
        let before = builder_baseline(7, Some(after.clone())).unwrap();
        assert!(!builder_artifact(&before, &after));
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
