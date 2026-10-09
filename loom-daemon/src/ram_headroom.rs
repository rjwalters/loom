//! RAM-headroom math for the autonomous work finder (#5270 — "dumb mode").
//!
//! # Why this exists
//!
//! #5270 removed the token axis from the dynamic concurrency cap entirely
//! (operator direction: "we should only ever limit parallelism based on the
//! machine disk/RAM/CPU" — a metered API key or an overage-enabled
//! subscription pool has no exhaustible per-account ceiling worth modeling).
//! Disk headroom ([`crate::disk_headroom`]) and the CPU saturation admission
//! brake ([`crate::admission_brake`]) already covered two of those three
//! machine axes; this module is the third — the available-RAM analog of
//! [`crate::disk_headroom`], mirroring its shape exactly:
//!
//! - [`available_ram_gb`] reads the host's currently-available memory (not
//!   total — "available" already accounts for the kernel's own reclaimable
//!   caches/buffers, so it is the number a scheduler should compare against,
//!   the same distinction `free -h`'s `available` column makes over `free`).
//!   `None` means the probe was unmeasurable, not that 0 GB is available
//!   (mirrors disk headroom's #4164 "unknown != zero" policy).
//! - [`ram_headroom`] is the pure floor-division term: how many worktrees
//!   `available_gb` can host at `LOOM_PER_WORKTREE_RAM_GB` each.
//! - [`ram_headroom_limit`] combines the two, and — exactly like
//!   [`crate::disk_headroom::disk_headroom_limit`] — SKIPS the clamp
//!   (`usize::MAX`) when the probe is unmeasurable, so an unsupported
//!   platform or a missing `/proc/meminfo` never silently pins concurrency
//!   to 0.
//!
//! # Why read `/proc/meminfo` / shell to `vm_stat` instead of a `sysinfo`-style
//! # crate
//!
//! Same precedent as [`crate::disk_headroom`] (shell to `df`) and
//! [`crate::cpu_headroom`] (read `/proc/stat` / shell to `iostat`): no new
//! crate dependency, OS-native sources kept small and split into pure parsing
//! functions ([`parse_meminfo_available_kb`], [`parse_vm_stat_available_pages`])
//! so the arithmetic is unit-testable without a real host.
//!
//! - **Linux** reads `MemAvailable` directly from `/proc/meminfo` — the
//!   kernel's own estimate of memory available for new allocations without
//!   swapping (accounts for reclaimable slab/page-cache; a raw `MemFree` would
//!   undercount by treating reclaimable cache as unavailable).
//! - **macOS** has no `MemAvailable` equivalent; `vm_stat` reports free +
//!   inactive pages (inactive pages are reclaimable, mirroring what
//!   `MemAvailable` counts on Linux) and `sysctl -n hw.pagesize` gives the
//!   page size to convert to bytes.
//!
//! # Fail-open, not fail-closed
//!
//! Mirrors [`crate::disk_headroom`]: an unmeasurable probe returns `None` /
//! `usize::MAX` (skip the clamp), never a fabricated `Some(0)` / `0` that would
//! look identical to "genuinely out of RAM".

#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

/// Environment variable overriding the conservative per-worktree RAM estimate
/// (GB). Mirrors [`crate::disk_headroom::PER_WORKTREE_GB_ENV`].
pub const PER_WORKTREE_RAM_GB_ENV: &str = "LOOM_PER_WORKTREE_RAM_GB";

/// Default per-worktree RAM estimate (GB). A `claude`-driven sweep's own
/// footprint is modest (client process + git + occasional `cargo`), so this
/// mirrors the disk default rather than assuming a build-heavy workload —
/// same posture disk headroom takes, and just as tunable per host.
pub const DEFAULT_PER_WORKTREE_RAM_GB: u64 = 2;

/// Resolve the per-worktree RAM GB estimate from [`PER_WORKTREE_RAM_GB_ENV`],
/// flooring to a minimum of 1 (mirrors [`crate::disk_headroom::per_worktree_gb`]).
#[must_use]
pub fn per_worktree_ram_gb() -> u64 {
    std::env::var(PER_WORKTREE_RAM_GB_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_PER_WORKTREE_RAM_GB)
}

/// The operator's explicit [`PER_WORKTREE_RAM_GB_ENV`] value, if set and valid
/// (unlike [`per_worktree_ram_gb`], does not fall back to the default).
#[must_use]
pub fn env_per_worktree_ram_gb() -> Option<u64> {
    std::env::var(PER_WORKTREE_RAM_GB_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n >= 1)
}

