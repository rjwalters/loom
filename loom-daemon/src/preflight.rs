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
//!
//! Path-scoped suites (#10860): `buildGate.preflightPathScopes` lists suites
//! that pre-flight drops (by exporting their `env` empty to the gate command)
//! when the diff against `origin/main` touches none of their `runWhenChanged`
//! globs. CI still runs them. Anything uncertain runs the full gate.
//!
//! A run that hits `timeoutSeconds` is [`Verdict::TimedOut`], not a check
//! failure: it names the `[build-gate]` stage that was running, never counts
//! toward the failure cap and never releases the claim.

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
/// Exit code: the gate ran out of `timeoutSeconds` (#10860) — not a check
/// failure, the claim is kept.
pub const EXIT_TIMED_OUT: i32 = 5;
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
    /// The command was killed at `timeoutSeconds` (#10860). `attempt` counts
    /// this episode's timeouts, separately from failures; at `attempt >= max`
    /// the Builder opens no PR, but the claim is still kept. `stage` is the
    /// `[build-gate]` stage that was running, when the log names one.
    TimedOut {
        attempt: u32,
        max: u32,
        stage: Option<String>,
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
            Self::TimedOut { .. } => EXIT_TIMED_OUT,
        }
    }

    /// Whether this verdict hands the issue back. Only a terminal check
    /// failure does; a timeout is load, not an abandoned claim (#10860).
    #[must_use]
    pub fn releases_claim(&self) -> bool {
        matches!(self, Self::Unresolved { .. })
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
    /// Timed-out runs this episode (#10860); never feeds `failed_attempts`.
    #[serde(default)]
    timed_out_attempts: u32,
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

/// SIGKILL the gate's whole process group (it leads its own, see
/// [`run_command`]) and reap the shell, so nothing from this attempt outlives
/// it to overlap a retry or a re-dispatched Builder.
fn kill_gate_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pgid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: plain syscall; negative pid targets the group we created.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// One `buildGate.preflightPathScopes` entry (#10860): a gate suite that
/// pre-flight may drop when the diff touches none of `run_when_changed`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathScope {
    /// Variable exported empty to the gate command to drop the suite.
    env: String,
    /// Label for the skip line.
    suite: String,
    run_when_changed: Vec<String>,
}

