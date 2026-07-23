//! Disk-headroom math for the autonomous work finder (#3811, Phase B of epic
//! #3809).
//!
//! This is the Rust port of the two `defaults/scripts/lib/disk-headroom.sh`
//! functions the `/loom:sweep` skill uses to resource-gate its wave size:
//!
//! - [`worktree_root_free_gb`] mirrors bash `loom_worktree_root_free_gb`: resolve
//!   the worktree-root filesystem (via [`crate::worktree_root::worktree_root`],
//!   the existing Rust-native port of `worktree-root.sh`), walk up to the nearest
//!   existing ancestor, and shell out to `df -Pk` to read the integer free GB on
//!   **that** volume (the dedicated scratch volume when `LOOM_WORKTREE_ROOT` /
//!   `worktree.root` is set — NOT the repo's own drive).
//! - [`disk_headroom`] mirrors the disk term of bash `loom_wave_size_from_disk`:
//!   `floor(free_gb / LOOM_PER_WORKTREE_GB)`, the number of worktrees the scratch
//!   volume can hold at the conservative per-worktree estimate.
//!
//! # Why shell out to `df` instead of a `statvfs` crate
//!
//! `loom-daemon/Cargo.toml` has no `libc`/`nix`/`sysinfo` dependency, and the
//! precedent set by [`crate::worktree_root`] (an explicit 1:1 port of a bash lib)
//! is to keep the Rust and bash implementations trivially comparable. Shelling to
//! `df -Pk <path>` — the same tool the bash version uses — avoids a new crate
//! dependency and keeps the two ports byte-for-byte auditable. The pure parsing
//! and arithmetic ([`parse_df_available_gb`], [`disk_headroom`]) are split out
//! from the I/O so they stay unit-testable without a real filesystem.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::worktree_root::worktree_root;

/// Environment variable overriding the conservative per-worktree disk estimate
/// (GB). Mirrors bash `LOOM_PER_WORKTREE_GB`.
pub const PER_WORKTREE_GB_ENV: &str = "LOOM_PER_WORKTREE_GB";

/// Default per-worktree disk estimate (GB). Matches the bash default of 2.
pub const DEFAULT_PER_WORKTREE_GB: u64 = 2;

/// Resolve the per-worktree GB estimate from [`PER_WORKTREE_GB_ENV`], flooring to
/// a minimum of 1 (a zero or unparseable value would make the disk term diverge).
/// Mirrors the bash `per` resolution and its `per < 1` guard.
#[must_use]
pub fn per_worktree_gb() -> u64 {
    std::env::var(PER_WORKTREE_GB_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_PER_WORKTREE_GB)
}

/// Parse the integer free GB from `df -Pk` output.
///
/// `df -Pk` prints a header row then exactly one single-line data row per
/// filesystem (the `-P` POSIX format pins one line per fs; `-k` pins 1024-byte
/// blocks). The 4th whitespace-delimited column of the data row is "Available" in
/// 1K blocks; this divides down to GB with an integer floor (`/ 1024 / 1024`),
/// matching the bash `avail_k / 1024 / 1024`.
///
/// Returns `None` when the output is malformed (missing data row, non-numeric
/// Available column) so the caller can floor to 0 free rather than panic.
#[must_use]
pub fn parse_df_available_gb(df_output: &str) -> Option<u64> {
    // Second line is the single data row (`-P` guarantees one line per fs).
    let data_row = df_output.lines().nth(1)?;
    // 4th column (0-based index 3) is "Available" in 1K blocks.
    let avail_k: u64 = data_row.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_k / 1024 / 1024)
}

/// Walk `path` up to the nearest existing ancestor (read-only; never creates a
/// directory). The worktree-root leaf usually does not exist yet, and `df` errors
/// on a non-existent path — mirrors the bash `while [[ ... ! -e $probe ]]` loop.
fn nearest_existing_ancestor(path: &Path) -> &Path {
    let mut probe = path;
    while !probe.exists() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }
    probe
}

/// Echo the integer free space (GB) on the filesystem hosting the resolved
/// worktree root for `repo_root`. Rust port of bash `loom_worktree_root_free_gb`.
///
/// Resolves the worktree root via [`worktree_root`] (override-aware:
/// `LOOM_WORKTREE_ROOT` / `worktree.root` / default `<repo>/.loom/worktrees`),
/// walks up to the nearest existing ancestor, and runs `df -Pk` on it. Returns 0
/// free on any failure (df missing/errored, unparseable output) so the caller
/// floors to a single worktree rather than crashing — matching the bash
/// `echo "0"` fallbacks.
#[must_use]
pub fn worktree_root_free_gb(repo_root: &Path) -> u64 {
    let wt_root = worktree_root(repo_root);
    let probe = nearest_existing_ancestor(&wt_root);

    let output = match Command::new("df")
        .arg("-Pk")
        .arg(probe)
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(_) | Err(_) => return 0,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_df_available_gb(&stdout).unwrap_or(0)
}

/// The disk-headroom concurrency term: how many worktrees `free_gb` can hold at
/// `per_gb` GB each. Pure `floor(free_gb / per_gb)`, mirroring the disk term of
/// bash `loom_wave_size_from_disk` (`free_gb / per`). A `per_gb` of 0 is treated
/// as 1 to avoid a divide-by-zero (the env resolver already floors it, but this
/// keeps the pure function total).
#[must_use]
pub fn disk_headroom(free_gb: u64, per_gb: u64) -> usize {
    let per = per_gb.max(1);
    usize::try_from(free_gb / per).unwrap_or(usize::MAX)
}

