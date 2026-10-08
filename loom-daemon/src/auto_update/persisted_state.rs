//! Auto-update state that survives a daemon restart (Issue #10713).
//!
//! Before this module every clock and counter in [`AutoUpdateState`] lived in
//! memory, so any restart (an operator unsticking a host, a crash, or the roll's
//! own restart) reset three things that are meant to accumulate:
//!
//! - **The settle ceiling** (#10418). `first_stale_since` bounds how long a
//!   steady release stream can defer a roll, at `6 × settleSecs`. On the AWS
//!   workers that is 24 h, and every restart started it again.
//! - **Roll-window consumption** (#10188 item 2). A restart inside an open window
//!   forgot the window was already used and could arm a second roll in it.
//! - **The stall detector** (#8998/#9010). A restart dropped a standing
//!   unsatisfiable declaration, so the next tick paused dispatch for another
//!   budget the detector had already counted as hopeless.
//!
//! # Format and location
//!
//! `<state dir>/auto_update_state.json`, where the state dir is the one
//! `auto-update-artifact-roll.json` already uses (`$LOOM_AUTO_UPDATE_STATE_DIR`,
//! else `~/.loom`). A JSON object with an integer `schema_version` (currently
//! [`SCHEMA_VERSION`]); see [`PersistedState`]. Monotonic `Instant`s are stored
//! as wall-clock UTC times and converted back on load, so an age survives the
//! restart even though the monotonic clock does not.
//!
//! # Failure handling
//!
//! Written atomically: a temp file in the same directory, `fsync`, `rename`. A
//! crash mid-write leaves either the old file or the new one, never half of one.
//! Loading never fails: a missing, unreadable, corrupt or other-version file is
//! a [`LoadOutcome`] that yields empty state (today's behaviour) and one log
//! line. Nothing in the state can stop a roll; at worst a clock restarts.
//!
//! # What a restart keeps
//!
//! The roll-window consumption is always restored (when the schedule is
//! unchanged): the restart that completes a roll is exactly the one that must
//! not re-arm in the same window.
//!
//! The settle clocks and the stall episode are restored only when the binary
//! that saved them is the one running now. A different binary means a roll
//! completed (or an operator installed one), which ends the stale streak, as a
//! successful roll always has, and proves the drain was satisfiable, which is
//! what clears a stall episode. Restoring them there would make the next release
//! roll at once on a ceiling that belongs to the previous one.

use super::stall_state::StallState;
use super::{AutoUpdateState, RebuildOutcome};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// File name of the persisted state under the state dir.
pub const STATE_FILE: &str = "auto_update_state.json";

/// The schema this binary reads and writes.
pub const SCHEMA_VERSION: u64 = 1;

/// The on-disk shape of `auto_update_state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedState {
    /// Always [`SCHEMA_VERSION`] when written by this binary.
    pub schema_version: u64,
    /// When the file was written (diagnostic only).
    pub saved_at: DateTime<Utc>,
    /// `<version>+<commit>` of the binary that wrote it. Compared on load to
    /// tell a plain restart from one that changed the binary (see the module
    /// doc).
    pub binary: String,
    /// The settle-window clocks.
    #[serde(default)]
    pub settle: SettleClocks,
    /// Roll-window consumption, `None` when windowing was off.
    #[serde(default)]
    pub window: Option<WindowConsumption>,
    /// The unsatisfiable-roll detector's state.
    #[serde(default)]
    pub stall: StallState,
}

/// The settle gate's clocks, as wall-clock times.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettleClocks {
    /// The target the streak is trying to roll onto.
    #[serde(default)]
    pub tracked_target: Option<String>,
    /// When the tracked target was first seen (the quiet-period origin).
    #[serde(default)]
    pub stale_since: Option<DateTime<Utc>>,
    /// When the stale streak began (the settle ceiling's origin, #6261).
    #[serde(default)]
    pub first_stale_since: Option<DateTime<Utc>>,
    /// When gate 4 first deferred in the current busy run (#4929).
    #[serde(default)]
    pub deferred_since: Option<DateTime<Utc>>,
}

/// Which roll windows have been used. Indices are relative to the schedule
/// (`period_secs`, `offset_secs`) recorded with them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowConsumption {
    /// The window period in force when saved.
    pub period_secs: u64,
    /// The per-host offset in force when saved.
    pub offset_secs: u64,
    /// Index of the latest window a roll was armed in.
    #[serde(default)]
    pub consumed: Option<i64>,
    /// Index of the window whose drain timed out and was abandoned.
    #[serde(default)]
    pub timed_out: Option<i64>,
}

