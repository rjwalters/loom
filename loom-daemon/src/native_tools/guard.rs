//! Reuse the established normalization/policy bridge, without a second regex table.
use super::{field, Request};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

/// Env override for the policy-check budget (issue #8451). Outranks the
/// `guards.nativePolicyTimeoutSecs` config key, matching the repo's documented
/// env > config > default precedence.
pub const TIMEOUT_ENV: &str = "LOOM_NATIVE_POLICY_TIMEOUT_SECS";
/// Dotted config key read from the effective (tiered) config when the env
/// override is absent.
const TIMEOUT_CONFIG_KEY: &str = "guards.nativePolicyTimeoutSecs";
/// The original fixed budget, chosen on an idle host (see
/// `guardrail-parity-native.md`); unchanged as the default.
pub const DEFAULT_TIMEOUT_SECS: u64 = 20;
/// Floor: a saturated host still must fail closed inside a bounded time, so
/// this cannot be configured down to (near-)zero and turned into a permanent
/// refusal.
const MIN_TIMEOUT_SECS: u64 = 5;
/// Ceiling: this budget also bounds how long a model waits per tool call, not
/// just how patient the guard is — unbounded would let #8451's starvation
/// risk recur from the other direction, one slow tool call at a time.
const MAX_TIMEOUT_SECS: u64 = 120;

/// Resolve the policy-check budget: `LOOM_NATIVE_POLICY_TIMEOUT_SECS`, else
/// `guards.nativePolicyTimeoutSecs` from the effective config, else
/// [`DEFAULT_TIMEOUT_SECS`] — always clamped to
/// `[MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS]`, so a stray/misconfigured value
/// still fails closed inside a bounded time rather than hanging indefinitely
/// or refusing everything instantly.
#[must_use]
pub fn timeout_secs(root: &Path) -> u64 {
    let from_env = std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok());
    let resolved = from_env.or_else(|| {
        let config = crate::config_resolver::resolve_effective_config(root);
        crate::config_resolver::get_path(&config, TIMEOUT_CONFIG_KEY).and_then(Value::as_u64)
    });
    resolved
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)
}

pub fn directory(root: &Path) -> PathBuf {
    std::env::var_os("LOOM_NATIVE_GUARD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let installed = root.join(".loom/hooks");
            if installed.is_dir() {
                installed
            } else {
                root.join("defaults/hooks")
            }
        })
}
pub fn ready(root: &Path) -> Result<()> {
    let dir = directory(root);
    for name in [
        "guard-codex-bridge.sh",
        "guard-destructive.sh",
        "guard-destructive-generic.sh",
        "guard-worktree-paths.sh",
        "guard-loom-workflow.sh",
    ] {
        if !dir.join(name).is_file() {
            bail!("policy error: native tool guard is missing: {}", dir.join(name).display());
        }
    }
    Ok(())
}

/// Best-effort per-worker policy-timeout counter (#8451 acceptance criterion
/// 4). Appended under `.loom/native-tools/`, one JSON line per timeout, keyed
/// by `LOOM_NATIVE_WORKER_PID` — the native harness's own pid, stable for the
/// sweep's lifetime and already the session-liveness identity documented in
/// `native-sweep.md`. Never fails the calling tool: a host too saturated to
/// append one small file has bigger problems than a missed telemetry line.
fn record_policy_timeout(root: &Path, budget_secs: u64) {
    let worker_pid =
        std::env::var("LOOM_NATIVE_WORKER_PID").unwrap_or_else(|_| "unknown".to_string());
    let dir = root.join(".loom/native-tools");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("policy-timeouts.jsonl"))
    else {
        return;
    };
    let line = json!({
        "worker_pid": worker_pid,
        "budget_secs": budget_secs,
        "at": chrono::Utc::now().to_rfc3339(),
    });
    let _ = writeln!(file, "{line}");
}

/// Count recorded policy timeouts for `worker_pid` (test/telemetry helper for
/// [`record_policy_timeout`]). Missing file reads as zero, matching that
/// function's best-effort contract.
#[must_use]
pub fn policy_timeout_count(root: &Path, worker_pid: &str) -> usize {
    let path = root.join(".loom/native-tools/policy-timeouts.jsonl");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return 0;
    };
    contents
        .lines()
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|value| {
                    value
                        .get("worker_pid")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .is_some_and(|pid| pid == worker_pid)
        })
        .count()
}

pub fn check(root: &Path, cwd: &Path, request: &Request) -> Result<()> {
    let (tool, input) = match request.tool.as_str() {
        "read" => {
            field(&request.input, "path")?;
            ("read_file", json!({}))
        }
        "write" | "edit" => ("write_file", json!({"path":field(&request.input,"path")?})),
        "bash" => ("shell_command", json!({"command":field(&request.input,"command")?})),
        _ => bail!("unsupported native tool; delegation and unverified tools are disabled"),
    };
    ready(root)?;
    let mut file = tempfile::tempfile()?;
    serde_json::to_writer(
        &mut file,
        &json!({"hook_event_name":"PreToolUse","tool_name":tool,"tool_input":input,"cwd":cwd}),
    )?;
    file.flush()?;
    file.seek(SeekFrom::Start(0))?;
    let mut command = Command::new("bash");
    command
        .arg(directory(root).join("guard-codex-bridge.sh"))
        .arg("--project-root")
        .arg(root)
        .current_dir(cwd)
        .stdin(file)
        .env("LOOM_CODEX_BRIDGE_GUARD_DIR", directory(root));
    let budget_secs = timeout_secs(root);
    let completion = crate::proc_exec::run_bounded_cancellable(
        command,
        Duration::from_secs(budget_secs),
        super::cancellation::requested,
    )
    .context("policy error: could not run the native policy check")?;
    // A timeout and a denial must never collapse into the same string: a
    // model that is told "Guard denials are failures, not approval prompts"
    // (native-sweep.md) would otherwise over-apply that rule to a check that
    // was never evaluated at all (#8451). Every branch below is prefixed with
    // its outcome class for exactly that reason.
    let output = match completion {
        crate::proc_exec::Completion::TimedOut { .. } => {
            record_policy_timeout(root, budget_secs);
            bail!(
                "policy timeout: the native policy check did not finish within {budget_secs}s \
                 (the host is likely CPU-saturated, not a denial). The command was NOT \
                 evaluated — this is transient, not a refusal: retrying the same command is \
                 reasonable. Configure a larger budget with guards.nativePolicyTimeoutSecs or \
                 {TIMEOUT_ENV} if this recurs."
            );
        }
        crate::proc_exec::Completion::Exited(output) => output,
    };
    if !output.status.success() {
        bail!("policy error: native policy check failed; refusing tool");
    }
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let result: Value = serde_json::from_slice(&output.stdout)
        .context("policy error: native policy check returned malformed output")?;
    // The shared bridge only permits empty success or an explicit deny. An
    // unexpected response must not create a new allow path.
    if result
        .pointer("/hookSpecificOutput/permissionDecision")
        .and_then(Value::as_str)
        == Some("deny")
    {
        bail!(
            "policy denied: {}",
            result
                .pointer("/hookSpecificOutput/permissionDecisionReason")
                .and_then(Value::as_str)
                .unwrap_or("denied by Loom policy")
        );
    }
    bail!("policy error: native policy check returned an unknown decision; refusing tool")
}
