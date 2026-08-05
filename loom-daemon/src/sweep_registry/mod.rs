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
//! - Sweep checkpoints under `.loom/sweep-checkpoint/issue-<N>.json` (#3373),
//!   but **only for daemon-owned sweeps**: `.loom/sweep-checkpoint/` is shared
//!   with the in-session `/loom:sweep` path, so a checkpoint is recovered only
//!   when a daemon-owned lock (`.loom/locks/issue-<N>/`, written exclusively by
//!   `dispatch`) also existed for that issue. This keeps the daemon from
//!   ingesting phantom entries for in-session sweeps it never dispatched
//!   (#3808). See [`SweepRegistry::reconstruct`].
//! - Forge labels (`loom:issue` vs `loom:building`).
//!
//! One exception (Issue #3953): `dispatch` also writes a minimal
//! `{repo, issue, pid, started_at}` liveness record to the machine-level
//! [`crate::sweep_journal`] (`~/.loom/sweeps.json`). This is NOT sweep state
//! in the sense above — it carries no phase/status/log-path information, only
//! what `loom-recover-orphans` needs to tell a live claim from a dead one
//! after a daemon restart wipes this in-memory registry. See
//! [`crate::claim_reconciliation`] for the startup consumer.
//!
//! # Module layout (#4711)
//!
//! This directory replaces the former 19k-line `sweep_registry.rs` monolith.
//! `mod.rs` keeps the `SweepRegistry`/`SweepRegistryConfig` struct
//! definitions, small config accessors, and the shared cross-cutting types
//! (`DispatchOutcome`, `StartupRaceConfig`, …); everything else is grouped by
//! concern into a sibling file, each contributing its own
//! `impl SweepRegistry { .. }` block(s): [`model`] (dispatch model/alias
//! resolution), [`crash_signals`] (child-log parsing and crash
//! classification), [`dispatch`] (the dispatch call path + backoff),
//! [`locks`] (claim-lock lifecycle + startup reconstruction),
//! [`outcome_journal`] (the `sweep.outcome` telemetry journal),
//! [`guards`] (pre-dispatch collision/label guards), [`quarantine`]
//! (insta-crash tallying + quarantine lifecycle), [`stacking`]
//! (`depends_on` bookkeeping), [`reaper`] (`reap_once`/cancel/resume), and
//! [`watchdog`] (the hung-sweep/midbuild/review-stall watchdogs). All public
//! call paths (`crate::sweep_registry::*`) are unchanged.

use crate::capacity;
use crate::event_bus::EventBus;
use crate::peer_claims::{self, ClaimAd, PeerClaimView};
use crate::quarantine_reconciliation;
use crate::sweep_journal;
use crate::sweep_outcomes;
use crate::telemetry;
use crate::tokens_pool::bad_tokens;
use crate::tokens_pool::{self, AccountId, AccountProvider, TerminalClassification};
use crate::types::{Event, SweepId, SweepInfo, SweepKind, SweepOutcome, SweepState};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

mod crash_signals;
mod dispatch;
mod guards;
mod locks;
mod model;
mod outcome_journal;
mod quarantine;
mod reaper;
mod stacking;
#[cfg(test)]
#[allow(unused_imports)]
mod test_support;
mod watchdog;

// Re-exported at the same effective visibility each item already declares
// (most are `pub(crate)`, a handful are genuinely `pub`), so every existing
// call path through `crate::sweep_registry::*` keeps working unchanged.
// Several submodules (e.g. `stacking`) only contribute inherent
// `impl SweepRegistry` methods with nothing free-standing to import, so the
// glob below is a no-op for them -- harmless, silenced explicitly.
#[allow(unused_imports)]
pub use crash_signals::*;
#[allow(unused_imports)]
pub use dispatch::*;
#[allow(unused_imports)]
pub use guards::*;
#[allow(unused_imports)]
pub use locks::*;
#[allow(unused_imports)]
pub use model::*;
#[allow(unused_imports)]
pub use outcome_journal::*;
#[allow(unused_imports)]
pub use quarantine::*;
#[allow(unused_imports)]
pub use reaper::*;
#[allow(unused_imports)]
pub use stacking::*;
#[allow(unused_imports)]
pub use watchdog::*;

/// Environment variable for overriding the dispatch entry point used by
/// the registry. Defaults to `defaults/scripts/spawn-worker.sh` relative to
/// the workspace. Used by integration tests to substitute a fake child.
pub const SPAWN_BIN_ENV: &str = "LOOM_SWEEP_SPAWN_BIN";

/// Environment variable for overriding the workspace root used by the
/// registry. Falls back to `LOOM_WORKSPACE`, then current dir.
pub const WORKSPACE_ENV: &str = "LOOM_WORKSPACE";