/// Parse `MemAvailable` (in kB) from Linux `/proc/meminfo` contents.
///
/// `/proc/meminfo` lines are `"<Key>:<spaces><value> kB\n"`. `MemAvailable` is
/// the kernel's own estimate of memory available for new allocations without
/// swapping — it already accounts for reclaimable slab/page-cache, unlike
/// `MemFree` (which would undercount). Returns `None` when the field is
/// missing or malformed.
#[must_use]
pub fn parse_meminfo_available_kb(contents: &str) -> Option<u64> {
    let line = contents.lines().find(|l| l.starts_with("MemAvailable:"))?;
    // "MemAvailable:    1234567 kB" — the numeric field is the 2nd token.
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Read the current `MemAvailable` (GB) on Linux via `/proc/meminfo`.
#[cfg(target_os = "linux")]
#[must_use]
pub fn available_ram_gb() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_available_kb(&contents).map(|kb| kb / 1024 / 1024)
}

/// Parse the free + inactive page counts from macOS `vm_stat` output.
///
/// `vm_stat` prints one `"<Label>:<spaces><count>."` line per stat; the
/// reclaimable-equivalent of Linux's `MemAvailable` is free + inactive pages
/// (inactive pages hold evictable content, mirroring what `MemAvailable`
/// counts). Returns `None` when either field is missing/malformed.
#[must_use]
pub fn parse_vm_stat_available_pages(output: &str) -> Option<u64> {
    let field = |label: &str| -> Option<u64> {
        let line = output.lines().find(|l| l.trim_start().starts_with(label))?;
        let value = line.rsplit(':').next()?.trim().trim_end_matches('.');
        value.parse().ok()
    };
    let free = field("Pages free")?;
    let inactive = field("Pages inactive")?;
    Some(free + inactive)
}

/// Read the host's page size (bytes) via `sysctl -n hw.pagesize` on macOS.
#[cfg(target_os = "macos")]
fn read_macos_page_size() -> Option<u64> {
    let output = Command::new("sysctl")
        .arg("-n")
        .arg("hw.pagesize")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Read the current available RAM (GB) on macOS via `vm_stat` (free + inactive
/// pages) × the host page size.
#[cfg(target_os = "macos")]
#[must_use]
pub fn available_ram_gb() -> Option<u64> {
    let output = Command::new("vm_stat")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let pages = parse_vm_stat_available_pages(&String::from_utf8_lossy(&output.stdout))?;
    let page_size = read_macos_page_size()?;
    Some(pages.saturating_mul(page_size) / 1024 / 1024 / 1024)
}

/// No known available-RAM source on other platforms — the caller skips the
/// RAM clamp entirely (mirrors [`crate::cpu_headroom::read_loadavg_1m`]'s
/// "no source" fallback).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[must_use]
pub fn available_ram_gb() -> Option<u64> {
    None
}

/// The RAM-headroom concurrency term: how many worktrees `available_gb` can
/// host at `per_gb` GB each. Pure `floor(available_gb / per_gb)`, mirroring
/// [`crate::disk_headroom::disk_headroom`]. A `per_gb` of 0 is treated as 1
/// to avoid a divide-by-zero.
#[must_use]
pub fn ram_headroom(available_gb: u64, per_gb: u64) -> usize {
    let per = per_gb.max(1);
    usize::try_from(available_gb / per).unwrap_or(usize::MAX)
}

/// RAM headroom net of the in-flight sweeps' unrealised peaks (#11094):
/// `floor((available - reserved) / charge)`, saturating at 0. Pure.
#[must_use]
pub fn ram_headroom_reserved(available_gb: u64, reserved_gb: u64, charge_gb: u64) -> usize {
    ram_headroom(available_gb.saturating_sub(reserved_gb), charge_gb)
}

/// The RAM term of the total-concurrency cap. `additional` (see
/// [`ram_headroom_reserved`]) already nets out the in-flight sweeps'
/// reservations, so it counts only sweeps *beyond* the running ones; when
/// `additive` it is added to `occupancy` so the dispatch comparison
/// (`occupancy >= cap`) does not charge running work a second time. Otherwise
/// it is the legacy total-cap figure (#11094). Saturating, so `usize::MAX`
/// stays unbounded.
#[must_use]
pub fn ram_total_cap(additional: usize, occupancy: usize, additive: bool) -> usize {
    if additive {
        occupancy.saturating_add(additional)
    } else {
        additional
    }
}

/// One workspace's per-sweep RAM charge (#11094).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoCharge {
    /// The repo key ([`crate::ram_peaks::repo_key`]) whose history set it.
    pub repo: String,
    pub gb: u64,
    pub source: crate::ram_peaks::ChargeSource,
}

/// The per-tick RAM budget the multi-workspace dispatch loop debits (#11094):
/// what is left of available memory after the live scopes' reservation, and
/// each workspace's OWN charge (parallel to the tick's roots). A candidate is
/// admitted only while its own repo's charge still fits, and each admission
/// debits that charge — so one heavy repo's history defers that repo's work,
/// never a small sibling's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RamBudget {
    pub remaining_gb: u64,
    pub charges: Vec<RepoCharge>,
}

