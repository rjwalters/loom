//! A small persisted per-UTC-day ledger of dispatch-paused seconds attributable
//! to drain-and-restart rolls (Issue #8652, split out of #8514's AC #4).
//!
//! **Why a persisted ledger, not a counter or an event-bus aggregation.** A
//! *successful* roll ends by exiting the process, so an in-memory counter is
//! zeroed by the very event that closes the interval it is accumulating — it
//! could only ever see pauses that ended in an abort, an abandon or a #8514
//! supersede. The event bus (`event_bus.rs`) is an in-memory ring buffer with
//! the same restart amnesia *plus* eviction. So, following the precedent of
//! `auto_update::ArtifactRollRecord` ("persisted so it survives the restart the
//! roll itself performs"), the totals live in a small JSON file next to it.
//!
//! **Restart-boundary correctness.** The file also persists the `open_since`
//! of the in-progress pause:
//!
//! - the drain supervisor closes the interval *before* it exits for the relaunch
//!   ([`super::DrainState::close_paused_interval`]), not in a `Drop`;
//! - a daemon killed mid-pause (SIGKILL, host reboot) leaves `open_since` set,
//!   and the next [`PausedLedger::load`] reconciles it exactly once — closed at
//!   `min(now, open_since + MAX_DRAIN_PENDING_BUDGET_SECS)`, since no roll can
//!   keep dispatch paused past its budget cap — rather than dropping it or
//!   double-counting it on a later close.
//!
//! **Strictly observational.** Every write is best-effort: a failure is logged
//! and swallowed, a corrupt or unreadable file degrades to "fresh ledger +
//! WARN", and nothing in the #6007 fail-safe policy (`drain_refusal_decision`,
//! `refuse_roll_deadline`, `abort`) ever *reads* the ledger. It is write-side
//! only; the one reader is `loom-daemon status`.
//!
//! The ledger counts every interval during which the drain flag paused dispatch
//! — i.e. every drain started through `DrainAndRestartDaemon`, the only thing
//! that sets that flag. That includes a `fleet drain` teardown (`then_exit`),
//! whose pause ends when the daemon exits and stays down.

use super::drain_roll::{paused_secs, MAX_DRAIN_PENDING_BUDGET_SECS};
use chrono::{DateTime, Days, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How many UTC days (including today) the ledger retains. Older days are
/// pruned on every write, so the file cannot grow without bound.
pub const PAUSED_LEDGER_RETENTION_DAYS: u64 = 30;

/// File name of the ledger under the state dir (next to
/// `auto-update-artifact-roll.json`).
pub const PAUSED_LEDGER_FILE: &str = "drain-paused-ledger.json";

/// The on-disk shape. `days` maps a UTC date to the paused seconds attributed
/// to it; `open_since` is the start of the in-progress pause, if any.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct LedgerFile {
    #[serde(default)]
    days: BTreeMap<NaiveDate, u64>,
    #[serde(default)]
    open_since: Option<DateTime<Utc>>,
}

/// Per-UTC-day paused-dispatch totals, persisted across restarts (#8652).
#[derive(Debug, Clone, Default)]
pub struct PausedLedger {
    /// Where the ledger is persisted, or `None` for an in-memory-only ledger
    /// (tests, and a host with no resolvable home directory).
    path: Option<PathBuf>,
    state: LedgerFile,
}

/// Resolve the ledger path: `$LOOM_AUTO_UPDATE_STATE_DIR` when set and
/// non-empty, else `~/.loom/` — the same directory `ArtifactRollRecord` uses.
#[must_use]
pub fn default_ledger_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(crate::auto_update::AUTO_UPDATE_STATE_DIR_ENV) {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir.trim()).join(PAUSED_LEDGER_FILE));
        }
    }
    dirs::home_dir().map(|h| h.join(".loom").join(PAUSED_LEDGER_FILE))
}

