//! Daily-rolling writer for `daemon.log` (Issue #13917).
//!
//! The daemon used to rotate `daemon.log` only at startup (10 MiB / keep 10),
//! so a long-lived daemon wrote one unbounded file and "keep 10" counted
//! restarts rather than time. This module keeps the **fixed live name**
//! `daemon.log` (so `tail -f` / `grep daemon.log` readers keep working) and
//! rolls closed days to `daemon.log.YYYY-MM-DD`:
//!
//! - the first write after 00:00 UTC renames the live file to the previous
//!   UTC day's name and reopens a fresh `daemon.log`;
//! - at startup a live file last modified on an earlier UTC day is rolled
//!   before logging begins ([`roll_stale_at_startup`]);
//! - a live file past [`SIZE_BACKSTOP_BYTES`] rolls to
//!   `daemon.log.YYYY-MM-DD.N` (an existing day file is never overwritten);
//! - retention is by **days** from the filename date (default
//!   [`DEFAULT_RETENTION_DAYS`], env `LOOM_DAEMON_LOG_RETENTION_DAYS`);
//! - archive hook: when `LOOM_DAEMON_LOG_REQUIRE_ARCHIVE=1`, retention deletes
//!   a day file only if a sidecar `<day file>.archived` exists (created by the
//!   host's archiver), and removes the pair.
//!
//! Pre-existing numbered `daemon.log.N` files are left alone: names that are
//! not `daemon.log.YYYY-MM-DD[.N]` are never touched by retention.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, NaiveDate, Utc};

/// Default number of closed UTC days kept.
pub const DEFAULT_RETENTION_DAYS: i64 = 14;
/// Size backstop for the live file (1 GiB).
pub const SIZE_BACKSTOP_BYTES: u64 = 1024 * 1024 * 1024;
/// Env var: retention in days.
pub const RETENTION_ENV: &str = "LOOM_DAEMON_LOG_RETENTION_DAYS";
/// Env var: `1` gates deletion on an `.archived` sidecar.
pub const REQUIRE_ARCHIVE_ENV: &str = "LOOM_DAEMON_LOG_REQUIRE_ARCHIVE";
/// Suffix of the archiver's sidecar marker.
pub const ARCHIVED_SUFFIX: &str = ".archived";

/// Retention settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub days: i64,
    pub require_archive: bool,
}

impl Retention {
    /// Read from the environment; invalid or non-positive days fall back to the default.
    #[must_use]
    pub fn from_env() -> Self {
        let days = std::env::var(RETENTION_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|d| *d > 0)
            .unwrap_or(DEFAULT_RETENTION_DAYS);
        let require_archive = std::env::var(REQUIRE_ARCHIVE_ENV).is_ok_and(|v| v.trim() == "1");
        Self {
            days,
            require_archive,
        }
    }
}

/// `daemon.log` + `.YYYY-MM-DD` (+ `.N` when `n > 0`).
#[must_use]
pub fn day_file_name(live: &Path, date: NaiveDate, n: u32) -> PathBuf {
    let mut s = live.as_os_str().to_os_string();
    s.push(format!(".{}", date.format("%Y-%m-%d")));
    if n > 0 {
        s.push(format!(".{n}"));
    }
    PathBuf::from(s)
}

/// First day-file name for `date` that does not exist yet.
#[must_use]
pub fn unique_day_path(live: &Path, date: NaiveDate) -> PathBuf {
    (0u32..)
        .map(|n| day_file_name(live, date, n))
        .find(|p| !p.exists())
        .unwrap_or_else(|| day_file_name(live, date, 0))
}

/// Parse the date out of a file name `<base>.YYYY-MM-DD[.N]`; `None` for any
/// other name (numbered rotations, sidecars, unrelated files).
fn parse_day_file(base: &str, name: &str) -> Option<NaiveDate> {
    let rest = name.strip_prefix(base)?.strip_prefix('.')?;
    let (date, suffix) = match rest.split_once('.') {
        Some((d, s)) => (d, Some(s)),
        None => (rest, None),
    };
    if let Some(s) = suffix {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    if date.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// Day files to delete under `retention` as of `today`, by filename date.
/// Pure over the directory listing so tests need no clock or live logger.
#[must_use]
pub fn retention_candidates(live: &Path, today: NaiveDate, retention: Retention) -> Vec<PathBuf> {
    let (Some(dir), Some(base)) = (live.parent(), live.file_name().and_then(|n| n.to_str())) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let cutoff = today - Duration::days(retention.days);
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let date = parse_day_file(base, &name)?;
            if date >= cutoff {
                return None;
            }
            let path = e.path();
            if retention.require_archive && !sidecar_path(&path).exists() {
                return None;
            }
            Some(path)
        })
        .collect();
    out.sort();
    out
}

