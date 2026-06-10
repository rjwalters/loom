//! Sweep registry — in-memory tracking of dispatched `/loom:sweep` children
//! (Issue #3452, Phase A of epic #3449).
//!
//! # Overview
//!
//! This module implements the daemon-side surface for dispatching and
//! tracking `/loom:sweep` runs. It is the foundation for the v0.10.0 daemon
//! rebuild — Phase A delivers:
//!
//! - A `Sweep` resource type (see [`crate::types::SweepInfo`]).
//! - In-memory `BTreeMap<SweepId, SweepInfo>` storage.
//! - `dispatch_sweep` primitive that shells out to
//!   `defaults/scripts/spawn-claude.sh` (NOT a Rust re-implementation of
//!   token rotation) and detaches a `claude -p "/loom:sweep N"` child.
//! - `list_sweeps` query with optional state filtering.
//! - Atomic `mkdir`-based claim locks under `.loom/locks/issue-<N>/`,
//!   matching the spawn-loop primitive at
//!   `defaults/scripts/spawn-loop.sh:293-309`.
//! - A reaper task that polls live PIDs on a 30s interval (env-overridable
//!   via `LOOM_SWEEP_REAPER_INTERVAL_SECS`, matching the spawn-loop
//!   `POLL_INTERVAL` default at `spawn-loop.sh:110`).
//! - Registry reconstruction on startup from live processes + checkpoints.
//!
//! # Idempotency
//!
//! When `idempotency_key` is provided and a `Running` sweep already holds
//! it, dispatch returns the existing `sweep_id` with no new spawn. Exited
//! or crashed entries with a matching key do NOT block re-dispatch — the
//! dedup window is the lifetime of the *running* entry.
//!
//! # Forge as source of truth
//!
//! Per the parent epic, the daemon does NOT persist sweep state to disk.
//! Recovery on restart relies on:
//!
//! - Live process detection (`kill(pid, 0)`).
//! - Sweep checkpoints under `.loom/sweep-checkpoint/issue-<N>.json` (#3373).
//! - Forge labels (`loom:issue` vs `loom:building`).

use crate::event_bus::EventBus;
use crate::types::{Event, SweepId, SweepInfo, SweepKind, SweepOutcome, SweepState};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ============================================================================
// Constants
// ============================================================================

/// Default reaper polling interval in seconds. Matches
/// `defaults/scripts/spawn-loop.sh:110` `POLL_INTERVAL`.
pub const DEFAULT_REAPER_INTERVAL_SECS: u64 = 30;

/// Environment variable for overriding the reaper interval. Naming follows
/// the existing `LOOM_*` conventions in `main.rs` (e.g., `LOOM_CLAIM_TTL_SECS`,
/// `LOOM_WORKSPACE`, `LOOM_SOCKET_PATH`).
pub const REAPER_INTERVAL_ENV: &str = "LOOM_SWEEP_REAPER_INTERVAL_SECS";

/// Environment variable for overriding the dispatch entry point used by
/// the registry. Defaults to `defaults/scripts/spawn-claude.sh` relative to
/// the workspace. Used by integration tests to substitute a fake child.
pub const SPAWN_BIN_ENV: &str = "LOOM_SWEEP_SPAWN_BIN";

/// Environment variable for overriding the workspace root used by the
/// registry. Falls back to `LOOM_WORKSPACE`, then current dir.
pub const WORKSPACE_ENV: &str = "LOOM_WORKSPACE";

/// Retention window after a sweep terminates before it is garbage-collected
/// from the in-memory map. One hour matches the operator intuition that
/// "recently exited sweeps should still show up in `list_sweeps`".
pub const TERMINAL_RETENTION_SECS: i64 = 3600;

// ============================================================================
// Registry
// ============================================================================

/// Configuration for a `SweepRegistry`.
///
/// All paths are resolved relative to `workspace_root`. Tests should supply
/// a `tempdir` here.
#[derive(Debug, Clone)]
pub struct SweepRegistryConfig {
    /// Absolute path to the workspace root (parent of `.loom/`).
    pub workspace_root: PathBuf,
    /// Optional override for the spawn binary. Defaults to
    /// `<workspace_root>/defaults/scripts/spawn-claude.sh` or, if absent,
    /// `<workspace_root>/.loom/scripts/spawn-claude.sh`.
    pub spawn_bin: Option<PathBuf>,
    /// Override the `gh` binary (for tests). Defaults to `gh` from `PATH`.
    pub gh_bin: Option<PathBuf>,
    /// When `true`, skip the actual label flip via `gh`. Used by unit tests
    /// that don't have GitHub credentials.
    pub skip_label_flip: bool,
}

impl SweepRegistryConfig {
    /// Construct a config rooted at `workspace_root` with default lookups.
    #[must_use]
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            spawn_bin: None,
            gh_bin: None,
            skip_label_flip: false,
        }
    }

    /// Resolve the spawn binary, preferring (in order):
    /// 1. `spawn_bin` explicit override.
    /// 2. `LOOM_SWEEP_SPAWN_BIN` env var.
    /// 3. `<workspace>/.loom/scripts/spawn-claude.sh`.
    /// 4. `<workspace>/defaults/scripts/spawn-claude.sh`.
    pub fn resolve_spawn_bin(&self) -> Result<PathBuf> {
        if let Some(ref p) = self.spawn_bin {
            return Ok(p.clone());
        }
        if let Ok(path) = std::env::var(SPAWN_BIN_ENV) {
            return Ok(PathBuf::from(path));
        }
        let installed = self
            .workspace_root
            .join(".loom")
            .join("scripts")
            .join("spawn-claude.sh");
        if installed.exists() {
            return Ok(installed);
        }
        let defaults = self
            .workspace_root
            .join("defaults")
            .join("scripts")
            .join("spawn-claude.sh");
        if defaults.exists() {
            return Ok(defaults);
        }
        Err(anyhow!(
            "spawn-claude.sh not found under {} (looked in .loom/scripts and defaults/scripts; \
             set {SPAWN_BIN_ENV} to override)",
            self.workspace_root.display()
        ))
    }

    /// Directory holding per-issue claim locks.
    #[must_use]
    pub fn locks_dir(&self) -> PathBuf {
        self.workspace_root.join(".loom").join("locks")
    }

    /// Directory holding per-sweep log files.
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.workspace_root.join(".loom").join("logs")
    }

    /// Directory holding sweep checkpoint files (#3373).
    #[must_use]
    pub fn checkpoint_dir(&self) -> PathBuf {
        self.workspace_root.join(".loom").join("sweep-checkpoint")
    }
}

/// On-disk owner metadata written inside the lock dir. Schema mirrors
/// `defaults/scripts/spawn-loop.sh:299-305`.
#[derive(Debug, Serialize, Deserialize)]
struct LockOwner {
    issue: u32,
    owner_pid: u32,
    acquired_at: String,
    sweep_id: SweepId,
}

/// In-memory registry of dispatched sweeps.
#[derive(Debug)]
pub struct SweepRegistry {
    config: SweepRegistryConfig,
    entries: BTreeMap<SweepId, SweepInfo>,
    /// Optional event bus for lifecycle events (Issue #3453, Phase B).
    /// When `None`, the registry behaves identically to Phase A — bus
    /// emission is best-effort and never blocks core dispatch/reaper
    /// progress.
    bus: Option<Arc<EventBus>>,
}

impl SweepRegistry {
    /// Construct an empty registry without an event bus.
    ///
    /// Equivalent to Phase A's behavior. Use [`set_event_bus`](Self::set_event_bus)
    /// or [`with_event_bus`](Self::with_event_bus) to attach a bus.
    #[must_use]
    pub fn new(config: SweepRegistryConfig) -> Self {
        Self {
            config,
            entries: BTreeMap::new(),
            bus: None,
        }
    }

