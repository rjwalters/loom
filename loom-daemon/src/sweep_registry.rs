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
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

/// Issue #3730: experiment-related env vars forwarded to the detached sweep
/// child via an EXPLICIT ALLOWLIST (never a blanket env_clear/copy). Byte-exact
/// names verified against `loom_tools/sweep_experiment.py` (`LOOM_MODEL_EXPERIMENT`,
/// `LOOM_MODEL_EXPERIMENT_CANARY`) and `.loom/scripts/archive-transcripts.sh`
/// (`LOOM_TRANSCRIPT_ARCHIVE`). Forwarding these makes env-based experiment
/// enablement reliable regardless of how the daemon itself was launched — an
/// operator can export them right before dispatching and have them reach the
/// child. Each is forwarded only when set to a non-empty value (see
/// `spawn_child`), so the spawn is a no-op when none are set.
pub const EXPERIMENT_ENV_ALLOWLIST: &[&str] = &[
    "LOOM_MODEL_EXPERIMENT",
    "LOOM_MODEL_EXPERIMENT_CANARY",
    "LOOM_TRANSCRIPT_ARCHIVE",
];

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
    /// Retained `Child` handles for sweeps this daemon instance spawned
    /// (Issue #3801). Keyed by `sweep_id`.
    ///
    /// The handle is kept — rather than dropped at spawn — so the reaper
    /// (and `cancel`) can `try_wait()` / `wait()` the child, which reaps
    /// the OS-level process (no `<defunct>` zombie under the daemon PID)
    /// AND yields the real exit status. `kill(pid, 0)` alone is proven
    /// insufficient: a terminated-but-unreaped child is a zombie whose PID
    /// is still allocated, so `kill(pid, 0)` reports it alive forever and
    /// the registry stays stuck `Running`.
    ///
    /// Reconstructed entries (from a prior daemon, see [`reconstruct`]) have
    /// no handle here — we never spawned them — so their liveness falls
    /// back to the `kill(pid, 0)` probe. Those entries are already admitted
    /// as terminal (`Crashed`) or point at the previous daemon's PID, so the
    /// fallback is correct for them.
    ///
    /// [`reconstruct`]: SweepRegistry::reconstruct
    children: BTreeMap<SweepId, Child>,
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
            children: BTreeMap::new(),
            bus: None,
        }
    }

    /// Construct an empty registry with the given event bus pre-attached.
    #[must_use]
    pub fn with_event_bus(config: SweepRegistryConfig, bus: Arc<EventBus>) -> Self {
        Self {
            config,
            entries: BTreeMap::new(),
            children: BTreeMap::new(),
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
    ///
    /// `effort` (issue #3716): mirrors `model` exactly. When `Some` and
    /// non-empty, the spawned child receives `--effort <level>` appended to
    /// the argv (immediately after any `--model`). When `None` or empty, no
    /// `--effort` flag is emitted at all — the session default reasoning
    /// effort is preserved end-to-end.
    ///
    /// `depends_on` (issue #3729, stacked-PR v1): when `Some(N)`, the spawned
    /// child receives `--depends-on <N>` appended to the argv (immediately
    /// after any `--model` / `--effort`), instructing `/loom:sweep` to branch
    /// its worktree/PR off `feature/issue-<N>`. When `None`, no
    /// `--depends-on` flag is emitted — byte-for-byte unchanged behavior. A
    /// single optional parent (not a list) makes diamonds unrepresentable.
    pub fn dispatch(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
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
        let (child, token_name) = self
            .spawn_child(issue_number, &log_path, &sweep_id, model, effort, depends_on)
            .context("failed to spawn sweep child")?;
        let pid = child.id();
        // Retain the handle so the reaper can `try_wait()` it (Issue #3801).
        self.children.insert(sweep_id.clone(), child);

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
            effort: effort.filter(|e| !e.is_empty()).map(String::from),
            depends_on,
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
    // Stacked-PR block-the-subtree (issue #3729, v1 item 4)
    // ------------------------------------------------------------------------

    /// Return the issue numbers of every still-live (`Running`/`Pending`)
    /// sweep whose `depends_on` names `parent`. Terminal children are
    /// excluded — they no longer need blocking. Because `depends_on` is a
    /// single optional parent, this only ever returns the *direct* children
    /// of `parent` (a linear chain hop, never a diamond).
    #[must_use]
    pub fn children_of(&self, parent: u32) -> Vec<u32> {
        self.entries
            .values()
            .filter(|info| {
                matches!(info.state, SweepState::Running | SweepState::Pending)
                    && info.depends_on == Some(parent)
            })
            .filter_map(|info| match &info.kind {
                SweepKind::Issue(n) => Some(*n),
                SweepKind::PrSet(_) => None,
            })
            .collect()
    }

    /// Block the subtree stacked on `parent` (issue #3729, v1 item 4).
    ///
    /// For each direct child of `parent` (see [`Self::children_of`]), emit a
    /// `sweep.issue.{child}.blocker` event on the existing frozen event-bus
    /// topic (#3453 — no new topic). This is the safety net that keeps a
    /// stacked child from auto-progressing (opening/merging its PR) when its
    /// parent ends in `loom:blocked`. Auto-detach (rebasing an orphaned child
    /// onto `main`) is explicitly out of v1 scope — block-the-subtree is the
    /// only cascade behavior.
    ///
    /// Returns the child issue numbers that were signalled. Emission is
    /// best-effort (no subscribers ⇒ debug log only), mirroring the rest of
    /// the reaper's event handling.
    pub fn block_children_of(&self, parent: u32, reason: &str) -> Vec<u32> {
        let children = self.children_of(parent);
        for child in &children {
            self.emit_event(Event::SweepBlocker {
                issue: *child,
                reason: reason.to_string(),
                label_added: "loom:blocked".to_string(),
            });
        }
        children
    }

    /// Best-effort check of whether `issue` currently carries the
    /// `loom:blocked` label on the forge. Used by the reaper to decide
    /// whether a terminated parent ended blocked (in which case its stacked
    /// children must be blocked too) versus completing successfully.
    ///
    /// Returns `false` on any error, when label flips are skipped (test
    /// fixtures), or when `gh` is unavailable — a conservative default that
    /// never blocks a child on an unverifiable parent state.
    fn issue_has_blocked_label(&self, issue: u32) -> bool {
        if self.config.skip_label_flip {
            return false;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("issue")
            .arg("view")
            .arg(issue.to_string())
            .arg("--json")
            .arg("labels")
            .arg("--jq")
            .arg(r#"[.labels[].name] | index("loom:blocked") != null"#);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stderr(Stdio::null());
        match cmd.output() {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim() == "true"
            }
            _ => false,
        }
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
        effort: Option<&str>,
        depends_on: Option<u32>,
    ) -> Result<(Child, String)> {
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
        // Reasoning-effort selection (issue #3716): the dispatch-param tier,
        // mirroring `--model` exactly. Appended as an explicit `--effort` arg
        // (which beats any ambient LOOM_EFFORT env inside spawn-claude.sh).
        // Empty strings are treated as unset — `--effort ""` must never be
        // emitted, so the session-default effort is preserved end-to-end.
        if let Some(e) = effort {
            if !e.is_empty() {
                cmd.arg("--effort").arg(e);
            }
        }
        // Stacked-PR dependency (issue #3729, v1): when a parent issue is
        // declared, append `--depends-on <N>` so `/loom:sweep` branches the
        // child worktree/PR off `feature/issue-<N>` instead of the default
        // branch. Absent the param, no flag is emitted (byte-for-byte
        // unchanged). Mirrors the `--model` / `--effort` append-only contract.
        if let Some(parent) = depends_on {
            cmd.arg("--depends-on").arg(parent.to_string());
        }
        cmd.env("LOOM_TERMINAL_ID", format!("daemon-{sweep_id}"))
            // Always pin LOOM_WORKSPACE to the registry's configured root so
            // spawn-claude.sh resolves `.loom/tokens/` from the same place
            // the daemon thinks the workspace is — never inheriting an
            // ambient value that might point elsewhere.
            .env(WORKSPACE_ENV, &self.config.workspace_root)
            // Issue #3730: pin the child's cwd to the resolved workspace root
            // so the child's relative `.loom/config.json` read
            // (loom_tools/sweep_experiment.py) and archive-transcripts.sh's
            // cwd-slug resolve deterministically, rather than depending on the
            // daemon's own cwd happening to be the workspace root.
            .current_dir(&self.config.workspace_root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_clone));

        // Issue #3800: put the sweep child in its OWN process group
        // (`setpgid(0, 0)` runs post-fork/pre-exec via `process_group(0)`,
        // stable since Rust 1.64). spawn-claude.sh ends in `exec claude`, so
        // the tracked PID becomes the `claude` process itself AND the leader
        // of a fresh group. `claude` forks real OS subprocesses for tool
        // execution (Bash-tool commands, MCP servers, git clones, …); those
        // descendants inherit this group. Making the child a group leader lets
        // `cancel()` signal the WHOLE group (`kill(-pgid, sig)`) so the entire
        // sweep subtree is torn down — instead of leaving orphans behind when
        // only the top-level PID is signalled.
        #[cfg(unix)]
        cmd.process_group(0);

        // Issue #3730: explicitly forward the experiment-related env vars to
        // the detached child via an EXPLICIT ALLOWLIST — never a blanket
        // env_clear/copy. Without this, `LOOM_MODEL_EXPERIMENT` /
        // `LOOM_MODEL_EXPERIMENT_CANARY` / `LOOM_TRANSCRIPT_ARCHIVE` only reach
        // the child if the daemon *itself* was launched with them; an operator
        // exporting them before dispatching would get a silent no-effect.
        //
        // `var_os` guards each name: an UNSET var is not forwarded, and an
        // empty-string value is not forwarded either (no empty-string
        // forwarding — mirrors the archiver / experiment-parser treatment of
        // empty as "unset"). This keeps the spawn a byte-for-byte no-op when
        // none of the vars are set.
        for name in EXPERIMENT_ENV_ALLOWLIST {
            if let Some(val) = std::env::var_os(name) {
                if !val.is_empty() {
                    cmd.env(name, val);
                }
            }
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {} -p '{}'", spawn_bin.display(), prompt))?;
        // Issue #3801: we RETAIN the `Child` handle (returned to `dispatch`,
        // which stores it in `self.children`) instead of dropping it. The
        // reaper `try_wait()`s it each tick so an exited child is reaped
        // (no `<defunct>` zombie) and the registry transitions to a terminal
        // state with the real exit status. spawn-claude.sh internally selects
        // a token; we record "unknown" here because the wrapper's selection is
        // logged to the per-sweep log, not exposed on stdout.
        Ok((child, "unknown".to_string()))
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

    /// Signal a sweep's process. When this daemon still owns a retained
    /// `Child` handle for `sweep_id` (i.e. we spawned it into its own process
    /// group via `process_group(0)`), the signal is delivered to the WHOLE
    /// process group (`kill(-pgid, sig)`, and the leader's pgid == its pid)
    /// so the entire `claude` subprocess subtree is reached (Issue #3800).
    /// Reconstructed entries with no handle fall back to single-PID delivery.
    fn signal_sweep(&self, sweep_id: &str, pid: u32, sig: i32) -> bool {
        if self.children.contains_key(sweep_id) {
            send_group_signal(pid, sig)
        } else {
            send_signal(pid, sig)
        }
    }

    /// Determine whether a sweep's child has terminated, reaping it when it
    /// has. Prefers the retained `Child` handle: `try_wait()` reaps an exited
    /// child (no zombie) and yields the real exit status. Falls back to the
    /// `kill(pid, 0)` liveness probe for reconstructed entries with no handle.
    ///
    /// Returns `(is_dead, exit_code)`. On a handle-observed exit the handle is
    /// removed from `self.children`; `exit_code` is `None` when the child was
    /// terminated by a signal (no clean code) or when liveness came from the
    /// fallback probe.
    fn poll_liveness(&mut self, sweep_id: &str, pid: u32) -> (bool, Option<i32>) {
        if let Some(child) = self.children.get_mut(sweep_id) {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code();
                    self.children.remove(sweep_id);
                    (true, code)
                }
                Ok(None) => (false, None),
                Err(e) => {
                    log::warn!("sweep_registry: try_wait for {sweep_id} (pid {pid}) failed: {e}");
                    let dead = !is_pid_alive(pid);
                    if dead {
                        self.children.remove(sweep_id);
                    }
                    (dead, None)
                }
            }
        } else {
            (!is_pid_alive(pid), None)
        }
    }

    /// Reap the retained `Child` handle for `sweep_id`, blocking briefly until
    /// it exits. Called after `cancel` has SIGKILL'd (or observed the exit of)
    /// the child so the OS-level zombie is reclaimed under the daemon PID.
    /// No-op when no handle is retained (reconstructed / test-injected entry).
    fn reap_handle(&mut self, sweep_id: &str) -> Option<std::process::ExitStatus> {
        self.children.remove(sweep_id).and_then(|mut child| {
            // Bounded: we only reach here once the child has exited or has
            // just been SIGKILL'd, so `wait()` returns promptly.
            child.wait().ok()
        })
    }

    /// Cancel a running sweep.
    ///
    /// Sends SIGTERM to the sweep's process group, waits up to `grace` for the
    /// child to exit, then SIGKILL to the group if still alive. On any path the
    /// registry entry is transitioned to `Exited{code: None, at: now}`
    /// and the per-issue lock is released. Emits the same lifecycle
    /// events the reaper would emit on a clean exit
    /// (`sweep.issue.{N}.exited` + `sweep.global.completed`).
    ///
    /// Returns [`CancelOutcome`] describing what actually happened. Calls
    /// against unknown sweep IDs return `Err`. Calls against already-
    /// terminal sweeps return `Ok` with `was_running = false` — cancel
    /// is idempotent so monitor-tool retries don't surface as errors.
    ///
    /// This is the **synchronous, self-contained** composition of the
    /// [`begin_cancel`](Self::begin_cancel) → [`poll_cancel`](Self::poll_cancel)
    /// → [`finish_cancel`](Self::finish_cancel) split. It holds `&mut self`
    /// (and therefore, when the registry lives behind a `Mutex`, the lock)
    /// for the entire grace window, so callers that must not freeze other
    /// registry access during the poll should orchestrate the three steps
    /// themselves and release the lock across the sleep (see the non-blocking
    /// IPC handler for `CancelSweep`, Issue #3807). Kept for direct callers
    /// and unit tests where lock contention is irrelevant.
    pub fn cancel(&mut self, sweep_id: &str, grace: Duration) -> Result<CancelOutcome> {
        let (pid, kind, started_at) = match self.begin_cancel(sweep_id)? {
            BeginCancel::AlreadyTerminal(outcome) => return Ok(outcome),
            BeginCancel::Signalled {
                pid,
                kind,
                started_at,
            } => (pid, kind, started_at),
        };

        // Poll for exit up to the grace window (100ms cadence, matching the
        // spawn-loop's shutdown-grace polling). Blocking sleep is fine here —
        // this path holds `&mut self` throughout by design.
        let poll_interval = Duration::from_millis(100);
        let deadline = std::time::Instant::now() + grace;
        let mut exited_within_grace = self.poll_cancel(sweep_id, pid);
        while !exited_within_grace && std::time::Instant::now() < deadline {
            std::thread::sleep(poll_interval);
            exited_within_grace = self.poll_cancel(sweep_id, pid);
        }

        Ok(self.finish_cancel(sweep_id, pid, &kind, started_at, exited_within_grace))
    }

    /// First, lock-scoped step of a split cancel (Issue #3807): read the
    /// target's pid/kind/liveness and, when it is still running, deliver
    /// SIGTERM to its process GROUP (Issue #3800). Returns quickly — it does
    /// **no** blocking poll — so the caller can release the registry lock
    /// before entering the (potentially multi-second) grace window.
    ///
    /// SIGTERM (signal 15) is sent to the whole process group via `kill(2)`
    /// directly rather than spawning `kill(1)` so the path is identical on
    /// macOS + Linux and doesn't depend on `PATH`. `signal_sweep` falls back
    /// to single-PID delivery for entries with no retained handle.
    ///
    /// - Unknown sweep IDs return `Err`.
    /// - Already-terminal sweeps return [`BeginCancel::AlreadyTerminal`] with
    ///   an idempotent `was_running = false` outcome (no signal, no state
    ///   change) — cancel-from-monitor retries stay idempotent.
    pub fn begin_cancel(&mut self, sweep_id: &str) -> Result<BeginCancel> {
        let (pid, kind, was_running, started_at) = {
            let info = self
                .entries
                .get(sweep_id)
                .ok_or_else(|| anyhow!("unknown sweep_id: {sweep_id}"))?;
            let alive = matches!(info.state, SweepState::Running | SweepState::Pending);
            (info.pid, info.kind.clone(), alive, info.started_at)
        };

        if !was_running {
            return Ok(BeginCancel::AlreadyTerminal(CancelOutcome {
                sweep_id: sweep_id.to_string(),
                pid,
                sigkill_sent: false,
                was_running: false,
            }));
        }

        let term_sent = self.signal_sweep(sweep_id, pid, 15);
        if !term_sent {
            log::warn!(
                "cancel_sweep: SIGTERM to pid {pid} for sweep {sweep_id} failed \
                 (process may already be dead)"
            );
        }

        Ok(BeginCancel::Signalled {
            pid,
            kind,
            started_at,
        })
    }

    /// One lock-scoped liveness poll for an in-progress cancel (Issue #3807).
    /// Returns `true` once the child has exited, reaping it via the retained
    /// `Child` handle so no `<defunct>` zombie survives (Issue #3801). The
    /// caller invokes this under a brief lock between *unlocked* sleep
    /// intervals, so the grace window never holds the registry mutex.
    pub fn poll_cancel(&mut self, sweep_id: &str, pid: u32) -> bool {
        self.poll_liveness(sweep_id, pid).0
    }

    /// Final, lock-scoped step of a split cancel (Issue #3807): SIGKILL the
    /// process group if the child did not exit within grace, reap the retained
    /// handle (Issue #3801), transition the entry to `Exited{code: None}`,
    /// release the per-issue lock, and emit the same lifecycle events a clean
    /// exit would (`sweep.issue.{N}.exited` + `sweep.global.completed`).
    ///
    /// `exited_within_grace` is the terminal result of the caller's poll loop.
    /// Returns the [`CancelOutcome`] for the (running) sweep.
    pub fn finish_cancel(
        &mut self,
        sweep_id: &str,
        pid: u32,
        kind: &SweepKind,
        started_at: DateTime<Utc>,
        exited_within_grace: bool,
    ) -> CancelOutcome {
        // SIGKILL the group if still alive.
        let sigkill_sent = if exited_within_grace {
            false
        } else {
            let killed = self.signal_sweep(sweep_id, pid, 9);
            if !killed {
                log::warn!("cancel_sweep: SIGKILL to pid {pid} also failed");
            }
            true
        };

        // Reap the retained handle so the killed leader does not linger as a
        // `<defunct>` zombie under the daemon PID (Issue #3801). A no-op when
        // the exit was already reaped in the poll loop above, or when no
        // handle is retained (reconstructed / test-injected entry).
        let _ = self.reap_handle(sweep_id);

        // Transition state, release lock, emit events.
        let now = Utc::now();
        let duration_sec = (now - started_at).num_seconds();
        if let Some(info) = self.entries.get_mut(sweep_id) {
            info.state = SweepState::Exited {
                code: None,
                at: now,
            };
        }
        if let SweepKind::Issue(issue) = kind {
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

        CancelOutcome {
            sweep_id: sweep_id.to_string(),
            pid,
            sigkill_sent,
            was_running: true,
        }
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
    #[allow(clippy::too_many_lines)]
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
            if !matches!(state, SweepState::Running | SweepState::Pending) {
                continue;
            }
            // Liveness via the retained `Child` handle when we own it: this
            // `try_wait()`s the child, reaping any zombie (Issue #3801) and
            // yielding the real exit code. Reconstructed entries with no
            // handle fall back to the `kill(pid, 0)` probe.
            let (is_dead, exit_code) = self.poll_liveness(&sweep_id, pid);
            if is_dead {
                {
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
                                    code: exit_code,
                                    at: now,
                                };
                            }
                            events_to_emit.push(Event::SweepExited {
                                issue,
                                exit_code,
                                duration_sec,
                            });
                            events_to_emit.push(Event::SweepGlobalCompleted {
                                sweep_id: sweep_id.clone(),
                                outcome: SweepOutcome::Exited,
                            });
                        }
                        // Block-the-subtree (issue #3729, v1 item 4): if this
                        // parent ended in `loom:blocked` and stacked children
                        // still depend on it, signal each child's blocker on
                        // the existing frozen topic so it does not
                        // auto-progress. Cheap-guarded: we only consult the
                        // forge label when direct children actually exist.
                        let children = self.children_of(issue);
                        if !children.is_empty() && self.issue_has_blocked_label(issue) {
                            let reason = format!(
                                "parent sweep #{issue} ended in loom:blocked; \
                                 stacked child cannot auto-progress (block-the-subtree, #3729)"
                            );
                            for child in children {
                                events_to_emit.push(Event::SweepBlocker {
                                    issue: child,
                                    reason: reason.clone(),
                                    label_added: "loom:blocked".to_string(),
                                });
                            }
                        }
                    } else {
                        if let Some(info) = self.entries.get_mut(&sweep_id) {
                            info.state = SweepState::Exited {
                                code: exit_code,
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
            // Defensive: a terminal entry should have had its handle reaped in
            // `poll_liveness` already, but drop any lingering handle so a
            // GC'd sweep never leaks a `Child` (Issue #3801).
            let _ = self.children.remove(&id);
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
                        // Effort is likewise unrecoverable from the lock (#3716).
                        effort: None,
                        // depends_on is not recorded in the lock owner (#3729).
                        depends_on: None,
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
                        model: None,      // not recoverable from a checkpoint-only entry
                        effort: None,     // not recoverable from a checkpoint-only entry
                        depends_on: None, // not recoverable from a checkpoint-only entry
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

/// Result of the lock-scoped [`begin_cancel`](SweepRegistry::begin_cancel)
/// step of a split cancel (Issue #3807).
///
/// Splitting `cancel` into begin → poll → finish lets the IPC handler run the
/// grace poll/sleep window WITHOUT holding the registry mutex, so concurrent
/// `ListSweeps` / `GetSweepStatus` / `DispatchSweep` for other sweeps are not
/// blocked for the (potentially multi-second) grace duration.
#[derive(Debug, Clone)]
pub enum BeginCancel {
    /// The sweep was already terminal — nothing was signalled. Carries the
    /// idempotent [`CancelOutcome`] (`was_running = false`) to return directly.
    AlreadyTerminal(CancelOutcome),
    /// SIGTERM has been delivered to the sweep's process group. The caller must
    /// now poll for exit (unlocked) via
    /// [`poll_cancel`](SweepRegistry::poll_cancel) and then call
    /// [`finish_cancel`](SweepRegistry::finish_cancel).
    Signalled {
        pid: u32,
        kind: SweepKind,
        started_at: DateTime<Utc>,
    },
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

/// Send a signal to the entire process GROUP led by `pgid` (Issue #3800).
///
/// POSIX `kill(-pgid, sig)` delivers `sig` to every process in the group
/// `pgid`. Because sweep children are spawned as group leaders
/// (`process_group(0)` → `setpgid(0, 0)`), a child's pgid equals its own PID,
/// so passing the tracked child PID here reaches the child AND every
/// descendant it forked (Bash-tool commands, MCP servers, git clones, …) —
/// tearing down the whole subtree instead of orphaning it.
///
/// Returns `true` on success. `pgid == 0` is rejected: `kill(0, sig)` targets
/// the *caller's* group (the daemon itself), which would be catastrophic.
#[cfg(unix)]
fn send_group_signal(pgid: u32, sig: i32) -> bool {
    if pgid == 0 {
        return false;
    }
    let Ok(pgid_t): Result<i32, _> = pgid.try_into() else {
        return false;
    };
    // Negative target = process group. See kill(2).
    libc_kill(-pgid_t, sig) == 0
}

#[cfg(not(unix))]
fn send_group_signal(_pgid: u32, _sig: i32) -> bool {
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
  printf 'LOOM_WORKSPACE=%s\n' "${{LOOM_WORKSPACE:-unset}}"
  printf 'PWD=%s\n' "$(pwd -P)"
  printf 'LOOM_MODEL_EXPERIMENT=%s\n' "${{LOOM_MODEL_EXPERIMENT:-unset}}"
  printf 'LOOM_MODEL_EXPERIMENT_CANARY=%s\n' "${{LOOM_MODEL_EXPERIMENT_CANARY:-unset}}"
  printf 'LOOM_TRANSCRIPT_ARCHIVE=%s\n' "${{LOOM_TRANSCRIPT_ARCHIVE:-unset}}"
  printf 'LOOM_TERMINAL_ID=%s\n' "${{LOOM_TERMINAL_ID:-unset}}"
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

    /// Poll `.loom/scripts/spawn-claude.sh` fixture installation: write a
    /// custom script body + exec bit into `workspace` and return a registry
    /// configured to spawn it. Used by the child-process-lifecycle tests
    /// (#3800/#3801) which need a long-lived / tree-forking child rather than
    /// the record-and-exit fake in `fixture_registry`.
    fn lifecycle_registry(workspace: &Path, script_body: &str) -> SweepRegistry {
        let scripts_dir = workspace.join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        std::fs::write(&fake_bin, script_body).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_bin) {
            let _ = f.sync_all();
        }
        let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
        config.spawn_bin = Some(fake_bin);
        config.skip_label_flip = true;
        SweepRegistry::new(config)
    }

    /// Poll until `pid` becomes alive (via `kill(pid, 0)`), up to
    /// `timeout_ms`. Returns true once alive, false on timeout.
    fn wait_until_alive(pid: u32, timeout_ms: u64) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < u128::from(timeout_ms) {
            if is_pid_alive(pid) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        is_pid_alive(pid)
    }

    /// Poll until `pid` is no longer alive (via `kill(pid, 0)`), up to
    /// `timeout_ms`. Returns the final liveness (false = dead = success).
    fn wait_until_dead(pid: u32, timeout_ms: u64) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < u128::from(timeout_ms) {
            if !is_pid_alive(pid) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        !is_pid_alive(pid)
    }

    /// Read a PID written to `path` by a fixture child, polling until a
    /// parseable integer appears (or timeout). Returns `None` on timeout.
    fn read_pid_file(path: &Path, timeout_ms: u64) -> Option<u32> {
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < u128::from(timeout_ms) {
            if let Ok(s) = std::fs::read_to_string(path) {
                if let Ok(p) = s.trim().parse::<u32>() {
                    return Some(p);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        None
    }

    #[test]
    #[serial]
    fn dispatch_happy_path_records_entry() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(42), None, None, None, None)
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
        // Issue #3716: with effort=None the spawned command must likewise NOT
        // receive a --effort flag at all (byte-for-byte unchanged default).
        assert!(
            !recorded.contains("--effort"),
            "effort=None must not emit --effort; got: {recorded}"
        );
        // #3482: model=None dispatches record no model on the entry.
        assert_eq!(registry.get(&outcome.sweep_id).unwrap().model, None);
        // #3716: effort=None dispatches record no effort on the entry.
        assert_eq!(registry.get(&outcome.sweep_id).unwrap().effort, None);

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
            .dispatch(&SweepKind::Issue(43), None, Some("claude-sonnet-4-6"), None, None)
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
            .dispatch(&SweepKind::Issue(44), None, Some(""), None, None)
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

    /// Issue #3716: an `effort` dispatch param threads through to the spawn
    /// command as an explicit `--effort <level>` argument, mirroring `--model`.
    #[test]
    #[serial]
    fn dispatch_with_effort_appends_effort_arg() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(45), None, None, Some("xhigh"), None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("argv: -p /loom:sweep 45 --effort xhigh"),
            "expected --effort in argv; got: {recorded}"
        );
        // The dispatch effort is carried on the registry entry so
        // list_sweeps / get_sweep_status report it (mirrors #3482 for model).
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().effort.as_deref(),
            Some("xhigh"),
            "dispatch effort must be recorded on the SweepInfo entry"
        );
    }

    /// Issue #3716: `model` + `effort` both set emit both flags, in the
    /// order `--model <m> --effort <e>` (effort appended right after model).
    #[test]
    #[serial]
    fn dispatch_with_model_and_effort_appends_both_args() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(46), None, Some("claude-sonnet-4-6"), Some("xhigh"), None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("argv: -p /loom:sweep 46 --model claude-sonnet-4-6 --effort xhigh"),
            "expected --model then --effort in argv; got: {recorded}"
        );
        let entry = registry.get(&outcome.sweep_id).unwrap();
        assert_eq!(entry.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(entry.effort.as_deref(), Some("xhigh"));
    }

    /// Issue #3716: an empty-string effort is treated as unset — `--effort ""`
    /// must never be emitted (no flag at all, not an empty flag).
    #[test]
    #[serial]
    fn dispatch_with_empty_effort_emits_no_effort_flag() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(47), None, None, Some(""), None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            !recorded.contains("--effort"),
            "empty effort must not emit --effort; got: {recorded}"
        );
        // Empty-string effort normalizes to None on the entry too.
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().effort,
            None,
            "empty effort must be recorded as None on the SweepInfo entry"
        );
    }

    /// Issue #3729 (stacked-PR v1): a `depends_on` dispatch param threads
    /// through to the spawn command as an explicit `--depends-on <N>`
    /// argument, mirroring `--model` / `--effort`. It is recorded on the
    /// SweepInfo entry so the reaper can block the subtree on parent failure.
    #[test]
    #[serial]
    fn dispatch_with_depends_on_appends_depends_on_arg() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(50), None, None, None, Some(49))
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("argv: -p /loom:sweep 50 --depends-on 49"),
            "expected --depends-on in argv; got: {recorded}"
        );
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().depends_on,
            Some(49),
            "dispatch depends_on must be recorded on the SweepInfo entry"
        );
    }

    /// Issue #3729: absent `depends_on`, no `--depends-on` flag is emitted —
    /// byte-for-byte unchanged behavior (opt-in, no default-path regression).
    #[test]
    #[serial]
    fn dispatch_without_depends_on_emits_no_flag() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(51), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            !recorded.contains("--depends-on"),
            "depends_on=None must not emit --depends-on; got: {recorded}"
        );
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().depends_on,
            None,
            "depends_on=None must be recorded as None on the SweepInfo entry"
        );
    }

    /// Issue #3729 (v1 item 4, block-the-subtree): `block_children_of` emits a
    /// `sweep.issue.{child}.blocker` event for every live child whose
    /// `depends_on` names the given parent — and nothing for unrelated sweeps.
    #[tokio::test]
    async fn block_children_of_emits_blocker_for_dependents_only() {
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        // Parent #60, a stacked child #61 (depends_on=60), and an unrelated
        // independent sweep #62 (depends_on=None).
        for (sid, issue, dep) in [
            ("sweep-issue-60", 60u32, None),
            ("sweep-issue-61", 61u32, Some(60u32)),
            ("sweep-issue-62", 62u32, None),
        ] {
            registry.entries.insert(
                sid.to_string(),
                SweepInfo {
                    sweep_id: sid.to_string(),
                    kind: SweepKind::Issue(issue),
                    pid: 2_147_483_640,
                    token_name: "unknown".into(),
                    log_path: registry.compute_log_path(issue),
                    idempotency_key: None,
                    started_at: Utc::now(),
                    state: SweepState::Running,
                    latest_phase: None,
                    pr_number: None,
                    model: None,
                    effort: None,
                    depends_on: dep,
                },
            );
        }

        let blocked = registry.block_children_of(60, "parent #60 blocked");
        assert_eq!(blocked, vec![61], "only #61 depends on #60");

        // Exactly one blocker event, for issue 61 on its .blocker topic.
        let ev = sub.recv().await.unwrap();
        match ev {
            Event::SweepBlocker {
                issue, label_added, ..
            } => {
                assert_eq!(issue, 61);
                assert_eq!(label_added, "loom:blocked");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Issue #3729: `children_of` only returns *live* direct children, and a
    /// terminal child is excluded (it no longer needs blocking).
    #[test]
    #[serial]
    fn children_of_returns_live_direct_children_only() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        fn mk(issue: u32, dep: Option<u32>, state: SweepState) -> SweepInfo {
            SweepInfo {
                sweep_id: format!("s{issue}"),
                kind: SweepKind::Issue(issue),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                log_path: PathBuf::from(format!(".loom/logs/sweep-issue-{issue}.log")),
                idempotency_key: None,
                started_at: Utc::now(),
                state,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: dep,
            }
        }
        registry
            .entries
            .insert("s70".into(), mk(70, None, SweepState::Running));
        registry
            .entries
            .insert("s71".into(), mk(71, Some(70), SweepState::Running));
        // Terminal child — excluded.
        registry.entries.insert(
            "s72".into(),
            mk(
                72,
                Some(70),
                SweepState::Exited {
                    code: None,
                    at: Utc::now(),
                },
            ),
        );

        let mut kids = registry.children_of(70);
        kids.sort_unstable();
        assert_eq!(kids, vec![71], "only the live child #71 is returned");
    }

    /// Issue #3730: when the experiment-related env vars are set in the daemon
    /// process, `spawn_child` forwards them (via the explicit allowlist) to the
    /// detached child, and pins the child's cwd to the workspace root.
    #[test]
    #[serial]
    fn dispatch_forwards_experiment_env_and_sets_cwd() {
        let dir = tempdir().unwrap();
        // Canonicalize because the fixture records `pwd -P` (symlink-resolved),
        // while tempdir() on macOS lives under a /var -> /private/var symlink.
        let expected_cwd = std::fs::canonicalize(dir.path()).unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        // Export the experiment vars into the daemon (test) process env just
        // before dispatch — this is exactly the operator scenario #3730 fixes.
        std::env::set_var("LOOM_MODEL_EXPERIMENT", "canary");
        std::env::set_var("LOOM_MODEL_EXPERIMENT_CANARY", "1");
        std::env::set_var("LOOM_TRANSCRIPT_ARCHIVE", "/tmp/loom-archive-3730");

        let outcome = registry
            .dispatch(&SweepKind::Issue(48), None, None, None, None)
            .expect("dispatch should succeed");

        // Clean up the process env immediately so a failure below can't leak
        // into sibling #[serial] tests.
        std::env::remove_var("LOOM_MODEL_EXPERIMENT");
        std::env::remove_var("LOOM_MODEL_EXPERIMENT_CANARY");
        std::env::remove_var("LOOM_TRANSCRIPT_ARCHIVE");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT=canary"),
            "expected LOOM_MODEL_EXPERIMENT forwarded to child; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT_CANARY=1"),
            "expected LOOM_MODEL_EXPERIMENT_CANARY forwarded to child; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_TRANSCRIPT_ARCHIVE=/tmp/loom-archive-3730"),
            "expected LOOM_TRANSCRIPT_ARCHIVE forwarded to child; got: {recorded}"
        );
        assert!(
            recorded.contains(&format!("PWD={}", expected_cwd.display())),
            "expected child cwd pinned to workspace root {}; got: {recorded}",
            expected_cwd.display()
        );
    }

    /// Issue #3730 no-op criterion: when none of the experiment env vars are
    /// set in the daemon process, `spawn_child` does NOT forward them to the
    /// child (the child observes them as unset). The cwd is still pinned to
    /// the workspace root regardless.
    #[test]
    #[serial]
    fn dispatch_does_not_forward_unset_experiment_env() {
        // Ensure a clean slate — a leaked value from another test would make
        // this a false pass.
        std::env::remove_var("LOOM_MODEL_EXPERIMENT");
        std::env::remove_var("LOOM_MODEL_EXPERIMENT_CANARY");
        std::env::remove_var("LOOM_TRANSCRIPT_ARCHIVE");

        let dir = tempdir().unwrap();
        let expected_cwd = std::fs::canonicalize(dir.path()).unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(49), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        assert!(
            wait_for_contents(&record_log, &needle, 10000),
            "fake spawn-claude.sh did not finish writing within 10s"
        );
        let recorded = std::fs::read_to_string(&record_log).unwrap();
        // The fixture prints `<VAR>=unset` when the child sees the var unset.
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT=unset"),
            "unset LOOM_MODEL_EXPERIMENT must not be forwarded; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT_CANARY=unset"),
            "unset LOOM_MODEL_EXPERIMENT_CANARY must not be forwarded; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_TRANSCRIPT_ARCHIVE=unset"),
            "unset LOOM_TRANSCRIPT_ARCHIVE must not be forwarded; got: {recorded}"
        );
        // cwd is pinned unconditionally.
        assert!(
            recorded.contains(&format!("PWD={}", expected_cwd.display())),
            "expected child cwd pinned to workspace root {}; got: {recorded}",
            expected_cwd.display()
        );
    }

    #[test]
    #[serial]
    fn dispatch_lock_collision_rejected() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let first = registry.dispatch(&SweepKind::Issue(7), None, None, None, None);
        assert!(first.is_ok());

        let second = registry.dispatch(&SweepKind::Issue(7), None, None, None, None);
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
            .dispatch(&SweepKind::Issue(99), Some("key-A".to_string()), None, None, None)
            .unwrap();
        assert!(first.was_new);

        // While still Running, a dispatch with the same key must dedup.
        // Issue #99 is the same kind, but we don't need a different issue —
        // the dedup is purely on the idempotency key.
        let second = registry
            .dispatch(&SweepKind::Issue(99), Some("key-A".to_string()), None, None, None)
            .unwrap();
        assert!(!second.was_new);
        assert_eq!(first.sweep_id, second.sweep_id);
    }

    #[test]
    fn pr_set_dispatch_rejected_in_phase_a() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let outcome = registry.dispatch(&SweepKind::PrSet(vec![1, 2, 3]), None, None, None, None);
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
            .dispatch(&SweepKind::Issue(11), None, None, None, None)
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
                effort: None,
                depends_on: None,
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
                effort: None,
                depends_on: None,
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
                effort: None,
                depends_on: None,
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
                effort: None,
                depends_on: None,
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
            .dispatch(&SweepKind::Issue(123), None, None, None, None)
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
            effort: Some("xhigh".to_string()),
            depends_on: None,
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
            "effort": "xhigh",
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
        // Pre-#3716 JSON also lacks the `effort` field — it must default to
        // None (#[serde(default)]) and be omitted on re-serialization
        // (skip_serializing_if).
        assert_eq!(legacy.effort, None);
        let reserialized = serde_json::to_value(&legacy).unwrap();
        assert!(
            reserialized.get("model").is_none(),
            "model=None must be omitted from serialized SweepInfo"
        );
        assert!(
            reserialized.get("effort").is_none(),
            "effort=None must be omitted from serialized SweepInfo"
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
                effort: None,
                depends_on: None,
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
                effort: None,
                depends_on: None,
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
                effort: None,
                depends_on: None,
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
                effort: None,
                depends_on: None,
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
                effort: None,
                depends_on: None,
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

    /// Issue #3807 core AC: the SIGTERM → grace-poll → SIGKILL escalation must
    /// NOT hold the registry lock for the full grace window. We drive the split
    /// `begin_cancel` → `poll_cancel` (unlocked sleeps between polls) →
    /// `finish_cancel` orchestration on one thread against a real trap-TERM
    /// child (forced to run the FULL grace before escalating), and assert a
    /// concurrent `get_status` on a DIFFERENT sweep returns PROMPTLY — well
    /// under the grace window — rather than blocking for it. With the old
    /// `cancel(&mut self)` (lock held throughout) the concurrent read would
    /// block for the entire grace.
    #[test]
    fn split_cancel_does_not_hold_lock_across_grace_window() {
        use std::sync::{Arc, Mutex};
        use std::thread;

        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());
        let registry = Arc::new(Mutex::new(registry));

        // A real child that traps (ignores) SIGTERM and sleeps, so the cancel
        // is forced to poll for the full grace before escalating to SIGKILL.
        let mut child = Command::new("bash")
            .arg("-c")
            .arg("trap '' TERM; sleep 30")
            .spawn()
            .expect("spawn fixture child");
        let target_pid = child.id();
        // Give bash a moment to install the trap before we TERM it.
        thread::sleep(Duration::from_millis(100));

        let target = "sweep-cancel-target".to_string();
        let other = "sweep-concurrent-reader".to_string();
        {
            let mut reg = registry.lock().unwrap();
            let target_log = reg.compute_log_path(880);
            let other_log = reg.compute_log_path(881);
            reg.entries.insert(
                target.clone(),
                SweepInfo {
                    sweep_id: target.clone(),
                    kind: SweepKind::Issue(880),
                    pid: target_pid,
                    token_name: "unknown".into(),
                    log_path: target_log,
                    idempotency_key: None,
                    started_at: Utc::now(),
                    state: SweepState::Running,
                    latest_phase: None,
                    pr_number: None,
                    model: None,
                    effort: None,
                    depends_on: None,
                },
            );
            reg.entries.insert(
                other.clone(),
                SweepInfo {
                    sweep_id: other.clone(),
                    kind: SweepKind::Issue(881),
                    pid: 2_147_483_640, // ~i32::MAX, harmless dead pid
                    token_name: "unknown".into(),
                    log_path: other_log,
                    idempotency_key: None,
                    started_at: Utc::now(),
                    state: SweepState::Running,
                    latest_phase: None,
                    pr_number: None,
                    model: None,
                    effort: None,
                    depends_on: None,
                },
            );
        }

        // 1s grace: long enough that a lock held throughout would clearly
        // block the concurrent read for ~1s, short enough to keep the test fast.
        let grace = Duration::from_millis(1000);

        // Thread A: run the split orchestration (mirrors the IPC handler),
        // releasing the mutex between the 100ms poll sleeps.
        let reg_a = Arc::clone(&registry);
        let target_a = target.clone();
        let canceller = thread::spawn(move || {
            let (pid, kind, started_at) =
                match reg_a.lock().unwrap().begin_cancel(&target_a).unwrap() {
                    BeginCancel::Signalled {
                        pid,
                        kind,
                        started_at,
                    } => (pid, kind, started_at),
                    BeginCancel::AlreadyTerminal(_) => panic!("target should be running"),
                };
            let deadline = std::time::Instant::now() + grace;
            let mut exited = reg_a.lock().unwrap().poll_cancel(&target_a, pid);
            while !exited && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(100));
                exited = reg_a.lock().unwrap().poll_cancel(&target_a, pid);
            }
            reg_a
                .lock()
                .unwrap()
                .finish_cancel(&target_a, pid, &kind, started_at, exited)
        });

        // Let thread A send SIGTERM and enter the (unlocked) poll loop.
        thread::sleep(Duration::from_millis(150));

        // Concurrent read on the OTHER sweep: must return well under the grace
        // window because the poll loop releases the mutex between polls.
        let start = std::time::Instant::now();
        let info = registry.lock().unwrap().get_status(&other);
        let elapsed = start.elapsed();
        assert!(info.is_some(), "other sweep should still be queryable");
        assert!(
            elapsed < Duration::from_millis(400),
            "concurrent get_status blocked for {elapsed:?} — the registry mutex \
             was held across the grace window (grace was {grace:?})"
        );

        let outcome = canceller.join().expect("cancel thread panicked");
        assert!(outcome.was_running);
        assert!(
            outcome.sigkill_sent,
            "trap-TERM child should have survived SIGTERM and escalated to SIGKILL"
        );

        // Reap the zombie so the PID leaves the process table.
        let exit_status = child.wait().expect("wait on cancelled child");
        assert!(!exit_status.success(), "child should not have exited cleanly after SIGKILL");

        let final_state = registry.lock().unwrap().get(&target).unwrap().state.clone();
        assert!(matches!(final_state, SweepState::Exited { .. }));
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
                effort: None,
                depends_on: None,
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

    /// Issue #3800: `cancel()` must tear down the WHOLE process tree, not just
    /// the tracked leader PID. We dispatch a fixture whose leader forks a
    /// backgrounded grandchild (both in the leader's process group, thanks to
    /// `dispatch()`'s `process_group(0)`), then cancel and assert BOTH the
    /// leader and the grandchild are gone within the grace window. A
    /// single-PID kill would orphan the backgrounded grandchild — this test
    /// fails without the group-kill fix.
    #[test]
    #[serial]
    fn cancel_terminates_whole_process_group_including_grandchild() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let gc_pidfile = workspace.join("grandchild.pid");

        // Leader (= group leader after process_group(0)) forks a background
        // grandchild that sleeps, records its PID, then blocks in a foreground
        // sleep. All three processes share the leader's process group.
        let script = format!(
            "#!/usr/bin/env bash\nsleep 300 &\necho \"$!\" > \"{gc}\"\nsleep 300\n",
            gc = gc_pidfile.display()
        );
        let mut registry = lifecycle_registry(workspace, &script);

        let outcome = registry
            .dispatch(&SweepKind::Issue(4242), None, None, None, None)
            .expect("dispatch should succeed");
        let leader_pid = outcome.pid;
        let sweep_id = outcome.sweep_id.clone();

        let gc_pid = read_pid_file(&gc_pidfile, 5000).expect("grandchild pid should be recorded");
        assert!(is_pid_alive(leader_pid), "leader should be running post-dispatch");
        assert!(is_pid_alive(gc_pid), "grandchild should be running post-dispatch");
        assert_ne!(leader_pid, gc_pid);

        // None of the processes trap SIGTERM, so a group SIGTERM tears the
        // whole tree down inside the grace window (no SIGKILL escalation).
        let cancel = registry
            .cancel(&sweep_id, Duration::from_secs(3))
            .expect("cancel should succeed");
        assert!(cancel.was_running);

        // The ENTIRE tree must be gone. The grandchild assertion is the crux:
        // it proves the signal reached the whole process group (#3800), not
        // just the tracked leader PID.
        assert!(wait_until_dead(leader_pid, 3000), "leader still alive after cancel");
        assert!(
            wait_until_dead(gc_pid, 3000),
            "grandchild survived cancel — group-kill did not reach it (single-PID regression)"
        );

        let info = registry.get(&sweep_id).unwrap();
        assert!(matches!(info.state, SweepState::Exited { .. }));
    }

    /// Issue #3801: a child killed OUT OF BAND (operator `kill -KILL`, not via
    /// `cancel()`) must be reaped by the reaper — no `<defunct>` zombie — and
    /// the registry entry must transition out of `Running`. Without the
    /// retained-`Child`-handle `try_wait()`, the killed leader becomes a
    /// zombie whose `kill(pid, 0)` still reports alive, so `reap_once()` would
    /// leave the entry stuck `Running` forever.
    #[test]
    #[serial]
    fn reaper_reaps_out_of_band_killed_child_and_transitions_state() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        let mut registry = lifecycle_registry(workspace, "#!/usr/bin/env bash\nsleep 300\n");

        let outcome = registry
            .dispatch(&SweepKind::Issue(5151), None, None, None, None)
            .expect("dispatch should succeed");
        let pid = outcome.pid;
        let sweep_id = outcome.sweep_id.clone();

        // Let the child start.
        assert!(wait_until_alive(pid, 3000), "child should have started");
        assert!(matches!(registry.get(&sweep_id).unwrap().state, SweepState::Running));

        // Kill out of band: SIGKILL the leader PID directly (mimics an
        // operator `kill -KILL <pid>`), bypassing cancel(). The leader is now
        // a zombie under the daemon (test) PID until we wait() it.
        assert!(send_signal(pid, 9), "SIGKILL to live child should succeed");

        // Drive reaper ticks. The retained handle's try_wait() reaps the
        // zombie and observes the exit, transitioning the entry to terminal.
        let mut transitioned = false;
        for _ in 0..80 {
            registry.reap_once();
            match registry.get(&sweep_id).map(|i| i.state.clone()) {
                Some(SweepState::Running | SweepState::Pending) => {}
                _ => {
                    transitioned = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            transitioned,
            "reaper did not transition the out-of-band-killed sweep out of Running"
        );

        // No zombie: because try_wait() reaped the child, kill(pid, 0) now
        // fails (the PID is no longer in the process table).
        assert!(
            wait_until_dead(pid, 2000),
            "killed child left a <defunct> zombie — reaper did not wait() it"
        );
    }
}
