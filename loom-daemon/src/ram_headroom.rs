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

/// Current available RAM (GB), or `None` when unmeasurable. Under
/// `cfg(test)` a [`set_available_ram_gb_override`] value wins (#11679).
#[must_use]
pub fn available_ram_gb() -> Option<u64> {
    #[cfg(test)]
    if let Some(v) = *AVAILABLE_RAM_GB_OVERRIDE.lock().unwrap() {
        return Some(v);
    }
    probe_available_ram_gb()
}

/// Test-only pin for [`available_ram_gb`] so tests do not depend on live host RAM.
#[cfg(test)]
static AVAILABLE_RAM_GB_OVERRIDE: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_available_ram_gb_override(v: Option<u64>) {
    *AVAILABLE_RAM_GB_OVERRIDE.lock().unwrap() = v;
}

/// Read the current `MemAvailable` (GB) on Linux via `/proc/meminfo`.
#[cfg(target_os = "linux")]
#[must_use]
fn probe_available_ram_gb() -> Option<u64> {
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
fn probe_available_ram_gb() -> Option<u64> {
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
fn probe_available_ram_gb() -> Option<u64> {
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
/// history), then return the RAM term, per-repo budget and the term's
/// telemetry for `roots` under `config` — the production tick entry point
/// (#11094, #12330). Scopes are sampled in every mode, so the peak history
/// stays current while a fleet runs `flat` or `off`.
#[must_use]
pub fn ram_headroom_limit_tick(
    pool: &std::sync::Arc<crate::workspace_pool::WorkspacePool>,
    roots: &[std::path::PathBuf],
    config: RamAdmissionConfig,
) -> (usize, Option<RamBudget>, RamTerm) {
    let (scopes, in_flight) = crate::ram_peaks::live_scopes(pool, roots);
    crate::ram_peaks::record_tick(&scopes, roots);
    ram_headroom_limit_for(roots, &in_flight, config)
}

// ============================================================================
// Admission mode (#12330): `autonomous.workFinder.ramAdmission` / `ramChargeGb`
// ============================================================================

/// Config path choosing how the RAM term is computed (#12330).
pub const RAM_ADMISSION_KEY: &str = "autonomous.workFinder.ramAdmission";
/// Config path of the flat per-sweep charge (GB) used by `flat` (#12330).
pub const RAM_CHARGE_GB_KEY: &str = "autonomous.workFinder.ramChargeGb";

/// How the work finder's RAM term is computed (#12330).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RamMode {
    /// Per-repo observed peaks (#11094): each in-flight sweep reserves its
    /// repo's unrealised high-water and each admission is charged its repo's
    /// padded peak. The default, and the behaviour before #12330.
    #[default]
    Observed,
    /// Every sweep, in flight or new, at one flat charge
    /// ([`RAM_CHARGE_GB_KEY`], or [`PER_WORKTREE_RAM_GB_ENV`]).
    Flat,
    /// No RAM term (`usize::MAX`): disk headroom and the configured
    /// ceiling still bound the cap.
    Off,
}

impl RamMode {
    /// The config / telemetry spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Flat => "flat",
            Self::Off => "off",
        }
    }

    /// Parse the config spelling (trimmed, case-insensitive); anything else
    /// is `None`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "observed" => Some(Self::Observed),
            "flat" => Some(Self::Flat),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// The resolved RAM admission settings for one tick (#12330).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamAdmissionConfig {
    pub mode: RamMode,
    /// The flat per-sweep charge (GB, >= 1); used by [`RamMode::Flat`].
    pub charge_gb: u64,
    /// Whether [`PER_WORKTREE_RAM_GB_ENV`] set it (forcing `flat`).
    pub from_env: bool,
}

impl Default for RamAdmissionConfig {
    fn default() -> Self {
        Self {
            mode: RamMode::Observed,
            charge_gb: DEFAULT_PER_WORKTREE_RAM_GB,
            from_env: false,
        }
    }
}

