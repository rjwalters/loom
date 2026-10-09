//! Disk admission with a measured per-repo charge and a reservation for
//! in-flight growth (#11191, part of epic #11189).
//!
//! # The rule
//!
//! A sweep of repo R is admitted only when
//!
//! ```text
//! free - floor - reserved >= charge(R)
//! reserved = sum over in-flight units u of max(0, charge(u.repo) - written(u))
//! ```
//!
//! - `free` is the worktree-root volume's free space ([`worktree_root_free_gb`]).
//! - `floor` is the disk-full halt floor ([`crate::disk_full_halt`], 3 GB by
//!   default), so admission stops before the terminal halt would.
//! - `charge(R)` resolves, first match wins: R's observed high-water mark plus
//!   [`MARGIN_PCT`] ([`crate::disk_footprint`]), else R's configured
//!   [`REPO_CHARGE_KEY`], else the global `LOOM_PER_WORKTREE_GB` (8 GB).
//!   Rounded up to whole GB, minimum 1, so a repo that builds nothing stays
//!   cheap.
//! - `reserved` charges each running sweep (and role run) for what it is still
//!   expected to write, not for what it already wrote: that part is already
//!   gone from `free`.
//!
//! # Where it is enforced
//!
//! - **The work finder** ([`tick`]): a [`DiskBudget`] per tick. A repo whose
//!   charge does not fit at the top of the tick is held with
//!   [`HaltCause::DiskReservation`](crate::work_finder::halt_cause::HaltCause);
//!   pass 2 debits each admission and defers whatever stops fitting. The disk
//!   term of the cap becomes `sweeps in flight + floor(remaining / smallest
//!   charge)`.
//! - **The dispatch seam** ([`admit_dispatch`]): `begin_prepared_issue_dispatch`,
//!   the one function every dispatch route goes through (the work finder, the
//!   three watchdogs including the review-stall re-dispatch #3910 and its
//!   PR-set conversion #7649, the reaper's resume #4256, IPC/CLI, the epic
//!   supervisor), runs the same check before any claim, label or spawn, and
//!   records the admitted sweep as a pending unit so the next admission
//!   reserves for it.
//!
//! `LOOM_DISK_ADMISSION=0` turns both off (the legacy flat disk term). An
//! unmeasurable free-space probe fails open, as everywhere else (#4164).

use std::path::{Path, PathBuf};

use crate::disk_footprint::{self, Store, GIB};
use crate::disk_headroom::{per_worktree_gb, worktree_root_free_gb};
use crate::types::SweepKind;

/// Env switch: `0`/`false`/`off`/`no` disables disk admission (legacy term).
pub const ENABLE_ENV: &str = "LOOM_DISK_ADMISSION";

/// Per-repo `.loom/config.json` key: this repo's charge (GB) while it has no
/// observed history.
pub const REPO_CHARGE_KEY: &str = "autonomous.workFinder.diskChargeGb";

/// Safety margin (percent) added on top of an observed high-water mark.
pub const MARGIN_PCT: u64 = 10;

/// Where a repo's charge came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeSource {
    /// The repo's observed high-water mark.
    Observed,
    /// The repo's configured [`REPO_CHARGE_KEY`].
    RepoConfig,
    /// The global `LOOM_PER_WORKTREE_GB`.
    Default,
}

impl ChargeSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::RepoConfig => "config",
            Self::Default => "default",
        }
    }
}

/// One repo's per-sweep disk charge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoCharge {
    pub repo: String,
    pub gb: u64,
    pub source: ChargeSource,
}

/// Resolve a charge: observed mark plus margin (whole GB, rounded up) >
/// per-repo config > global default. Each is floored at 1 GB.
#[must_use]
pub fn charge_gb(
    observed_bytes: Option<u64>,
    repo_config_gb: Option<u64>,
    global_gb: u64,
) -> (u64, ChargeSource) {
    if let Some(bytes) = observed_bytes {
        let padded = bytes.saturating_add(bytes / 100 * MARGIN_PCT);
        return (padded.div_ceil(GIB).max(1), ChargeSource::Observed);
    }
    if let Some(gb) = repo_config_gb {
        return (gb.max(1), ChargeSource::RepoConfig);
    }
    (global_gb.max(1), ChargeSource::Default)
}