impl RamBudget {
    /// Workspace `idx`'s charge. A missing entry fails open (no charge).
    #[must_use]
    pub fn charge(&self, idx: usize) -> Option<&RepoCharge> {
        self.charges.get(idx)
    }

    /// Whether workspace `idx`'s charge still fits the remaining budget.
    #[must_use]
    pub fn fits(&self, idx: usize) -> bool {
        self.charge(idx).is_none_or(|c| c.gb <= self.remaining_gb)
    }

    /// Debit one admission of workspace `idx`, logging (INFO) which repo's
    /// history set an observed charge.
    pub fn debit(&mut self, idx: usize) {
        let Some(c) = self.charges.get(idx) else {
            return;
        };
        let before = self.remaining_gb;
        self.remaining_gb = before.saturating_sub(c.gb);
        if c.source == crate::ram_peaks::ChargeSource::Observed {
            log::info!(
                "ram_headroom: admitted a {} sweep at {}GB, charged from {}'s observed peak \
                 history; RAM budget {before}GB -> {}GB",
                c.repo,
                c.gb,
                c.repo,
                self.remaining_gb
            );
        }
    }

    /// Why workspace `idx` was deferred, for the ready-queue row.
    #[must_use]
    pub fn deferral_detail(&self, idx: usize) -> String {
        self.charge(idx).map_or_else(String::new, |c| {
            format!(
                "ram: {} charge {}GB ({}) exceeds remaining {}GB",
                c.repo,
                c.gb,
                c.source.as_str(),
                self.remaining_gb
            )
        })
    }
}

/// Sample the live agent scopes (folding finished scopes' peaks into the repo
/// history), then return the RAM term and per-repo budget for `roots` — the
/// production tick entry point (#11094).
#[must_use]
pub fn ram_headroom_limit_tick(
    pool: &std::sync::Arc<crate::workspace_pool::WorkspacePool>,
    roots: &[std::path::PathBuf],
) -> (usize, Option<RamBudget>) {
    let (scopes, occupancy) = crate::ram_peaks::live_scopes(pool, roots);
    crate::ram_peaks::record_tick(&scopes);
    ram_headroom_limit_for(roots, occupancy)
}

/// Pure RAM admission terms for `roots` (#11094), given `available_gb`, the
/// peak `store`, the number of sweeps in flight and the env override.
///
/// Each root is charged by its OWN history ([`crate::ram_peaks::charge_gb`]):
/// env override, else its observed high-water mark plus margin, else the flat
/// default. The live scopes' unrealised peaks are reserved first.
///
/// - With no history for any root and nothing reserved this is exactly the
///   legacy [`ram_headroom`] cap, and no budget.
/// - Otherwise the cap is `occupancy` plus what still fits at the SMALLEST
///   charge (so it never blocks a small repo on a heavy one's account), and
///   the returned [`RamBudget`] defers and debits per candidate.
#[must_use]
pub fn ram_admission(
    available_gb: u64,
    store: &crate::ram_peaks::Store,
    roots: &[std::path::PathBuf],
    occupancy: usize,
    env_gb: Option<u64>,
) -> (usize, Option<RamBudget>) {
    use crate::ram_peaks::{self, ChargeSource};
    let charges: Vec<RepoCharge> = roots
        .iter()
        .map(|r| {
            let repo = ram_peaks::repo_key(r);
            let observed = store
                .repos
                .get(&repo)
                .and_then(|h| ram_peaks::high_water_bytes(h));
            let (gb, source) = ram_peaks::charge_gb(env_gb, observed, DEFAULT_PER_WORKTREE_RAM_GB);
            RepoCharge { repo, gb, source }
        })
        .collect();
    let reserved_gb = ram_peaks::reserved_bytes(store).div_ceil(1024 * 1024 * 1024);
    let flat = env_gb.unwrap_or(DEFAULT_PER_WORKTREE_RAM_GB);
    if reserved_gb == 0 && charges.iter().all(|c| c.source != ChargeSource::Observed) {
        return (ram_headroom(available_gb, flat), None);
    }
    let min_charge = charges.iter().map(|c| c.gb).min().unwrap_or(flat);
    let additional = ram_headroom_reserved(available_gb, reserved_gb, min_charge);
    let budget = RamBudget {
        remaining_gb: available_gb.saturating_sub(reserved_gb),
        charges,
    };
    (ram_total_cap(additional, occupancy, true), Some(budget))
}