    /// Construct an empty registry with the given event bus pre-attached.
    #[must_use]
    pub fn with_event_bus(config: SweepRegistryConfig, bus: Arc<EventBus>) -> Self {
        Self {
            config,
            entries: BTreeMap::new(),
            bus: Some(bus),
        }
    }

    /// Attach (or replace) the event bus used for lifecycle emission.
    /// Additive setter — exposed so `main.rs` can construct the bus and
    /// the registry separately, then wire them together at startup.
    pub fn set_event_bus(&mut self, bus: Arc<EventBus>) {
        self.bus = Some(bus);
    }

    /// Read-only accessor for the event bus, if any. Exposed so external
    /// callers (IPC handlers) can publish directly via the same bus the
    /// registry uses.
    #[must_use]
    pub fn event_bus(&self) -> Option<&Arc<EventBus>> {
        self.bus.as_ref()
    }

    /// Returns a shared, mutex-guarded registry suitable for tokio tasks.
    #[must_use]
    pub fn shared(config: SweepRegistryConfig) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::new(config)))
    }

    /// Read-only view of the registry config.
    #[must_use]
    pub fn config(&self) -> &SweepRegistryConfig {
        &self.config
    }

    /// Test/inspection helper: number of tracked sweeps.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Test/inspection helper.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up a sweep by ID.
    #[must_use]
    pub fn get(&self, sweep_id: &str) -> Option<&SweepInfo> {
        self.entries.get(sweep_id)
    }

    /// Return all tracked sweeps matching the optional state filter.
    pub fn list(&self, filter: Option<&SweepState>) -> Vec<SweepInfo> {
        self.entries
            .values()
            .filter(|info| match filter {
                None => true,
                Some(target) => {
                    std::mem::discriminant(&info.state) == std::mem::discriminant(target)
                }
            })
            .cloned()
            .collect()
    }

    // ------------------------------------------------------------------------
    // Dispatch
    // ------------------------------------------------------------------------

    /// Dispatch a sweep. See module docs.
    ///
    /// On idempotency hit returns the existing entry with `was_new = false`.
    ///
    /// `model` (issue #3477): when `Some` and non-empty, the spawned child
    /// receives `--model <value>` appended to the `spawn-claude.sh` argv.
    /// When `None`, no `--model` flag is emitted at all — the session/CLI
    /// default is preserved end-to-end.
    pub fn dispatch(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: Option<&str>,
    ) -> Result<DispatchOutcome> {
        // 1. Idempotency dedup against Running entries.
        if let Some(ref key) = idempotency_key {
            if let Some(existing) = self.find_running_by_key(key) {
                return Ok(DispatchOutcome {
                    sweep_id: existing.sweep_id.clone(),
                    pid: existing.pid,
                    token_name: existing.token_name.clone(),
                    log_path: existing.log_path.clone(),
                    was_new: false,
                });
            }
        }

        // 2. Phase A only fully implements Issue dispatch.
        let issue_number = match kind {
            SweepKind::Issue(n) => *n,
            SweepKind::PrSet(_) => {
                return Err(anyhow!(
                    "PrSet dispatch is reserved for a future phase of #3449 \
                     (Phase A handles Issue dispatch only)"
                ));
            }
        };

        // 3. Acquire the claim lock atomically.
        let sweep_id = generate_sweep_id(kind);
        self.acquire_lock(issue_number, &sweep_id)?;

        // 4. Flip the forge label loom:issue -> loom:building (best-effort
        //    when the dispatcher has gh credentials; tests opt out via
        //    `skip_label_flip`).
        if !self.config.skip_label_flip {
            if let Err(e) = self.flip_label_to_building(issue_number) {
                log::warn!(
                    "label flip for issue #{issue_number} failed (continuing dispatch): {e}"
                );
            }
        }

        // 5. Compute the log path and spawn the child.
        let log_path = self.compute_log_path(issue_number);
        let (pid, token_name) = self
            .spawn_child(issue_number, &log_path, &sweep_id, model)
            .context("failed to spawn sweep child")?;

        // 6. Record the entry. The model is carried on the registry entry
        //    (#3482, Phase 3a observability) so `list_sweeps` /
        //    `get_sweep_status` can report which model a sweep runs. Empty
        //    strings are normalized to None, matching the spawn-side rule
        //    that `--model ""` is never emitted.
        let info = SweepInfo {
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
            pid,
            token_name: token_name.clone(),
            log_path: log_path.clone(),
            idempotency_key,
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: model.filter(|m| !m.is_empty()).map(String::from),
        };
        self.entries.insert(sweep_id.clone(), info);

        // 7. Emit `sweep.global.dispatch` (best-effort — never block
        //    dispatch progress on the bus). If no subscribers are
        //    listening, the bus returns NoSubscribers; log at debug.
        self.emit_event(Event::SweepGlobalDispatch {
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
        });

        Ok(DispatchOutcome {
            sweep_id,
            pid,
            token_name,
            log_path,
            was_new: true,
        })
    }

    /// Internal helper: publish an event on the attached bus (if any).
    /// Best-effort — logs a debug line if no subscribers are listening.
    fn emit_event(&self, event: Event) {
        if let Some(ref bus) = self.bus {
            let topic = event.topic();
            match bus.publish(event) {
                Ok(n) => log::debug!("event_bus: published {topic} to {n} subscriber(s)"),
                Err(_) => {
                    log::debug!("event_bus: published {topic} (no subscribers)");
                }
            }
        }
    }

    fn find_running_by_key(&self, key: &str) -> Option<&SweepInfo> {
        self.entries.values().find(|info| {
            matches!(info.state, SweepState::Running | SweepState::Pending)
                && info.idempotency_key.as_deref() == Some(key)
        })
    }

    // ------------------------------------------------------------------------
    // Lock primitive (mirrors spawn-loop.sh:293-309)
    // ------------------------------------------------------------------------

    fn acquire_lock(&self, issue: u32, sweep_id: &str) -> Result<()> {
        let locks_dir = self.config.locks_dir();
        std::fs::create_dir_all(&locks_dir)
            .with_context(|| format!("failed to create locks dir {}", locks_dir.display()))?;
        let lock = locks_dir.join(format!("issue-{issue}"));

        // `mkdir` is POSIX-atomic — see spawn-loop.sh:286-292 for rationale.
        match std::fs::create_dir(&lock) {
            Ok(()) => {
                let owner = LockOwner {
                    issue,
                    owner_pid: std::process::id(),
                    acquired_at: Utc::now().to_rfc3339(),
                    sweep_id: sweep_id.to_string(),
                };
                let owner_json =
                    serde_json::to_string_pretty(&owner).context("serialize lock owner")?;
                std::fs::write(lock.join("owner.json"), owner_json)
                    .context("write lock owner.json")?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow!(
                "lock collision: issue #{issue} is already claimed (lock at {})",
                lock.display()
            )),
            Err(e) => {
                Err(anyhow!("failed to acquire lock for issue #{issue} at {}: {e}", lock.display()))
            }
        }
    }

    /// Release the lock dir for an issue (idempotent).
    pub fn release_lock(&self, issue: u32) -> Result<()> {
        let lock = self.config.locks_dir().join(format!("issue-{issue}"));
        if lock.exists() {
            std::fs::remove_dir_all(&lock)
                .with_context(|| format!("failed to remove lock dir {}", lock.display()))?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------------
    // Forge label flip
    // ------------------------------------------------------------------------

    fn flip_label_to_building(&self, issue: u32) -> Result<()> {
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--remove-label")
            .arg("loom:issue")
            .arg("--add-label")
            .arg("loom:building");
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let output = cmd
            .output()
            .with_context(|| format!("failed to invoke {} for issue #{issue}", gh.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("gh issue edit failed for #{issue}: {}", stderr.trim()));
        }
        Ok(())
    }

    fn restore_label_to_ready(&self, issue: u32) -> Result<()> {
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--remove-label")
            .arg("loom:building")
            .arg("--add-label")
            .arg("loom:issue");
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let _ = cmd.output()?; // best-effort during reap
        Ok(())
    }

    // ------------------------------------------------------------------------
    // Spawn
    // ------------------------------------------------------------------------

    fn compute_log_path(&self, issue: u32) -> PathBuf {
        self.config
            .logs_dir()
            .join(format!("sweep-issue-{issue}.log"))
    }

    fn spawn_child(
        &self,
        issue: u32,
        log_path: &Path,
        sweep_id: &str,
        model: Option<&str>,
    ) -> Result<(u32, String)> {
        let spawn_bin = self.config.resolve_spawn_bin()?;

        // Ensure log dir exists.
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log dir {}", parent.display()))?;
        }

        // Append a header so reruns are distinguishable. Mirrors
        // spawn-loop.sh:377-380.
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
            {
                let _ = writeln!(
                    f,
                    "\n==== loom-daemon dispatch: {} sweep_id={sweep_id} issue={issue} ====",
                    Utc::now().to_rfc3339()
                );
            }
        }

        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("failed to open log {}", log_path.display()))?;
        let log_clone = log_file.try_clone()?;

        let prompt = format!("/loom:sweep {issue}");
        let mut cmd = Command::new(&spawn_bin);
        cmd.arg("-p").arg(&prompt);
        // Model selection (issue #3477, Phase 1): the dispatch-param tier of
        // the precedence chain. Appended as an explicit `--model` arg (which
        // beats any ambient LOOM_MODEL env inside spawn-claude.sh). Empty
        // strings are treated as unset — `--model ""` must never be emitted.
        if let Some(m) = model {
            if !m.is_empty() {
                cmd.arg("--model").arg(m);
            }
        }
        cmd.env("LOOM_TERMINAL_ID", format!("daemon-{sweep_id}"))
            // Always pin LOOM_WORKSPACE to the registry's configured root so
            // spawn-claude.sh resolves `.loom/tokens/` from the same place
            // the daemon thinks the workspace is — never inheriting an
            // ambient value that might point elsewhere.
            .env(WORKSPACE_ENV, &self.config.workspace_root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_clone));

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {} -p '{}'", spawn_bin.display(), prompt))?;
        let pid = child.id();
        // We do NOT wait on the child — detach by dropping the handle. The
        // reaper detects exit via `kill(pid, 0)`. spawn-claude.sh internally
        // selects a token; we record "unknown" here because the wrapper's
        // selection is logged to the per-sweep log, not exposed on stdout.
        std::mem::drop(child);

        Ok((pid, "unknown".to_string()))
    }

    // ------------------------------------------------------------------------
    // Cancellation + status accessors (Issue #3455, Phase C)
    // ------------------------------------------------------------------------

    /// Return the `SweepInfo` for the given sweep ID, cloned (so callers
    /// can release the registry lock immediately). Phase C exposes this
    /// as the `get_sweep_status` MCP tool.
    #[must_use]
    pub fn get_status(&self, sweep_id: &str) -> Option<SweepInfo> {
        self.entries.get(sweep_id).cloned()
    }

    /// Cancel a running sweep.
    ///
    /// Sends SIGTERM, waits up to `grace` for the child to exit (polled
    /// via `kill(pid, 0)`), then SIGKILL if still alive. On any path the
    /// registry entry is transitioned to `Exited{code: None, at: now}`
    /// and the per-issue lock is released. Emits the same lifecycle
    /// events the reaper would emit on a clean exit
    /// (`sweep.issue.{N}.exited` + `sweep.global.completed`).
    ///
    /// Returns [`CancelOutcome`] describing what actually happened. Calls
    /// against unknown sweep IDs return `Err`. Calls against already-
    /// terminal sweeps return `Ok` with `was_running = false` — cancel
    /// is idempotent so monitor-tool retries don't surface as errors.
    pub fn cancel(&mut self, sweep_id: &str, grace: Duration) -> Result<CancelOutcome> {
        let (pid, kind, was_running, started_at) = {
            let info = self
                .entries
                .get(sweep_id)
                .ok_or_else(|| anyhow!("unknown sweep_id: {sweep_id}"))?;
            let alive = matches!(info.state, SweepState::Running | SweepState::Pending);
            (info.pid, info.kind.clone(), alive, info.started_at)
        };

        if !was_running {
            // Already terminal — nothing to signal. Return success so
            // duplicate cancel-from-monitor requests stay idempotent.
            return Ok(CancelOutcome {
                sweep_id: sweep_id.to_string(),
                pid,
                sigkill_sent: false,
                was_running: false,
            });
        }

        // Step 1: SIGTERM (signal 15). We send via `kill(2)` directly
        // rather than spawning `kill(1)` so the path is identical on
        // macOS + Linux and we don't depend on `PATH`.
        let term_sent = send_signal(pid, 15);
        if !term_sent {
            log::warn!(
                "cancel_sweep: SIGTERM to pid {pid} for sweep {sweep_id} failed \
                 (process may already be dead)"
            );
        }

        // Step 2: poll for exit up to the grace window. We poll every
        // 100ms (matches the spawn-loop's shutdown-grace polling
        // cadence). The poll loop is a blocking sleep — acceptable for
        // an IPC handler since cancel is a low-frequency operator action.
        let poll_interval = Duration::from_millis(100);
        let deadline = std::time::Instant::now() + grace;
        let mut exited_within_grace = !is_pid_alive(pid);
        while !exited_within_grace && std::time::Instant::now() < deadline {
            std::thread::sleep(poll_interval);
            exited_within_grace = !is_pid_alive(pid);
        }

        // Step 3: SIGKILL if still alive.
        let sigkill_sent = if exited_within_grace {
            false
        } else {
            let killed = send_signal(pid, 9);
            if !killed {
                log::warn!("cancel_sweep: SIGKILL to pid {pid} also failed");
            }
            true
        };

        // Step 4: transition state, release lock, emit events.
        let now = Utc::now();
        let duration_sec = (now - started_at).num_seconds();
        if let Some(info) = self.entries.get_mut(sweep_id) {
            info.state = SweepState::Exited {
                code: None,
                at: now,
            };
        }
        if let SweepKind::Issue(issue) = &kind {
            let _ = self.release_lock(*issue);
            self.emit_event(Event::SweepExited {
                issue: *issue,
                exit_code: None,
                duration_sec,
            });
        }
        self.emit_event(Event::SweepGlobalCompleted {
            sweep_id: sweep_id.to_string(),
            outcome: SweepOutcome::Exited,
        });

        Ok(CancelOutcome {
            sweep_id: sweep_id.to_string(),
            pid,
            sigkill_sent,
            was_running: true,
        })
    }

    /// Read the last `lines` lines from a sweep's log file.
    ///
    /// Resolves the log path from the registry entry (so callers don't
    /// have to know the workspace-relative naming convention). Returns
    /// the absolute log path alongside the tail so the MCP layer can
    /// surface it.
    pub fn tail_log(&self, sweep_id: &str, lines: usize) -> Result<(PathBuf, Vec<String>)> {
        let info = self
            .entries
            .get(sweep_id)
            .ok_or_else(|| anyhow!("unknown sweep_id: {sweep_id}"))?;
        let log_path = info.log_path.clone();
        let tail = tail_lines(&log_path, lines)
            .with_context(|| format!("failed to tail {}", log_path.display()))?;
        Ok((log_path, tail))
    }

    // ------------------------------------------------------------------------
    // Reaper
    // ------------------------------------------------------------------------

    /// Run one reaper tick. Updates entry state for dead PIDs, releases
    /// locks, restores labels on crashed sweeps (if a checkpoint exists),
    /// and GCs entries older than the retention window.
    ///
    /// Returns the number of entries whose state changed.
    ///
    /// Emits the following events when an attached event bus is present
    /// (Issue #3453, Phase B):
    ///
    /// - `sweep.issue.{N}.exited` on a clean-exit transition.
    /// - `sweep.issue.{N}.crashed` on a checkpoint-present transition
    ///   (which also re-arms the `loom:issue` label).
    /// - `sweep.global.completed` on every terminal transition, regardless
    ///   of which per-issue event also fired.
    pub fn reap_once(&mut self) -> usize {
        let mut changes = 0usize;

        // Snapshot keys + pids first so we can borrow mutably below.
        // Capture started_at so we can compute durations for Exited events.
        let candidates: Vec<(SweepId, u32, SweepState, SweepKind, chrono::DateTime<Utc>)> = self
            .entries
            .iter()
            .map(|(id, info)| {
                (id.clone(), info.pid, info.state.clone(), info.kind.clone(), info.started_at)
            })
            .collect();

        // Buffer events to emit after we've finished mutating the
        // registry — so we never call into the bus while holding the
        // registry mutex's lifetime budget unnecessarily.
        let mut events_to_emit: Vec<Event> = Vec::new();

        for (sweep_id, pid, state, kind, started_at) in candidates {
            match state {
                SweepState::Running | SweepState::Pending if !is_pid_alive(pid) => {
                    changes += 1;
                    let issue = match &kind {
                        SweepKind::Issue(n) => Some(*n),
                        SweepKind::PrSet(_) => None,
                    };
                    let now = Utc::now();
                    let duration_sec = (now - started_at).num_seconds();
                    // Release lock and decide between Exited vs Crashed.
                    if let Some(issue) = issue {
                        let _ = self.release_lock(issue);
                        let checkpoint = self
                            .config
                            .checkpoint_dir()
                            .join(format!("issue-{issue}.json"));
                        if checkpoint.exists() {
                            if !self.config.skip_label_flip {
                                let _ = self.restore_label_to_ready(issue);
                            }
                            let checkpoint_phase = read_checkpoint_phase(&checkpoint);
                            if let Some(info) = self.entries.get_mut(&sweep_id) {
                                info.state = SweepState::Crashed { at: now };
                                if info.latest_phase.is_none() {
                                    info.latest_phase.clone_from(&checkpoint_phase);
                                }
                            }
                            events_to_emit.push(Event::SweepCrashed {
                                issue,
                                checkpoint_phase,
                            });
                            events_to_emit.push(Event::SweepGlobalCompleted {
                                sweep_id: sweep_id.clone(),
                                outcome: SweepOutcome::Crashed,
                            });
                        } else {
                            if let Some(info) = self.entries.get_mut(&sweep_id) {
                                info.state = SweepState::Exited {
                                    code: None,
                                    at: now,
                                };
                            }
                            events_to_emit.push(Event::SweepExited {
                                issue,
                                exit_code: None,
                                duration_sec,
                            });
                            events_to_emit.push(Event::SweepGlobalCompleted {
                                sweep_id: sweep_id.clone(),
                                outcome: SweepOutcome::Exited,
                            });
                        }
                    } else {
                        if let Some(info) = self.entries.get_mut(&sweep_id) {
                            info.state = SweepState::Exited {
                                code: None,
                                at: now,
                            };
                        }
                        // PrSet sweeps don't have a single issue id, so we
                        // only emit the global event. Per-issue events are
                        // intentionally not emitted for PrSet (out of scope
                        // for Phase A — see sweep_registry::dispatch).
                        events_to_emit.push(Event::SweepGlobalCompleted {
                            sweep_id: sweep_id.clone(),
                            outcome: SweepOutcome::Exited,
                        });
                    }
                }
                _ => {}
            }
        }

        // Drain the buffered events onto the bus. Each emission is
        // best-effort and never propagates an error back into reaper
        // progress.
        for event in events_to_emit {
            self.emit_event(event);
        }

        // GC: drop terminal entries past the retention window.
        let cutoff = Utc::now() - chrono::Duration::seconds(TERMINAL_RETENTION_SECS);
        let to_drop: Vec<SweepId> = self
            .entries
            .iter()
            .filter_map(|(id, info)| {
                let terminated_at = match &info.state {
                    SweepState::Exited { at, .. } | SweepState::Crashed { at } => Some(*at),
                    _ => None,
                };
                terminated_at.filter(|t| *t < cutoff).map(|_| id.clone())
            })
            .collect();
        for id in to_drop {
            self.entries.remove(&id);
            changes += 1;
        }
        changes
    }

    // ------------------------------------------------------------------------
    // Reconstruction
    // ------------------------------------------------------------------------

    /// Reconstruct registry entries on daemon startup by combining:
    ///
    /// 1. Live lock dirs under `.loom/locks/issue-<N>/` (the lock's
    ///    `owner.json` records the dispatching daemon's PID and sweep ID).
    /// 2. Sweep checkpoints under `.loom/sweep-checkpoint/issue-<N>.json`
    ///    (#3373) — these survive crashes and signal that a sweep was in
    ///    flight even if the lock is gone.
    ///
    /// This is best-effort: locks whose `owner_pid` is dead are released
    /// (they're stale); locks whose owner is live are admitted as `Running`;
    /// checkpoints without a corresponding lock are admitted as `Crashed`
    /// so a subsequent dispatch will re-run them via the checkpoint resume.
    #[allow(clippy::too_many_lines)]
    pub fn reconstruct(&mut self) -> Result<usize> {
        let locks_dir = self.config.locks_dir();
        let mut admitted = 0usize;

        if locks_dir.exists() {
            for entry in std::fs::read_dir(&locks_dir)? {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("read_dir error in {}: {e}", locks_dir.display());
                        continue;
                    }
                };
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                let Some(issue_str) = name.strip_prefix("issue-") else {
                    continue;
                };
                let Ok(issue): Result<u32, _> = issue_str.parse() else {
                    continue;
                };
                let owner_path = path.join("owner.json");
                let owner: Option<LockOwner> = std::fs::read_to_string(&owner_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok());
                let Some(owner) = owner else {
                    // No owner.json — treat as stale, remove.
                    let _ = std::fs::remove_dir_all(&path);
                    continue;
                };
                if !is_pid_alive(owner.owner_pid) {
                    // Stale lock: owner is dead. Drop the lock and continue;
                    // checkpoint reconstruction below will admit a Crashed
                    // entry if appropriate.
                    let _ = std::fs::remove_dir_all(&path);
                    continue;
                }
                let log_path = self.compute_log_path(issue);
                let started_at = chrono::DateTime::parse_from_rfc3339(&owner.acquired_at)
                    .map_or_else(|_| Utc::now(), |t| t.with_timezone(&Utc));
                self.entries.insert(
                    owner.sweep_id.clone(),
                    SweepInfo {
                        sweep_id: owner.sweep_id.clone(),
                        kind: SweepKind::Issue(issue),
                        pid: owner.owner_pid,
                        token_name: "unknown".to_string(),
                        log_path,
                        idempotency_key: None,
                        started_at,
                        state: SweepState::Running,
                        latest_phase: None,
                        pr_number: None,
                        // Lock owner.json does not record the model; the
                        // dispatching daemon instance is gone (#3482).
                        model: None,
                    },
                );
                admitted += 1;
            }
        }

        // Checkpoints without a live lock -> Crashed entries (so list_sweeps
        // shows them; the next dispatch will resume via the sweep skill).
        let checkpoint_dir = self.config.checkpoint_dir();
        if checkpoint_dir.exists() {
            for entry in std::fs::read_dir(&checkpoint_dir)? {
                let Ok(entry) = entry else { continue };
                let path = entry.path();
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                let Some(rest) = name.strip_prefix("issue-") else {
                    continue;
                };
                let Some(issue_str) = rest.strip_suffix(".json") else {
                    continue;
                };
                let Ok(issue): Result<u32, _> = issue_str.parse() else {
                    continue;
                };
                // Skip if we already have a Running entry for this issue.
                let already_running = self.entries.values().any(|info| {
                    matches!(info.state, SweepState::Running | SweepState::Pending)
                        && matches!(info.kind, SweepKind::Issue(n) if n == issue)
                });
                if already_running {
                    continue;
                }
                let sweep_id = format!("sweep-issue-{issue}-recovered-{}", Utc::now().timestamp());
                let phase = read_checkpoint_phase(&path);
                self.entries.insert(
                    sweep_id.clone(),
                    SweepInfo {
                        sweep_id,
                        kind: SweepKind::Issue(issue),
                        pid: 0, // unknown — owner is gone
                        token_name: "unknown".to_string(),
                        log_path: self.compute_log_path(issue),
                        idempotency_key: None,
                        started_at: Utc::now(),
                        state: SweepState::Crashed { at: Utc::now() },
                        latest_phase: phase,
                        pr_number: None,
                        model: None, // not recoverable from a checkpoint-only entry
                    },
                );
                admitted += 1;
            }
        }

        Ok(admitted)
    }
}

