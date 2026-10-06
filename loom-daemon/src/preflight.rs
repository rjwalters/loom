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
//! be committed): a count of failed runs scoped to one dispatch *episode*
//! (see [`episode_id`]), and a *receipt* recording the `HEAD` that last passed. `create-pr.sh` calls [`check`], which refuses a PR whose
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
    /// Dispatch episode the attempt budget belongs to; a different episode
    /// starts a fresh budget (a released-and-redispatched issue reuses the
    /// same worktree, so the worktree alone cannot scope the budget).
    #[serde(default)]
    episode: String,
    #[serde(default)]
    failed_attempts: u32,
    #[serde(default)]
    passed_head: Option<String>,
}

/// Identifier of the current Builder dispatch episode: the sweep id the daemon
/// exports to the session, else the session pid, else empty (a manual run —
/// one shared episode, so retries stay bounded).
#[must_use]
pub fn episode_id() -> String {
    ["LOOM_SWEEP_ID", "LOOM_AGENT_SESSION_PID"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_default()
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
    run_in_episode(worktree, &episode_id())
}

/// [`run`] with an explicit dispatch episode. Attempts are bounded within one
/// episode; entering a new one resets the budget.
#[must_use]
pub fn run_in_episode(worktree: &Path, episode: &str) -> Verdict {
    let Some(cfg) = crate::main_health_gate::read_build_gate_config(worktree) else {
        return Verdict::Disabled;
    };
    let max = max_attempts(worktree);
    let mut st = load(worktree);
    if st.episode != episode {
        st.episode = episode.to_string();
        st.failed_attempts = 0;
    }
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

/// Release the Builder's claim on `issue` after a terminal verdict, through
/// the same protected path the reaper uses: the stale `loom:building` claim is
/// always removed, `loom:issue` is re-added only if the issue is not parked
/// (`loom:blocked` / `loom:operator-only`), not a PR and not closed, and every
/// forge call is scoped to `worktree`'s repository (`LOOM_REPO` still wins).
///
/// # Errors
/// The forge mutation could not be run, timed out, or exited non-zero.
pub fn release_claim(worktree: &Path, issue: u32) -> anyhow::Result<()> {
    release_claim_with(
        crate::sweep_registry::SweepRegistryConfig::new(worktree.to_path_buf()),
        issue,
    )
}

fn release_claim_with(
    config: crate::sweep_registry::SweepRegistryConfig,
    issue: u32,
) -> anyhow::Result<()> {
    crate::sweep_registry::SweepRegistry::new(config).restore_label_to_ready(issue)
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
        assert_eq!(run_in_episode(d.path(), "e1"), Verdict::Disabled);
        assert_eq!(run_in_episode(d.path(), "e1").exit_code(), 0);
        assert!(check(d.path()));
    }

    #[test]
    fn disabled_block_is_noop() {
        let d = repo(Some(r#"{"enabled":false,"command":"false"}"#));
        assert_eq!(run_in_episode(d.path(), "e1"), Verdict::Disabled);
    }

    #[test]
    fn pass_writes_receipt() {
        let d = repo(Some(r#"{"enabled":true,"command":"true"}"#));
        assert!(!check(d.path()), "no receipt before a run");
        assert_eq!(run_in_episode(d.path(), "e1"), Verdict::Pass);
        assert!(check(d.path()));
    }

    #[test]
    fn failure_returns_output_tail_then_caps() {
        let d = repo(Some(
            r#"{"enabled":true,"command":"echo boom-marker; exit 3","preflightMaxAttempts":2}"#,
        ));
        match run_in_episode(d.path(), "e1") {
            Verdict::Failed {
                attempt: 1,
                max: 2,
                tail,
            } => assert!(tail.contains("boom-marker")),
            v => panic!("unexpected {v:?}"),
        }
        assert!(!check(d.path()));
        let v = run_in_episode(d.path(), "e1");
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
        assert!(matches!(run_in_episode(d.path(), "e1"), Verdict::Unresolved { .. }));
    }

    #[test]
    fn receipt_invalidated_by_new_commit() {
        let d = repo(Some(r#"{"enabled":true,"command":"true"}"#));
        assert_eq!(run_in_episode(d.path(), "e1"), Verdict::Pass);
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

    #[test]
    fn redispatch_into_same_worktree_gets_a_fresh_budget() {
        let d =
            repo(Some(r#"{"enabled":true,"command":"test -f fixed","preflightMaxAttempts":2}"#));
        assert!(matches!(run_in_episode(d.path(), "e1"), Verdict::Failed { .. }));
        assert!(matches!(run_in_episode(d.path(), "e1"), Verdict::Unresolved { .. }));
        // Same episode stays terminal even once the cause is fixed.
        std::fs::write(d.path().join("fixed"), "").unwrap();
        assert!(matches!(run_in_episode(d.path(), "e1"), Verdict::Unresolved { .. }));
        // A new dispatch into the same worktree starts a fresh budget.
        assert_eq!(run_in_episode(d.path(), "e2"), Verdict::Pass);
        assert!(check(d.path()));
    }

    mod release {
        use super::*;
        use crate::sweep_registry::SweepRegistryConfig;
        use std::os::unix::fs::PermissionsExt;

        /// Fake `gh`: logs `<cwd> <args>`; `labels` is the `loom:*` label
        /// answer for `issue view`, `state` the issue state, `edit_rc` the
        /// exit status of `issue edit`.
        fn fake_gh(dir: &Path, labels: &str, state: &str, edit_rc: i32) -> (PathBuf, PathBuf) {
            let gh = dir.join("fake-gh.sh");
            let log = dir.join("gh.log");
            let script = format!(
                r#"#!/usr/bin/env bash
printf '%s | %s\n' "$PWD" "$*" >> "{log}"
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then echo "{labels}"; exit 0; fi
if [ "$1" = "issue" ] && [ "$2" = "edit" ]; then echo "boom" >&2; exit {edit_rc}; fi
if [ "$1" = "api" ]; then
  case "$*" in
    *".pull_request != null"*) echo false ;;
    *".state"*) printf '{{"body":"","state":"{state}","closed_at":null,"labels":[]}}' ;;
    *) echo "" ;;
  esac
  exit 0
fi
exit 0
"#,
                log = log.display()
            );
            std::fs::write(&gh, script).unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            (gh, log)
        }

        fn release(wt: &Path, gh: &Path) -> anyhow::Result<()> {
            let mut cfg = SweepRegistryConfig::new(wt.to_path_buf());
            cfg.gh_bin = Some(gh.to_path_buf());
            release_claim_with(cfg, 77)
        }

        #[test]
        #[serial_test::serial]
        fn open_issue_is_requeued_in_the_worktree_repo() {
            let tools = tempfile::tempdir().unwrap();
            let wt = tempfile::tempdir().unwrap();
            let (gh, log) = fake_gh(tools.path(), "false", "open", 0);
            release(wt.path(), &gh).unwrap();
            let calls = std::fs::read_to_string(&log).unwrap();
            let wt_real = std::fs::canonicalize(wt.path()).unwrap();
            assert!(
                calls
                    .lines()
                    .any(|l| l.starts_with(&wt_real.display().to_string())
                        && l.contains(
                            "issue edit 77 --remove-label loom:building --add-label loom:issue"
                        )),
                "gh must run inside the requested worktree, not the caller cwd: {calls}"
            );
        }

        #[test]
        #[serial_test::serial]
        fn closed_issue_is_not_requeued() {
            let tools = tempfile::tempdir().unwrap();
            let wt = tempfile::tempdir().unwrap();
            let (gh, log) = fake_gh(tools.path(), "false", "closed", 0);
            release(wt.path(), &gh).unwrap();
            let calls = std::fs::read_to_string(&log).unwrap();
            assert!(calls.contains("--remove-label loom:building"), "{calls}");
            assert!(!calls.contains("--add-label loom:issue"), "{calls}");
        }

        #[test]
        #[serial_test::serial]
        fn parked_issue_is_not_requeued() {
            let tools = tempfile::tempdir().unwrap();
            let wt = tempfile::tempdir().unwrap();
            let (gh, log) = fake_gh(tools.path(), "true", "open", 0);
            release(wt.path(), &gh).unwrap();
            let calls = std::fs::read_to_string(&log).unwrap();
            assert!(calls.contains("--remove-label loom:building"), "{calls}");
            assert!(!calls.contains("--add-label loom:issue"), "{calls}");
        }

        #[test]
        #[serial_test::serial]
        fn forge_failure_is_reported() {
            let tools = tempfile::tempdir().unwrap();
            let wt = tempfile::tempdir().unwrap();
            let (gh, _log) = fake_gh(tools.path(), "false", "open", 1);
            let err = release(wt.path(), &gh).unwrap_err().to_string();
            assert!(err.contains("failed") && err.contains("boom"), "{err}");
        }
    }
}
