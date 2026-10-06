//! In-session, bounded pre-PR gate (Issue #10476).
//!
//! `buildGate` already runs orchestrator-side *after* the Builder exits. That
//! catches a broken PR but throws the failing output away: a fresh Builder
//! restarts from scratch. This module is the same `buildGate.command`, run
//! **inside the Builder's session, before the PR is opened**, so the Builder
//! that wrote the change gets the failure tail back and can fix it.
//!
//! One definition of "the gate": the command, timeout and opt-in semantics come
//! from [`crate::main_health_gate::read_build_gate_config`]; nothing here
//! re-implements a check.
//!
//! State lives in the worktree's git dir (never in the work tree, so it cannot
//! be committed): a count of failed runs, and a *receipt* recording the `HEAD`
//! that last passed. `create-pr.sh` calls [`check`], which refuses a PR whose
//! `HEAD` has no receipt.
//!
//! Opt-in: no (enabled) `buildGate` block means every entry point is a no-op
//! that exits 0.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Default `buildGate.preflightMaxAttempts`.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Bytes of failing output handed back to the Builder.
const TAIL_BYTES: usize = 6000;
const STATE_FILE: &str = "loom-preflight.json";

/// Exit code: gate failed, attempts remain — fix and re-run.
pub const EXIT_FAILED: i32 = 1;
/// Exit code: attempts exhausted — terminal `preflight_unresolved`.
pub const EXIT_UNRESOLVED: i32 = 4;
/// Exit code from `check`: no passing receipt for the current `HEAD`.
pub const EXIT_NOT_PASSED: i32 = 7;

/// Result of one pre-flight run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// No enabled `buildGate` command: nothing to do.
    Disabled,
    /// The command exited 0; a receipt was written.
    Pass,
    /// The command failed and `attempt < max`: fix and re-run.
    Failed {
        attempt: u32,
        max: u32,
        tail: String,
    },
    /// The command failed and attempts are used up: stop, fail closed.
    Unresolved {
        attempts: u32,
        max: u32,
        tail: String,
    },
}

impl Verdict {
    /// Process exit code for this verdict.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Disabled | Self::Pass => 0,
            Self::Failed { .. } => EXIT_FAILED,
            Self::Unresolved { .. } => EXIT_UNRESOLVED,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    failed_attempts: u32,
    #[serde(default)]
    passed_head: Option<String>,
}

fn git_out(worktree: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(worktree)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn state_path(worktree: &Path) -> PathBuf {
    git_out(worktree, &["rev-parse", "--absolute-git-dir"]).map_or_else(
        || worktree.join(format!(".loom-{STATE_FILE}")),
        |d| PathBuf::from(d).join(STATE_FILE),
    )
}

fn load(worktree: &Path) -> State {
    std::fs::read_to_string(state_path(worktree))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(worktree: &Path, st: &State) {
    if let Ok(s) = serde_json::to_string(st) {
        let _ = std::fs::write(state_path(worktree), s);
    }
}

/// `buildGate.preflightMaxAttempts` (positive integer), default 3.
#[must_use]
pub fn max_attempts(worktree: &Path) -> u32 {
    let eff = crate::config_resolver::resolve_effective_config(worktree);
    crate::config_resolver::get_path(&eff, "buildGate")
        .and_then(|g| g.get("preflightMaxAttempts"))
        .and_then(serde_json::Value::as_u64)
        .filter(|&n| n > 0)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

/// Run `command` via `sh -c`; `Ok(())` on exit 0, else `Err(tail)`.
fn run_command(command: &str, cwd: &Path, timeout: Duration) -> Result<(), String> {
    let log = std::env::temp_dir().join(format!("loom-preflight-{}.log", uuid::Uuid::new_v4()));
    let out = std::fs::File::create(&log).map_err(|e| format!("cannot create output file: {e}"))?;
    let err = out
        .try_clone()
        .map_err(|e| format!("cannot clone output file: {e}"))?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .map_err(|e| format!("failed to spawn '{command}': {e}"))?;
    let start = Instant::now();
    let note = loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => {
                let _ = std::fs::remove_file(&log);
                return Ok(());
            }
            Ok(Some(s)) => break format!("command exited with {s}"),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break format!("command timed out after {}s and was killed", timeout.as_secs());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => break format!("failed to poll command: {e}"),
        }
    };
    let bytes = std::fs::read(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    let tail =
        String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(TAIL_BYTES)..]).into_owned();
    Err(format!("{note}\n{}", tail.trim()))
}