/// Resolve the RAM admission settings from an effective config and the env
/// override, with precedence env > config > default (#12330). Pure.
///
/// - [`PER_WORKTREE_RAM_GB_ENV`] set (a positive integer) forces `flat` at
///   that charge, whatever the config says.
/// - Else [`RAM_ADMISSION_KEY`] (`observed` / `flat` / `off`) and
///   [`RAM_CHARGE_GB_KEY`] (a positive integer). An unknown mode or a
///   non-positive / non-integer charge falls back to its default (`observed`,
///   [`DEFAULT_PER_WORKTREE_RAM_GB`]), never to an error or a zero charge.
#[must_use]
pub fn ram_admission_config_from(
    effective: &serde_json::Value,
    env_gb: Option<u64>,
) -> RamAdmissionConfig {
    if let Some(gb) = env_gb {
        return RamAdmissionConfig {
            mode: RamMode::Flat,
            charge_gb: gb.max(1),
            from_env: true,
        };
    }
    let mode = crate::config_resolver::get_path(effective, RAM_ADMISSION_KEY)
        .and_then(serde_json::Value::as_str)
        .and_then(RamMode::parse)
        .unwrap_or_default();
    let charge_gb = crate::config_resolver::get_path(effective, RAM_CHARGE_GB_KEY)
        .and_then(serde_json::Value::as_u64)
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_PER_WORKTREE_RAM_GB);
    RamAdmissionConfig {
        mode,
        charge_gb,
        from_env: false,
    }
}

/// The RAM admission settings for the daemon rooted at `root`, read live from
/// its layered config (host-local tier included) every tick (#12330).
#[must_use]
pub fn ram_admission_config(root: &std::path::Path) -> RamAdmissionConfig {
    ram_admission_config_from(
        &crate::config_resolver::resolve_effective_config(root),
        env_per_worktree_ram_gb(),
    )
}

/// What one tick's RAM term was, for the `loom.dispatch.tick` span (#12330).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamTerm {
    pub mode: RamMode,
    /// GB held back for the in-flight sweeps; `None` when not computed
    /// (`off`, or available RAM unmeasurable).
    pub reserved_gb: Option<u64>,
    /// Available GB left after the reservation; `None` when not computed.
    pub remaining_gb: Option<u64>,
}

/// The RAM term under `config` (#12330). Pure.
///
/// - `observed`: [`ram_admission_reserving`] (no env charge — env forces
///   `flat`), unchanged from #11094.
/// - `flat`: every in-flight sweep reserves `charge_gb` and each admission is
///   charged `charge_gb`, so the cap is the in-flight count plus what fits in
///   `available - in_flight * charge_gb`. With no sweeps in flight this is the
///   legacy `floor(available / charge)`.
/// - `off`: `usize::MAX` and no budget.
#[must_use]
pub fn ram_admission_mode(
    config: RamAdmissionConfig,
    available_gb: u64,
    store: &crate::ram_peaks::Store,
    roots: &[std::path::PathBuf],
    in_flight: &[usize],
) -> (usize, Option<RamBudget>, RamTerm) {
    let term = |reserved: Option<u64>, remaining: Option<u64>| RamTerm {
        mode: config.mode,
        reserved_gb: reserved,
        remaining_gb: remaining,
    };
    match config.mode {
        RamMode::Off => (usize::MAX, None, term(None, None)),
        RamMode::Observed => {
            let (cap, budget, reserved) =
                ram_admission_reserving(available_gb, store, roots, in_flight, None);
            let remaining = budget.as_ref().map_or(available_gb, |b| b.remaining_gb);
            (cap, budget, term(Some(reserved), Some(remaining)))
        }
        RamMode::Flat => {
            let charge = config.charge_gb.max(1);
            let occupancy: usize = in_flight.iter().sum();
            let reserved = u64::try_from(occupancy)
                .unwrap_or(u64::MAX)
                .saturating_mul(charge);
            let remaining = available_gb.saturating_sub(reserved);
            let source = if config.from_env {
                crate::ram_peaks::ChargeSource::Env
            } else {
                crate::ram_peaks::ChargeSource::Config
            };
            let charges = roots
                .iter()
                .map(|r| RepoCharge {
                    repo: crate::ram_peaks::repo_key(r),
                    gb: charge,
                    source,
                })
                .collect();
            let additional = ram_headroom(remaining, charge);
            let budget = RamBudget {
                remaining_gb: remaining,
                charges,
            };
            (
                ram_total_cap(additional, occupancy, true),
                Some(budget),
                term(Some(reserved), Some(remaining)),
            )
        }
    }
}

