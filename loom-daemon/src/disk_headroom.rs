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
//!   `worktree.root` is set — NOT the repo's own drive). Returns `Option<u64>`:
//!   `None` means the probe was unmeasurable (df missing/errored, unparseable
//!   output), not that 0 GB is free (#4164 — unknown != zero, mirroring the
//!   bash-side fix). [`disk_headroom_limit`] skips the disk clamp on `None`
//!   instead of treating an unmeasurable probe as a full disk.
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
/// Available column) so the caller ([`worktree_root_free_gb`]) can propagate
/// "unmeasurable" rather than either panicking or manufacturing a fake `0`
/// (#4164 — unknown != zero).
#[must_use]
pub fn parse_df_available_gb(df_output: &str) -> Option<u64> {
    // Second line is the single data row (`-P` guarantees one line per fs).
    let data_row = df_output.lines().nth(1)?;
    // 4th column (0-based index 3) is "Available" in 1K blocks.
    let avail_k: u64 = data_row.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_k / 1024 / 1024)
}

/// Parse the integer total (capacity) GB from `df -Pk` output — the same data
/// row [`parse_df_available_gb`] reads, one column over. `df -Pk`'s POSIX
/// format is `Filesystem 1024-blocks Used Available Capacity Mounted-on`, so
/// the 2nd whitespace-delimited column (0-based index 1) is the filesystem's
/// total size in 1K blocks (#5356 — this is the denominator
/// `worktree_root_free_gb` needs to become a percentage downstream).
///
/// Returns `None` on the same malformed-output conditions
/// [`parse_df_available_gb`] does, so an unmeasurable total is *absent*, never
/// a fabricated `0` (the same "unknown != zero" contract, #4164).
#[must_use]
pub fn parse_df_total_gb(df_output: &str) -> Option<u64> {
    let data_row = df_output.lines().nth(1)?;
    // 2nd column (0-based index 1) is "1024-blocks" (total capacity).
    let total_k: u64 = data_row.split_whitespace().nth(1)?.parse().ok()?;
    Some(total_k / 1024 / 1024)
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

/// Run `df -Pk` against `probe` and return its raw stdout on success, `None`
/// on any probe failure (missing `df` binary, non-zero exit). Shared by
/// [`worktree_root_free_gb`], [`worktree_root_total_gb`], and
/// [`worktree_root_disk_gb`] so a caller that wants both free and total reads
/// them from the SAME `df` sample instead of two separate invocations racing
/// a filesystem that can change size between them (#5356).
fn df_probe_output(probe: &Path) -> Option<String> {
    let output = match Command::new("df")
        .arg("-Pk")
        .arg(probe)
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(_) | Err(_) => return None,
    };
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Echo the integer free space (GB) on the filesystem hosting the resolved
/// worktree root for `repo_root`. Rust port of bash `loom_worktree_root_free_gb`.
///
/// Resolves the worktree root via [`worktree_root`] (override-aware:
/// `LOOM_WORKTREE_ROOT` / `worktree.root` / default `<repo>/.loom/worktrees`),
/// walks up to the nearest existing ancestor, and runs `df -Pk` on it.
///
/// Unknown != zero (#4164): returns `None` on any failure to actually measure
/// free space (`df` missing/errored, unparseable output) instead of a fake
/// `0` — a `0` used to flow straight into [`disk_headroom`] and look
/// identical to a genuinely full disk. Callers (`disk_headroom_limit`) must
/// treat `None` as "skip the disk clamp", not as "0 free".
#[must_use]
pub fn worktree_root_free_gb(repo_root: &Path) -> Option<u64> {
    let wt_root = worktree_root(repo_root);
    let probe = nearest_existing_ancestor(&wt_root);
    parse_df_available_gb(&df_probe_output(probe)?)
}

/// Echo the integer total capacity (GB) of the filesystem hosting the
/// resolved worktree root for `repo_root` — the denominator a consumer needs
/// to render [`worktree_root_free_gb`] as a percentage instead of a bare
/// absolute number that is not comparable across a heterogeneous fleet
/// (#5356).
///
/// Same resolution and "unknown != zero" contract as `worktree_root_free_gb`:
/// `None` means the probe could not measure total capacity (`df`
/// missing/errored, unparseable output), never a fabricated `0`.
#[must_use]
pub fn worktree_root_total_gb(repo_root: &Path) -> Option<u64> {
    let wt_root = worktree_root(repo_root);
    let probe = nearest_existing_ancestor(&wt_root);
    parse_df_total_gb(&df_probe_output(probe)?)
}

/// Probe both free and total GB on the worktree-root filesystem for
/// `repo_root` in a SINGLE `df -Pk` invocation (#5356) — `host.health`
/// sampling wants both every tick, and they are two columns of the same `df`
/// row, so this avoids spawning `df` twice per sample.
///
/// Each half of the pair follows the free-standing functions' own "unknown !=
/// zero" contract independently: a malformed Available column does not
/// prevent a well-formed Total column (or vice versa) from still resolving,
/// though in practice a `df` invocation that fails at all (missing binary,
/// non-zero exit) yields `(None, None)` together.
#[must_use]
pub fn worktree_root_disk_gb(repo_root: &Path) -> (Option<u64>, Option<u64>) {
    let wt_root = worktree_root(repo_root);
    let probe = nearest_existing_ancestor(&wt_root);
    match df_probe_output(probe) {
        Some(output) => (parse_df_available_gb(&output), parse_df_total_gb(&output)),
        None => (None, None),
    }
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
///
/// Unknown != zero (#4164): when the free-space probe is unmeasurable
/// ([`worktree_root_free_gb`] returns `None`), this SKIPS the disk clamp
/// entirely — returning `usize::MAX` so the disk term never binds the
/// `min(...)` concurrency expression callers (`ipc.rs`, `work_finder.rs`)
/// compose it into — and logs a warning naming the repo root, rather than
/// silently treating the unmeasurable probe as a full disk.
#[must_use]
pub fn disk_headroom_limit(repo_root: &Path) -> usize {
    match worktree_root_free_gb(repo_root) {
        Some(free_gb) => disk_headroom(free_gb, per_worktree_gb()),
        None => {
            log::warn!(
                "disk_headroom: could not measure free space on the worktree-root \
                 filesystem for {} — skipping the disk clamp (treating as unbounded)",
                repo_root.display()
            );
            usize::MAX
        }
    }
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
    // parse_df_total_gb — df output parsing (#5356)
    // ===================================================================

    #[test]
    fn test_parse_df_total_macos_shape() {
        // Same fixture as test_parse_df_macos_shape: total (col 2) is
        // 976490576 1K blocks ≈ 931 GB (integer floor).
        let out = "Filesystem 1024-blocks      Used Available Capacity  Mounted on\n\
                   /dev/disk3s1 976490576 300000000 209715200      60%    /\n";
        assert_eq!(parse_df_total_gb(out), Some(976490576 / 1024 / 1024));
    }

    #[test]
    fn test_parse_df_total_linux_shape() {
        let out = "Filesystem     1024-blocks    Used Available Use% Mounted on\n\
                   /dev/sda1        103081248 47000000  52428800  48% /\n";
        assert_eq!(parse_df_total_gb(out), Some(103081248 / 1024 / 1024));
    }

    #[test]
    fn test_parse_df_total_missing_data_row_is_none() {
        assert_eq!(parse_df_total_gb("only a header line\n"), None);
        assert_eq!(parse_df_total_gb(""), None);
    }

    #[test]
    fn test_parse_df_total_non_numeric_total_is_none() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted\n\
                   /dev/x not-a-number 1 200 1% /\n";
        assert_eq!(parse_df_total_gb(out), None);
    }

    #[test]
    fn test_parse_df_free_and_total_read_from_the_same_row() {
        // Free and total are two independent columns of the SAME data row —
        // this pins that they parse consistently against one shared fixture,
        // the exact shape worktree_root_disk_gb's single-df-call design
        // relies on.
        let out = "Filesystem 1024-blocks      Used Available Capacity  Mounted on\n\
                   /dev/disk3s1 1048576000 838860800 209715200      80%    /\n";
        assert_eq!(parse_df_total_gb(out), Some(1000));
        assert_eq!(parse_df_available_gb(out), Some(200));
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
    #[serial]
    fn test_worktree_root_free_gb_returns_a_value() {
        // Integration smoke: the repo root's volume has some free space, so a
        // real `df -Pk` should parse to a value. We only assert it doesn't panic
        // and returns a plausible (non-astronomical) integer — the exact GB is
        // environment-dependent.
        //
        // `#[serial]` is load-bearing (#4525): the sibling tests below prepend a
        // stub-`df` dir to the process-global `PATH`, and cargo's default
        // multi-threaded runner would otherwise schedule this real-`df` probe
        // inside that window — the stub always exits 1, so the probe returns
        // `None` and the `.expect(...)` panics. Sharing the serial lock with
        // those PATH mutators closes the race.
        let tmp = tempfile::tempdir().unwrap();
        let free = worktree_root_free_gb(tmp.path())
            .expect("a real df -Pk against a real tempdir should succeed in the test env");
        // A modern dev/CI volume has < 1 EB free; this just guards the parse.
        assert!(free < 1_000_000_000);
    }

    // ===================================================================
    // worktree_root_total_gb / worktree_root_disk_gb — smoke tests against
    // the real df (#5356)
    // ===================================================================

    #[test]
    #[serial]
    fn test_worktree_root_total_gb_returns_a_value() {
        // Same smoke-test shape as test_worktree_root_free_gb_returns_a_value:
        // real df, plausible (non-astronomical) value, `#[serial]` shares the
        // PATH-mutation lock with the stub-df tests below.
        let tmp = tempfile::tempdir().unwrap();
        let total = worktree_root_total_gb(tmp.path())
            .expect("a real df -Pk against a real tempdir should succeed in the test env");
        assert!(total < 1_000_000_000);
        // A filesystem's total capacity is always >= 0 and, on any real dev/CI
        // box, strictly positive.
        assert!(total > 0);
    }

    #[test]
    #[serial]
    fn test_worktree_root_disk_gb_returns_free_and_total_together() {
        let tmp = tempfile::tempdir().unwrap();
        let (free, total) = worktree_root_disk_gb(tmp.path());
        let free = free.expect("real df should measure free space");
        let total = total.expect("real df should measure total capacity");
        // Total capacity can never be smaller than free space on the same
        // filesystem sample.
        assert!(total >= free, "total {total} GB should be >= free {free} GB");
    }

    // ===================================================================
    // worktree_root_total_gb / worktree_root_disk_gb — unmeasurable (None)
    // path (#5356, mirroring #4164's free-space contract)
    // ===================================================================

    #[test]
    #[serial]
    fn test_worktree_root_total_gb_returns_none_on_df_failure() {
        // A `df` on PATH that always fails must surface as `None`
        // (unmeasurable), never a fake `Some(0)` — mirrors
        // test_worktree_root_free_gb_returns_none_on_df_failure exactly.
        let stub_dir = tempfile::tempdir().unwrap();
        let stub_df = stub_dir.path().join("df");
        std::fs::write(&stub_df, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub_df).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub_df, perms).unwrap();
        }

        let tmp = tempfile::tempdir().unwrap();
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", stub_dir.path().display()));
        let result = worktree_root_total_gb(tmp.path());
        std::env::set_var("PATH", old_path);

        assert_eq!(result, None, "a failing df must yield None (unmeasurable), not a fake Some(0)");
    }

    #[test]
    #[serial]
    fn test_worktree_root_disk_gb_returns_none_none_on_df_failure() {
        // The combined probe must degrade both halves together on a failed
        // `df` invocation — never a partial (Some, None) or (None, Some) pair
        // from a single failed sample.
        let stub_dir = tempfile::tempdir().unwrap();
        let stub_df = stub_dir.path().join("df");
        std::fs::write(&stub_df, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub_df).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub_df, perms).unwrap();
        }

        let tmp = tempfile::tempdir().unwrap();
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", stub_dir.path().display()));
        let result = worktree_root_disk_gb(tmp.path());
        std::env::set_var("PATH", old_path);

        assert_eq!(result, (None, None));
    }

    #[test]
    #[serial]
    fn test_worktree_root_disk_gb_matches_the_separate_probes_on_a_stub_df() {
        // The combined probe's (free, total) pair must match what the two
        // standalone functions would each independently parse from the same
        // fixture — pins that worktree_root_disk_gb's single-df-call design
        // does not silently diverge from worktree_root_free_gb /
        // worktree_root_total_gb.
        let stub_dir = tempfile::tempdir().unwrap();
        let stub_df = stub_dir.path().join("df");
        std::fs::write(
            &stub_df,
            "#!/bin/sh\n\
             printf 'Filesystem 1024-blocks      Used Available Capacity  Mounted on\\n'\n\
             printf '/dev/disk3s1 1048576000 838860800 209715200      80%%    /\\n'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub_df).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub_df, perms).unwrap();
        }

        let tmp = tempfile::tempdir().unwrap();
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", stub_dir.path().display()));
        let (free, total) = worktree_root_disk_gb(tmp.path());
        std::env::set_var("PATH", old_path);

        assert_eq!(free, Some(200));
        assert_eq!(total, Some(1000));
    }

    // ===================================================================
    // worktree_root_free_gb / disk_headroom_limit — unmeasurable (None) path
    // (#4164: unknown != zero)
    // ===================================================================

    #[test]
    #[serial]
    fn test_worktree_root_free_gb_returns_none_on_df_failure() {
        // A `df` on PATH that always fails must surface as `None` (unmeasurable),
        // never as a fake `Some(0)` — a 0 free-GB reading must mean "measured a
        // genuinely full disk", not "the probe itself failed".
        let stub_dir = tempfile::tempdir().unwrap();
        let stub_df = stub_dir.path().join("df");
        std::fs::write(&stub_df, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub_df).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub_df, perms).unwrap();
        }

        let tmp = tempfile::tempdir().unwrap();
        let old_path = std::env::var("PATH").unwrap_or_default();
        // Prepend the stub dir so its `df` shadows the real one.
        std::env::set_var("PATH", format!("{}:{old_path}", stub_dir.path().display()));
        let result = worktree_root_free_gb(tmp.path());
        std::env::set_var("PATH", old_path);

        assert_eq!(result, None, "a failing df must yield None (unmeasurable), not a fake Some(0)");
    }

    #[test]
    #[serial]
    fn test_disk_headroom_limit_skips_clamp_on_unmeasurable_probe() {
        // When the probe is unmeasurable, disk_headroom_limit must NOT clamp to
        // 0 (which would look identical to "disk is completely full" and wrongly
        // bind every `min(...)` concurrency expression down to 0) — it must skip
        // the disk term entirely (usize::MAX, i.e. "no constraint from this axis").
        let stub_dir = tempfile::tempdir().unwrap();
        let stub_df = stub_dir.path().join("df");
        std::fs::write(&stub_df, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub_df).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub_df, perms).unwrap();
        }

        let tmp = tempfile::tempdir().unwrap();
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", stub_dir.path().display()));
        let limit = disk_headroom_limit(tmp.path());
        std::env::set_var("PATH", old_path);

        assert_eq!(
            limit,
            usize::MAX,
            "an unmeasurable disk probe must skip the clamp (usize::MAX), not silently become 0"
        );
    }

    #[test]
    fn test_disk_headroom_limit_measured_zero_still_clamps() {
        // Regression: a REAL 0-free-GB measurement (genuinely full disk) is a
        // legitimate, non-unmeasurable result and must still clamp to 0 via
        // disk_headroom's normal floor-division math. This guards against
        // over-correcting #4164 into "None and Some(0) behave the same".
        assert_eq!(disk_headroom(0, per_worktree_gb().max(1)), 0);
    }
}