/// `root`'s configured [`REPO_CHARGE_KEY`], if set to a positive integer.
#[must_use]
pub fn repo_config_gb(root: &Path) -> Option<u64> {
    let config = crate::config_resolver::resolve_effective_config(root);
    crate::config_resolver::get_path(&config, REPO_CHARGE_KEY)
        .and_then(serde_json::Value::as_u64)
        .filter(|&n| n >= 1)
}

/// The charge for workspace `root`, given the footprint `store`.
#[must_use]
pub fn charge_for_root(root: &Path, store: &Store, global_gb: u64) -> RepoCharge {
    let repo = crate::ram_peaks::repo_key(root);
    let (gb, source) = charge_gb(store.high_water_bytes(&repo), repo_config_gb(root), global_gb);
    RepoCharge { repo, gb, source }
}

/// Whether disk admission is on ([`ENABLE_ENV`], default on).
#[must_use]
pub fn enabled() -> bool {
    std::env::var(ENABLE_ENV).map_or(true, |v| {
        !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
    })
}

/// The free-space floor admission keeps clear: the disk-full halt floor.
#[must_use]
pub fn floor_gb() -> u64 {
    crate::disk_full_halt::DiskHaltConfig::from_env().floor_gb
}

/// One tick's disk budget: the inputs, what is left after the reservation,
/// and each workspace's own charge (parallel to the tick's roots).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskBudget {
    pub free_gb: u64,
    pub floor_gb: u64,
    /// Expected growth still to come from the in-flight units, whole GB.
    pub reserved_gb: u64,
    /// `free - floor - reserved`, then debited per admission this tick.
    pub remaining_gb: u64,
    /// Sweeps (not role runs) counted in flight.
    pub sweeps_in_flight: usize,
    pub charges: Vec<RepoCharge>,
}

/// Bytes the in-flight units of `store` are still expected to write: for
/// each counted unit, its repo's charge (from `charges` when the repo is one
/// of them, else its own history or `global_gb`) minus what it has written.
/// `exclude` skips one unit key (the dispatch being decided: a re-dispatch
/// replaces its predecessor rather than adding to it).
#[must_use]
pub fn reserved_bytes(
    store: &Store,
    charges: &[RepoCharge],
    global_gb: u64,
    now: i64,
    exclude: Option<&str>,
) -> (u64, usize) {
    let fresh = store.fresh(now);
    let mut sweeps = 0;
    let mut total = 0u64;
    for (key, unit) in &store.inflight {
        if Some(key.as_str()) == exclude || !unit.counts(fresh, now) {
            continue;
        }
        if unit.is_sweep(key) {
            sweeps += 1;
        }
        let gb = charges.iter().find(|c| c.repo == unit.repo).map_or_else(
            || charge_gb(store.high_water_bytes(&unit.repo), None, global_gb).0,
            |c| c.gb,
        );
        total = total.saturating_add(gb.saturating_mul(GIB).saturating_sub(unit.current_bytes));
    }
    (total, sweeps)
}

/// Build a budget (pure).
#[must_use]
pub fn assess(
    free_gb: u64,
    floor_gb: u64,
    store: &Store,
    charges: Vec<RepoCharge>,
    global_gb: u64,
    now: i64,
    exclude: Option<&str>,
) -> DiskBudget {
    let (reserved, sweeps_in_flight) = reserved_bytes(store, &charges, global_gb, now, exclude);
    let reserved_gb = reserved.div_ceil(GIB);
    DiskBudget {
        free_gb,
        floor_gb,
        reserved_gb,
        remaining_gb: free_gb.saturating_sub(floor_gb).saturating_sub(reserved_gb),
        sweeps_in_flight,
        charges,
    }
}

impl DiskBudget {
    /// Workspace `idx`'s charge.
    #[must_use]
    pub fn charge(&self, idx: usize) -> Option<&RepoCharge> {
        self.charges.get(idx)
    }

    /// Whether workspace `idx`'s charge still fits. A missing entry fails open.
    #[must_use]
    pub fn fits(&self, idx: usize) -> bool {
        self.charge(idx).is_none_or(|c| c.gb <= self.remaining_gb)
    }

    /// Debit one admission of workspace `idx`.
    pub fn debit(&mut self, idx: usize) {
        if let Some(c) = self.charges.get(idx) {
            self.remaining_gb = self.remaining_gb.saturating_sub(c.gb);
        }
    }