/// Split `[start, end)` across UTC midnights into `(day, seconds)` segments, so
/// a pause straddling midnight is attributed to each day in proportion rather
/// than wholly to the day it ended. An empty or inverted interval yields
/// nothing. The start is clamped to the retention window so a wild clock step
/// cannot make this loop over years of days that would be pruned anyway.
#[must_use]
pub fn split_by_utc_day(start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<(NaiveDate, u64)> {
    let floor = end
        .checked_sub_days(Days::new(PAUSED_LEDGER_RETENTION_DAYS))
        .unwrap_or(start);
    let mut cur = start.max(floor);
    let mut out = Vec::new();
    while cur < end {
        let day = cur.date_naive();
        let next_midnight = day
            .checked_add_days(Days::new(1))
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map_or(end, |n| n.and_utc());
        let seg_end = next_midnight.min(end);
        let secs = paused_secs(Some(cur), seg_end);
        if secs > 0 {
            out.push((day, secs));
        }
        cur = seg_end;
    }
    out
}

impl PausedLedger {
    /// An in-memory-only ledger: records and answers queries, never touches
    /// disk. What [`super::DrainState::new`] uses, so no test ever writes a
    /// file under the real home directory.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Load the ledger from `path`, reconciling an interval a killed daemon
    /// left open. A missing file is a fresh ledger; an unreadable or corrupt
    /// one is a fresh ledger **plus a WARN** — never a panic, never an error a
    /// drain transition could observe.
    #[must_use]
    pub fn load(path: Option<PathBuf>, now: DateTime<Utc>) -> Self {
        let state = path.as_deref().map(read_state).unwrap_or_default();
        let mut ledger = Self { path, state };
        if let Some(open) = ledger.state.open_since {
            let cap = open
                + chrono::Duration::seconds(
                    i64::try_from(MAX_DRAIN_PENDING_BUDGET_SECS).unwrap_or(i64::MAX),
                );
            let end = now.min(cap);
            log::warn!(
                "drain ledger: a previous daemon exited mid-pause (open since {open}); \
                 closing that interval at {end}"
            );
            ledger.close(end);
        }
        ledger
    }

    /// Open an interval at `at` (a drain began). An interval that is somehow
    /// still open is closed at `at` first, so nothing is lost or counted twice.
    pub fn open(&mut self, at: DateTime<Utc>) {
        if let Some(prev) = self.state.open_since.take() {
            self.add(prev, at);
        }
        self.state.open_since = Some(at);
        self.prune(at);
        self.persist();
    }

    /// Close the open interval at `at` (dispatch resumed, or the daemon is
    /// about to exit for the roll). A no-op when nothing is open.
    pub fn close(&mut self, at: DateTime<Utc>) {
        let Some(start) = self.state.open_since.take() else {
            return;
        };
        self.add(start, at);
        self.prune(at);
        self.persist();
    }

    /// Per-day totals as of `now`, **including** the elapsed portion of an
    /// in-progress pause — a host sitting paused right now must not report `0`
    /// for the thing being asked about.
    #[must_use]
    pub fn totals(&self, now: DateTime<Utc>) -> BTreeMap<NaiveDate, u64> {
        let mut days = self.state.days.clone();
        if let Some(open) = self.state.open_since {
            for (day, secs) in split_by_utc_day(open, now) {
                *days.entry(day).or_default() += secs;
            }
        }
        days
    }

    fn add(&mut self, start: DateTime<Utc>, end: DateTime<Utc>) {
        for (day, secs) in split_by_utc_day(start, end) {
            let slot = self.state.days.entry(day).or_default();
            *slot = slot.saturating_add(secs);
        }
    }

    /// Drop every day older than the retention window ending at `now`.
    fn prune(&mut self, now: DateTime<Utc>) {
        let Some(oldest) = now
            .date_naive()
            .checked_sub_days(Days::new(PAUSED_LEDGER_RETENTION_DAYS - 1))
        else {
            return;
        };
        self.state.days.retain(|day, _| *day >= oldest);
    }

    /// Best-effort atomic write (temp file + rename). Failures are logged and
    /// swallowed: the ledger is observational and must never fail a drain.
    fn persist(&self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        if let Err(e) = write_state(path, &self.state) {
            log::warn!(
                "drain ledger: could not persist {}: {e} (paused-time totals for this pause \
                 may not survive a restart; the drain itself is unaffected)",
                path.display()
            );
        }
    }
}

fn read_state(path: &Path) -> LedgerFile {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            log::warn!(
                "drain ledger: {} is corrupt ({e}); starting a fresh ledger",
                path.display()
            );
            LedgerFile::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LedgerFile::default(),
        Err(e) => {
            log::warn!(
                "drain ledger: could not read {} ({e}); starting a fresh ledger",
                path.display()
            );
            LedgerFile::default()
        }
    }
}