fn sidecar_path(day_file: &Path) -> PathBuf {
    let mut s = day_file.as_os_str().to_os_string();
    s.push(ARCHIVED_SUFFIX);
    PathBuf::from(s)
}

/// Delete day files past retention (and their sidecars). Best-effort.
pub fn apply_retention(live: &Path, today: NaiveDate, retention: Retention) {
    for path in retention_candidates(live, today, retention) {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(sidecar_path(&path));
    }
}

fn utc_date_of(time: std::time::SystemTime) -> NaiveDate {
    DateTime::<Utc>::from(time).date_naive()
}

/// Startup roll: a non-empty live file last modified on an earlier UTC day than
/// `today` is renamed to its (mtime) day's name. Returns the new path if rolled.
///
/// # Errors
/// Propagates a failed `rename`; the caller may log to the old file instead.
pub fn roll_stale_at_startup(live: &Path, today: NaiveDate) -> io::Result<Option<PathBuf>> {
    let Ok(meta) = fs::metadata(live) else {
        return Ok(None);
    };
    if meta.len() == 0 {
        return Ok(None);
    }
    let day = utc_date_of(meta.modified()?);
    if day >= today {
        return Ok(None);
    }
    let target = unique_day_path(live, day);
    fs::rename(live, &target)?;
    Ok(Some(target))
}

type Clock = Box<dyn Fn() -> DateTime<Utc> + Send>;

/// `io::Write` over `daemon.log` that rolls at UTC midnight and at the size backstop.
pub struct DailyLogWriter {
    live: PathBuf,
    file: File,
    day: NaiveDate,
    bytes: u64,
    retention: Retention,
    size_cap: u64,
    clock: Clock,
}

impl DailyLogWriter {
    /// Open `live` for append, rolling a prior-day file first and applying retention.
    ///
    /// # Errors
    /// Fails if the live file cannot be opened.
    pub fn open(live: &Path, retention: Retention) -> io::Result<Self> {
        Self::open_with(live, retention, SIZE_BACKSTOP_BYTES, Box::new(Utc::now))
    }

    fn open_with(
        live: &Path,
        retention: Retention,
        size_cap: u64,
        clock: Clock,
    ) -> io::Result<Self> {
        let today = clock().date_naive();
        if let Err(e) = roll_stale_at_startup(live, today) {
            eprintln!("daemon log: startup roll of {} failed: {e}", live.display());
        }
        let file = OpenOptions::new().create(true).append(true).open(live)?;
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        apply_retention(live, today, retention);
        Ok(Self {
            live: live.to_path_buf(),
            file,
            day: today,
            bytes,
            retention,
            size_cap,
            clock,
        })
    }

    /// Rename the live file to a day-file name for `self.day` and reopen. On
    /// any failure the old fd stays in use, so no line is dropped.
    fn roll(&mut self, new_day: NaiveDate) {
        let target = unique_day_path(&self.live, self.day);
        let result = fs::rename(&self.live, &target).and_then(|()| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.live)
        });
        match result {
            Ok(file) => {
                self.file = file;
                self.bytes = 0;
            }
            Err(e) => {
                eprintln!("daemon log: roll of {} failed: {e}", self.live.display());
            }
        }
        self.day = new_day;
        apply_retention(&self.live, new_day, self.retention);
    }
}