    /// The disk term of the total-concurrency cap: the sweeps already running
    /// plus how many more fit at the smallest charge, so a heavy repo's charge
    /// never caps a light sibling. Saturating.
    #[must_use]
    pub fn cap_term(&self) -> usize {
        let min = self.charges.iter().map(|c| c.gb).min().unwrap_or(1).max(1);
        let more = usize::try_from(self.remaining_gb / min).unwrap_or(usize::MAX);
        self.sweeps_in_flight.saturating_add(more)
    }

    /// The per-repo charges, `repo:NGB(source)`, comma-separated.
    #[must_use]
    pub fn charges_summary(&self) -> String {
        self.charges
            .iter()
            .map(|c| format!("{}:{}GB({})", c.repo, c.gb, c.source.as_str()))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The figures for the `work_finder: tick` line.
    #[must_use]
    pub fn note(&self) -> String {
        format!(
            "free {}GB - floor {}GB - reserved {}GB = {}GB; charges [{}]",
            self.free_gb,
            self.floor_gb,
            self.reserved_gb,
            self.remaining_gb,
            self.charges_summary()
        )
    }

    /// Why workspace `idx` was deferred, for its ready-queue row.
    #[must_use]
    pub fn deferral_detail(&self, idx: usize) -> String {
        self.charge(idx).map_or_else(String::new, |c| {
            format!(
                "disk: {} charge {}GB ({}) exceeds remaining {}GB (free {}GB - floor {}GB - \
                 reserved {}GB, #11191)",
                c.repo,
                c.gb,
                c.source.as_str(),
                self.remaining_gb,
                self.free_gb,
                self.floor_gb,
                self.reserved_gb
            )
        })
    }

    /// Which workspaces cannot admit even one sweep right now.
    #[must_use]
    pub fn held(&self) -> Vec<bool> {
        (0..self.charges.len()).map(|i| !self.fits(i)).collect()
    }
}

/// The `work_finder: tick` line's disk note: the budget figures, or `flat`
/// when disk admission is off or unmeasured.
#[must_use]
pub fn note(budget: Option<&DiskBudget>) -> String {
    budget.map_or_else(|| "flat".to_string(), DiskBudget::note)
}

/// Log the budget: INFO when it changes, WARN on the edge into refusing some
/// repo, DEBUG otherwise. Each line names every figure the decision used.
fn log_budget(b: &DiskBudget) {
    let held: Vec<&str> = b
        .charges
        .iter()
        .enumerate()
        .filter(|(i, _)| !b.fits(*i))
        .map(|(_, c)| c.repo.as_str())
        .collect();
    let line = format!(
        "disk_admission: {}; sweeps_in_flight={} cap_term={} refusing=[{}] (#11191)",
        b.note(),
        b.sweeps_in_flight,
        b.cap_term(),
        held.join(",")
    );
    static LAST: std::sync::Mutex<Option<(String, bool)>> = std::sync::Mutex::new(None);
    let key = (
        format!("{}/{}/{}", b.charges_summary(), b.reserved_gb, b.cap_term()),
        !held.is_empty(),
    );
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let was_refusing = last.as_ref().is_some_and(|(_, r)| *r);
    if last.as_ref() == Some(&key) {
        log::debug!("{line}");
    } else if key.1 && !was_refusing {
        log::warn!("{line}");
    } else {
        log::info!("{line}");
    }
    *last = Some(key);
}

/// The work finder's disk term and per-repo budget for `roots`, measured on
/// `fallback_root`'s worktree-root volume. Starts (or re-points) the
/// footprint sampler. Disabled or unmeasurable: the legacy flat term and no
/// budget.
#[must_use]
pub fn tick(roots: &[PathBuf], fallback_root: &Path) -> (usize, Option<DiskBudget>) {
    if !enabled() {
        return (crate::disk_headroom::disk_headroom_limit(fallback_root), None);
    }
    disk_footprint::publish_roots(roots);
    let Some(free) = worktree_root_free_gb(fallback_root) else {
        return (crate::disk_headroom::disk_headroom_limit(fallback_root), None);
    };
    let store = disk_footprint::load_current();
    let global = per_worktree_gb();
    let charges = roots
        .iter()
        .map(|r| charge_for_root(r, &store, global))
        .collect();
    let budget =
        assess(free, floor_gb(), &store, charges, global, disk_footprint::now_secs(), None);
    log_budget(&budget);
    (budget.cap_term(), Some(budget))
}

// ---------------------------------------------------------------------------
// The dispatch seam
// ---------------------------------------------------------------------------

/// Typed refusal from [`admit_dispatch`]: the figures that refused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskAdmissionRefused {
    pub what: String,
    pub repo: String,
    pub free_gb: u64,
    pub floor_gb: u64,
    pub reserved_gb: u64,
    pub charge_gb: u64,
    pub source: ChargeSource,
}

