//! Auto-update state that survives a daemon restart (Issue #10713).
//!
//! Before this module every clock and counter in [`AutoUpdateState`] lived in
//! memory, so any restart (an operator unsticking a host, a crash, or the roll's
//! own restart) reset things that are meant to accumulate:
//!
//! - **The settle ceiling** (#10418). `first_stale_since` bounds how long a
//!   steady release stream can defer a roll, at `6 × settleSecs`. On the AWS
//!   workers that is 24 h, and every restart started it again.
//!
//! (It also persisted which roll window had been used, #10188 item 2. #10885
//! removed roll windows, and with them the `window` field: an older file's
//! `window` key is ignored on load and dropped on the next write. The schema
//! version is unchanged, so a rollback onto a binary that still has windows
//! reads this binary's file, finds no `window`, and keeps the settle clocks.)
//!
//! (It also persisted the #8998/#9010 stall detector's state. #10831 removed
//! that detector with the wait-for-zero roll it guarded, and with it the
//! `stall` field: an older file's `stall` key is ignored on load. Pause-and-roll
//! needs no stall state across its own restart: a roll no longer waits on
//! anything a restart could forget.)
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
//! Written atomically: a temp file in the same directory, `fsync`, `rename`,
//! then a best-effort `fsync` of the directory (#10880). A
//! crash mid-write leaves either the old file or the new one, never half of one.
//! Loading never fails: a missing, unreadable, corrupt or other-version file is
//! a [`LoadOutcome`] that yields empty state (today's behaviour) and one log
//! line. Nothing in the state can stop a roll; at worst a clock restarts.
//!
//! # What a restart keeps
//!
//! The unsatisfiable-floor alert record (#10866) is always handed back. It
//! is valid for its floor and running version, not for a build commit, and
//! [`FloorState::set_basis`](super::floor_roll::FloorState::set_basis) keeps it
//! on the first tick only if both still match.
//!
//! The settle clocks are restored only when the binary that saved them is the
//! one running now. A different binary means a roll completed (or an operator
//! installed one), which ends the stale streak, as a successful roll always
//! has. Restoring them there would make the next release roll at once on a
//! ceiling that belongs to the previous one.
//!
//! The roll-attempt record (#10880, [`super::roll_attempt`]) is also always
//! handed back, whatever binary saved it: the binary a failed roll leaves
//! running is the one that must judge it. It is written before a roll is
//! armed, not only at the end of a tick.
//!
//! # Wall-clock times on load
//!
//! Every restored wall-clock time is bounded by `now`: a settle clock in the
//! future restores as `now`, a roll attempt's arm times are clamped to `now`
//! and its retry time to `now + 6 h`. (#10880 item 3 asked the same of the
//! drain-stall timestamps; #10831 removed that state, so it applies to these.)
//!
//! `saved_at` is **not** used to subtract the downtime from restored ages
//! (#10880 item 6, declined). Restored ages deliberately include downtime: a
//! release that has been out for two days has been quiet for two days, and
//! since #10885 the settle clocks only matter on a host with no fleet store.
//! The load line reports the file's age, and a `saved_at` in the future is one
//! WARN; the per-field clamps already bound its effect.
//!
//! # Durability
//!
//! After the rename the directory itself is synced (Unix only, best-effort:
//! a failure is logged at debug and does not fail the write), so a power loss
//! does not undo the rename. Persisting runs under its own `catch_unwind`, so
//! a panic there is a counted fault, never the end of the loop.

use super::floor_roll::alert::FloorStallState;
use super::roll_attempt::RollAttempt;
use super::{AutoUpdateState, RebuildOutcome};
use crate::observability::ops::liveness::{self, Fault};
use crate::task_liveness::AUTO_UPDATE;
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
    /// #10866: the last unsatisfiable-floor alert, absent when no such stall
    /// stands. It outlives a binary change. An older binary ignores the key,
    /// and a file without it loads as `None`, so the schema version is
    /// unchanged. (The drain detector's `stall` key was removed by #10831 and
    /// the roll window's `window` key by #10885; a file that still carries
    /// either loads, and the key is ignored.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_stall: Option<FloorStallState>,
    /// #10880: the last version roll this host armed (see
    /// [`super::roll_attempt`]). Written before the arm and restored whatever
    /// binary saved it. Absent in older files, and ignored (then dropped on
    /// the next write) by a binary older than #10880; the schema version is
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_attempt: Option<RollAttempt>,
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
    // #10880 item 5: make the rename itself durable. Best-effort.
    report_dir_sync(dir, sync_dir(dir));
    Ok(())
}

/// `fsync` the directory, so a rename into it survives a power loss.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Not supported off Unix (a directory cannot be opened to sync it there).
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// A failed directory sync costs only durability: logged at debug, never an
/// error of the write.
fn report_dir_sync(dir: &Path, result: std::io::Result<()>) {
    if let Err(e) = result {
        log::debug!("auto_update: could not sync {} after the rename: {e}", dir.display());
    }
}