impl Write for DailyLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let today = (self.clock)().date_naive();
        // A clock moving backwards never rolls.
        if today > self.day
            || (self.bytes > 0 && self.bytes.saturating_add(buf.len() as u64) > self.size_cap)
        {
            self.roll(today.max(self.day));
        }
        let n = self.file.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn fake_clock(t: &Arc<Mutex<DateTime<Utc>>>) -> Clock {
        let t = Arc::clone(t);
        Box::new(move || *t.lock().unwrap())
    }

    fn ret(days: i64, require_archive: bool) -> Retention {
        Retention {
            days,
            require_archive,
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn midnight_roll_loses_no_line() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("daemon.log");
        let t = Arc::new(Mutex::new(at("2026-11-15T23:59:58Z")));
        let mut w =
            DailyLogWriter::open_with(&live, ret(14, false), 1 << 30, fake_clock(&t)).unwrap();
        w.write_all(b"a\n").unwrap();
        *t.lock().unwrap() = at("2026-11-16T00:00:01Z");
        w.write_all(b"b\n").unwrap();
        w.write_all(b"c\n").unwrap();
        assert_eq!(fs::read_to_string(&live).unwrap(), "b\nc\n");
        assert_eq!(fs::read_to_string(dir.path().join("daemon.log.2026-11-15")).unwrap(), "a\n");
    }

    #[test]
    fn startup_rolls_prior_day_file_and_appends_today() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("daemon.log");
        fs::write(&live, "old\n").unwrap();
        // File mtime is "now" (real clock); a fake clock two days ahead makes it stale.
        let mtime_day = utc_date_of(fs::metadata(&live).unwrap().modified().unwrap());
        let t = Arc::new(Mutex::new(
            (mtime_day + Duration::days(2))
                .and_hms_opt(1, 0, 0)
                .unwrap()
                .and_utc(),
        ));
        let mut w =
            DailyLogWriter::open_with(&live, ret(14, false), 1 << 30, fake_clock(&t)).unwrap();
        w.write_all(b"new\n").unwrap();
        let rolled = day_file_name(&live, mtime_day, 0);
        assert_eq!(fs::read_to_string(&rolled).unwrap(), "old\n");
        assert_eq!(fs::read_to_string(&live).unwrap(), "new\n");
        drop(w);

        // A live file from today (by mtime) is appended to, not rolled.
        let t2 = Arc::new(Mutex::new(mtime_day.and_hms_opt(23, 0, 0).unwrap().and_utc()));
        let mut w2 =
            DailyLogWriter::open_with(&live, ret(14, false), 1 << 30, fake_clock(&t2)).unwrap();
        w2.write_all(b"more\n").unwrap();
        assert_eq!(fs::read_to_string(&live).unwrap(), "new\nmore\n");
    }

    #[test]
    fn name_collision_gets_numeric_suffix_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("daemon.log");
        fs::write(day_file_name(&live, d("2026-11-15"), 0), "keep\n").unwrap();
        let t = Arc::new(Mutex::new(at("2026-11-15T10:00:00Z")));
        let mut w =
            DailyLogWriter::open_with(&live, ret(14, false), 1 << 30, fake_clock(&t)).unwrap();
        w.write_all(b"x\n").unwrap();
        *t.lock().unwrap() = at("2026-11-16T00:00:01Z");
        w.write_all(b"y\n").unwrap();
        assert_eq!(fs::read_to_string(day_file_name(&live, d("2026-11-15"), 0)).unwrap(), "keep\n");
        assert_eq!(fs::read_to_string(day_file_name(&live, d("2026-11-15"), 1)).unwrap(), "x\n");
    }

    #[test]
    fn size_backstop_rolls_to_dotted_n() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("daemon.log");
        let t = Arc::new(Mutex::new(at("2026-11-15T10:00:00Z")));
        let mut w = DailyLogWriter::open_with(&live, ret(14, false), 8, fake_clock(&t)).unwrap();
        w.write_all(b"12345\n").unwrap();
        w.write_all(b"67890\n").unwrap();
        w.write_all(b"abcde\n").unwrap();
        assert_eq!(fs::read_to_string(&live).unwrap(), "abcde\n");
        assert_eq!(
            fs::read_to_string(day_file_name(&live, d("2026-11-15"), 0)).unwrap(),
            "12345\n"
        );
        assert_eq!(
            fs::read_to_string(day_file_name(&live, d("2026-11-15"), 1)).unwrap(),
            "67890\n"
        );
    }

    #[test]
    fn retention_uses_filename_date_and_ignores_other_names() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("daemon.log");
        for n in [
            "daemon.log.2026-10-01",
            "daemon.log.2026-10-01.1",
            "daemon.log.2026-11-10",
            "daemon.log.3",
            "daemon.log.2026-10-01.archived",
            "other.log.2026-10-01",
        ] {
            fs::write(dir.path().join(n), "x").unwrap();
        }
        apply_retention(&live, d("2026-11-16"), ret(14, false));
        assert_eq!(
            names(dir.path()),
            vec![
                "daemon.log.2026-11-10",
                "daemon.log.3",
                "other.log.2026-10-01"
            ]
        );
    }

    #[test]
    fn archive_gated_retention_keeps_unarchived() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("daemon.log");
        for n in [
            "daemon.log.2026-10-01",
            "daemon.log.2026-10-01.archived",
            "daemon.log.2026-10-02",
        ] {
            fs::write(dir.path().join(n), "x").unwrap();
        }
        apply_retention(&live, d("2026-11-16"), ret(14, true));
        assert_eq!(names(dir.path()), vec!["daemon.log.2026-10-02"]);
        // Env off: plain age-based deletion.
        apply_retention(&live, d("2026-11-16"), ret(14, false));
        assert!(names(dir.path()).is_empty());
    }

    #[test]
    fn clock_moving_backwards_does_not_roll() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("daemon.log");
        let t = Arc::new(Mutex::new(at("2026-11-16T10:00:00Z")));
        let mut w =
            DailyLogWriter::open_with(&live, ret(14, false), 1 << 30, fake_clock(&t)).unwrap();
        w.write_all(b"a\n").unwrap();
        *t.lock().unwrap() = at("2026-11-15T10:00:00Z");
        w.write_all(b"b\n").unwrap();
        assert_eq!(names(dir.path()), vec!["daemon.log"]);
        assert_eq!(fs::read_to_string(&live).unwrap(), "a\nb\n");
    }

    #[test]
    fn failed_rename_keeps_logging_to_old_fd() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("daemon.log");
        let t = Arc::new(Mutex::new(at("2026-11-15T10:00:00Z")));
        let mut w =
            DailyLogWriter::open_with(&live, ret(14, false), 1 << 30, fake_clock(&t)).unwrap();
        w.write_all(b"a\n").unwrap();
        // Make rename fail: remove the live file's directory entry's parent? Use a
        // target directory that cannot be created by pointing `live` elsewhere.
        w.live = dir.path().join("missing").join("daemon.log");
        *t.lock().unwrap() = at("2026-11-16T00:00:01Z");
        w.write_all(b"b\n").unwrap();
        w.flush().unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("daemon.log")).unwrap(), "a\nb\n");
    }

    #[test]
    fn parse_day_file_shapes() {
        assert_eq!(parse_day_file("daemon.log", "daemon.log.2026-11-15"), Some(d("2026-11-15")));
        assert_eq!(parse_day_file("daemon.log", "daemon.log.2026-11-15.2"), Some(d("2026-11-15")));
        assert_eq!(parse_day_file("daemon.log", "daemon.log.2026-11-15.archived"), None);
        assert_eq!(parse_day_file("daemon.log", "daemon.log.7"), None);
        assert_eq!(parse_day_file("daemon.log", "daemon.log"), None);
    }

    #[test]
    fn health_fallback_reads_newest_day_file_when_live_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("daemon.log");
        fs::write(&live, "").unwrap();
        fs::write(
            day_file_name(&live, d("2026-11-14"), 0),
            "[2026-11-14T10:00:00.000Z] [INFO] work_finder: tick old\n",
        )
        .unwrap();
        let newest = day_file_name(&live, d("2026-11-15"), 1);
        fs::write(&newest, "[2026-11-15T23:59:00.000Z] [INFO] work_finder: tick new\n").unwrap();
        fs::write(dir.path().join("daemon.log.3"), "legacy").unwrap();
        assert_eq!(crate::health::newest_day_file(&live), Some(newest.clone()));
        let tail = fs::read_to_string(&newest).unwrap();
        let now = at("2026-11-16T00:00:30Z").naive_utc();
        assert_eq!(crate::health::work_finder_log_tick_age_secs(&tail, now), Some(90));
    }
}