/// `buildGate.preflightPathScopes`. Malformed entries are dropped (the
/// soft-fail contract of `read_build_gate_config`): an entry with no usable
/// `env` or no globs can never be skipped, so dropping it is fail-safe.
fn path_scopes(worktree: &Path) -> Vec<PathScope> {
    let eff = crate::config_resolver::resolve_effective_config(worktree);
    let Some(scopes) = crate::config_resolver::get_path(&eff, "buildGate")
        .and_then(|g| g.get("preflightPathScopes"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    scopes
        .iter()
        .filter_map(|s| {
            let env = s.get("env")?.as_str()?.trim();
            if env.is_empty() || !env.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return None;
            }
            let run_when_changed: Vec<String> = s
                .get("runWhenChanged")?
                .as_array()?
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect();
            if run_when_changed.is_empty() {
                return None;
            }
            let suite = s
                .get("suite")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(env);
            Some(PathScope {
                env: env.to_string(),
                suite: suite.to_string(),
                run_when_changed,
            })
        })
        .collect()
}

/// Paths this worktree changes against `origin/main`: committed since the
/// merge-base, tracked but uncommitted, and untracked (not ignored). `None`
/// when any part cannot be computed or the set is empty.
fn changed_paths(worktree: &Path) -> Option<Vec<String>> {
    let base =
        git_out(worktree, &["merge-base", "HEAD", "origin/main"]).filter(|b| !b.is_empty())?;
    let mut paths = Vec::new();
    for args in [
        // `--no-renames`: a rename lists both its source and destination,
        // so moving a file out of a scope's inputs still touches the scope.
        &[
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            base.as_str(),
            "HEAD",
        ][..],
        &["diff", "--no-renames", "--name-only", "-z", "HEAD"],
        &["ls-files", "--others", "--exclude-standard", "-z"],
    ] {
        let out = git_out(worktree, args)?;
        paths.extend(
            out.split('\0')
                .filter(|p| !p.is_empty())
                .map(str::to_string),
        );
    }
    paths.sort();
    paths.dedup();
    (!paths.is_empty()).then_some(paths)
}

/// The variable `build-gate.sh` reads to drop its installer suite.
const INSTALLER_SUITE_ENV: &str = "LOOM_BUILD_GATE_INSTALLER_SUITE";

/// Environment for the gate command from the path scopes: `Some("")` drops a
/// suite whose inputs the diff does not touch (announced on stderr); `None`
/// removes the variable so the suite runs. Every scope runs when the changed
/// set is unknown or empty (fail safe). `INSTALLER_SUITE_ENV` is always
/// present, so a value inherited from the caller can never skip silently.
fn scope_env(worktree: &Path) -> Vec<(String, Option<String>)> {
    let mut env = scoped_env(worktree);
    if !env.iter().any(|(k, _)| k == INSTALLER_SUITE_ENV) {
        env.push((INSTALLER_SUITE_ENV.to_string(), None));
    }
    env
}

fn scoped_env(worktree: &Path) -> Vec<(String, Option<String>)> {
    let scopes = path_scopes(worktree);
    if scopes.is_empty() {
        return Vec::new();
    }
    let changed = changed_paths(worktree);
    if changed.is_none() {
        eprintln!("preflight: no diff against origin/main could be computed — running every suite");
    }
    scopes
        .into_iter()
        .map(|s| {
            let touched = changed.as_ref().is_none_or(|c| {
                c.iter().any(|p| {
                    s.run_when_changed
                        .iter()
                        .any(|g| crate::main_health_gate::glob_matches(g, p))
                })
            });
            if touched {
                return (s.env, None);
            }
            eprintln!(
                "preflight: skipping {} — diff touches none of its inputs (CI still runs it)",
                s.suite
            );
            (s.env, Some(String::new()))
        })
        .collect()
}

/// The last `[build-gate] <stage>` marker in the full gate log — the stage
/// that was running when the budget ran out (#10860). `WARNING`/`note:` lines
/// are not stages. `None` for a gate that prints no markers.
fn running_stage(log: &str) -> Option<String> {
    log.lines().rev().find_map(|l| {
        let s = l.strip_prefix("[build-gate] ")?.trim();
        (!s.is_empty() && !s.starts_with("WARNING") && !s.starts_with("note:"))
            .then(|| s.to_string())
    })
}

/// Why a gate run did not pass.
#[derive(Debug)]
enum RunError {
    /// Non-zero exit, or the command could not be run: `note` + output tail.
    Failed(String),
    /// Killed at the budget (#10860).
    TimedOut { stage: Option<String>, tail: String },
}

/// Run `command` via `sh -c` with `env` applied (`None` removes a variable).
fn run_command(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    env: &[(String, Option<String>)],
) -> Result<(), RunError> {
    let log = std::env::temp_dir().join(format!("loom-preflight-{}.log", uuid::Uuid::new_v4()));
    let out = std::fs::File::create(&log)
        .map_err(|e| RunError::Failed(format!("cannot create output file: {e}")))?;
    let err = out
        .try_clone()
        .map_err(|e| RunError::Failed(format!("cannot clone output file: {e}")))?;
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    for (k, v) in env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    // Own process group, so a timeout can kill the whole gate subtree (the
    // shell's compiler/test descendants), not just `sh`.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| RunError::Failed(format!("failed to spawn '{command}': {e}")))?;
    let start = Instant::now();
    let mut timed_out = false;
    let note = loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => {
                let _ = std::fs::remove_file(&log);
                return Ok(());
            }
            Ok(Some(s)) => break format!("command exited with {s}"),
            Ok(None) if start.elapsed() >= timeout => {
                kill_gate_tree(&mut child);
                timed_out = true;
                break format!("command timed out after {}s and was killed", timeout.as_secs());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => {
                kill_gate_tree(&mut child);
                break format!("failed to poll command: {e}");
            }
        }
    };
    let bytes = std::fs::read(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    let tail =
        String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(TAIL_BYTES)..]).into_owned();
    if !timed_out {
        return Err(RunError::Failed(format!("{note}\n{}", tail.trim())));
    }
    // The stage header has usually scrolled out of the tail: read the full log.
    let stage = running_stage(&String::from_utf8_lossy(&bytes));
    let note = match &stage {
        Some(st) => format!("{note} — running stage: {st}"),
        None => note,
    };
    Err(RunError::TimedOut {
        stage,
        tail: format!("{note}\n{}", tail.trim()),
    })
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
        st.timed_out_attempts = 0;
    }
    if st.failed_attempts >= max {
        // Already terminal: a further run must not reopen the loop.
        return Verdict::Unresolved {
            attempts: st.failed_attempts,
            max,
            tail: String::new(),
        };
    }
    let env = scope_env(worktree);
    match run_command(&cfg.command, worktree, cfg.timeout, &env) {
        Ok(()) => {
            st.failed_attempts = 0;
            st.timed_out_attempts = 0;
            st.passed_head = git_out(worktree, &["rev-parse", "HEAD"]);
            save(worktree, &st);
            Verdict::Pass
        }
        Err(RunError::TimedOut { stage, tail }) => {
            st.timed_out_attempts += 1;
            st.passed_head = None;
            save(worktree, &st);
            Verdict::TimedOut {
                attempt: st.timed_out_attempts,
                max,
                stage,
                tail,
            }
        }
        Err(RunError::Failed(tail)) => {
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

/// Release the claim on `issue` if `verdict` calls for it
/// ([`Verdict::releases_claim`]); `None` when the claim is kept.
#[must_use]
pub fn settle_claim(worktree: &Path, issue: u32, verdict: &Verdict) -> Option<anyhow::Result<()>> {
    settle_claim_with(
        crate::sweep_registry::SweepRegistryConfig::new(worktree.to_path_buf()),
        issue,
        verdict,
    )
}

fn settle_claim_with(
    config: crate::sweep_registry::SweepRegistryConfig,
    issue: u32,
    verdict: &Verdict,
) -> Option<anyhow::Result<()>> {
    verdict
        .releases_claim()
        .then(|| release_claim_with(config, issue))
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

    #[test]
    fn timeout_kills_nested_descendants() {
        let d = repo(Some(
            r#"{"enabled":true,"command":"sh -c 'sleep 2; echo survived > marker' & wait","timeoutSeconds":1,"preflightMaxAttempts":1}"#,
        ));
        let t = Instant::now();
        assert!(matches!(run_in_episode(d.path(), "e1"), Verdict::TimedOut { .. }));
        assert!(t.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!d.path().join("marker").exists(), "descendant outlived the timeout");
    }

    fn git_in(p: &Path, a: &[&str]) {
        let st = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(a)
            .current_dir(p)
            .status()
            .unwrap();
        assert!(st.success(), "git {a:?}");
    }

    /// Gate passes iff the scope variable is exported empty (the suite was
    /// skipped); scoped to `defaults/*`. Config is committed and
    /// `origin/main` points at that commit, so the diff starts empty.
    fn scoped_repo() -> tempfile::TempDir {
        let d = repo(Some(
            r#"{"enabled":true,"command":"test -z \"${LOOM_BUILD_GATE_INSTALLER_SUITE-x}\"","preflightMaxAttempts":9,"preflightPathScopes":[{"env":"LOOM_BUILD_GATE_INSTALLER_SUITE","suite":"test-installer","runWhenChanged":["defaults/*"]}]}"#,
        ));
        git_in(d.path(), &["add", "-A"]);
        git_in(d.path(), &["commit", "-q", "-m", "cfg"]);
        git_in(d.path(), &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        d
    }

    fn commit_file(p: &Path, rel: &str) {
        let f = p.join(rel);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "x").unwrap();
        git_in(p, &["add", rel]);
        git_in(p, &["commit", "-q", "-m", rel]);
    }

    #[test]
    fn scope_skipped_when_diff_misses_its_inputs() {
        let d = scoped_repo();
        commit_file(d.path(), "loom-daemon/src/foo.rs");
        assert_eq!(run_in_episode(d.path(), "e1"), Verdict::Pass);
    }

    #[test]
    fn scope_runs_when_diff_touches_its_inputs() {
        let d = scoped_repo();
        commit_file(d.path(), "loom-daemon/src/foo.rs");
        commit_file(d.path(), "defaults/x");
        assert!(matches!(run_in_episode(d.path(), "e1"), Verdict::Failed { .. }));
    }

    #[test]
    fn rename_out_of_inputs_counts_as_changed() {
        let d = scoped_repo();
        commit_file(d.path(), "defaults/docs/x.md");
        git_in(d.path(), &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git_in(d.path(), &["mv", "defaults/docs/x.md", "x.md"]);
        // Staged rename (the `diff HEAD` leg) …
        assert!(matches!(run_in_episode(d.path(), "e1"), Verdict::Failed { .. }));
        // … and committed (the merge-base leg).
        git_in(d.path(), &["commit", "-q", "-m", "mv"]);
        assert!(matches!(run_in_episode(d.path(), "e2"), Verdict::Failed { .. }));
    }

    #[test]
    fn installer_suite_env_is_always_set_explicitly() {
        let unset = (INSTALLER_SUITE_ENV.to_string(), None);
        // No scopes configured, and only malformed scopes: removed, never inherited.
        let d = repo(Some(r#"{"enabled":true,"command":"true"}"#));
        assert_eq!(scope_env(d.path()), vec![unset.clone()]);
        let d = repo(Some(
            r#"{"enabled":true,"command":"true","preflightPathScopes":[{"env":"LOOM_BUILD_GATE_INSTALLER_SUITE"}]}"#,
        ));
        assert_eq!(scope_env(d.path()), vec![unset.clone()]);
        // A configured scope that runs: removed exactly once.
        let d = scoped_repo();
        commit_file(d.path(), "defaults/x");
        assert_eq!(scope_env(d.path()), vec![unset]);
    }

    #[test]
    fn untracked_input_counts_as_changed() {
        let d = scoped_repo();
        commit_file(d.path(), "loom-daemon/src/foo.rs");
        std::fs::create_dir_all(d.path().join("defaults")).unwrap();
        std::fs::write(d.path().join("defaults/new"), "x").unwrap();
        assert!(matches!(run_in_episode(d.path(), "e1"), Verdict::Failed { .. }));
    }

    #[test]
    fn uncommitted_input_counts_as_changed() {
        let d = scoped_repo();
        commit_file(d.path(), "defaults/x");
        git_in(d.path(), &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        std::fs::write(d.path().join("defaults/x"), "edited").unwrap();
        assert!(matches!(run_in_episode(d.path(), "e1"), Verdict::Failed { .. }));
    }

    #[test]
    fn scope_fails_safe_without_merge_base_or_diff() {
        // Empty diff: HEAD == origin/main, nothing uncommitted.
        let d = scoped_repo();
        assert!(matches!(run_in_episode(d.path(), "e1"), Verdict::Failed { .. }));
        // No origin/main ref: merge-base cannot be computed.
        let d = scoped_repo();
        git_in(d.path(), &["update-ref", "-d", "refs/remotes/origin/main"]);
        commit_file(d.path(), "loom-daemon/src/foo.rs");
        assert!(matches!(run_in_episode(d.path(), "e1"), Verdict::Failed { .. }));
    }

    #[test]
    fn malformed_scopes_are_dropped() {
        let d = repo(Some(
            r#"{"enabled":true,"command":"true","preflightPathScopes":[{"env":"A B","runWhenChanged":["x"]},{"env":"OK"},{"env":"GOOD","runWhenChanged":["d/*"]}]}"#,
        ));
        let scopes = path_scopes(d.path());
        assert_eq!(scopes.len(), 1, "{scopes:?}");
        assert_eq!(scopes[0].env, "GOOD");
        assert_eq!(scopes[0].suite, "GOOD");
    }

    #[test]
    fn timeout_names_the_running_stage_from_the_full_log() {
        let d = repo(Some(
            r#"{"enabled":true,"command":"echo '[build-gate] bash scripts/test-installer.sh'; echo '[build-gate] WARNING: noise'; yes noise | head -c 20000; sleep 5","timeoutSeconds":1}"#,
        ));
        match run_in_episode(d.path(), "e1") {
            Verdict::TimedOut {
                attempt: 1,
                stage,
                tail,
                ..
            } => {
                assert_eq!(stage.as_deref(), Some("bash scripts/test-installer.sh"));
                assert!(
                    tail.starts_with("command timed out after 1s and was killed — running stage: bash scripts/test-installer.sh"),
                    "{}",
                    &tail[..200.min(tail.len())]
                );
            }
            v => panic!("unexpected {v:?}"),
        }
    }

    #[test]
    fn timeout_without_marker_keeps_generic_message() {
        let d = repo(Some(r#"{"enabled":true,"command":"sleep 5","timeoutSeconds":1}"#));
        match run_in_episode(d.path(), "e1") {
            Verdict::TimedOut {
                stage: None, tail, ..
            } => assert_eq!(tail.trim_end(), "command timed out after 1s and was killed"),
            v => panic!("unexpected {v:?}"),
        }
    }

    #[test]
    fn timeouts_are_not_failures() {
        let d = repo(Some(
            r#"{"enabled":true,"command":"test -f fast || sleep 5","timeoutSeconds":1,"preflightMaxAttempts":2}"#,
        ));
        for n in 1..=3 {
            let v = run_in_episode(d.path(), "e1");
            assert!(
                matches!(v, Verdict::TimedOut { attempt, max: 2, .. } if attempt == n),
                "{v:?}"
            );
            assert_eq!(v.exit_code(), EXIT_TIMED_OUT);
            assert!(!v.releases_claim());
            assert_eq!(load(d.path()).failed_attempts, 0);
            assert!(!check(d.path()), "a timeout must never leave a receipt");
        }
        std::fs::write(d.path().join("fast"), "").unwrap();
        assert_eq!(run_in_episode(d.path(), "e1"), Verdict::Pass);
        assert_eq!(load(d.path()).timed_out_attempts, 0);
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

        fn settle(wt: &Path, gh: &Path, v: &Verdict) -> Option<anyhow::Result<()>> {
            let mut cfg = SweepRegistryConfig::new(wt.to_path_buf());
            cfg.gh_bin = Some(gh.to_path_buf());
            settle_claim_with(cfg, 77, v)
        }

        #[test]
        #[serial_test::serial]
        fn timeout_keeps_the_claim() {
            let tools = tempfile::tempdir().unwrap();
            let wt = tempfile::tempdir().unwrap();
            let (gh, log) = fake_gh(tools.path(), "false", "open", 0);
            let v = Verdict::TimedOut {
                attempt: 3,
                max: 3,
                stage: None,
                tail: String::new(),
            };
            assert!(settle(wt.path(), &gh, &v).is_none());
            let calls = std::fs::read_to_string(&log).unwrap_or_default();
            assert!(!calls.contains("--remove-label loom:building"), "{calls}");
            // The terminal check failure still releases, through the same path.
            let v = Verdict::Unresolved {
                attempts: 3,
                max: 3,
                tail: String::new(),
            };
            assert!(matches!(settle(wt.path(), &gh, &v), Some(Ok(()))));
            let calls = std::fs::read_to_string(&log).unwrap();
            assert!(calls.contains("--remove-label loom:building"), "{calls}");
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