/// Observed-history-aware RAM term and per-repo budget for the given
/// workspace roots (#11094), where `occupancy` is the number of sweeps already
/// in flight; see [`ram_admission`]. Each repo's charge, its source, the
/// reservation and the remaining budget are logged (INFO on change, DEBUG
/// otherwise), so the log names the repo whose history set each charge.
#[must_use]
pub fn ram_headroom_limit_for(
    roots: &[std::path::PathBuf],
    occupancy: usize,
) -> (usize, Option<RamBudget>) {
    use crate::ram_peaks;
    let Some(available) = available_ram_gb() else {
        return (ram_headroom_limit(), None);
    };
    let store = ram_peaks::store_path()
        .map(|p| ram_peaks::load(&p))
        .unwrap_or_default();
    let (headroom, budget) =
        ram_admission(available, &store, roots, occupancy, env_per_worktree_ram_gb());
    let reserved_gb = ram_peaks::reserved_bytes(&store).div_ceil(1024 * 1024 * 1024);
    let charges = budget.as_ref().map_or_else(
        || "flat".to_string(),
        |b| {
            b.charges
                .iter()
                .map(|c| format!("{}:{}GB({})", c.repo, c.gb, c.source.as_str()))
                .collect::<Vec<_>>()
                .join(",")
        },
    );
    let remaining = budget.as_ref().map_or(available, |b| b.remaining_gb);
    let line = format!(
        "ram_headroom: charges=[{charges}] in_flight_reservation={reserved_gb}GB \
         available={available}GB remaining={remaining}GB in_flight={occupancy} \
         headroom={headroom}"
    );
    static LAST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let key = format!("{charges}/{reserved_gb}/{headroom}");
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.as_deref() == Some(key.as_str()) {
        log::debug!("{line}");
    } else {
        log::info!("{line}");
        *last = Some(key);
    }
    (headroom, budget)
}

