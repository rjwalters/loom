//! Journal rotation behind the export cursor (#11045, scoped by #8917).
//!
//! The journal (`.loom/logs/ci-telemetry.jsonl`) used to be append-only
//! forever. On the fleet captain it reached 5.4 GB at about 1 GB/day, and
//! startup read it whole several times at once until the daemon was
//! OOM-killed in a loop. Rotation bounds it:
//!
//! - **When.** Once the export cursor has passed
//!   `autonomous.ciTelemetry.journalRotateBytes` (default
//!   [`DEFAULT_ROTATE_BYTES`]). The cursor need **not** be at the end of the
//!   file: a host whose poller keeps appending while the bulk drain is capped
//!   (#11115) never lands exactly on EOF, so an at-EOF rule never fired (the
//!   7.36 GB captain journal, #13636). Any unexported tail
//!   `[cursor, EOF)` — including a torn fragment — is carried into the new
//!   generation ([`carry_tail`] / [`rotate_carrying`]), so the cost is one
//!   copy bounded by the backlog. With `keep = 0` (no rotation is retained
//!   to replay from) rotation still waits for an empty tail. The export pass ([`super::export::backfill`]) is the only caller,
//!   and it holds the per-host cycle lock (`poll.lock`), so no poll cycle or
//!   targeted capture can append in between.
//! - **How.** `ci-telemetry.jsonl` is renamed to `ci-telemetry.jsonl.1`,
//!   older rotations shift up one (`.1` → `.2`, …) and anything past
//!   `autonomous.ciTelemetry.journalRotateKeep` (default
//!   [`DEFAULT_ROTATE_KEEP`]) is deleted. The next append creates a fresh
//!   journal. The caller has **already durably saved** a zero cursor offset
//!   before this runs (see `export::rotate_if_due`).
//! - **Dedup is unaffected.** The ledger (`seen.jsonl`), not the journal, is
//!   the "never emit twice" authority, and rotation never touches it. The one
//!   reader of old journal content is the crash-recovery replay, which also
//!   scans the retained rotations ([`super::journal::Journal::missing`]).
//!
//! The zero cursor is persisted (file + parent-directory fsync) *before* the
//! rename, so a crash at any point leaves either the old journal with a zero
//! cursor (the already-offered generation is replayed: at-least-once) or a new
//! generation with a zero cursor. An old offset is never trusted for a new
//! generation, however long that generation grows. The length check in the
//! export pass only covers a journal replaced by something other than us.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use super::CiTelemetryConfig;

/// Default `autonomous.ciTelemetry.journalRotateBytes`: 256 MiB.
pub const DEFAULT_ROTATE_BYTES: u64 = 256 * 1024 * 1024;
/// Default `autonomous.ciTelemetry.journalRotateKeep`.
pub const DEFAULT_ROTATE_KEEP: usize = 2;
/// `autonomous.ciTelemetry.journalRotateBytes` env override.
pub const ROTATE_BYTES_ENV: &str = "LOOM_CI_TELEMETRY_JOURNAL_ROTATE_BYTES";
/// `autonomous.ciTelemetry.journalRotateKeep` env override.
pub const ROTATE_KEEP_ENV: &str = "LOOM_CI_TELEMETRY_JOURNAL_ROTATE_KEEP";
/// Highest rotation index ever looked at — bounds pruning (after a lowered
/// `journalRotateKeep`) and the recovery scan.
pub const MAX_ROTATIONS: usize = 32;

/// When to rotate, and how many rotated journals to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPolicy {
    /// Rotate once the export cursor is at least this far into the journal.
    pub threshold_bytes: u64,
    /// Rotated journals kept (`.1` newest … `.keep` oldest). `0` deletes the
    /// journal on rotation instead of keeping it.
    pub keep: usize,
}

impl Default for RotationPolicy {
    fn default() -> Self {
        RotationPolicy {
            threshold_bytes: DEFAULT_ROTATE_BYTES,
            keep: DEFAULT_ROTATE_KEEP,
        }
    }
}

impl RotationPolicy {
    /// Resolve **env > config > default**. A zero or unparseable threshold
    /// falls back to the default; `keep` is capped at [`MAX_ROTATIONS`].
    #[must_use]
    pub fn resolve(config: &CiTelemetryConfig) -> Self {
        let env_u64 = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        RotationPolicy {
            threshold_bytes: env_u64(ROTATE_BYTES_ENV)
                .filter(|v| *v > 0)
                .or(config.journal_rotate_bytes)
                .unwrap_or(DEFAULT_ROTATE_BYTES),
            keep: env_u64(ROTATE_KEEP_ENV)
                .and_then(|v| usize::try_from(v).ok())
                .or(config.journal_rotate_keep)
                .unwrap_or(DEFAULT_ROTATE_KEEP)
                .min(MAX_ROTATIONS),
        }
    }
}