/// What loading the state file found. Every outcome but [`Self::Loaded`] means
/// "start empty".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// A current-schema file was read.
    Loaded(Box<PersistedState>),
    /// There is no file (first start, or persistence never wrote one).
    Missing,
    /// The file exists but could not be read or parsed.
    Corrupt(String),
    /// The file is valid JSON with a `schema_version` this binary does not
    /// read (newer, or an older format).
    UnknownVersion(u64),
}

/// Where the state file lives: next to the artifact-roll record. `None` when no
/// state dir resolves (no home directory), which disables persistence.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    super::artifact_roll_record_path().and_then(|p| p.parent().map(|dir| dir.join(STATE_FILE)))
}

/// The identity compared on load: crate version plus build commit.
#[must_use]
pub fn running_binary() -> String {
    format!("{}+{}", env!("CARGO_PKG_VERSION"), crate::self_update::BUILT_COMMIT_FULL)
}

/// Read `path`. Never panics and never errors: see [`LoadOutcome`].
#[must_use]
pub fn load(path: &Path) -> LoadOutcome {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LoadOutcome::Missing,
        Err(e) => return LoadOutcome::Corrupt(format!("unreadable: {e}")),
    };
    let value: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(value) => value,
        Err(e) => return LoadOutcome::Corrupt(format!("not valid JSON: {e}")),
    };
    let Some(version) = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    else {
        return LoadOutcome::Corrupt("no integer schema_version".to_string());
    };
    if version != SCHEMA_VERSION {
        return LoadOutcome::UnknownVersion(version);
    }
    match serde_json::from_value(value) {
        Ok(state) => LoadOutcome::Loaded(Box::new(state)),
        Err(e) => LoadOutcome::Corrupt(format!("schema {version} does not parse: {e}")),
    }
}

/// Write `state` to `path` atomically: a temp file in the same directory,
/// flushed to disk, then renamed over `path`.
///
/// # Errors
///
/// The directory could not be created, or the write, sync or rename failed. The
/// previous file, if any, is untouched in every error case.
pub fn store(path: &Path, state: &PersistedState) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".auto_update_state.")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    tmp.write_all(&body)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Where persistence writes, held by [`AutoUpdateState`]. The default is
/// disabled, so a state built for a test never touches the real state dir.
#[derive(Debug, Default)]
pub struct Persistence {
    path: Option<PathBuf>,
}

/// `now - age`, or the earliest `Instant` this platform can represent when the
/// monotonic clock does not reach back that far (it may start at boot). Clamping
/// shortens the restored age, which only makes a gate wait longer, never less.
fn instant_ago(now: Instant, age: Duration) -> Instant {
    if let Some(at) = now.checked_sub(age) {
        return at;
    }
    let (mut reachable, mut unreachable) = (0_u64, age.as_secs());
    while unreachable - reachable > 1 {
        let mid = reachable + (unreachable - reachable) / 2;
        if now.checked_sub(Duration::from_secs(mid)).is_some() {
            reachable = mid;
        } else {
            unreachable = mid;
        }
    }
    now.checked_sub(Duration::from_secs(reachable))
        .unwrap_or(now)
}

/// A monotonic `at` as wall-clock time, given one reading of both clocks.
fn to_wall(at: Instant, now: Instant, now_utc: DateTime<Utc>) -> DateTime<Utc> {
    let age = chrono::Duration::from_std(now.saturating_duration_since(at))
        .unwrap_or_else(|_| chrono::Duration::zero());
    now_utc - age
}

/// A wall-clock `at` as a monotonic instant. A time in the future (the wall
/// clock stepped back since it was saved) is taken as `now`.
fn to_instant(at: DateTime<Utc>, now: Instant, now_utc: DateTime<Utc>) -> Instant {
    let age = (now_utc - at).to_std().unwrap_or(Duration::ZERO);
    instant_ago(now, age)
}

impl AutoUpdateState {
    /// The state to persist, read against one sample of both clocks.
    #[must_use]
    pub(super) fn persisted_state(
        &self,
        now: Instant,
        now_utc: DateTime<Utc>,
        binary: &str,
    ) -> PersistedState {
        let wall = |at: Option<Instant>| at.map(|at| to_wall(at, now, now_utc));
        PersistedState {
            schema_version: SCHEMA_VERSION,
            saved_at: now_utc,
            binary: binary.to_string(),
            settle: SettleClocks {
                tracked_target: self.tracked_target.clone(),
                stale_since: wall(self.stale_since),
                first_stale_since: wall(self.first_stale_since),
                deferred_since: wall(self.deferred_since),
            },
            window: self.window.consumption(),
            stall: self.roll_stall.stall_state(),
        }
    }

