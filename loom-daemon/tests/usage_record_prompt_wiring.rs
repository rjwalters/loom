//! Extract-and-execute wiring test for the in-session `usage-record` step
//! (Issue #9303).
//!
//! The sweep prompt's "Usage record" fence is the call site that makes an
//! in-session sweep record per-attempt usage. Rather than pinning its text,
//! this pulls the `loom-daemon usage-record` line out of the fence, fills the
//! placeholders, and RUNS it: against the real binary (the flags must parse and
//! the command must exit 0 even with tracing off and no transcript), and
//! against a failing stand-in daemon (an older daemon without the subcommand)
//! to prove the step is fail-open and can never fail the sweep.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn prompt() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../defaults/.claude/commands/loom/sweep-execution-model.md");
    std::fs::read_to_string(path).expect("sweep-execution-model.md")
}

/// The `loom-daemon usage-record …` line of the first fence after the
/// "Usage record" lead-in.
fn usage_record_line(content: &str) -> String {
    let from = content
        .find("**Usage record")
        .expect("the sweep prompt documents the usage-record step");
    let rest = &content[from..];
    let open = rest.find("```").expect("a fenced command follows");
    let body = &rest[open + 3..];
    let body = &body[body.find('\n').unwrap() + 1..];
    let body = &body[..body.find("```").expect("the fence closes")];
    body.lines()
        .find(|l| l.contains("loom-daemon usage-record"))
        .expect("the fence runs loom-daemon usage-record")
        .to_string()
}

fn filled(line: &str) -> String {
    line.replace("--issue N", "--issue 9303")
        .replace("<role>", "builder")
        .replace("<k>", "1")
        .replace("<agent-id>", "abc123")
        .replace("\"$RUN_ID\"", "sweep-test-run")
}

/// Run `line` under bash with `bin_dir` first on PATH, in an empty scratch
/// workspace with its own (empty) Claude config dir.
fn run(line: &str, bin_dir: &Path, workspace: &Path) -> std::process::Output {
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());
    Command::new("bash")
        .args(["-c", line])
        .current_dir(workspace)
        .env("PATH", path)
        .env("CLAUDE_CONFIG_DIR", workspace.join(".claude"))
        .env_remove("LOOM_TRACE_CONTEXT_FILE")
        .env_remove("LOOM_TRACEPARENT")
        .env_remove("LOOM_WORKSPACE")
        .output()
        .expect("bash runs")
}

fn bin_dir_with(tmp: &Path, name: &str, target: Option<&Path>) -> PathBuf {
    let dir = tmp.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("loom-daemon");
    match target {
        Some(real) => std::os::unix::fs::symlink(real, &exe).unwrap(),
        None => {
            // An older daemon: unknown subcommand, clap's exit 2.
            std::fs::write(&exe, "#!/usr/bin/env bash\necho unrecognized >&2\nexit 2\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    dir
}

#[test]
fn the_prompts_usage_record_step_runs_against_the_real_binary_and_exits_zero() {
    let line = filled(&usage_record_line(&prompt()));
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let bin = bin_dir_with(tmp.path(), "real", Some(Path::new(env!("CARGO_BIN_EXE_loom-daemon"))));
    let out = run(&line, &bin, &workspace);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "`{line}` failed: {stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let summary: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON line");
    assert_eq!(summary["recorded"], 0, "tracing is off here: {summary}");
}

#[test]
fn the_prompts_usage_record_step_is_fail_open_against_an_older_daemon() {
    let line = filled(&usage_record_line(&prompt()));
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let bin = bin_dir_with(tmp.path(), "old", None);
    let out = run(&line, &bin, &workspace);
    assert!(out.status.success(), "a recorder failure must not fail the sweep step");
}

/// The step lives in the Execution Model's experiment section, which an
/// orchestrator running with the experiment `off` (the default) may skip. Each
/// in-session lifecycle file that writes checkpoints must point at it, or the
/// main path records nothing.
#[test]
fn every_checkpoint_writing_lifecycle_points_at_the_usage_record_step() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/.claude/commands/loom");
    for name in ["sweep-wave-lifecycle.md", "sweep-mode-c-lifecycle.md"] {
        let text = std::fs::read_to_string(dir.join(name)).expect(name);
        assert!(
            text.contains("sweep-checkpoint.sh write"),
            "{name} no longer writes checkpoints"
        );
        assert!(
            text.contains("\"Usage record\""),
            "{name} writes checkpoints but no longer points at the Execution Model \"Usage record\" step (#9303)"
        );
    }
}