/// Resolve the RAM-headroom concurrency bound for the current host: the
/// number of worktrees the currently-available memory can hold at the
/// resolved per-worktree estimate.
///
/// Unknown != zero (mirrors [`crate::disk_headroom::disk_headroom_limit`]):
/// when the probe is unmeasurable, this SKIPS the RAM clamp entirely —
/// returning `usize::MAX` so the RAM term never binds the `min(...)`
/// concurrency expression — and logs a warning, rather than silently treating
/// the unmeasurable probe as "no RAM available".
#[must_use]
pub fn ram_headroom_limit() -> usize {
    match available_ram_gb() {
        Some(gb) => ram_headroom(gb, per_worktree_ram_gb()),
        None => {
            log::warn!(
                "ram_headroom: could not measure available RAM on this host — skipping the RAM \
                 clamp (treating as unbounded)"
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
    // parse_meminfo_available_kb — /proc/meminfo parsing (Linux)
    // ===================================================================

    #[test]
    fn test_parse_meminfo_reads_mem_available() {
        let out = "MemTotal:       16384000 kB\n\
                    MemFree:         2048000 kB\n\
                    MemAvailable:    8192000 kB\n\
                    Buffers:          512000 kB\n";
        assert_eq!(parse_meminfo_available_kb(out), Some(8_192_000));
    }

    #[test]
    fn test_parse_meminfo_missing_field_is_none() {
        assert_eq!(parse_meminfo_available_kb("MemTotal: 16384000 kB\n"), None);
        assert_eq!(parse_meminfo_available_kb(""), None);
    }

    #[test]
    fn test_parse_meminfo_malformed_value_is_none() {
        assert_eq!(parse_meminfo_available_kb("MemAvailable: not-a-number kB\n"), None);
    }

    // ===================================================================
    // parse_vm_stat_available_pages — vm_stat parsing (macOS)
    // ===================================================================

    #[test]
    fn test_parse_vm_stat_sums_free_and_inactive() {
        let out = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
                    Pages free:                              10000.\n\
                    Pages active:                            50000.\n\
                    Pages inactive:                           5000.\n\
                    Pages speculative:                         100.\n";
        assert_eq!(parse_vm_stat_available_pages(out), Some(15_000));
    }

    #[test]
    fn test_parse_vm_stat_missing_field_is_none() {
        assert_eq!(
            parse_vm_stat_available_pages("Pages free: 10000.\n"),
            None,
            "missing 'Pages inactive'"
        );
        assert_eq!(parse_vm_stat_available_pages(""), None);
    }

    // ===================================================================
    // ram_headroom — pure floor division
    // ===================================================================

    #[test]
    fn test_ram_headroom_floors() {
        assert_eq!(ram_headroom(20, 2), 10);
        assert_eq!(ram_headroom(21, 2), 10); // floor
        assert_eq!(ram_headroom(1, 2), 0); // less than one worktree fits
        assert_eq!(ram_headroom(0, 2), 0);
    }

    #[test]
    fn test_ram_headroom_per_gb_zero_treated_as_one() {
        assert_eq!(ram_headroom(5, 0), 5);
    }

    // ===================================================================
    // ram_headroom_reserved — #11094 in-flight reservation
    // ===================================================================

    #[test]
    fn test_ram_headroom_reserved_blocks_second_admission() {
        // 13 GB repo peak (charge 15 with margin), 30 GB host, one sweep
        // running at 4 GB so far: available 26, reservation 9 -> 17 / 15 = 1,
        // and with 11 GB already realised elsewhere (available 19): 10/15 = 0.
        assert_eq!(ram_headroom_reserved(26, 9, 15), 1);
        assert_eq!(ram_headroom_reserved(19, 9, 15), 0);
    }

    #[test]
    fn test_ram_total_cap_does_not_charge_running_sweeps_twice() {
        // available 26, reservation 9, charge 15 -> one more sweep fits.
        let additional = ram_headroom_reserved(26, 9, 15);
        // One sweep already running: the cap is 1 + 1 = 2, so the dispatch
        // comparison `occupancy >= cap` (1 >= 2) admits the second sweep ...
        let cap = ram_total_cap(additional, 1, true);
        assert_eq!(cap, 2);
        assert!(crate::work_finder::resolve_dynamic_max_concurrent(usize::MAX, cap, 4) > 1);
        // ... and not a third (2 >= 2).
        assert_eq!(ram_total_cap(ram_headroom_reserved(19, 9, 15), 2, true), 2);
        // Legacy figure is passed through untouched; usize::MAX stays unbounded.
        assert_eq!(ram_total_cap(3, 5, false), 3);
        assert_eq!(ram_total_cap(usize::MAX, 5, true), usize::MAX);
    }

    // ===================================================================
    // ram_admission / RamBudget — #11094 per-repo charge
    // ===================================================================

    const GIB: u64 = 1024 * 1024 * 1024;

    fn roots() -> Vec<std::path::PathBuf> {
        vec!["/w/heavy".into(), "/w/small".into()]
    }

    #[test]
    fn test_ram_admission_charges_each_repo_its_own_history() {
        let mut store = crate::ram_peaks::Store::default();
        store.repos.insert("heavy".into(), vec![13 * GIB]);
        // 14 GB available, nothing running: the heavy repo's 15 GB charge
        // must not zero the cap for the small repo (2 GB default).
        let (cap, budget) = ram_admission(14, &store, &roots(), 0, None);
        let b = budget.unwrap();
        assert_eq!(cap, 7, "cap is set by the smallest charge, not the largest");
        assert_eq!((b.charges[0].gb, b.charges[1].gb), (15, 2));
        assert_eq!(b.charges[0].repo, "heavy");
        assert!(!b.fits(0), "heavy: 15 > 14");
        assert!(b.fits(1), "small: 2 <= 14");
        assert!(b
            .deferral_detail(0)
            .contains("heavy charge 15GB (observed)"));
    }

    #[test]
    fn test_ram_budget_debits_each_admission() {
        let mut store = crate::ram_peaks::Store::default();
        store.repos.insert("heavy".into(), vec![13 * GIB]);
        let (_, budget) = ram_admission(20, &store, &roots(), 0, None);
        let mut b = budget.unwrap();
        assert!(b.fits(0));
        b.debit(0); // 20 - 15 = 5
        assert_eq!(b.remaining_gb, 5);
        assert!(!b.fits(0), "a second heavy sweep no longer fits");
        b.debit(1);
        b.debit(1);
        assert_eq!(b.remaining_gb, 1);
        assert!(!b.fits(1));
        b.debit(1);
        assert_eq!(b.remaining_gb, 0, "saturates");
        assert!(b.fits(9), "an unseeded index fails open");
    }

    #[test]
    fn test_ram_admission_reserves_live_scopes_host_wide() {
        let mut store = crate::ram_peaks::Store::default();
        store.repos.insert("small".into(), vec![3 * GIB]);
        // A role agent with no issue lock, in `small`, at 1 of its 3 GB.
        store.inflight.insert(
            "loom-agent-9-9.scope".into(),
            crate::ram_peaks::InFlight {
                repo: "small".into(),
                issue: None,
                peak_bytes: GIB,
                current_bytes: GIB,
            },
        );
        let (cap, budget) = ram_admission(10, &store, &roots(), 0, None);
        let b = budget.unwrap();
        assert_eq!(b.remaining_gb, 8, "10 available - 2 reserved");
        // small is charged 3 + 10% -> 4; heavy has no history -> 2.
        assert_eq!((b.charges[0].gb, b.charges[1].gb), (2, 4));
        assert_eq!(cap, 4);
    }

    #[test]
    fn test_ram_admission_no_history_is_the_legacy_flat_cap() {
        let store = crate::ram_peaks::Store::default();
        assert_eq!(ram_admission(21, &store, &roots(), 3, None), (10, None));
        assert_eq!(ram_admission(21, &store, &roots(), 3, Some(4)), (5, None));
    }

    #[test]
    fn test_ram_headroom_reserved_no_history_equals_flat() {
        assert_eq!(ram_headroom_reserved(21, 0, 2), ram_headroom(21, 2));
    }

    #[test]
    fn test_ram_headroom_reserved_saturates_at_zero() {
        assert_eq!(ram_headroom_reserved(5, 50, 2), 0);
    }

    // ===================================================================
    // per_worktree_ram_gb — env resolution
    // ===================================================================

    #[test]
    #[serial]
    fn test_per_worktree_ram_gb_default_and_override() {
        std::env::remove_var(PER_WORKTREE_RAM_GB_ENV);
        assert_eq!(per_worktree_ram_gb(), DEFAULT_PER_WORKTREE_RAM_GB);

        std::env::set_var(PER_WORKTREE_RAM_GB_ENV, "4");
        assert_eq!(per_worktree_ram_gb(), 4);

        // Zero and unparseable fall back to the default.
        std::env::set_var(PER_WORKTREE_RAM_GB_ENV, "0");
        assert_eq!(per_worktree_ram_gb(), DEFAULT_PER_WORKTREE_RAM_GB);
        std::env::set_var(PER_WORKTREE_RAM_GB_ENV, "garbage");
        assert_eq!(per_worktree_ram_gb(), DEFAULT_PER_WORKTREE_RAM_GB);
        std::env::remove_var(PER_WORKTREE_RAM_GB_ENV);
    }

    // ===================================================================
    // available_ram_gb / ram_headroom_limit — smoke tests against the real host
    // ===================================================================

    #[test]
    fn test_available_ram_gb_returns_a_plausible_value_or_none() {
        // Integration smoke: on Linux/macOS this should return Some(<plausible
        // value>); on any other platform it is unconditionally None. Either
        // way it must never panic, and a Some value must be sane.
        if let Some(gb) = available_ram_gb() {
            assert!(gb < 100_000, "no real host has 100+ TB of available RAM");
        }
    }

    #[test]
    #[serial]
    fn test_ram_headroom_limit_never_panics() {
        // Whatever this host reports, the limit must be well-typed — either a
        // real headroom count or the unmeasurable sentinel.
        let limit = ram_headroom_limit();
        assert!(limit == usize::MAX || limit < 100_000_000);
    }
}