    /// Apply a loaded state (see the module doc for what a binary change keeps).
    /// Returns a short description of what was restored, for the log line.
    pub(super) fn apply_persisted_state(
        &mut self,
        saved: PersistedState,
        now: Instant,
        now_utc: DateTime<Utc>,
        binary: &str,
    ) -> String {
        let window = match &saved.window {
            Some(consumption) if self.window.restore_consumption(consumption) => {
                "window consumption"
            }
            Some(_) => "no window consumption (the schedule changed or windowing is off)",
            None => "no window consumption (none saved)",
        };
        if saved.binary != binary {
            return format!(
                "restored {window}; settle clocks and stall state DROPPED because the binary \
                 changed ({} -> {binary}), i.e. a roll completed since they were saved",
                saved.binary
            );
        }
        let instant = |at: Option<DateTime<Utc>>| at.map(|at| to_instant(at, now, now_utc));
        let SettleClocks {
            tracked_target,
            stale_since,
            first_stale_since,
            deferred_since,
        } = saved.settle;
        self.tracked_target = tracked_target;
        self.stale_since = instant(stale_since);
        self.first_stale_since = instant(first_stale_since);
        self.deferred_since = instant(deferred_since);
        let stall = stall_kind(&saved.stall);
        self.roll_stall.restore_stall_state(saved.stall);
        let since = first_stale_since
            .map_or_else(|| "none".to_string(), |at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        format!("restored settle clocks (streak since {since}), {window}, stall state {stall}")
    }

    /// Enable persistence at `path` and restore whatever it holds. Logs exactly
    /// one line for the load outcome. `None` leaves persistence disabled.
    pub(super) fn attach_persistence(&mut self, path: Option<PathBuf>) {
        let Some(path) = path else {
            log::info!("auto_update: no state dir resolves; update state is not persisted");
            return;
        };
        let shown = path.display().to_string();
        match load(&path) {
            LoadOutcome::Loaded(saved) => {
                let restored = self.apply_persisted_state(
                    *saved,
                    Instant::now(),
                    Utc::now(),
                    &running_binary(),
                );
                log::info!("auto_update: loaded {shown}: {restored}");
            }
            LoadOutcome::Missing => {
                log::info!("auto_update: no saved update state at {shown}; starting empty");
            }
            LoadOutcome::Corrupt(why) => log::warn!(
                "auto_update: saved update state at {shown} is corrupt ({why}); starting empty — \
                 the settle ceiling, window consumption and stall state restart from now"
            ),
            LoadOutcome::UnknownVersion(version) => log::warn!(
                "auto_update: saved update state at {shown} has schema_version {version}, this \
                 binary reads {SCHEMA_VERSION}; starting empty"
            ),
        }
        self.persist = Persistence { path: Some(path) };
    }

    /// Called just before the tick arms a drain: record the roll window it
    /// consumes and persist, ahead of the restart the roll may perform at once.
    /// Pass the result to [`Self::end_roll_arm`].
    #[must_use]
    pub(super) fn begin_roll_arm(&mut self, outcome: &RebuildOutcome) -> Option<Option<i64>> {
        if !matches!(outcome, RebuildOutcome::Success) {
            return None; // nothing is armed after a failed roll
        }
        let prior = self.window.mark_armed();
        if prior.is_some() {
            self.persist_state();
        }
        prior
    }

    /// Called after the arm attempt: a drain that was not armed gives the
    /// window back. The end-of-tick save then records the corrected state.
    pub(super) fn end_roll_arm(&mut self, prior: Option<Option<i64>>, armed: bool) {
        if let (false, Some(prior)) = (armed, prior) {
            self.window.unmark_armed(prior);
        }
    }

    /// Write the current state. Best-effort: a failure is logged, and the
    /// in-memory state is unaffected.
    pub(super) fn persist_state(&self) {
        let Some(path) = self.persist.path.as_deref() else {
            return;
        };
        let state = self.persisted_state(Instant::now(), Utc::now(), &running_binary());
        if let Err(e) = store(path, &state) {
            log::warn!(
                "auto_update: could not persist update state to {}: {e} (a restart now resets \
                 the settle ceiling, window consumption and stall state)",
                path.display()
            );
        }
    }
}

/// The variant name, for the restore log line.
fn stall_kind(stall: &StallState) -> &'static str {
    match stall {
        StallState::None { .. } => "none",
        StallState::DrainDeadlines { .. } => "drain_deadlines",
        StallState::Unsatisfiable { .. } => "unsatisfiable",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