// ============================================================================
// Public helpers
// ============================================================================

/// Result of a successful dispatch.
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    pub sweep_id: SweepId,
    pub pid: u32,
    pub token_name: String,
    pub log_path: PathBuf,
    /// `false` when the dispatch was an idempotency hit on an existing
    /// `Running` entry.
    pub was_new: bool,
}

/// Result of a `cancel` call (Issue #3455, Phase C).
#[derive(Debug, Clone)]
pub struct CancelOutcome {
    pub sweep_id: SweepId,
    pub pid: u32,
    /// `true` when the child did not exit within the grace window and
    /// a SIGKILL was issued.
    pub sigkill_sent: bool,
    /// `true` when the sweep was in `Running`/`Pending` state at the
    /// moment of the call; `false` when it was already terminal.
    pub was_running: bool,
}

/// Generate a stable sweep ID for the given kind. Format follows the
/// spawn-loop log naming convention so operators can correlate.
#[must_use]
pub fn generate_sweep_id(kind: &SweepKind) -> SweepId {
    let ts = Utc::now().timestamp();
    match kind {
        SweepKind::Issue(n) => format!("sweep-issue-{n}-{ts}"),
        SweepKind::PrSet(prs) => {
            let joined = prs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("-");
            format!("sweep-prs-{joined}-{ts}")
        }
    }
}