/// #10880 item 6: the load line's note on `saved_at`. `Err` (a WARN) when it
/// is in the future; the per-field clamps already bound the effect, so the
/// file still loads.
fn saved_age(saved_at: DateTime<Utc>, now_utc: DateTime<Utc>) -> Result<String, String> {
    let age = now_utc - saved_at;
    if age < chrono::Duration::zero() {
        return Err(format!(
            "it was saved at {saved_at}, {}s in the future (the clock stepped back, or ran ahead \
             when it was written); restored times are clamped to now",
            -age.num_seconds()
        ));
    }
    Ok(format!("saved {}s ago", age.num_seconds()))
}

/// Where persistence writes, held by [`AutoUpdateState`]. The default is
/// disabled, so a state built for a test never touches the real state dir.
#[derive(Debug, Default)]
pub struct Persistence {
    path: Option<PathBuf>,
    /// Test hook (#10880 item 4): the next persist panics.
    #[cfg(test)]
    pub(super) panic_on_store: bool,
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
            floor_stall: self.floor.stall_state(),
            roll_attempt: self.attempt.record().cloned(),
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
        // #10866: before the binary check. A floor stall depends on the floor
        // and the running version, which `set_basis` compares on the first tick.
        let floor = match &saved.floor_stall {
            Some(stall) => format!(
                "; unsatisfiable-floor alert for floor {} on {} (last logged {}), kept if the \
                 first tick finds the same floor and running version",
                stall.floor,
                stall.running,
                stall.last_alerted_at.format("%Y-%m-%dT%H:%M:%SZ")
            ),
            None => String::new(),
        };
        self.floor.restore_stall_state(saved.floor_stall);
        // #10880: also before the binary check. The binary that comes back
        // after a roll that did not take is exactly the case it is for.
        let running = binary.split('+').next().unwrap_or(binary);
        let attempt = self
            .attempt
            .restore(saved.roll_attempt, now, now_utc, running);
        let floor = format!("{floor}{attempt}");
        if saved.binary != binary {
            return format!(
                "settle clocks DROPPED because the binary changed ({} -> {binary}), i.e. a roll \
                 completed since they were saved{floor}",
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
        let since = first_stale_since
            .map_or_else(|| "none".to_string(), |at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        format!("restored settle clocks (streak since {since}){floor}")
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
                let now_utc = Utc::now();
                let age = saved_age(saved.saved_at, now_utc).unwrap_or_else(|warning| {
                    log::warn!("auto_update: saved update state at {shown}: {warning}");
                    "saved in the future".to_string()
                });
                let restored =
                    self.apply_persisted_state(*saved, Instant::now(), now_utc, &running_binary());
                log::info!("auto_update: loaded {shown} ({age}): {restored}");
            }
            LoadOutcome::Missing => {
                log::info!("auto_update: no saved update state at {shown}; starting empty");
            }
            LoadOutcome::Corrupt(why) => log::warn!(
                "auto_update: saved update state at {shown} is corrupt ({why}); starting empty — \
                 the settle ceiling restarts from now"
            ),
            LoadOutcome::UnknownVersion(version) => log::warn!(
                "auto_update: saved update state at {shown} has schema_version {version}, this \
                 binary reads {SCHEMA_VERSION}; starting empty"
            ),
        }
        self.persist.path = Some(path);
    }

    /// Called just before the tick arms a roll: write the state ahead of the
    /// restart the roll may perform at once, which the end-of-tick save would
    /// not survive. Nothing is armed after a failed install, so nothing is
    /// written for one.
    pub(super) fn persist_before_arm(&self, outcome: &RebuildOutcome) {
        if matches!(outcome, RebuildOutcome::Success) {
            self.persist_state();
        }
    }

    /// Write the current state. Best-effort: a failure is logged, and the
    /// in-memory state is unaffected.
    ///
    /// #10880 item 4: under its own `catch_unwind`, for both callers (the end
    /// of `guarded_tick` and the pre-arm write). A panic is logged at ERROR and
    /// counted as a `panic` fault of the auto-update task; the loop continues.
    pub(super) fn persist_state(&self) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.store_now()));
        if let Err(payload) = result {
            log::error!(
                "auto_update: persisting the update state panicked ({}); the loop keeps running",
                super::runner::panic_message(payload.as_ref())
            );
            liveness::fault(AUTO_UPDATE, Fault::Panic);
        }
    }

    fn store_now(&self) {
        #[cfg(test)]
        assert!(!self.persist.panic_on_store, "test hook: persist panics");
        let Some(path) = self.persist.path.as_deref() else {
            return;
        };
        let state = self.persisted_state(Instant::now(), Utc::now(), &running_binary());
        if let Err(e) = store(path, &state) {
            log::warn!(
                "auto_update: could not persist update state to {}: {e} (a restart now resets \
                 the settle ceiling)",
                path.display()
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
