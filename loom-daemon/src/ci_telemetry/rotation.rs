//! Journal rotation behind the export cursor (#11045, scoped by #8917).
//!
//! The journal (`.loom/logs/ci-telemetry.jsonl`) used to be append-only
//! forever. On the fleet captain it reached 5.4 GB at about 1 GB/day, and
//! startup read it whole several times at once until the daemon was
//! OOM-killed in a loop. Rotation bounds it:
//!
//! - **When.** Once the export cursor has passed
//!   `autonomous.ciTelemetry.journalRotateBytes` (default
//!   [`DEFAULT_ROTATE_BYTES`]) **and** sits exactly at the end of the file —
//!   every line has been offered to the export queue and there is no torn
//!   tail. The export pass ([`super::export::backfill`]) is the only caller,
//!   and it holds the per-host cycle lock (`poll.lock`), so no poll cycle or
//!   targeted capture can append in between.
//! - **How.** `ci-telemetry.jsonl` is renamed to `ci-telemetry.jsonl.1`,
//!   older rotations shift up one (`.1` → `.2`, …) and anything past
//!   `autonomous.ciTelemetry.journalRotateKeep` (default
//!   [`DEFAULT_ROTATE_KEEP`]) is deleted. The next append creates a fresh
//!   journal; the caller then resets the cursor's byte offset to 0.
//! - **Dedup is unaffected.** The ledger (`seen.jsonl`), not the journal, is
//!   the "never emit twice" authority, and rotation never touches it. The one
//!   reader of old journal content is the crash-recovery replay, which also
//!   scans the retained rotations ([`super::journal::Journal::missing`]).
//!
//! A crash between the rename and the cursor reset leaves a cursor past the
//! end of the (new, short or missing) journal, which the export pass already
//! treats as "the journal was replaced": it restarts from offset 0.

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

/// Rotate `journal` keeping at most `keep` rotations. **The caller must hold
/// the cycle lock and must have exported every line** (the cursor at end of
/// file) — this only moves files.
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
        if let Ok(dir) = File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}