/// Resolve the disk-headroom concurrency bound for `repo_root`: the number of
/// worktrees the worktree-root scratch volume can hold at the resolved
/// per-worktree estimate. Combines [`worktree_root_free_gb`] (I/O) with
/// [`disk_headroom`] (pure math) and [`per_worktree_gb`] (env).
#[must_use]
pub fn disk_headroom_limit(repo_root: &Path) -> usize {
    disk_headroom(worktree_root_free_gb(repo_root), per_worktree_gb())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ===================================================================
    // parse_df_available_gb — df output parsing
    // ===================================================================

    #[test]
    fn test_parse_df_macos_shape() {
        // macOS `df -Pk /` shape: header + one data row. Available (col 4) is
        // 200 GB worth of 1K blocks (200 * 1024 * 1024).
        let out = "Filesystem 1024-blocks      Used Available Capacity  Mounted on\n\
                   /dev/disk3s1 976490576 300000000 209715200      60%    /\n";
        assert_eq!(parse_df_available_gb(out), Some(200));
    }

    #[test]
    fn test_parse_df_linux_shape() {
        // GNU `df -Pk` shape. Available = 50 GB (50 * 1024 * 1024 = 52428800).
        let out = "Filesystem     1024-blocks    Used Available Use% Mounted on\n\
                   /dev/sda1        103081248 47000000  52428800  48% /\n";
        assert_eq!(parse_df_available_gb(out), Some(50));
    }

    #[test]
    fn test_parse_df_floors_partial_gb() {
        // 1.5 GB of 1K blocks floors to 1.
        let avail_k = 1024 * 1024 + 512 * 1024;
        let out = format!("H E A D E R\n/dev/x 999 1 {avail_k} 1% /\n");
        assert_eq!(parse_df_available_gb(&out), Some(1));
    }

    #[test]
    fn test_parse_df_missing_data_row_is_none() {
        assert_eq!(parse_df_available_gb("only a header line\n"), None);
        assert_eq!(parse_df_available_gb(""), None);
    }

    #[test]
    fn test_parse_df_non_numeric_available_is_none() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted\n\
                   /dev/x 999 1 not-a-number 1% /\n";
        assert_eq!(parse_df_available_gb(out), None);
    }

    // ===================================================================
    // disk_headroom — pure floor division
    // ===================================================================

    #[test]
    fn test_disk_headroom_floors() {
        assert_eq!(disk_headroom(20, 2), 10);
        assert_eq!(disk_headroom(21, 2), 10); // floor
        assert_eq!(disk_headroom(1, 2), 0); // less than one worktree fits
        assert_eq!(disk_headroom(0, 2), 0);
    }

    #[test]
    fn test_disk_headroom_per_gb_zero_treated_as_one() {
        // Defensive: a 0 per_gb must not divide-by-zero.
        assert_eq!(disk_headroom(5, 0), 5);
    }

    // ===================================================================
    // per_worktree_gb — env resolution
    // ===================================================================

    #[test]
    #[serial]
    fn test_per_worktree_gb_default_and_override() {
        std::env::remove_var(PER_WORKTREE_GB_ENV);
        assert_eq!(per_worktree_gb(), DEFAULT_PER_WORKTREE_GB);

        std::env::set_var(PER_WORKTREE_GB_ENV, "5");
        assert_eq!(per_worktree_gb(), 5);

        // Zero and unparseable fall back to the default (bash floors per >= 1).
        std::env::set_var(PER_WORKTREE_GB_ENV, "0");
        assert_eq!(per_worktree_gb(), DEFAULT_PER_WORKTREE_GB);
        std::env::set_var(PER_WORKTREE_GB_ENV, "garbage");
        assert_eq!(per_worktree_gb(), DEFAULT_PER_WORKTREE_GB);
        std::env::remove_var(PER_WORKTREE_GB_ENV);
    }

    // ===================================================================
    // nearest_existing_ancestor — read-only ancestor walk
    // ===================================================================

    #[test]
    fn test_nearest_existing_ancestor_walks_up() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("does/not/exist/yet");
        // Walks up to the tempdir, which exists.
        assert_eq!(nearest_existing_ancestor(&deep), tmp.path());
    }

    #[test]
    fn test_nearest_existing_ancestor_returns_self_when_exists() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(nearest_existing_ancestor(tmp.path()), tmp.path());
    }

    // ===================================================================
    // worktree_root_free_gb — smoke test against the real df
    // ===================================================================

    #[test]
    fn test_worktree_root_free_gb_returns_a_value() {
        // Integration smoke: the repo root's volume has some free space, so a
        // real `df -Pk` should parse to a value. We only assert it doesn't panic
        // and returns a plausible (non-astronomical) integer — the exact GB is
        // environment-dependent.
        let tmp = tempfile::tempdir().unwrap();
        let free = worktree_root_free_gb(tmp.path());
        // A modern dev/CI volume has < 1 EB free; this just guards the parse.
        assert!(free < 1_000_000_000);
    }
}