fn write_state(path: &Path, state: &LedgerFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serialized)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    // ---- midnight-splitting arithmetic ------------------------------------

    #[test]
    fn an_interval_within_one_day_is_one_segment() {
        assert_eq!(
            split_by_utc_day(t("2026-09-22T10:00:00Z"), t("2026-09-22T11:30:00Z")),
            vec![(d("2026-09-22"), 5400)]
        );
    }

    #[test]
    fn a_pause_straddling_midnight_is_split_in_proportion() {
        // The issue's own example: a 2h budget starting at 23:10Z.
        assert_eq!(
            split_by_utc_day(t("2026-09-22T23:10:00Z"), t("2026-09-23T01:10:00Z")),
            vec![(d("2026-09-22"), 50 * 60), (d("2026-09-23"), 70 * 60)]
        );
    }

    #[test]
    fn an_interval_spanning_more_than_a_full_day_attributes_every_day() {
        let segs = split_by_utc_day(t("2026-09-22T22:00:00Z"), t("2026-09-24T03:00:00Z"));
        assert_eq!(
            segs,
            vec![
                (d("2026-09-22"), 2 * 3600),
                (d("2026-09-23"), 24 * 3600),
                (d("2026-09-24"), 3 * 3600),
            ]
        );
        let total: u64 = segs.iter().map(|(_, s)| s).sum();
        assert_eq!(total, 29 * 3600);
    }

    #[test]
    fn an_empty_or_inverted_interval_records_nothing() {
        let at = t("2026-09-22T10:00:00Z");
        assert!(split_by_utc_day(at, at).is_empty());
        assert!(split_by_utc_day(at, at - chrono::Duration::seconds(5)).is_empty());
    }

    #[test]
    fn a_wild_clock_step_is_clamped_to_the_retention_window() {
        let segs = split_by_utc_day(t("2000-01-01T00:00:00Z"), t("2026-09-22T12:00:00Z"));
        assert!(segs.len() <= usize::try_from(PAUSED_LEDGER_RETENTION_DAYS).unwrap() + 1);
    }

    // ---- ledger behaviour --------------------------------------------------

    #[test]
    fn open_then_close_records_the_interval_across_midnight() {
        let mut ledger = PausedLedger::in_memory();
        ledger.open(t("2026-09-22T23:30:00Z"));
        ledger.close(t("2026-09-23T00:15:00Z"));
        let totals = ledger.totals(t("2026-09-23T06:00:00Z"));
        assert_eq!(totals.get(&d("2026-09-22")), Some(&1800));
        assert_eq!(totals.get(&d("2026-09-23")), Some(&900));
    }

    #[test]
    fn close_without_an_open_interval_is_a_no_op() {
        let mut ledger = PausedLedger::in_memory();
        ledger.close(t("2026-09-22T10:00:00Z"));
        assert!(ledger.totals(t("2026-09-22T11:00:00Z")).is_empty());
    }

    #[test]
    fn an_in_progress_pause_counts_toward_today_and_advances_between_queries() {
        let mut ledger = PausedLedger::in_memory();
        ledger.open(t("2026-09-22T10:00:00Z"));
        ledger.close(t("2026-09-22T10:10:00Z"));
        ledger.open(t("2026-09-22T12:00:00Z"));
        let first = ledger.totals(t("2026-09-22T12:00:30Z"))[&d("2026-09-22")];
        let second = ledger.totals(t("2026-09-22T12:05:00Z"))[&d("2026-09-22")];
        assert_eq!(first, 600 + 30);
        assert_eq!(second, 600 + 300);
        assert!(second > first);
    }

    #[test]
    fn retention_is_bounded_and_pruned_on_write() {
        let mut ledger = PausedLedger::in_memory();
        let start = t("2026-08-01T10:00:00Z");
        for day in 0..45 {
            let at = start + chrono::Duration::days(day);
            ledger.open(at);
            ledger.close(at + chrono::Duration::seconds(60));
        }
        let last = start + chrono::Duration::days(44);
        let totals = ledger.totals(last);
        assert_eq!(totals.len(), usize::try_from(PAUSED_LEDGER_RETENTION_DAYS).unwrap());
        let oldest = *totals.keys().next().unwrap();
        assert_eq!(
            oldest,
            last.date_naive()
                .checked_sub_days(Days::new(PAUSED_LEDGER_RETENTION_DAYS - 1))
                .unwrap()
        );
    }

    // ---- persistence -------------------------------------------------------

    #[test]
    fn totals_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PAUSED_LEDGER_FILE);
        let now = t("2026-09-22T12:00:00Z");
        let mut ledger = PausedLedger::load(Some(path.clone()), now);
        ledger.open(t("2026-09-22T09:00:00Z"));
        ledger.close(t("2026-09-22T10:00:00Z"));

        let reloaded = PausedLedger::load(Some(path), now);
        assert_eq!(reloaded.totals(now).get(&d("2026-09-22")), Some(&3600));
    }

    #[test]
    fn an_interval_closed_before_exit_survives_the_restart_uncounted_twice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PAUSED_LEDGER_FILE);
        let mut ledger = PausedLedger::load(Some(path.clone()), t("2026-09-22T09:00:00Z"));
        ledger.open(t("2026-09-22T09:00:00Z"));
        // The supervisor's exit path closes the interval before EXIT_RESTART.
        ledger.close(t("2026-09-22T09:20:00Z"));
        drop(ledger);
        // The relaunched daemon loads it well after — no orphan to reconcile.
        let later = t("2026-09-22T15:00:00Z");
        let reloaded = PausedLedger::load(Some(path), later);
        assert_eq!(reloaded.totals(later)[&d("2026-09-22")], 1200);
    }

    #[test]
    fn a_pause_left_open_by_a_killed_daemon_is_reconciled_once_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PAUSED_LEDGER_FILE);
        let mut ledger = PausedLedger::load(Some(path.clone()), t("2026-09-22T09:00:00Z"));
        ledger.open(t("2026-09-22T09:00:00Z"));
        drop(ledger); // SIGKILL: never closed.

        // Restarted 10 minutes later: closed at `now`.
        let now = t("2026-09-22T09:10:00Z");
        let reloaded = PausedLedger::load(Some(path.clone()), now);
        assert_eq!(reloaded.totals(now)[&d("2026-09-22")], 600);
        // A second load finds nothing open — not double-counted.
        let again = PausedLedger::load(Some(path), t("2026-09-22T12:00:00Z"));
        assert_eq!(again.totals(t("2026-09-22T12:00:00Z"))[&d("2026-09-22")], 600);
    }

    #[test]
    fn a_long_dead_daemons_orphan_is_capped_at_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PAUSED_LEDGER_FILE);
        let mut ledger = PausedLedger::load(Some(path.clone()), t("2026-09-22T01:00:00Z"));
        ledger.open(t("2026-09-22T01:00:00Z"));
        drop(ledger);
        // Host was powered off for 20h; the pause cannot have outlived the cap.
        let now = t("2026-09-22T21:00:00Z");
        let reloaded = PausedLedger::load(Some(path), now);
        assert_eq!(reloaded.totals(now)[&d("2026-09-22")], MAX_DRAIN_PENDING_BUDGET_SECS);
    }

    #[test]
    fn a_corrupt_ledger_file_degrades_to_a_fresh_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PAUSED_LEDGER_FILE);
        std::fs::write(&path, "{ not json").unwrap();
        let now = t("2026-09-22T12:00:00Z");
        let mut ledger = PausedLedger::load(Some(path.clone()), now);
        assert!(ledger.totals(now).is_empty());
        // And it is writable again afterwards.
        ledger.open(t("2026-09-22T11:00:00Z"));
        ledger.close(now);
        assert_eq!(PausedLedger::load(Some(path), now).totals(now)[&d("2026-09-22")], 3600);
    }

    #[test]
    fn an_unwritable_path_is_swallowed_not_propagated() {
        let dir = tempfile::tempdir().unwrap();
        // A path whose parent is a regular file cannot be created.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let path = blocker.join(PAUSED_LEDGER_FILE);
        let now = t("2026-09-22T12:00:00Z");
        let mut ledger = PausedLedger::load(Some(path), now);
        ledger.open(t("2026-09-22T11:00:00Z"));
        ledger.close(now);
        // In-memory totals still answer even though nothing reached disk.
        assert_eq!(ledger.totals(now)[&d("2026-09-22")], 3600);
    }
}