/// Resolve the configured reaper interval from the environment, falling
/// back to [`DEFAULT_REAPER_INTERVAL_SECS`].
#[must_use]
pub fn resolve_reaper_interval() -> Duration {
    let secs = std::env::var(REAPER_INTERVAL_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_REAPER_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Spawn the long-running reaper task. Returns the task handle so the
/// daemon can keep it alive for the lifetime of the process.
///
/// The reaper takes the registry lock briefly each tick; it never holds
/// the lock across the sleep.
pub fn spawn_reaper_task(registry: Arc<Mutex<SweepRegistry>>) -> tokio::task::JoinHandle<()> {
    let interval = resolve_reaper_interval();
    log::info!("sweep_registry: starting reaper with interval={}s", interval.as_secs());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; skip it so we don't churn at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let changed = {
                match registry.lock() {
                    Ok(mut r) => r.reap_once(),
                    Err(poisoned) => {
                        log::error!("sweep_registry: mutex poisoned ({poisoned:?})");
                        return;
                    }
                }
            };
            if changed > 0 {
                log::info!(
                    "sweep_registry: reaper changed {changed} entr{}",
                    if changed == 1 { "y" } else { "ies" }
                );
            }
        }
    })
}

// ============================================================================
// Internal helpers
// ============================================================================