/// Run the pre-flight gate in `worktree`, recording the outcome.
#[must_use]
pub fn run(worktree: &Path) -> Verdict {
    let Some(cfg) = crate::main_health_gate::read_build_gate_config(worktree) else {
        return Verdict::Disabled;
    };
    let max = max_attempts(worktree);
    let mut st = load(worktree);
    if st.failed_attempts >= max {
        // Already terminal: a further run must not reopen the loop.
        return Verdict::Unresolved {
            attempts: st.failed_attempts,
            max,
            tail: String::new(),
        };
    }
    match run_command(&cfg.command, worktree, cfg.timeout) {
        Ok(()) => {
            st.failed_attempts = 0;
            st.passed_head = git_out(worktree, &["rev-parse", "HEAD"]);
            save(worktree, &st);
            Verdict::Pass
        }
        Err(tail) => {
            st.failed_attempts += 1;
            st.passed_head = None;
            save(worktree, &st);
            if st.failed_attempts >= max {
                Verdict::Unresolved {
                    attempts: st.failed_attempts,
                    max,
                    tail,
                }
            } else {
                Verdict::Failed {
                    attempt: st.failed_attempts,
                    max,
                    tail,
                }
            }
        }
    }
}

/// Enforcement probe for `create-pr.sh`: `true` when the gate is disabled or
/// the current `HEAD` has a passing receipt.
#[must_use]
pub fn check(worktree: &Path) -> bool {
    if crate::main_health_gate::read_build_gate_config(worktree).is_none() {
        return true;
    }
    let head = git_out(worktree, &["rev-parse", "HEAD"]);
    head.is_some() && load(worktree).passed_head == head
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(gate: Option<&str>) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        let git = |a: &[&str]| {
            assert!(Command::new("git")
                .args(a)
                .current_dir(p)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "i",
        ]);
        if let Some(g) = gate {
            std::fs::create_dir_all(p.join(".loom")).unwrap();
            std::fs::write(p.join(".loom/config.json"), format!(r#"{{"buildGate":{g}}}"#)).unwrap();
        }
        d
    }

    #[test]
    fn absent_block_is_noop() {
        let d = repo(None);
        assert_eq!(run(d.path()), Verdict::Disabled);
        assert_eq!(run(d.path()).exit_code(), 0);
        assert!(check(d.path()));
    }

    #[test]
    fn disabled_block_is_noop() {
        let d = repo(Some(r#"{"enabled":false,"command":"false"}"#));
        assert_eq!(run(d.path()), Verdict::Disabled);
    }

    #[test]
    fn pass_writes_receipt() {
        let d = repo(Some(r#"{"enabled":true,"command":"true"}"#));
        assert!(!check(d.path()), "no receipt before a run");
        assert_eq!(run(d.path()), Verdict::Pass);
        assert!(check(d.path()));
    }

    #[test]
    fn failure_returns_output_tail_then_caps() {
        let d = repo(Some(
            r#"{"enabled":true,"command":"echo boom-marker; exit 3","preflightMaxAttempts":2}"#,
        ));
        match run(d.path()) {
            Verdict::Failed {
                attempt: 1,
                max: 2,
                tail,
            } => assert!(tail.contains("boom-marker")),
            v => panic!("unexpected {v:?}"),
        }
        assert!(!check(d.path()));
        let v = run(d.path());
        assert!(
            matches!(
                v,
                Verdict::Unresolved {
                    attempts: 2,
                    max: 2,
                    ..
                }
            ),
            "{v:?}"
        );
        assert_eq!(v.exit_code(), EXIT_UNRESOLVED);
        // Terminal: stays unresolved without running the command again.
        assert!(matches!(run(d.path()), Verdict::Unresolved { .. }));
    }

    #[test]
    fn receipt_invalidated_by_new_commit() {
        let d = repo(Some(r#"{"enabled":true,"command":"true"}"#));
        assert_eq!(run(d.path()), Verdict::Pass);
        let ok = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ])
            .current_dir(d.path())
            .status()
            .unwrap();
        assert!(ok.success());
        assert!(!check(d.path()));
    }
}
