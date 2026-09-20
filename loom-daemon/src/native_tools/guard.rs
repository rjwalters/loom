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
            bail!("native tool guard is missing: {}", dir.join(name).display());
        }
    }
    Ok(())
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
    let output = crate::proc_exec::run_bounded_cancellable(
        command,
        Duration::from_secs(20),
        super::cancellation::requested,
    )?
    .output()
    .context("native policy check timed out; refusing tool")?;
    if !output.status.success() {
        bail!("native policy check failed; refusing tool");
    }
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let result: Value = serde_json::from_slice(&output.stdout)
        .context("native policy check returned malformed output")?;
    // The shared bridge only permits empty success or an explicit deny. An
    // unexpected response must not create a new allow path.
    if result
        .pointer("/hookSpecificOutput/permissionDecision")
        .and_then(Value::as_str)
        == Some("deny")
    {
        bail!(
            "{}",
            result
                .pointer("/hookSpecificOutput/permissionDecisionReason")
                .and_then(Value::as_str)
                .unwrap_or("denied by Loom policy")
        );
    }
    bail!("native policy check returned an unknown decision; refusing tool")
}