/// Liveness probe via `kill(pid, 0)`. Returns true when the signal would
/// be deliverable (i.e. the process exists and is owned by us). PID 0 is
/// always treated as dead.
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) is a documented liveness probe; signal 0 is not
    // sent, just checked.
    let pid_t: i32 = match pid.try_into() {
        Ok(p) => p,
        Err(_) => return false,
    };
    libc_kill(pid_t, 0) == 0
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    // Non-unix platforms are not supported targets for Loom; assume alive
    // so the test suite can run without a hard panic.
    true
}

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
#[allow(non_snake_case)]
fn libc_kill(pid: i32, sig: i32) -> i32 {
    // Indirection so we can stub this in unit tests if needed.
    unsafe { kill(pid, sig) }
}

/// Best-effort extraction of the `phase` field from a sweep checkpoint
/// JSON file. Schema is owned by the sweep skill (#3373); we treat the
/// file as opaque and only peek at one field.
fn read_checkpoint_phase(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("phase")
        .and_then(|p| p.as_str())
        .map(ToString::to_string)
}

/// Send a signal to a PID. Returns `true` on success (signal queued or
/// process already absent and the caller can treat that as "done"). PID
/// 0 is rejected to avoid the POSIX broadcast-to-group semantics.
#[cfg(unix)]
fn send_signal(pid: u32, sig: i32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(pid_t): Result<i32, _> = pid.try_into() else {
        return false;
    };
    libc_kill(pid_t, sig) == 0
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _sig: i32) -> bool {
    // Non-unix platforms are not supported; return false so the cancel
    // path surfaces a "kill failed" log but still transitions state.
    false
}