/// Env var overriding this host's identity string in collision records (Issue
/// #4085). Falls back to `$HOSTNAME`, then the `hostname` binary, then
/// `"unknown-host"`. Set it when the daemon runs somewhere `$HOSTNAME` is not
/// exported (a non-interactive service unit) so cross-host collision logs stay
/// attributable.
pub const HOST_ID_ENV: &str = "LOOM_HOST_ID";

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
    /// `<workspace_root>/.loom/scripts/spawn-worker.sh` or, if absent,
    /// `<workspace_root>/defaults/scripts/spawn-worker.sh`.
    pub spawn_bin: Option<PathBuf>,
    /// Override the `gh` binary (for tests). Defaults to `gh` from `PATH`.
    pub gh_bin: Option<PathBuf>,
    /// When `true`, skip the actual label flip via `gh`. Used by unit tests
    /// that don't have GitHub credentials.
    pub skip_label_flip: bool,
    /// Override the machine-level sweep journal path (Issue #3953). Defaults
    /// to [`sweep_journal::default_journal_path`] (`~/.loom/sweeps.json`,
    /// env-overridable via `LOOM_SWEEPS_JOURNAL_PATH`). Tests set this to a
    /// tempdir path so `dispatch`/reap never touch the real machine-level
    /// file.
    pub journal_path: Option<PathBuf>,
    /// Override the durable terminal-outcomes journal path (Issue #4644).
    /// Defaults to [`sweep_outcomes::default_outcomes_path`]
    /// (`<workspace_root>/.loom/logs/sweep-outcomes.jsonl`,
    /// env-overridable via `LOOM_SWEEP_OUTCOMES_JOURNAL_PATH`). Tests set this
    /// to a tempdir path so terminal-outcome recording never touches a real
    /// workspace's log directory. Unlike [`journal_path`](Self::journal_path)
    /// (a machine-level, upsert-keyed liveness snapshot), this is a per-
    /// workspace, append-only history of every terminal sweep outcome.
    pub outcomes_journal_path: Option<PathBuf>,
    /// Override the `sweep.outcome` telemetry journal path (Issue #4704).
    /// Defaults to [`sweep_outcomes::default_outcome_telemetry_path`]
    /// (`<workspace_root>/.loom/logs/sweep-outcome-telemetry.jsonl`,
    /// env-overridable via `LOOM_SWEEP_OUTCOME_TELEMETRY_JOURNAL_PATH`). Tests
    /// set this to a tempdir path so telemetry recording never touches a real
    /// workspace's log directory. A distinct file from
    /// [`outcomes_journal_path`](Self::outcomes_journal_path) — see the
    /// `sweep_outcomes` module doc for why the two are kept separate.
    pub outcome_telemetry_path: Option<PathBuf>,
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
            journal_path: None,
            outcomes_journal_path: None,
            outcome_telemetry_path: None,
        }
    }

    /// Resolve the sweep journal path: `journal_path` explicit override, else
    /// [`sweep_journal::default_journal_path`].
    pub fn resolve_journal_path(&self) -> Result<PathBuf> {
        if let Some(ref p) = self.journal_path {
            return Ok(p.clone());
        }
        sweep_journal::default_journal_path()
    }

    /// Resolve the durable terminal-outcomes journal path (Issue #4644):
    /// `outcomes_journal_path` explicit override, else
    /// [`sweep_outcomes::default_outcomes_path`].
    #[must_use]
    pub fn resolve_outcomes_journal_path(&self) -> PathBuf {
        self.outcomes_journal_path
            .clone()
            .unwrap_or_else(|| sweep_outcomes::default_outcomes_path(&self.workspace_root))
    }

    /// Resolve the `sweep.outcome` telemetry journal path (Issue #4704):
    /// `outcome_telemetry_path` explicit override, else
    /// [`sweep_outcomes::default_outcome_telemetry_path`].
    #[must_use]
    pub fn resolve_outcome_telemetry_path(&self) -> PathBuf {
        self.outcome_telemetry_path
            .clone()
            .unwrap_or_else(|| sweep_outcomes::default_outcome_telemetry_path(&self.workspace_root))
    }

    /// Resolve the spawn binary, preferring (in order):
    /// 1. `spawn_bin` explicit override.
    /// 2. `LOOM_SWEEP_SPAWN_BIN` env var.
    /// 3. `<workspace>/.loom/scripts/spawn-worker.sh`.
    /// 4. `<workspace>/defaults/scripts/spawn-worker.sh`.
    pub fn resolve_spawn_bin(&self) -> Result<PathBuf> {
        if let Some(ref p) = self.spawn_bin {
            return Ok(p.clone());
        }
        if let Ok(path) = std::env::var(SPAWN_BIN_ENV) {
            if !path.trim().is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
        let installed = self
            .workspace_root
            .join(".loom")
            .join("scripts")
            .join("spawn-worker.sh");
        if installed.exists() {
            return Ok(installed);
        }
        let defaults = self
            .workspace_root
            .join("defaults")
            .join("scripts")
            .join("spawn-worker.sh");
        if defaults.exists() {
            return Ok(defaults);
        }
        Err(anyhow!(
            "spawn-worker.sh not found under {} (looked in .loom/scripts and defaults/scripts; \
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

    /// Whether this workspace has the `/loom:sweep` slash command installed
    /// (Issue #4027). The commands under `.claude/commands/loom/` are
    /// install-not-committed (gitignored; populated by `loom-daemon init`
    /// from `defaults/.claude/commands/loom/`), so a bare `git clone` has
    /// `.git`/`.loom` — `looks_like_workspace()` in `workspace_registry.rs`
    /// passes — but NOT this file. Dispatching `/loom:sweep <N>` into such a
    /// workspace insta-crashes the child on `Unknown command: /loom:sweep`
    /// within seconds. A cheap existence check (one `stat`), not a content
    /// check — an empty/stale file still counts as "installed"; a genuinely
    /// broken command definition is a different failure mode than "never
    /// initialized".
    #[must_use]
    pub fn has_sweep_command(&self) -> bool {
        self.workspace_root
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("sweep.md")
            .exists()
    }
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
    /// Minimum wall-clock gap enforced between consecutive child spawns to
    /// avoid the simultaneous-startup MCP-init race (Issue #3887). Defaults to
    /// `Duration::ZERO` (no stagger — byte-for-byte the pre-#3887 behavior and
    /// zero added latency in tests); `main.rs` sets the resolved
    /// env > config > default value on the production registry.
    dispatch_stagger: Duration,
    /// Instant of the most recent child spawn, used with `dispatch_stagger` to
    /// compute the stagger wait (Issue #3887). `None` until the first spawn.
    last_spawn_at: Option<Instant>,
    /// How long a freshly-dispatched sweep counts toward the work-finder's
    /// occupancy budget even with zero observed startup-proof signal (Issue
    /// #4003). Defaults to [`DEFAULT_STARTUP_PROOF_GRACE_SECS`]; `main.rs` /
    /// [`crate::workspace_pool::WorkspacePool`] set the resolved env > config >
    /// default value per workspace, mirroring `dispatch_stagger`. Past this
    /// window, a sweep with no worktree, no checkpoint, and no log output past
    /// the spawn header is excluded from [`occupied_issues`](Self::occupied_issues)
    /// — freeing its slot well before the (unchanged) startup watchdog fires.
    startup_proof_grace: Duration,
    /// Issues the watchdog has already auto-restarted once (Issue #3887). The
    /// re-dispatch is bounded to a single attempt per issue — a second hang
    /// resolves to [`WatchdogDecision::GiveUp`], never another restart.
    watchdog_retried: HashSet<u32>,
    /// Issues the watchdog has already logged a give-up for, so the loud
    /// give-up warning fires once per issue rather than every tick.
    watchdog_gaveup: HashSet<u32>,
    /// Sweeps the startup watchdog has ever observed making progress, latched so
    /// the "has progressed" signal is monotonic (Issue #4088). `sweep_made_progress`
    /// re-derives progress from mutable filesystem state (worktree / checkpoint /
    /// log) every tick, all of which are torn down at successful completion — so a
    /// *finished* sweep reads as *never started* and the memoryless
    /// [`watchdog_decision`] re-dispatches it. Once a `SweepId` lands here the
    /// startup watchdog leaves it alone forever, delegating any later crash to the
    /// mid-build-death (#3895) / review-stall (#3910) backstops, which is the
    /// division of labor the module doc already specifies.
    ///
    /// Keyed by `SweepId`, NOT issue: an issue-keyed latch would persist across
    /// re-dispatch, so a re-dispatched sweep that then genuinely hangs at startup
    /// would read as "already progressed" and never be rescued — silently
    /// defanging the watchdog and violating AC2. Grows per dispatch, so it is
    /// pruned alongside entry GC in [`reap_once`](Self::reap_once).
    watchdog_progressed: HashSet<SweepId>,
    /// Issues the mid-build-death watchdog has already recovered once (Issue
    /// #3895). Distinct from `watchdog_retried` (startup-hang, no progress):
    /// this bounds the "made progress then the child died" recovery to a
    /// single re-dispatch per issue.
    midbuild_retried: HashSet<u32>,
    /// Issues the mid-build-death watchdog has already logged a give-up for
    /// (Issue #3895), so the loud give-up warning fires once per issue.
    midbuild_gaveup: HashSet<u32>,
    /// Issues whose worktree the mid-build-death watchdog has already refused to
    /// reset because a live, untracked session still holds it (Issue #4449).
    /// Log-once bookkeeping only — it never suppresses the refusal itself, and
    /// the issue is removed again as soon as the worktree stops being in use so a
    /// later refusal (or a genuine give-up) still surfaces.
    midbuild_inuse: HashSet<u32>,
    /// Issues whose mid-build recovery the watchdog has already refused because
    /// the issue still has a confirmed-live sweep claim (Issue #4556). Log-once
    /// bookkeeping only — exactly like `midbuild_inuse`, it never suppresses the
    /// refusal itself, never consumes the single recovery retry, and is cleared
    /// as soon as the claim goes away so a later refusal still surfaces.
    midbuild_liveclaim: HashSet<u32>,
    /// Issues the review-phase stall watchdog has already restarted once (Issue
    /// #3910). Bounds the "log went silent mid-review (hung Judge/Doctor)"
    /// recovery to a single re-dispatch per issue — a second stall resolves to
    /// [`WatchdogDecision::GiveUp`], never another restart.
    review_stall_retried: HashSet<u32>,
    /// Issues the review-phase stall watchdog has already logged a give-up for
    /// (Issue #3910), so the loud give-up warning fires once per issue.
    review_stall_gaveup: HashSet<u32>,
    /// Insta-crash quarantine parameters (Issue #3939). `main.rs` /
    /// [`crate::workspace_pool::WorkspacePool`] set the resolved env > config >
    /// default value; the [`QuarantineConfig::default`] is the shipped-on default.
    quarantine_config: QuarantineConfig,
    /// Consecutive insta-crash counts per issue (Issue #3939). Incremented by the
    /// reaper on a checkpoint-less death inside the insta-crash window; reset to
    /// zero on any terminal outcome that made real progress or exited cleanly.
    insta_crash_counts: HashMap<u32, u32>,
    /// Consecutive reaper-driven resume dispatches per issue (Issue #4256, Judge
    /// residual-risk backstop). Incremented each time [`reap_once`](Self::reap_once)
    /// resume-dispatches a crashed post-Builder sweep; reset to zero when a run
    /// advances the checkpoint (real progress — the `checkpoint_written_by_run`
    /// branch). Once an issue reaches [`MAX_RESUME_ATTEMPTS`] consecutive
    /// checkpoint-less resume crashes the reaper stops resuming it, replacing the
    /// #4123 open-PR backstop that the resume path deliberately bypasses so an
    /// issue that reliably dies in the ~2s..stall window cannot resume forever.
    /// Keyed by issue like [`insta_crash_counts`](Self::insta_crash_counts).
    resume_attempt_counts: HashMap<u32, u32>,
    /// Currently-quarantined issues → the instant they were quarantined (Issue
    /// #3939). The work finder skips these until the entry ages past
    /// [`QuarantineConfig::ttl`], at which point [`reap_once`](Self::reap_once)
    /// releases it. Keyed by issue number; since each registry is scoped to one
    /// workspace root, this is effectively a `(workspace, issue)` key.
    quarantined: HashMap<u32, DateTime<Utc>>,
    /// Issues whose `loom:blocked` -> `loom:issue` label restore failed at
    /// least once (Issue #4110): [`release_quarantine_label`](Self::release_quarantine_label)
    /// is a best-effort `gh` call, and a transient failure must not silently
    /// strand the issue at `loom:blocked` forever — the in-memory quarantine
    /// state is already gone by the time the label edit runs, so this set is
    /// the only remaining record that a retry is owed. [`reap_once`](Self::reap_once)
    /// retries every entry here on each tick until the flip succeeds (or the
    /// operator fixes the forge state by hand, at which point the retried
    /// `gh issue edit` is a harmless idempotent no-op).
    pending_quarantine_release: HashSet<u32>,
    /// Whether cross-host dispatch-collision detection is enabled (Issue #4085,
    /// Phase 0 of #4028). When `true`, [`dispatch`](Self::dispatch) issues a
    /// pre-flip `gh issue view --json labels` read and classifies whether a peer
    /// host already flipped `loom:issue → loom:building`. Off by default (the
    /// probe adds one extra API round-trip); `main.rs` / [`WorkspacePool`] set
    /// the resolved env > config > default value. Detection only — a detected
    /// collision never changes dispatch behavior.
    ///
    /// [`WorkspacePool`]: crate::workspace_pool::WorkspacePool
    detect_collisions: bool,
    /// Cumulative count of cross-host dispatch collisions this registry has
    /// observed (Issue #4085). Incremented once per dispatch whose pre-flip
    /// label read showed `loom:issue` already gone (or `loom:building` already
    /// present) — i.e. another host claimed the issue first. Surfaced to
    /// operators via the work-finder's per-tick summary line. Always `0` when
    /// `detect_collisions` is `false`. Monotonic for the life of the process.
    collision_count: u64,
    /// Outbound peer-claim advertiser (Issue #4028). When present, [`dispatch`](Self::dispatch)
    /// publishes an `advertise` ad **before** the (non-atomic) label flip and
    /// [`emit_event`](Self::emit_event) publishes a `retract` ad on a terminal
    /// sweep outcome, both over the shared safehouse room via a bounded
    /// non-blocking `try_send`. `None` (the default) is a **byte-for-byte no-op**:
    /// no channel, no ad, no syscalls — set only when `safehouse.enabled` is true.
    peer_claim_publisher: Option<tokio::sync::mpsc::Sender<ClaimAd>>,
    /// Inbound peer-claim view (Issue #4028), shared with the safehouse
    /// coordination task that feeds it. When present, [`peer_claimed_issues`](Self::peer_claimed_issues)
    /// returns the issues a peer host has advertised as in-flight (TTL-bounded),
    /// which the work-finder skips. `None` (default) ⇒ the work-finder sees an
    /// empty set (no behavior change).
    peer_claims: Option<Arc<Mutex<PeerClaimView>>>,
    /// Workspace-level claude-wrapper pre-flight-death tripwire parameters
    /// (Issue #4386). `main.rs` / [`crate::workspace_pool::WorkspacePool`] set
    /// the resolved env > config > default value at provision time, mirroring
    /// [`quarantine_config`](Self::quarantine_config).
    preflight_tripwire_config: PreflightTripwireConfig,
    /// Consecutive claude-wrapper pre-flight deaths this registry's reaper has
    /// observed, **across issues** (Issue #4386) — distinct from
    /// [`insta_crash_counts`](Self::insta_crash_counts), which is per-issue and
    /// so never trips on a fleet-wide environmental failure spread across many
    /// different issues. Incremented by [`record_preflight_streak`](Self::record_preflight_streak)
    /// on a death classified as pre-flight; reset to `0` by any death that
    /// reached `# CLAUDE_CLI_START` (or by genuine checkpoint-proven progress).
    preflight_death_streak: u32,
    /// The most recent pre-flight death-class marker that fed
    /// [`preflight_death_streak`](Self::preflight_death_streak) (e.g.
    /// `"preflight-mcp-failed"`), carried into the advisory message. `None`
    /// once the streak resets to `0`.
    preflight_death_last_marker: Option<String>,
    /// Whether the workspace-level pre-flight advisory is currently tripped
    /// (Issue #4386) — mirrors the dedup discipline `daemon.capacity.advisory`
    /// / `daemon.dispatch.headroom_advisory` use, so
    /// [`Event::PreflightAdvisory`] fires only on a state-change transition,
    /// never every tick.
    preflight_advisory_tripped: bool,
    /// Half-open recovery-probe clock for the dispatch breaker (Issue #5030).
    /// While [`preflight_advisory_tripped`](Self::preflight_advisory_tripped) is
    /// `true`, [`preflight_dispatch_gate`](Self::preflight_dispatch_gate) holds
    /// new dispatch to this workspace except for one probe dispatch per
    /// [`PreflightTripwireConfig::probe_cooldown`]; this records when the last
    /// probe (or the first tick observing the trip) was let through. Cleared
    /// back to `None` the moment the advisory un-trips, so a freshly re-tripped
    /// advisory always waits a full cooldown before its first probe.
    preflight_probe_last_at: Option<DateTime<Utc>>,
    /// Wall-clock time of the most recent
    /// [`preflight_advisory_tripped`](Self::preflight_advisory_tripped)
    /// state-change transition (Issue #5029) — `None` until the first trip or
    /// clear this process observes. Stamped exclusively inside
    /// [`update_preflight_advisory`](Self::update_preflight_advisory)'s
    /// existing state-change branch (never on an unrelated tick that leaves
    /// `preflight_advisory_tripped` unchanged), so it is purely a display/
    /// reporting addition — the trip/clear *decision* and
    /// `preflight_death_streak` increment/reset semantics are untouched.
    /// Lets `loom-daemon status` show "as of" freshness so a historical,
    /// already-cleared tripped count is never mistaken for a live one.
    preflight_advisory_changed_at: Option<DateTime<Utc>>,
    /// Per-issue dispatch-backoff parameters (Issue #4485). `main.rs` /
    /// [`crate::workspace_pool::WorkspacePool`] set the resolved env > config >
    /// default value at provision time, mirroring
    /// [`quarantine_config`](Self::quarantine_config).
    dispatch_backoff_config: DispatchBackoffConfig,
    /// Per-issue dispatch backoff state (Issue #4485): consecutive failed
    /// dispatch outcomes and the instant the next attempt is allowed. Written by
    /// [`record_dispatch_failure`](Self::record_dispatch_failure) from
    /// [`reap_once`](Self::reap_once) and read by
    /// [`dispatch`](Self::dispatch)'s step-2.8 guard.
    ///
    /// Deliberately **wider** than [`insta_crash_counts`](Self::insta_crash_counts):
    /// the quarantine tally exempts account-exhaustion (#4122) and
    /// claude-wrapper pre-flight (#4386) deaths on purpose, which is exactly
    /// how an issue can be re-dispatched indefinitely without ever
    /// quarantining. This map counts *every* no-progress outcome, so the retry
    /// cadence is bounded regardless of blame.
    dispatch_backoff: HashMap<u32, DispatchBackoffState>,
    /// Trailing timestamps of this registry's own `loom:issue` <->
    /// `loom:building` label writes per issue (Issue #4485), pruned to
    /// [`DEFAULT_FLAP_WINDOW_SECS`]. Powers the flap warning in
    /// [`note_label_flip`](Self::note_label_flip) — the detection half of
    /// #4485, since nothing surfaced the original ~90-flip incident until an
    /// operator hand-inspected the forge timeline.
    ///
    /// Only *this process's* writes are visible here: a flap driven by a second
    /// daemon instance (the shape #4485 actually observed) is bounded by the
    /// backoff above but is not counted by this detector.
    label_flip_log: HashMap<u32, VecDeque<DateTime<Utc>>>,
    /// When the flap warning last fired per issue (Issue #4485), so a sustained
    /// flap logs at most once per [`DEFAULT_FLAP_WINDOW_SECS`] instead of on
    /// every flip.
    flap_warned_at: HashMap<u32, DateTime<Utc>>,
    /// Observed lifecycle-phase transitions per live sweep (Issue #4704), the
    /// raw material for the durable `sweep.outcome` record's
    /// [`phase_durations`](crate::telemetry::SweepOutcomeRecord::phase_durations).
    ///
    /// Sampled by [`reap_once`](Self::reap_once) — see
    /// [`sample_phase_transition`](Self::sample_phase_transition) — because the
    /// on-disk checkpoint (`.loom/sweep-checkpoint/issue-<N>.json`) is
    /// *overwritten* at each phase boundary and **deleted** by the sweep skill
    /// on success, so it is a point-in-time value, never a history. Keeping the
    /// observations in the registry is what lets a completed sweep's record
    /// carry a real per-phase breakdown after its checkpoint is gone.
    ///
    /// Keyed by `SweepId` (not issue) so a re-dispatch starts a fresh history,
    /// and pruned alongside entry GC in [`reap_once`](Self::reap_once) exactly
    /// like [`watchdog_progressed`](Self::watchdog_progressed). Each vector is
    /// capped at [`MAX_PHASE_OBSERVATIONS`] so a pathological Judge/Doctor loop
    /// cannot grow it without bound.
    phase_history: HashMap<SweepId, Vec<PhaseObservation>>,
    /// Most recently opportunistically-sampled `(lines_added,
    /// lines_deleted)` local diffstat per live sweep (Issue #5357), taken
    /// while its worktree still exists — see
    /// [`sample_phase_transition`](Self::sample_phase_transition)'s per-tick
    /// snapshot. Exists so a `--merge`-mode sweep's own synchronous
    /// `merge-pr.sh` worktree cleanup (which can complete inside the
    /// sweep's own process, well before this reaper ever observes its
    /// exit) does not silently erase the LOC figure `sweep.outcome` wants.
    /// Pruned alongside `phase_history` at the same terminal-entry GC site,
    /// for the same "unbounded across many dispatches" reason.
    sampled_loc: HashMap<SweepId, (i64, i64)>,
    /// Orphaned process groups awaiting SIGKILL escalation (Issue #4980).
    ///
    /// Written by [`reap_orphaned_group`](Self::reap_orphaned_group) when a
    /// crash path (the reaper's dead-leader branch, or `reconstruct()`'s
    /// stale-lock branch) finds a *dead* sweep leader whose process group still
    /// has live members, and drained by
    /// [`escalate_pending_group_reaps`](Self::escalate_pending_group_reaps) at
    /// the top of each [`reap_once`](Self::reap_once) tick.
    ///
    /// Deferring the escalation to a later tick — rather than sleeping through
    /// the grace inline — is deliberate: `reap_once` also runs on the
    /// `ListSweeps` / `GetSweepStatus` read path while holding the registry
    /// mutex, and blocking there is the 2026-07-26 wedge shape. Entries are
    /// removed as soon as the group drains, so this never accumulates.
    pending_group_reaps: HashMap<SweepId, PendingGroupReap>,
}

/// Resolve this host's identity string for collision records (Issue #4085) and
/// peer-claim advertisements (Issue #4028), precedence `LOOM_HOST_ID` env >
/// `$HOSTNAME` env > the `hostname` binary > `"unknown-host"`. This is loom's
/// single, explicit host-identity concept — derived (not a new config block) and
/// stable across restarts (`$HOSTNAME` / the machine hostname do not change).
/// safehoused stamps the socket `from` from the *persona* (all daemons share
/// `loom_daemon`), which cannot distinguish hosts, so the claim payload carries
/// this identity in its body for self-claim recognition.
#[must_use]
pub fn host_identity() -> String {
    for var in [HOST_ID_ENV, "HOSTNAME"] {
        if let Ok(v) = std::env::var(var) {
            let t = v.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    if let Ok(out) = Command::new("hostname").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "unknown-host".to_string()
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
            dispatch_stagger: Duration::ZERO,
            last_spawn_at: None,
            startup_proof_grace: Duration::from_secs(DEFAULT_STARTUP_PROOF_GRACE_SECS),
            watchdog_retried: HashSet::new(),
            watchdog_gaveup: HashSet::new(),
            watchdog_progressed: HashSet::new(),
            midbuild_retried: HashSet::new(),
            midbuild_gaveup: HashSet::new(),
            midbuild_inuse: HashSet::new(),
            midbuild_liveclaim: HashSet::new(),
            review_stall_retried: HashSet::new(),
            review_stall_gaveup: HashSet::new(),
            quarantine_config: QuarantineConfig::default(),
            insta_crash_counts: HashMap::new(),
            resume_attempt_counts: HashMap::new(),
            quarantined: HashMap::new(),
            pending_quarantine_release: HashSet::new(),
            detect_collisions: false,
            collision_count: 0,
            peer_claim_publisher: None,
            peer_claims: None,
            preflight_tripwire_config: PreflightTripwireConfig::default(),
            preflight_death_streak: 0,
            preflight_death_last_marker: None,
            preflight_advisory_tripped: false,
            preflight_probe_last_at: None,
            preflight_advisory_changed_at: None,
            dispatch_backoff_config: DispatchBackoffConfig::default(),
            dispatch_backoff: HashMap::new(),
            label_flip_log: HashMap::new(),
            flap_warned_at: HashMap::new(),
            phase_history: HashMap::new(),
            sampled_loc: HashMap::new(),
            pending_group_reaps: HashMap::new(),
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
            dispatch_stagger: Duration::ZERO,
            last_spawn_at: None,
            startup_proof_grace: Duration::from_secs(DEFAULT_STARTUP_PROOF_GRACE_SECS),
            watchdog_retried: HashSet::new(),
            watchdog_gaveup: HashSet::new(),
            watchdog_progressed: HashSet::new(),
            midbuild_retried: HashSet::new(),
            midbuild_gaveup: HashSet::new(),
            midbuild_inuse: HashSet::new(),
            midbuild_liveclaim: HashSet::new(),
            review_stall_retried: HashSet::new(),
            review_stall_gaveup: HashSet::new(),
            quarantine_config: QuarantineConfig::default(),
            insta_crash_counts: HashMap::new(),
            resume_attempt_counts: HashMap::new(),
            quarantined: HashMap::new(),
            pending_quarantine_release: HashSet::new(),
            detect_collisions: false,
            collision_count: 0,
            peer_claim_publisher: None,
            peer_claims: None,
            preflight_tripwire_config: PreflightTripwireConfig::default(),
            preflight_death_streak: 0,
            preflight_death_last_marker: None,
            preflight_advisory_tripped: false,
            preflight_probe_last_at: None,
            preflight_advisory_changed_at: None,
            dispatch_backoff_config: DispatchBackoffConfig::default(),
            dispatch_backoff: HashMap::new(),
            label_flip_log: HashMap::new(),
            flap_warned_at: HashMap::new(),
            phase_history: HashMap::new(),
            sampled_loc: HashMap::new(),
            pending_group_reaps: HashMap::new(),
        }
    }

    /// Attach (or replace) the event bus used for lifecycle emission.
    /// Additive setter — exposed so `main.rs` can construct the bus and
    /// the registry separately, then wire them together at startup.
    pub fn set_event_bus(&mut self, bus: Arc<EventBus>) {
        self.bus = Some(bus);
    }

    /// Set the minimum wall-clock gap enforced between consecutive child spawns
    /// (Issue #3887). `main.rs` calls this once at startup with the resolved
    /// env > config > default value. `Duration::ZERO` disables the stagger.
    pub fn set_dispatch_stagger(&mut self, stagger: Duration) {
        self.dispatch_stagger = stagger;
    }

    /// Read-only accessor for the configured dispatch stagger (Issue #3887).
    #[must_use]
    pub fn dispatch_stagger(&self) -> Duration {
        self.dispatch_stagger
    }

    /// Set the startup-proof occupancy grace window (Issue #4003). `main.rs` /
    /// the workspace pool call this once per workspace at provision time with
    /// the resolved env > config > default value, mirroring
    /// [`set_dispatch_stagger`](Self::set_dispatch_stagger).
    pub fn set_startup_proof_grace(&mut self, grace: Duration) {
        self.startup_proof_grace = grace;
    }

    /// Read-only accessor for the configured startup-proof occupancy grace
    /// window (Issue #4003).
    #[must_use]
    pub fn startup_proof_grace(&self) -> Duration {
        self.startup_proof_grace
    }

    /// Set the insta-crash quarantine parameters (Issue #3939). `main.rs` and the
    /// workspace pool call this once at provision time with the resolved
    /// env > config > default value.
    pub fn set_quarantine_config(&mut self, config: QuarantineConfig) {
        self.quarantine_config = config;
    }

    /// Read-only accessor for the quarantine parameters (Issue #3939).
    #[must_use]
    pub fn quarantine_config(&self) -> QuarantineConfig {
        self.quarantine_config
    }

    /// Set the per-issue dispatch-backoff parameters (Issue #4485). `main.rs`
    /// and the workspace pool call this once at provision time with the
    /// resolved env > config > default value, mirroring
    /// [`set_quarantine_config`](Self::set_quarantine_config).
    pub fn set_dispatch_backoff_config(&mut self, config: DispatchBackoffConfig) {
        self.dispatch_backoff_config = config;
    }

    /// Read-only accessor for the dispatch-backoff parameters (Issue #4485).
    #[must_use]
    pub fn dispatch_backoff_config(&self) -> DispatchBackoffConfig {
        self.dispatch_backoff_config
    }

    /// Set the claude-wrapper pre-flight-death workspace-tripwire parameters
    /// (Issue #4386). `main.rs` and the workspace pool call this once at
    /// provision time with the resolved env > config > default value,
    /// mirroring [`set_quarantine_config`](Self::set_quarantine_config).
    pub fn set_preflight_tripwire_config(&mut self, config: PreflightTripwireConfig) {
        self.preflight_tripwire_config = config;
    }

    /// Read-only accessor for the pre-flight tripwire parameters (Issue #4386).
    #[must_use]
    pub fn preflight_tripwire_config(&self) -> PreflightTripwireConfig {
        self.preflight_tripwire_config
    }

    /// Current pre-flight-death advisory state (Issue #4386), for
    /// `DaemonStatusReport`: `(tripped, message)`. `message` is always `None`
    /// when `tripped` is `false`. Issue #4644: when a live re-read of
    /// `.ranking` still confirms zero healthy accounts, the message names
    /// that specific whole-pool-dead cause instead of the generic streak
    /// wording — so `loom-daemon status` never reads as a garden-variety
    /// `.mcp.json` hint while the entire account pool is actually dead.
    #[must_use]
    pub fn preflight_advisory(&self) -> (bool, Option<String>) {
        if !self.preflight_advisory_tripped {
            return (false, None);
        }
        let marker = self
            .preflight_death_last_marker
            .as_deref()
            .unwrap_or("unknown");
        let message = self.preflight_advisory_message(self.preflight_pool_exhausted_now(), marker);
        (true, Some(message))
    }

    /// Wall-clock time of the most recent trip/clear transition backing
    /// [`preflight_advisory`](Self::preflight_advisory) (Issue #5029), or
    /// `None` before the first transition this process has observed. Purely a
    /// freshness signal for `DaemonStatusReport` / `loom-daemon status` — it
    /// does not participate in the trip/clear decision itself.
    #[must_use]
    pub fn preflight_advisory_changed_at(&self) -> Option<DateTime<Utc>> {
        self.preflight_advisory_changed_at
    }

    /// Current consecutive pre-flight-death streak (Issue #4386), for tests /
    /// observability. Read-only — production code should consult
    /// [`preflight_advisory`](Self::preflight_advisory) instead.
    #[must_use]
    pub fn preflight_death_streak(&self) -> u32 {
        self.preflight_death_streak
    }

    /// Enable or disable cross-host dispatch-collision detection (Issue #4085).
    /// `main.rs` and the workspace pool call this once at provision time with
    /// the resolved env > config > default value (see
    /// [`resolve_collision_detection`]).
    pub fn set_collision_detection(&mut self, enabled: bool) {
        self.detect_collisions = enabled;
    }

    /// Read-only accessor for whether collision detection is enabled (Issue
    /// #4085).
    #[must_use]
    pub fn collision_detection_enabled(&self) -> bool {
        self.detect_collisions
    }

    /// Cumulative count of cross-host dispatch collisions observed by this
    /// registry (Issue #4085). Always `0` when detection is disabled. Read by
    /// the work-finder to surface the running baseline on its per-tick line.
    #[must_use]
    pub fn collision_count(&self) -> u64 {
        self.collision_count
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

    /// For a `Running`/`Pending` entry with no `latest_phase` yet (i.e. every
    /// live sweep — see the [`SweepInfo::latest_phase`] doc comment), overlay
    /// the phase read live from the on-disk checkpoint
    /// (`.loom/sweep-checkpoint/issue-<N>.json`), the same source
    /// [`read_checkpoint_phase`] and the reaper's crash path already use
    /// (#4328). Checkpoint writes are unconditional at every phase boundary
    /// (`sweep-checkpoint.sh`), unlike the sweep skill's best-effort
    /// `PublishEvent` IPC call, which the registry never routed anywhere —
    /// so this is the only path that reliably surfaces live phase. The
    /// #4009 freshness guard ([`checkpoint_written_by_run`]) still applies,
    /// so a checkpoint left on disk by an earlier dispatch of the same issue
    /// is never misread as this run's progress. This is a read-time overlay
    /// only — the stored entry in `self.entries` is untouched; callers apply
    /// it to an already-cloned [`SweepInfo`].
    pub(crate) fn overlay_live_phase(&self, info: &mut SweepInfo) {
        if info.latest_phase.is_none()
            && matches!(info.state, SweepState::Running | SweepState::Pending)
        {
            if let SweepKind::Issue(issue) = info.kind {
                let checkpoint = self
                    .config
                    .checkpoint_dir()
                    .join(format!("issue-{issue}.json"));
                if checkpoint_written_by_run(&checkpoint, info.started_at) {
                    info.latest_phase = read_checkpoint_phase(&checkpoint);
                }
            }
        }
    }

    /// Return all tracked sweeps matching the optional state filter, with the
    /// live-phase overlay applied (see [`Self::overlay_live_phase`], #4328).
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
            .map(|mut info| {
                self.overlay_live_phase(&mut info);
                info
            })
            .collect()
    }

    /// Internal helper: publish an event on the attached bus (if any).
    /// Best-effort — logs a debug line if no subscribers are listening.
    ///
    /// Sweep-scoped events are stamped with this registry's owning workspace
    /// root (Issue #3929) so multi-repo subscribers can disambiguate two managed
    /// repos' issue #N — the topic string is unchanged; `repo` lives in the
    /// payload only.
    pub(crate) fn emit_event(&self, mut event: Event) {
        // Peer-claim early retraction (Issue #4028): a terminal outcome for one
        // of THIS host's sweeps retracts its soft claim over the room so peers
        // free the issue before its TTL would lapse. Centralized here so every
        // Exited/Crashed emit site is covered by one insertion. Best-effort /
        // non-blocking / fail-open (a no-op when no publisher is attached).
        match &event {
            Event::SweepExited { issue, .. } | Event::SweepCrashed { issue, .. } => {
                self.publish_peer_claim(peer_claims::ClaimKind::Retract, *issue);
            }
            _ => {}
        }
        event.set_repo_if_absent(&self.config.workspace_root.display().to_string());
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

    pub(crate) fn find_running_by_key(&self, key: &str) -> Option<&SweepInfo> {
        self.entries.values().find(|info| {
            matches!(info.state, SweepState::Running | SweepState::Pending)
                && info.idempotency_key.as_deref() == Some(key)
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;
    use tempfile::tempdir;

    #[test]
    #[serial]
    fn resolve_spawn_bin_prefers_worker_install_then_source_tree() {
        let dir = tempdir().unwrap();
        let config = SweepRegistryConfig::new(dir.path().to_path_buf());
        std::env::remove_var(SPAWN_BIN_ENV);

        let defaults = dir.path().join("defaults/scripts/spawn-worker.sh");
        std::fs::create_dir_all(defaults.parent().unwrap()).unwrap();
        std::fs::write(&defaults, "# fixture\n").unwrap();
        assert_eq!(config.resolve_spawn_bin().unwrap(), defaults);

        let installed = dir.path().join(".loom/scripts/spawn-worker.sh");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, "# fixture\n").unwrap();
        assert_eq!(config.resolve_spawn_bin().unwrap(), installed);

        let explicit = dir.path().join("explicit-worker");
        let mut explicit_config = config.clone();
        explicit_config.spawn_bin = Some(explicit.clone());
        assert_eq!(explicit_config.resolve_spawn_bin().unwrap(), explicit);

        std::env::set_var(SPAWN_BIN_ENV, "/tmp/loom-worker-override");
        assert_eq!(config.resolve_spawn_bin().unwrap(), PathBuf::from("/tmp/loom-worker-override"));
        std::env::remove_var(SPAWN_BIN_ENV);
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

    /// A `Running` entry whose issue has a fresh checkpoint (mtime at/after
    /// `started_at`) reports that phase — verbatim, not remapped — via
    /// `list()` (AC 2: this is what feeds both `loom-daemon status`'s default
    /// and `--workspace`-scoped views, since both read through this method).
    #[test]
    fn list_overlays_live_phase_from_fresh_checkpoint() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let started_at = Utc::now() - Duration::from_secs(60);
        insert_running_at(&mut registry, 4328, 0, started_at);
        write_checkpoint_with_mtime(&registry, 4328, "builder-done", SystemTime::now());

        let info = registry
            .list(None)
            .into_iter()
            .find(|i| matches!(i.kind, SweepKind::Issue(4328)))
            .expect("issue 4328 entry present");
        assert_eq!(
            info.latest_phase.as_deref(),
            Some("builder-done"),
            "fresh checkpoint phase should surface verbatim"
        );
        // Overlay is read-time only — the stored entry itself is untouched.
        assert!(
            registry
                .get("sweep-issue-4328-0")
                .unwrap()
                .latest_phase
                .is_none(),
            "list() must not mutate the stored registry entry"
        );
    }

    /// A `Running` entry with a checkpoint left on disk by an EARLIER
    /// dispatch (mtime before this run's `started_at`) must NOT surface that
    /// stale phase — the #4009 freshness guard applies here exactly as it
    /// does on the reaper's crash path.
    #[test]
    fn list_ignores_stale_checkpoint_from_earlier_dispatch() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Checkpoint written well before this run started.
        write_checkpoint_with_mtime(
            &registry,
            4329,
            "judge-done",
            SystemTime::now() - Duration::from_secs(3600),
        );
        let started_at = Utc::now();
        insert_running_at(&mut registry, 4329, 0, started_at);

        let info = registry
            .list(None)
            .into_iter()
            .find(|i| matches!(i.kind, SweepKind::Issue(4329)))
            .unwrap();
        assert!(
            info.latest_phase.is_none(),
            "a stale (pre-dispatch) checkpoint must not be reported as this run's phase"
        );
    }

    /// A `Running` entry with no checkpoint file on disk at all reports `-`
    /// (i.e. `latest_phase == None`) — the genuinely-hasn't-reached-a-phase-
    /// boundary-yet case (AC 3).
    #[test]
    fn list_reports_none_when_no_checkpoint_exists() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        insert_running_at(&mut registry, 4330, 0, Utc::now());

        let info = registry
            .list(None)
            .into_iter()
            .find(|i| matches!(i.kind, SweepKind::Issue(4330)))
            .unwrap();
        assert!(info.latest_phase.is_none());
    }

    /// An unreadable/corrupt checkpoint file degrades to `None` rather than
    /// panicking — `read_checkpoint_phase` already returns `None` on a parse
    /// failure; this locks in that the `list()` overlay inherits the same
    /// best-effort behavior.
    #[test]
    fn list_degrades_gracefully_on_corrupt_checkpoint() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let started_at = Utc::now() - Duration::from_secs(60);
        insert_running_at(&mut registry, 4331, 0, started_at);
        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        let path = cp_dir.join("issue-4331.json");
        std::fs::write(&path, "not valid json{{{").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now()).unwrap();

        let info = registry
            .list(None)
            .into_iter()
            .find(|i| matches!(i.kind, SweepKind::Issue(4331)))
            .unwrap();
        assert!(
            info.latest_phase.is_none(),
            "corrupt checkpoint JSON must not panic or fabricate a phase"
        );
    }

    /// Two workspaces (registries) each with an in-flight sweep for the SAME
    /// issue number must not leak each other's checkpoint phase — each
    /// registry only ever reads its own `workspace_root`-scoped checkpoint
    /// dir, so this is a structural guarantee, not a special case in the
    /// overlay code. Locks it in against a future refactor that might thread
    /// a shared/global checkpoint dir through by mistake.
    #[test]
    fn list_does_not_leak_phase_across_workspaces_for_same_issue_number() {
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        let (mut registry_a, _log_a) = fixture_registry(dir_a.path());
        let (mut registry_b, _log_b) = fixture_registry(dir_b.path());

        let started_at = Utc::now() - Duration::from_secs(60);
        insert_running_at(&mut registry_a, 777, 0, started_at);
        insert_running_at(&mut registry_b, 777, 0, started_at);
        write_checkpoint_with_mtime(&registry_a, 777, "builder-done", SystemTime::now());
        // registry_b has no checkpoint at all for issue 777.

        let info_a = registry_a
            .list(None)
            .into_iter()
            .find(|i| matches!(i.kind, SweepKind::Issue(777)))
            .unwrap();
        let info_b = registry_b
            .list(None)
            .into_iter()
            .find(|i| matches!(i.kind, SweepKind::Issue(777)))
            .unwrap();
        assert_eq!(info_a.latest_phase.as_deref(), Some("builder-done"));
        assert!(
            info_b.latest_phase.is_none(),
            "workspace B's issue #777 must not see workspace A's checkpoint phase"
        );
    }

    #[test]
    fn sweep_id_format() {
        let id = generate_sweep_id(&SweepKind::Issue(42));
        assert!(id.starts_with("sweep-issue-42-"));

        let pr = generate_sweep_id(&SweepKind::PrSet(vec![10, 20]));
        assert!(pr.starts_with("sweep-prs-10-20-"));
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
            // Issue #4980: a sweep is spawned as its own group leader, so a
            // live dispatch's pgid equals its pid.
            pgid: Some(12_345),
            token_name: "agent-1.token".to_string(),
            runtime: "unknown".into(),
            runtime_source: None,
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
            repo: None,
        };
        let json = serde_json::to_value(vec![info]).unwrap();
        let expected = serde_json::json!([{
            "sweep_id": "sweep-issue-42-1700000000",
            "kind": {"type": "Issue", "value": 42},
            "pid": 12_345,
            "pgid": 12_345,
            "token_name": "agent-1.token",
            "runtime": "unknown",
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
        // Pre-#4980 JSON lacks `pgid` too — it must default to None and be
        // omitted on re-serialization, so an older client/daemon on the other
        // end of the socket is unaffected.
        assert_eq!(legacy.pgid, None);
        assert_eq!(legacy.runtime, "unknown");
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
}