impl std::fmt::Display for DiskAdmissionRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "disk admission refused {} in {}: free {}GB - floor {}GB - reserved {}GB for \
             in-flight growth = {}GB, below this repo's {}GB charge ({}) (#11191)",
            self.what,
            self.repo,
            self.free_gb,
            self.floor_gb,
            self.reserved_gb,
            self.free_gb
                .saturating_sub(self.floor_gb)
                .saturating_sub(self.reserved_gb),
            self.charge_gb,
            self.source.as_str()
        )
    }
}

impl std::error::Error for DiskAdmissionRefused {}

/// The unit key a dispatch of `kind` in `repo` is recorded under.
#[must_use]
pub fn kind_key(repo: &str, kind: &SweepKind) -> (String, Option<u32>, String) {
    match kind {
        SweepKind::Issue(n) => {
            (disk_footprint::issue_key(repo, *n), Some(*n), format!("issue #{n}"))
        }
        SweepKind::PrSet(prs) => {
            let first = prs.iter().min().copied().unwrap_or(0);
            (format!("{repo}#prs-{first}"), None, format!("PR set {prs:?}"))
        }
    }
}

/// Decide one dispatch against `store` (pure apart from `store`): refuse it,
/// or record it as a pending unit and admit it.
///
/// # Errors
/// [`DiskAdmissionRefused`] when the repo's charge does not fit.
pub fn decide(
    root: &Path,
    kind: &SweepKind,
    free_gb: u64,
    floor_gb: u64,
    store: &mut Store,
    global_gb: u64,
    now: i64,
) -> Result<(), DiskAdmissionRefused> {
    let charge = charge_for_root(root, store, global_gb);
    let (key, issue, what) = kind_key(&charge.repo, kind);
    let b = assess(free_gb, floor_gb, store, vec![charge.clone()], global_gb, now, Some(&key));
    if !b.fits(0) {
        return Err(DiskAdmissionRefused {
            what,
            repo: charge.repo,
            free_gb,
            floor_gb,
            reserved_gb: b.reserved_gb,
            charge_gb: charge.gb,
            source: charge.source,
        });
    }
    disk_footprint::record_pending(store, &key, &charge.repo, issue, now);
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test hook: `(free_gb, floor_gb, store)` the seam decides against on
    /// this thread. Unset, the seam admits everything, so registry tests never
    /// read the host's disk or the operator's store.
    pub static TEST_SEAM: std::cell::RefCell<Option<(u64, u64, Store)>> =
        const { std::cell::RefCell::new(None) };
}

/// The dispatch-seam check (#11191): run by `begin_prepared_issue_dispatch`
/// before any claim, label or spawn, so every dispatch route is gated by the
/// same rule as the work finder. Logs a refusal at WARN.
///
/// # Errors
/// [`DiskAdmissionRefused`] (downcastable) when the repo's charge does not
/// fit the free space left after the floor and the in-flight reservation.
pub fn admit_dispatch(root: &Path, kind: &SweepKind) -> anyhow::Result<()> {
    let result = seam_decide(root, kind);
    if let Err(refused) = &result {
        log::warn!("disk_admission: {refused}");
    }
    result.map_err(anyhow::Error::new)
}

#[cfg(not(test))]
fn seam_decide(root: &Path, kind: &SweepKind) -> Result<(), DiskAdmissionRefused> {
    if !enabled() {
        return Ok(());
    }
    let Some(free) = worktree_root_free_gb(root) else {
        return Ok(());
    };
    let (floor, global, now) = (floor_gb(), per_worktree_gb(), disk_footprint::now_secs());
    disk_footprint::with_store(|s| decide(root, kind, free, floor, s, global, now))
        .unwrap_or(Ok(()))
}

#[cfg(test)]
fn seam_decide(root: &Path, kind: &SweepKind) -> Result<(), DiskAdmissionRefused> {
    TEST_SEAM.with(|cell| match cell.borrow_mut().as_mut() {
        Some((free, floor, store)) => {
            decide(root, kind, *free, *floor, store, per_worktree_gb(), disk_footprint::now_secs())
        }
        None => Ok(()),
    })
}

#[cfg(test)]
#[path = "disk_admission_tests.rs"]
mod tests;