/// Read the last `n` lines of a file. Returns an empty vec when the
/// file is empty; returns an error when the file does not exist (so the
/// caller can distinguish "no log yet" from "log gone").
///
/// Implementation is a simple full-read + split — sweep logs are
/// bounded by the lifetime of a sweep (~tens of minutes typical) and
/// the buffering overhead is dwarfed by the IPC round-trip. If sweep
/// logs grow to GB-scale in a future release, swap this for a reverse
/// reader.
fn tail_lines(path: &Path, n: usize) -> Result<Vec<String>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = contents.lines().map(ToString::to_string).collect();
    if out.len() > n {
        out = out.split_off(out.len() - n);
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// Build a temp-workspace registry with a fake spawn binary that
    /// records its argv + env into a log and exits immediately. This lets
    /// us assert on the dispatch behavior without invoking real `claude`.
    ///
    /// We invoke the fake via `bash -c '...'` (returned from
    /// `SweepRegistryConfig.spawn_bin`) rather than relying on a shebang +
    /// exec bit, because parallel-test load on macOS occasionally races the
    /// chmod with the child's posix_spawn exec call and the script silently
    /// fails to launch (no shebang resolution, no exec-bit yet).
    fn fixture_registry(workspace: &Path) -> (SweepRegistry, PathBuf) {
        let record_log = workspace.join("fake-spawn.log");
        // We use /bin/bash as the spawn binary, and the dispatch path appends
        // "-p <prompt>" — we ignore the args via `--`, then run an inline
        // recording script.
        let scripts_dir = workspace.join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        // Use exec on bash directly with arguments: we write a wrapper that
        // bash will invoke. The wrapper is small enough that a bad chmod
        // would be a system-level problem, not a test-flake.
        let script = format!(
            r#"#!/usr/bin/env bash
# Test fixture: record dispatch args + selected env into a log.
{{
  printf 'argv: %s\n' "$*"
  printf 'CLAUDE_CODE_OAUTH_TOKEN=%s\n' "${{CLAUDE_CODE_OAUTH_TOKEN:-unset}}"
  printf 'LOOM_TERMINAL_ID=%s\n' "${{LOOM_TERMINAL_ID:-unset}}"
  printf 'LOOM_WORKSPACE=%s\n' "${{LOOM_WORKSPACE:-unset}}"
}} >> "{rec}" 2>&1
exit 0
"#,
            rec = record_log.display()
        );
        std::fs::write(&fake_bin, script).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();
        // Sync the perms change to the filesystem so the child sees it.
        // On macOS APFS under heavy load, posix_spawn occasionally exec's
        // before the chmod is visible to the child process.
        if let Ok(f) = std::fs::File::open(&fake_bin) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
        config.spawn_bin = Some(fake_bin);
        config.skip_label_flip = true;
        (SweepRegistry::new(config), record_log)
    }

    /// Wait until `path` exists AND contains `needle`. Returns true on
    /// success, false on timeout.
    fn wait_for_contents(path: &Path, needle: &str, timeout_ms: u64) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < u128::from(timeout_ms) {
            if let Ok(s) = std::fs::read_to_string(path) {
                if s.contains(needle) {
                    return true;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    #[test]
    #[serial]
    fn dispatch_happy_path_records_entry() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(42), None, None)
            .expect("dispatch should succeed");

        assert!(outcome.was_new);
        assert!(outcome.pid > 0);
        assert_eq!(outcome.token_name, "unknown");
        assert_eq!(registry.len(), 1);

        let info = registry.get(&outcome.sweep_id).unwrap();
        assert!(matches!(info.kind, SweepKind::Issue(42)));
        assert!(matches!(info.state, SweepState::Running));

        // Wait for the fake spawn to record its invocation. We wait for
        // the final line (LOOM_TERMINAL_ID) so the assertion isn't racing
        // mid-write.
        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let sweep_log = dir
            .path()
            .join(".loom")
            .join("logs")
            .join("sweep-issue-42.log");
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s\n  record_log: {}\n  record_log exists: {}\n  sweep_log: {}",
            std::fs::read_to_string(&record_log).unwrap_or_default(),
            record_log.exists(),
            std::fs::read_to_string(&sweep_log).unwrap_or_default(),
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("argv: -p /loom:sweep 42"),
            "expected argv in recorded log; got: {recorded}"
        );
        // Issue #3477 zero-behavior-change criterion: with model=None the
        // spawned command must NOT receive a --model flag at all.
        assert!(
            !recorded.contains("--model"),
            "model=None must not emit --model; got: {recorded}"
        );
        // #3482: model=None dispatches record no model on the entry.
        assert_eq!(registry.get(&outcome.sweep_id).unwrap().model, None);

        // The lock dir should exist while Running.
        let lock = dir.path().join(".loom").join("locks").join("issue-42");
        assert!(lock.exists(), "expected lock dir at {}", lock.display());
    }

    /// Issue #3477 (Phase 1): a `model` dispatch param threads through to
    /// the spawn command as an explicit `--model <value>` argument.
    #[test]
    #[serial]
    fn dispatch_with_model_appends_model_arg() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(43), None, Some("claude-sonnet-4-6"))
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("argv: -p /loom:sweep 43 --model claude-sonnet-4-6"),
            "expected --model in argv; got: {recorded}"
        );
        // #3482 (Phase 3a): the dispatch model is carried on the registry
        // entry so list_sweeps / get_sweep_status report it.
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().model.as_deref(),
            Some("claude-sonnet-4-6"),
            "dispatch model must be recorded on the SweepInfo entry"
        );
    }

    /// Issue #3477: an empty-string model is treated as unset — `--model ""`
    /// must never be emitted (acceptance criterion: no flag at all, not an
    /// empty flag).
    #[test]
    #[serial]
    fn dispatch_with_empty_model_emits_no_model_flag() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(44), None, Some(""))
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            !recorded.contains("--model"),
            "empty model must not emit --model; got: {recorded}"
        );
        // #3482: empty-string model normalizes to None on the entry too.
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().model,
            None,
            "empty model must be recorded as None on the SweepInfo entry"
        );
    }

    #[test]
    #[serial]
    fn dispatch_lock_collision_rejected() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let first = registry.dispatch(&SweepKind::Issue(7), None, None);
        assert!(first.is_ok());

        let second = registry.dispatch(&SweepKind::Issue(7), None, None);
        assert!(second.is_err(), "second dispatch for issue #7 should fail (lock collision)");
        let err = second.unwrap_err().to_string();
        assert!(err.contains("lock collision"), "expected lock collision error; got: {err}");
    }

    #[test]
    #[serial]
    fn dispatch_idempotency_returns_existing() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let first = registry
            .dispatch(&SweepKind::Issue(99), Some("key-A".to_string()), None)
            .unwrap();
        assert!(first.was_new);

        // While still Running, a dispatch with the same key must dedup.
        // Issue #99 is the same kind, but we don't need a different issue —
        // the dedup is purely on the idempotency key.
        let second = registry
            .dispatch(&SweepKind::Issue(99), Some("key-A".to_string()), None)
            .unwrap();
        assert!(!second.was_new);
        assert_eq!(first.sweep_id, second.sweep_id);
    }

    #[test]
    fn pr_set_dispatch_rejected_in_phase_a() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let outcome = registry.dispatch(&SweepKind::PrSet(vec![1, 2, 3]), None, None);
        assert!(outcome.is_err());
        assert!(outcome
            .unwrap_err()
            .to_string()
            .contains("PrSet dispatch is reserved"));
    }

    #[test]
    fn list_sweeps_filters_by_state() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Dispatch and then poke an entry into Exited state directly.
        let outcome = registry
            .dispatch(&SweepKind::Issue(11), None, None)
            .unwrap();
        let entry = registry.entries.get_mut(&outcome.sweep_id).unwrap();
        entry.state = SweepState::Exited {
            code: Some(0),
            at: Utc::now(),
        };

        let running = registry.list(Some(&SweepState::Running));
        assert!(running.is_empty());

        let exited = registry.list(Some(&SweepState::Exited {
            code: None,
            at: Utc::now(),
        }));
        assert_eq!(exited.len(), 1);

        let all = registry.list(None);
        assert_eq!(all.len(), 1);
    }

    /// AC #2: reaper emits `sweep.issue.{N}.crashed` AND re-arms the
    /// `loom:building` -> `loom:issue` label when a dead pid has a
    /// checkpoint on disk. We don't actually invoke `gh` here (that's
    /// covered by integration tests with `skip_label_flip = false`); we
    /// assert the event payload and the registry state transition, which
    /// is the contract Phase B exposes to subscribers.
    #[tokio::test]
    async fn reaper_emits_crashed_event_with_checkpoint_phase() {
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("issue-55.json"), r#"{"phase":"doctor","issue":55}"#).unwrap();

        let sweep_id = "sweep-issue-55-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(55),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(55),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
            },
        );

        let changed = registry.reap_once();
        assert!(changed >= 1);

        // Should observe: sweep.issue.55.crashed + sweep.global.completed
        let mut saw_crashed = false;
        let mut saw_completed = false;
        for _ in 0..2 {
            let ev = sub.recv().await.unwrap();
            match ev {
                Event::SweepCrashed {
                    issue,
                    checkpoint_phase,
                } => {
                    assert_eq!(issue, 55);
                    assert_eq!(checkpoint_phase.as_deref(), Some("doctor"));
                    saw_crashed = true;
                }
                Event::SweepGlobalCompleted {
                    sweep_id: sid,
                    outcome,
                } => {
                    assert_eq!(sid, sweep_id);
                    assert_eq!(outcome, SweepOutcome::Crashed);
                    saw_completed = true;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(saw_crashed, "expected sweep.issue.55.crashed event");
        assert!(saw_completed, "expected sweep.global.completed event");

        // And the registry state should be Crashed (the label re-arm
        // side-effect is suppressed because skip_label_flip is true in
        // the fixture; the contract is the state transition + event
        // emission, which together signal the re-arm has happened in
        // production).
        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Crashed { .. }));
    }

    /// Clean-exit (no checkpoint) emits `sweep.issue.{N}.exited` plus
    /// `sweep.global.completed{outcome=Exited}`.
    #[tokio::test]
    async fn reaper_emits_exited_event_for_clean_exit() {
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let sweep_id = "sweep-issue-66-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(66),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(66),
                idempotency_key: None,
                started_at: Utc::now() - chrono::Duration::seconds(10),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
            },
        );

        let changed = registry.reap_once();
        assert!(changed >= 1);

        let mut saw_exited = false;
        let mut saw_completed = false;
        for _ in 0..2 {
            let ev = sub.recv().await.unwrap();
            match ev {
                Event::SweepExited {
                    issue,
                    duration_sec,
                    ..
                } => {
                    assert_eq!(issue, 66);
                    assert!(duration_sec >= 0);
                    saw_exited = true;
                }
                Event::SweepGlobalCompleted { outcome, .. } => {
                    assert_eq!(outcome, SweepOutcome::Exited);
                    saw_completed = true;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(saw_exited);
        assert!(saw_completed);
    }

    #[test]
    fn reap_marks_dead_pid_exited_when_no_checkpoint() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Stuff an entry with a guaranteed-dead PID (very large pid_t).
        let sweep_id = "sweep-issue-21-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(21),
                pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(21),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
            },
        );

        let changed = registry.reap_once();
        assert!(changed >= 1);
        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));
    }

    #[test]
    fn reap_marks_dead_pid_crashed_when_checkpoint_present() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Create a checkpoint file so the reaper picks Crashed over Exited.
        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("issue-33.json"), r#"{"phase":"builder","issue":33}"#).unwrap();

        let sweep_id = "sweep-issue-33-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(33),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(33),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
            },
        );

        let changed = registry.reap_once();
        assert!(changed >= 1);
        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Crashed { .. }));
    }

    #[test]
    fn reconstruct_admits_live_lock_owners() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Write a lock dir with our own PID as the owner (guaranteed alive).
        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-77");
        std::fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            issue: 77,
            owner_pid: std::process::id(),
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-77-reconstruct".to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        let admitted = registry.reconstruct().unwrap();
        assert!(admitted >= 1);
        let info = registry.get("sweep-issue-77-reconstruct").unwrap();
        assert_eq!(info.pid, std::process::id());
        assert!(matches!(info.state, SweepState::Running));
    }

    #[test]
    fn reconstruct_drops_stale_locks() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-78");
        std::fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            issue: 78,
            owner_pid: 2_147_483_640, // dead
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-78-stale".to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        let _ = registry.reconstruct().unwrap();
        assert!(!lock.exists(), "stale lock should be removed");
        assert!(registry.get("sweep-issue-78-stale").is_none());
    }

    #[test]
    fn reconstruct_admits_orphan_checkpoints_as_crashed() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("issue-91.json"), r#"{"phase":"judge","issue":91}"#).unwrap();

        let admitted = registry.reconstruct().unwrap();
        assert!(admitted >= 1);
        let crashed = registry.list(Some(&SweepState::Crashed { at: Utc::now() }));
        assert_eq!(crashed.len(), 1);
        assert_eq!(crashed[0].latest_phase.as_deref(), Some("judge"));
    }

    #[test]
    fn sweep_id_format() {
        let id = generate_sweep_id(&SweepKind::Issue(42));
        assert!(id.starts_with("sweep-issue-42-"));

        let pr = generate_sweep_id(&SweepKind::PrSet(vec![10, 20]));
        assert!(pr.starts_with("sweep-prs-10-20-"));
    }

    #[test]
    #[serial]
    fn reaper_interval_env_override() {
        // Serialized: this test mutates a process-wide env var.
        std::env::remove_var(REAPER_INTERVAL_ENV);
        let d = resolve_reaper_interval();
        assert_eq!(d.as_secs(), DEFAULT_REAPER_INTERVAL_SECS);

        std::env::set_var(REAPER_INTERVAL_ENV, "7");
        let d = resolve_reaper_interval();
        assert_eq!(d.as_secs(), 7);
        std::env::remove_var(REAPER_INTERVAL_ENV);
    }

    /// AC #3: assert that the spawned child receives a
    /// `CLAUDE_CODE_OAUTH_TOKEN` env var that came from `.loom/tokens/`.
    /// We achieve this with a fixture tokens dir and a fixture spawn-claude
    /// that selects from it. The real `spawn-claude.sh` would invoke the
    /// Python selector; here we substitute a thin shell that picks the
    /// first token file and exports it, so the test exercises the dispatch
    /// path end-to-end without depending on a working Python install.
    #[test]
    #[serial]
    fn dispatch_propagates_oauth_token_from_tokens_dir() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        // Build a fixture tokens dir with one token.
        let tokens_dir = workspace.join(".loom").join("tokens");
        std::fs::create_dir_all(&tokens_dir).unwrap();
        let token_value = "sk-ant-oat01-fixture-token-value";
        let token_path = tokens_dir.join("agent-1.token");
        std::fs::write(&token_path, token_value).unwrap();
        let mut perms = std::fs::metadata(&token_path).unwrap().permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&token_path, perms).unwrap();

        // Build a fake spawn-claude that selects the first token file and
        // records the exported CLAUDE_CODE_OAUTH_TOKEN. This is a stand-in
        // for the real wrapper's Python-backed selection — the assertion
        // is that the *registry's* dispatch path produces a child whose
        // OAuth token came from `.loom/tokens/`.
        let scripts_dir = workspace.join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        let record_log = workspace.join("oauth-record.log");
        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail
ws="${{LOOM_WORKSPACE:-{ws}}}"
tokens_dir="$ws/.loom/tokens"
token_file="$(ls "$tokens_dir"/*.token 2>/dev/null | head -n1)"
if [ -z "$token_file" ]; then
  echo "no token files in $tokens_dir" >&2
  exit 78
fi
export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$token_file")"
{{
  echo "TOKEN_SOURCE=$token_file"
  echo "CLAUDE_CODE_OAUTH_TOKEN=$CLAUDE_CODE_OAUTH_TOKEN"
  echo "argv: $*"
}} >> "{rec}"
exit 0
"#,
            ws = workspace.display(),
            rec = record_log.display()
        );
        std::fs::write(&fake_bin, script).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();

        let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
        config.spawn_bin = Some(fake_bin);
        config.skip_label_flip = true;
        let mut registry = SweepRegistry::new(config);

        let outcome = registry
            .dispatch(&SweepKind::Issue(123), None, None)
            .unwrap();
        assert!(outcome.was_new);

        let needle = format!("CLAUDE_CODE_OAUTH_TOKEN={token_value}");
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn did not record OAuth token within 10s; got: {}",
            std::fs::read_to_string(&record_log).unwrap_or_default()
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains(".loom/tokens/agent-1.token"),
            "expected TOKEN_SOURCE to point at .loom/tokens/; got: {recorded}"
        );
    }

    /// AC #4: snapshot the JSON shape produced by serializing
    /// `Vec<SweepInfo>`. If this shape changes in a future PR, this test
    /// will fail and force a deliberate update — pinning the schema.
    #[test]
    fn sweep_info_schema_snapshot() {
        let info = SweepInfo {
            sweep_id: "sweep-issue-42-1700000000".to_string(),
            kind: SweepKind::Issue(42),
            pid: 12_345,
            token_name: "agent-1.token".to_string(),
            log_path: PathBuf::from(".loom/logs/sweep-issue-42.log"),
            idempotency_key: Some("operator-key".to_string()),
            started_at: chrono::DateTime::parse_from_rfc3339("2026-06-05T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            state: SweepState::Running,
            latest_phase: Some("builder".to_string()),
            pr_number: Some(456),
            model: Some("claude-sonnet-4-6".to_string()),
        };
        let json = serde_json::to_value(vec![info]).unwrap();
        let expected = serde_json::json!([{
            "sweep_id": "sweep-issue-42-1700000000",
            "kind": {"type": "Issue", "value": 42},
            "pid": 12_345,
            "token_name": "agent-1.token",
            "log_path": ".loom/logs/sweep-issue-42.log",
            "idempotency_key": "operator-key",
            "started_at": "2026-06-05T10:00:00Z",
            "state": {"state": "Running"},
            "latest_phase": "builder",
            "pr_number": 456,
            "model": "claude-sonnet-4-6",
        }]);
        assert_eq!(
            json, expected,
            "SweepInfo wire schema drifted — update the snapshot intentionally if this is desired"
        );

        // model=None is omitted from the wire (skip_serializing_if), and
        // pre-#3482 JSON without the field deserializes to model=None —
        // the backward-compat half of the schema pin.
        let legacy_json = serde_json::json!({
            "sweep_id": "sweep-issue-43-1700000000",
            "kind": {"type": "Issue", "value": 43},
            "pid": 1,
            "token_name": "unknown",
            "log_path": ".loom/logs/sweep-issue-43.log",
            "started_at": "2026-06-05T10:00:00Z",
            "state": {"state": "Running"},
        });
        let legacy: SweepInfo =
            serde_json::from_value(legacy_json).expect("legacy SweepInfo without model must parse");
        assert_eq!(legacy.model, None);
        let reserialized = serde_json::to_value(&legacy).unwrap();
        assert!(
            reserialized.get("model").is_none(),
            "model=None must be omitted from serialized SweepInfo"
        );

        // Also pin the variant shapes for Exited and Crashed.
        let exited = serde_json::to_value(SweepState::Exited {
            code: Some(0),
            at: chrono::DateTime::parse_from_rfc3339("2026-06-05T10:05:00Z")
                .unwrap()
                .with_timezone(&Utc),
        })
        .unwrap();
        assert_eq!(
            exited,
            serde_json::json!({
                "state": "Exited",
                "details": {"code": 0, "at": "2026-06-05T10:05:00Z"}
            })
        );

        let crashed = serde_json::to_value(SweepState::Crashed {
            at: chrono::DateTime::parse_from_rfc3339("2026-06-05T10:05:00Z")
                .unwrap()
                .with_timezone(&Utc),
        })
        .unwrap();
        assert_eq!(
            crashed,
            serde_json::json!({
                "state": "Crashed",
                "details": {"at": "2026-06-05T10:05:00Z"}
            })
        );
    }

    // ========================================================================
    // Phase C tests (Issue #3455)
    // ========================================================================

    #[test]
    fn get_status_returns_clone_or_none() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        assert!(registry.get_status("missing").is_none());

        let sweep_id = "sweep-status-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(42),
                pid: 1234,
                token_name: "agent-1.token".into(),
                log_path: registry.compute_log_path(42),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: Some("builder".into()),
                pr_number: None,
                model: None,
            },
        );

        let info = registry.get_status(&sweep_id).expect("status should exist");
        assert_eq!(info.pid, 1234);
        assert!(matches!(info.kind, SweepKind::Issue(42)));
        assert!(matches!(info.state, SweepState::Running));
    }

    #[test]
    fn tail_log_returns_last_n_lines() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let log_path = registry.compute_log_path(99);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        let body = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&log_path, body).unwrap();

        let sweep_id = "sweep-tail-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(99),
                pid: 1,
                token_name: "unknown".into(),
                log_path: log_path.clone(),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
            },
        );

        let (path, tail) = registry.tail_log(&sweep_id, 5).unwrap();
        assert_eq!(path, log_path);
        assert_eq!(tail.len(), 5);
        assert_eq!(tail[0], "line 16");
        assert_eq!(tail[4], "line 20");

        // Requesting more lines than the file has should yield the whole file.
        let (_path, tail) = registry.tail_log(&sweep_id, 1000).unwrap();
        assert_eq!(tail.len(), 20);

        // Zero is honored (returns empty vec).
        let (_path, tail) = registry.tail_log(&sweep_id, 0).unwrap();
        assert!(tail.is_empty());
    }

    #[test]
    fn tail_log_rejects_unknown_sweep() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());
        let err = registry.tail_log("nope", 10).unwrap_err();
        assert!(err.to_string().contains("unknown sweep_id"));
    }

    #[test]
    fn cancel_unknown_sweep_returns_error() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let err = registry
            .cancel("does-not-exist", Duration::from_millis(50))
            .unwrap_err();
        assert!(err.to_string().contains("unknown sweep_id"));
    }

    #[test]
    fn cancel_on_already_terminal_is_idempotent_noop() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let sweep_id = "sweep-already-exited".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(11),
                pid: 1,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(11),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Exited {
                    code: Some(0),
                    at: Utc::now(),
                },
                latest_phase: None,
                pr_number: None,
                model: None,
            },
        );

        let outcome = registry
            .cancel(&sweep_id, Duration::from_millis(50))
            .unwrap();
        assert!(!outcome.was_running);
        assert!(!outcome.sigkill_sent);
        // State should remain Exited (not flipped to Exited{None, now}).
        let info = registry.get(&sweep_id).unwrap();
        if let SweepState::Exited { code, .. } = &info.state {
            assert_eq!(*code, Some(0));
        } else {
            panic!("state should remain Exited");
        }
    }

    #[test]
    fn cancel_dead_pid_transitions_to_exited_without_sigkill() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let sweep_id = "sweep-dead-pid".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(22),
                pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(22),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
            },
        );

        let outcome = registry
            .cancel(&sweep_id, Duration::from_millis(200))
            .unwrap();
        assert!(outcome.was_running);
        // SIGTERM to a dead pid is a no-op success; the poll loop sees
        // pid dead immediately and never escalates to SIGKILL.
        assert!(!outcome.sigkill_sent);
        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));
    }

    /// AC #3: SIGTERM -> grace -> SIGKILL against a fixture child that
    /// ignores SIGTERM. Spawns `bash -c 'trap "" TERM; sleep 5'`, asks
    /// the registry to cancel with a short grace, and asserts that the
    /// registry transitioned + sigkill_sent=true. We then `wait()` on the
    /// `Child` handle to reap the zombie before asserting liveness.
    #[test]
    fn cancel_escalates_to_sigkill_when_child_ignores_sigterm() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Spawn a real child that traps SIGTERM and sleeps for 30s.
        // We need a real PID so SIGTERM/SIGKILL paths are exercised end
        // to end. We keep the Child handle so we can `wait()` after the
        // cancel — without that, SIGKILL leaves the child as a zombie
        // and `kill(pid, 0)` still returns success (the PID is still in
        // the process table).
        let mut child = Command::new("bash")
            .arg("-c")
            .arg("trap '' TERM; sleep 30")
            .spawn()
            .expect("spawn fixture child");
        let pid = child.id();

        // Give bash a moment to install the trap before we try to TERM it.
        std::thread::sleep(Duration::from_millis(100));

        let sweep_id = "sweep-trap-term".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(77),
                pid,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(77),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
            },
        );

        // Use a short grace — long enough for SIGTERM to be delivered to
        // a healthy bash (~200ms), short enough to keep the test fast.
        let outcome = registry
            .cancel(&sweep_id, Duration::from_millis(500))
            .expect("cancel should succeed");
        assert!(outcome.was_running);
        assert!(
            outcome.sigkill_sent,
            "trap '' TERM child should have survived SIGTERM and escalated to SIGKILL"
        );

        // Reap the zombie so the PID is truly gone from the process table.
        let exit_status = child.wait().expect("wait on cancelled child");
        // Exit status: killed by SIGKILL means no clean exit code on Unix;
        // `success()` should be false. We don't assert specifics — the
        // platform's signal-vs-exit-code reporting varies.
        assert!(!exit_status.success(), "child should not have exited cleanly after SIGKILL");

        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));
    }

    #[test]
    fn cancel_emits_exited_and_completed_events() {
        // Bus emission path: cancel a dead-pid sweep and confirm we
        // see sweep.issue.{N}.exited + sweep.global.completed.
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let sweep_id = "sweep-cancel-event".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(88),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: registry.compute_log_path(88),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
            },
        );

        registry
            .cancel(&sweep_id, Duration::from_millis(100))
            .unwrap();

        // Drain two events synchronously (cancel emits inline).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut saw_exited = false;
            let mut saw_completed = false;
            for _ in 0..2 {
                match sub.recv().await.unwrap() {
                    Event::SweepExited { issue, .. } => {
                        assert_eq!(issue, 88);
                        saw_exited = true;
                    }
                    Event::SweepGlobalCompleted { outcome, .. } => {
                        assert_eq!(outcome, SweepOutcome::Exited);
                        saw_completed = true;
                    }
                    other => panic!("unexpected event: {other:?}"),
                }
            }
            assert!(saw_exited);
            assert!(saw_completed);
        });
    }
}