/// The last multi-workspace tick's RAM term (#12330), read by the tick span.
static LAST_TERM: std::sync::Mutex<Option<RamTerm>> = std::sync::Mutex::new(None);

/// Record this tick's RAM term for the `loom.dispatch.tick` span.
pub fn record_term(term: RamTerm) {
    *LAST_TERM
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(term);
}

/// The last recorded RAM term, or `None` before the first tick.
#[must_use]
pub fn last_term() -> Option<RamTerm> {
    *LAST_TERM
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Pure RAM admission terms for `roots` (#11094), given `available_gb`, the
/// peak `store`, the number of sweeps in flight in each root (`in_flight`,
/// parallel to `roots`) and the env override.
///
/// Each root is charged by its OWN history ([`crate::ram_peaks::charge_gb`]):
/// env override, else its observed high-water mark plus margin, else the flat
/// default. The live scopes' unrealised peaks are reserved first.
///
/// - With no history for any root and nothing reserved this is exactly the
///   legacy [`ram_headroom`] cap, and no budget.
/// - Otherwise every in-flight sweep is accounted for exactly once before it
///   is credited back into the cap: a sweep sampled with an expected peak
///   ([`crate::ram_peaks::accounted_sweeps`]) by its live reservation, and any
///   other — no scope yet, an unreadable cgroup, a repo with no history — by
///   a full charge of its own repo, as if it had not started. The cap is then
///   the in-flight total plus what still fits at the SMALLEST charge (so it
///   never blocks a small repo on a heavy one's account), and the returned
///   [`RamBudget`] defers and debits per candidate.
#[must_use]
pub fn ram_admission(
    available_gb: u64,
    store: &crate::ram_peaks::Store,
    roots: &[std::path::PathBuf],
    in_flight: &[usize],
    env_gb: Option<u64>,
) -> (usize, Option<RamBudget>) {
    let (cap, budget, _) = ram_admission_reserving(available_gb, store, roots, in_flight, env_gb);
    (cap, budget)
}

/// [`ram_admission`], also returning the GB it held back for the in-flight
/// sweeps (the live reservation plus the unaccounted sweeps' charges; `0` on
/// the legacy flat path) — the figure the tick span exports (#12330).
#[must_use]
pub fn ram_admission_reserving(
    available_gb: u64,
    store: &crate::ram_peaks::Store,
    roots: &[std::path::PathBuf],
    in_flight: &[usize],
    env_gb: Option<u64>,
) -> (usize, Option<RamBudget>, u64) {
    let occupancy: usize = in_flight.iter().sum();
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
        return (ram_headroom(available_gb, flat), None, 0);
    }
    let reserved_gb = reserved_gb.saturating_add(unaccounted_gb(store, &charges, in_flight));
    let min_charge = charges.iter().map(|c| c.gb).min().unwrap_or(flat);
    let additional = ram_headroom_reserved(available_gb, reserved_gb, min_charge);
    let budget = RamBudget {
        remaining_gb: available_gb.saturating_sub(reserved_gb),
        charges,
    };
    (ram_total_cap(additional, occupancy, true), Some(budget), reserved_gb)
}

/// GB to hold back for the in-flight sweeps the live reservation does NOT
/// account for (#11094): each root's lock count minus its sampled sweeps with
/// an expected peak, each charged that root's own charge. A repo key's
/// accounted sweeps are handed out once across roots sharing that key, so a
/// sample is never credited twice.
fn unaccounted_gb(
    store: &crate::ram_peaks::Store,
    charges: &[RepoCharge],
    in_flight: &[usize],
) -> u64 {
    let mut accounted = crate::ram_peaks::accounted_sweeps(store);
    charges
        .iter()
        .zip(in_flight)
        .map(|(c, &n)| {
            let pool = accounted.get_mut(&c.repo);
            let covered = pool.map_or(0, |p| {
                let take = (*p).min(n);
                *p -= take;
                take
            });
            u64::try_from(n - covered)
                .unwrap_or(u64::MAX)
                .saturating_mul(c.gb)
        })
        .fold(0, u64::saturating_add)
}

/// The RAM term, per-repo budget and telemetry for the given workspace roots
/// under `config` (#11094, #12330), where `in_flight` is the number of sweeps
/// already in flight in each root; see [`ram_admission_mode`]. Each repo's
/// charge, its source, the reservation and the remaining budget are logged
/// (INFO on change, DEBUG otherwise), so the log names the repo whose history
/// set each charge.
#[must_use]
pub fn ram_headroom_limit_for(
    roots: &[std::path::PathBuf],
    in_flight: &[usize],
    config: RamAdmissionConfig,
) -> (usize, Option<RamBudget>, RamTerm) {
    let occupancy: usize = in_flight.iter().sum();
    let unmeasured = RamTerm {
        mode: config.mode,
        reserved_gb: None,
        remaining_gb: None,
    };
    if config.mode == RamMode::Off {
        return (usize::MAX, None, unmeasured);
    }
    let Some(available) = available_ram_gb() else {
        // Unknown != zero: skip the clamp (logs the warning).
        return (ram_headroom_limit_at(None, config.charge_gb), None, unmeasured);
    };
    let store = crate::ram_peaks::store_path()
        .map(|p| crate::ram_peaks::load(&p))
        .unwrap_or_default();
    let (headroom, budget, term) = ram_admission_mode(config, available, &store, roots, in_flight);
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
    let reserved_gb = term.reserved_gb.unwrap_or(0);
    let remaining = term.remaining_gb.unwrap_or(available);
    let mode = config.mode.as_str();
    let line = format!(
        "ram_headroom: mode={mode} charges=[{charges}] in_flight_reservation={reserved_gb}GB \
         available={available}GB remaining={remaining}GB in_flight={occupancy} \
         headroom={headroom}"
    );
    static LAST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let key = format!("{mode}/{charges}/{reserved_gb}/{headroom}");
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.as_deref() == Some(key.as_str()) {
        log::debug!("{line}");
    } else {
        log::info!("{line}");
        *last = Some(key);
    }
    (headroom, budget, term)
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
    ram_headroom_limit_at(available_ram_gb(), per_worktree_ram_gb())
}

/// [`ram_headroom_limit`] for a given probe result and charge.
fn ram_headroom_limit_at(available: Option<u64>, per_gb: u64) -> usize {
    match available {
        Some(gb) => ram_headroom(gb, per_gb),
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
        let (cap, budget) = ram_admission(14, &store, &roots(), &[0, 0], None);
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
        let (_, budget) = ram_admission(20, &store, &roots(), &[0, 0], None);
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
                ..crate::ram_peaks::InFlight::default()
            },
        );
        let (cap, budget) = ram_admission(10, &store, &roots(), &[0, 0], None);
        let b = budget.unwrap();
        assert_eq!(b.remaining_gb, 8, "10 available - 2 reserved");
        // small is charged 3 + 10% -> 4; heavy has no history -> 2.
        assert_eq!((b.charges[0].gb, b.charges[1].gb), (2, 4));
        assert_eq!(cap, 4);
    }

    #[test]
    fn test_ram_admission_no_history_is_the_legacy_flat_cap() {
        let store = crate::ram_peaks::Store::default();
        assert_eq!(ram_admission(21, &store, &roots(), &[3, 0], None), (10, None));
        assert_eq!(ram_admission(21, &store, &roots(), &[3, 0], Some(4)), (5, None));
    }

    fn sweep(repo: &str, issue: u32, current: u64) -> crate::ram_peaks::InFlight {
        crate::ram_peaks::InFlight {
            repo: repo.into(),
            issue: Some(issue),
            peak_bytes: current,
            current_bytes: current,
            ..crate::ram_peaks::InFlight::default()
        }
    }

    #[test]
    fn test_ram_admission_unsampled_sweeps_are_not_credited_for_free() {
        // The Judge's counterexample: 28 GB available, three heavy sweeps in
        // flight with no scope stamped yet (nothing in `inflight`), heavy's
        // observed charge 15 GB. Crediting all three back gave 3 + 1 = 4 and
        // a budget that admitted a fourth. Each unsampled sweep is now charged
        // its repo's 15 GB, so nothing is left: cap == occupancy, no admission.
        let mut store = crate::ram_peaks::Store::default();
        store.repos.insert("heavy".into(), vec![13 * GIB]);
        let (cap, budget) = ram_admission(28, &store, &roots(), &[3, 0], None);
        let b = budget.unwrap();
        assert_eq!(cap, 3, "no room beyond the three running sweeps");
        assert_eq!(b.remaining_gb, 0, "28 - 3 x 15 saturates");
        assert!(!b.fits(0), "a fourth heavy sweep is deferred");
        assert!(!b.fits(1), "and so is a small one: the host is spoken for");
        // Legacy (flat 2 GB) would give floor(28 / 2) = 14 >= 3 here; the
        // observed-history path is now the more conservative of the two.
        let legacy = ram_headroom(28, DEFAULT_PER_WORKTREE_RAM_GB);
        assert!(cap <= legacy);
    }

    #[test]
    fn test_ram_admission_unreadable_cgroup_is_charged_like_no_scope() {
        // A stamped scope whose cgroup files cannot be read is skipped by
        // `observe`, so it never reaches `inflight` and must be charged.
        let mut store = crate::ram_peaks::Store::default();
        store.repos.insert("heavy".into(), vec![13 * GIB]);
        let live = vec![crate::ram_peaks::LiveScope {
            scope: "loom-agent-1-1.scope".into(),
            repo: "heavy".into(),
            issue: Some(1),
            role: None,
        }];
        crate::ram_peaks::observe(&mut store, &live, |_| None);
        assert!(store.inflight.is_empty());
        let (cap, budget) = ram_admission(28, &store, &roots(), &[1, 0], None);
        let b = budget.unwrap();
        assert_eq!(b.remaining_gb, 13, "28 - the unsampled sweep's 15 GB");
        assert_eq!(cap, 1 + 6, "one running + floor(13 / 2) at small's charge");
        assert!(!b.fits(0), "15 > 13: no second heavy sweep");
    }

    #[test]
    fn test_ram_admission_stale_unreadable_sweep_is_charged_in_full() {
        // Sampled once, then its cgroup stays unreadable: below the threshold
        // it is still credited; at MAX_UNREADABLE_TICKS it is charged in full.
        use crate::ram_peaks::{observe, LiveScope, MAX_UNREADABLE_TICKS};
        let mut store = crate::ram_peaks::Store::default();
        store.repos.insert("heavy".into(), vec![13 * GIB]);
        let live = vec![LiveScope {
            scope: "loom-agent-1-1.scope".into(),
            repo: "heavy".into(),
            issue: Some(1),
            role: None,
        }];
        observe(&mut store, &live, |_| Some((5 * GIB, 5 * GIB)));
        for _ in 0..MAX_UNREADABLE_TICKS - 1 {
            observe(&mut store, &live, |_| None);
        }
        let roots: Vec<std::path::PathBuf> = vec!["/w/heavy".into()];
        let (_, b) = ram_admission(28, &store, &roots, &[1], None);
        assert_eq!(b.unwrap().remaining_gb, 20, "still credited: 28 - 8");
        observe(&mut store, &live, |_| None);
        assert!(store.inflight.is_empty());
        let (_, b) = ram_admission(28, &store, &roots, &[1], None);
        assert_eq!(b.unwrap().remaining_gb, 13, "charged in full: 28 - 15");
    }

    #[test]
    fn test_ram_admission_sampled_sweep_is_credited_with_its_reservation() {
        // One heavy sweep sampled at 5 GB of its 13 GB high-water mark: it
        // reserves 8 GB and is credited back, with no second, flat charge.
        let mut store = crate::ram_peaks::Store::default();
        store.repos.insert("heavy".into(), vec![13 * GIB]);
        store
            .inflight
            .insert("s1".into(), sweep("heavy", 1, 5 * GIB));
        let (cap, budget) = ram_admission(28, &store, &["/w/heavy".into()], &[1], None);
        let b = budget.unwrap();
        assert_eq!(b.remaining_gb, 20, "28 - 8 reserved, not also - 15");
        assert_eq!(cap, 1 + 1, "the running sweep + floor(20 / 15)");
        assert!(b.fits(0));
    }

    // ===================================================================
    // ramAdmission / ramChargeGb — #12330
    // ===================================================================

    /// The #12330 host: 60 GB available, 12 light in-flight sweeps (1 GB
    /// each) of a repo whose recorded peak pads to 15 GB.
    fn idle_host_with_heavy_history() -> (crate::ram_peaks::Store, Vec<std::path::PathBuf>) {
        let mut store = crate::ram_peaks::Store::default();
        store.repos.insert("heavy".into(), vec![13 * GIB + GIB / 2]);
        for i in 0..12 {
            store
                .inflight
                .insert(format!("s{i}"), sweep("heavy", i + 1, GIB));
        }
        (store, vec!["/w/heavy".into()])
    }

    fn cfg(mode: RamMode, charge_gb: u64) -> RamAdmissionConfig {
        RamAdmissionConfig {
            mode,
            charge_gb,
            from_env: false,
        }
    }

    #[test]
    fn test_ram_mode_observed_keeps_the_11094_numbers() {
        let (store, roots) = idle_host_with_heavy_history();
        let (cap, budget, term) =
            ram_admission_mode(RamAdmissionConfig::default(), 60, &store, &roots, &[12]);
        // Each sweep reserves 13.5 - 1 = 12.5 GB of unrealised peak: 150 GB
        // reserved against 60 available, so nothing new fits.
        assert_eq!(
            (cap, budget.clone()),
            ram_admission(60, &store, &roots, &[12], None),
            "observed is exactly the pre-#12330 term"
        );
        assert_eq!(cap, 12, "pinned at the in-flight count: no new admission");
        assert_eq!(term.mode, RamMode::Observed);
        assert_eq!(term.reserved_gb, Some(150));
        assert_eq!(term.remaining_gb, Some(0));
        assert_eq!(budget.unwrap().charges[0].gb, 15);
    }

    #[test]
    fn test_ram_mode_flat_does_not_pin_an_idle_host() {
        let (store, roots) = idle_host_with_heavy_history();
        let (cap, budget, term) =
            ram_admission_mode(cfg(RamMode::Flat, 2), 60, &store, &roots, &[12]);
        // 12 x 2 GB reserved, (60 - 24) / 2 = 18 more.
        assert_eq!(cap, 12 + 18);
        assert_eq!(term.reserved_gb, Some(24));
        assert_eq!(term.remaining_gb, Some(36));
        let b = budget.unwrap();
        assert_eq!(b.charges[0].gb, 2);
        assert_eq!(b.charges[0].source, crate::ram_peaks::ChargeSource::Config);
        // With nothing in flight it is the legacy floor(available / charge).
        let (cap, _, _) = ram_admission_mode(cfg(RamMode::Flat, 4), 21, &store, &roots, &[0]);
        assert_eq!(cap, 5);
        // An over-committed host admits nothing new, and never goes below
        // the running count.
        let (cap, _, term) = ram_admission_mode(cfg(RamMode::Flat, 8), 60, &store, &roots, &[12]);
        assert_eq!((cap, term.remaining_gb), (12, Some(0)));
    }

    #[test]
    fn test_ram_mode_off_is_unbounded() {
        let (store, roots) = idle_host_with_heavy_history();
        let (cap, budget, term) =
            ram_admission_mode(cfg(RamMode::Off, 2), 60, &store, &roots, &[12]);
        assert_eq!(cap, usize::MAX);
        assert!(budget.is_none());
        assert_eq!((term.reserved_gb, term.remaining_gb), (None, None));
        // Disk and the configured max still bound the cap.
        assert_eq!(crate::work_finder::resolve_dynamic_max_concurrent(9, cap, 20), 9);
        assert_eq!(crate::work_finder::resolve_dynamic_max_concurrent(40, cap, 20), 20);
    }

    #[test]
    fn test_ram_admission_config_reads_the_keys() {
        use serde_json::json;
        let at = |mode: serde_json::Value, gb: serde_json::Value| json!({"autonomous": {"workFinder": {"ramAdmission": mode, "ramChargeGb": gb}}});
        assert_eq!(ram_admission_config_from(&json!({}), None), RamAdmissionConfig::default());
        let c = ram_admission_config_from(&at(json!("flat"), json!(3)), None);
        assert_eq!((c.mode, c.charge_gb, c.from_env), (RamMode::Flat, 3, false));
        let c = ram_admission_config_from(&at(json!(" OFF "), json!(3)), None);
        assert_eq!(c.mode, RamMode::Off);
        let c = ram_admission_config_from(&at(json!("observed"), json!(3)), None);
        assert_eq!(c.mode, RamMode::Observed);
    }

    #[test]
    fn test_ram_admission_config_garbage_falls_back_to_defaults() {
        use serde_json::json;
        let at = |mode: serde_json::Value, gb: serde_json::Value| json!({"autonomous": {"workFinder": {"ramAdmission": mode, "ramChargeGb": gb}}});
        for (mode, gb) in [
            (json!("realistic"), json!(0)),
            (json!(7), json!(-2)),
            (json!(null), json!("4")),
            (json!(["flat"]), json!(2.5)),
        ] {
            assert_eq!(
                ram_admission_config_from(&at(mode.clone(), gb.clone()), None),
                RamAdmissionConfig::default(),
                "{mode} / {gb}"
            );
        }
    }

    #[test]
    fn test_ram_admission_env_beats_config_and_forces_flat() {
        use serde_json::json;
        let off = json!({"autonomous": {"workFinder": {"ramAdmission": "off", "ramChargeGb": 9}}});
        let c = ram_admission_config_from(&off, Some(4));
        assert_eq!((c.mode, c.charge_gb, c.from_env), (RamMode::Flat, 4, true));
        let (store, roots) = idle_host_with_heavy_history();
        let (_, budget, _) = ram_admission_mode(c, 60, &store, &roots, &[12]);
        assert_eq!(budget.unwrap().charges[0].source, crate::ram_peaks::ChargeSource::Env);
    }

    #[test]
    #[serial]
    fn test_ram_admission_config_host_tier_beats_workspace_tier() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".loom")).unwrap();
        std::fs::create_dir_all(root.join(".loom-local")).unwrap();
        std::fs::write(
            root.join(".loom/config.json"),
            r#"{"autonomous": {"workFinder": {"ramAdmission": "observed", "ramChargeGb": 6}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".loom-local/local.json"),
            r#"{"autonomous": {"workFinder": {"ramAdmission": "flat"}}}"#,
        )
        .unwrap();
        std::env::remove_var(PER_WORKTREE_RAM_GB_ENV);
        let c = ram_admission_config(root);
        assert_eq!((c.mode, c.charge_gb), (RamMode::Flat, 6), "host tier wins, keys merge");
    }

    #[test]
    fn test_ram_admission_mixes_sampled_unsampled_and_default_repos() {
        // heavy: one sweep sampled at 5 GB (reserves 8) and one unsampled
        // (charged 15). small has no history: its two sampled sweeps have no
        // expected peak, so each is charged the 2 GB default, like legacy.
        let mut store = crate::ram_peaks::Store::default();
        store.repos.insert("heavy".into(), vec![13 * GIB]);
        store
            .inflight
            .insert("h1".into(), sweep("heavy", 1, 5 * GIB));
        store.inflight.insert("s1".into(), sweep("small", 2, GIB));
        store.inflight.insert("s2".into(), sweep("small", 3, GIB));
        let (cap, budget) = ram_admission(40, &store, &roots(), &[2, 2], None);
        let b = budget.unwrap();
        assert_eq!(b.remaining_gb, 40 - 8 - 15 - 2 * 2);
        assert_eq!(cap, 4 + 13 / 2, "four running + floor(13 / 2)");
        assert!(!b.fits(0), "heavy: 15 > 13");
        assert!(b.fits(1), "small: 2 <= 13");
    }

    #[test]
    fn test_ram_admission_hands_a_shared_repo_keys_samples_out_once() {
        // Two roots with the same key and one sampled sweep between them: the
        // sample covers one lock, never both.
        let mut store = crate::ram_peaks::Store::default();
        store.repos.insert("heavy".into(), vec![13 * GIB]);
        store
            .inflight
            .insert("h1".into(), sweep("heavy", 1, 13 * GIB));
        let both: Vec<std::path::PathBuf> = vec!["/a/heavy".into(), "/b/heavy".into()];
        let (_, budget) = ram_admission(30, &store, &both, &[1, 1], None);
        assert_eq!(budget.unwrap().remaining_gb, 15, "30 - 0 reserved - 15 unsampled");
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