/// `<journal>.<n>` — the `n`th most recent rotation (`1` is the newest).
#[must_use]
pub fn rotated_path(journal: &Path, n: usize) -> PathBuf {
    let mut name = journal.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{n}"));
    journal.with_file_name(name)
}

/// The retained rotations of `journal` that exist, newest first.
#[must_use]
pub fn existing_rotations(journal: &Path) -> Vec<PathBuf> {
    (1..=MAX_ROTATIONS)
        .map(|n| rotated_path(journal, n))
        .filter(|path| path.exists())
        .collect()
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// `<journal>.carry` — where the unexported tail is staged before rotation.
pub(crate) fn carry_path(journal: &Path) -> PathBuf {
    let mut name = journal.file_name().unwrap_or_default().to_os_string();
    name.push(".carry");
    journal.with_file_name(name)
}

/// Stream `[from, EOF)` of `journal` into a fsynced staging file next to it
/// (never reading it whole: #11045) and return its path. Replaces a stale
/// staging file left by a crash.
pub fn carry_tail(journal: &Path, from: u64) -> io::Result<PathBuf> {
    use std::io::{Seek, SeekFrom};
    let staged = carry_path(journal);
    let mut src = File::open(journal)?;
    src.seek(SeekFrom::Start(from))?;
    let mut dst = File::create(&staged)?;
    let result = io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all());
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    Ok(staged)
}

/// [`rotate`], then move the staged `tail` (from [`carry_tail`]) into place as
/// the new journal. Between the two renames there is no journal file and the
/// tail lives only in `<journal>.carry` (and in `.1`, which the exporter never
/// reads): [`recover_carry`] puts it back on the next export pass. Needs
/// `keep >= 1`.
pub fn rotate_carrying(journal: &Path, keep: usize, tail: &Path) -> io::Result<()> {
    rotate(journal, keep)?;
    std::fs::rename(tail, journal)?;
    if let Some(dir) = journal.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Finish a tail-carrying rotation that was interrupted (a crash between the
/// two renames, or a failed second rename). Returns whether a staged tail was
/// consumed. The caller holds the cycle lock.
///
/// - Journal absent: the staged tail becomes the journal.
/// - Journal present and already ending with the staged bytes: the crash came
///   before the rotation, the tail is still in the old generation; just drop
///   the staging file.
/// - Otherwise the poller recreated the journal first: stream the staged tail
///   onto its end, fsync, then drop the staging file. A crash mid-append at
///   worst leaves the staging file to be appended again (at-least-once; the
///   ledger dedups emission).
pub fn recover_carry(journal: &Path) -> io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let staged = carry_path(journal);
    if !staged.exists() {
        return Ok(false);
    }
    let sync_dir = || -> io::Result<()> {
        if let Some(dir) = journal.parent() {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    };
    if !journal.exists() {
        std::fs::rename(&staged, journal)?;
        sync_dir()?;
        return Ok(true);
    }
    let staged_len = std::fs::metadata(&staged)?.len();
    let mut live = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(journal)?;
    let live_len = live.metadata()?.len();
    if staged_len <= live_len {
        // Compare the journal's suffix with the staged bytes, streaming.
        live.seek(SeekFrom::Start(live_len - staged_len))?;
        let mut a = io::BufReader::new(File::open(&staged)?).bytes();
        let mut b = io::BufReader::new(&mut live).bytes();
        let mut same = true;
        for _ in 0..staged_len {
            if a.next().transpose()? != b.next().transpose()? {
                same = false;
                break;
            }
        }
        drop(b);
        if same {
            remove_if_present(&staged)?;
            sync_dir()?;
            return Ok(true);
        }
    }
    let mut src = File::open(&staged)?;
    io::copy(&mut src, &mut live)?;
    live.sync_all()?;
    remove_if_present(&staged)?;
    sync_dir()?;
    Ok(true)
}

/// Rotate `journal` keeping at most `keep` rotations. This only moves files:
/// the caller must hold the cycle lock and has either exported every line
/// (cursor at end of file) or staged the unexported tail ([`carry_tail`]).
pub fn rotate(journal: &Path, keep: usize) -> io::Result<()> {
    // Drop everything at or past the last retained slot first, so the shift
    // below never overwrites a file that should survive and a lowered `keep`
    // still prunes the old surplus.
    for n in keep.max(1)..=MAX_ROTATIONS {
        remove_if_present(&rotated_path(journal, n))?;
    }
    if keep == 0 {
        remove_if_present(journal)?;
    } else {
        for n in (1..keep).rev() {
            let from = rotated_path(journal, n);
            if from.exists() {
                std::fs::rename(&from, rotated_path(journal, n + 1))?;
            }
        }
        std::fs::rename(journal, rotated_path(journal, 1))?;
    }
    if let Some(dir) = journal.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}
