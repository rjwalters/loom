//! Demand-weighted role width and Champion-first ceiling reservation for the
//! multi-workspace role loop (issue #9392, Phase 2a of #9391).
//!
//! Phase 1 ([`super::concurrent_dispatch`]) gives every role a static budget
//! against one host ceiling, and nothing prefers PR work: with 28 PRs in
//! `loom:review-requested` and 59 in `loom:pr`, curator, auditor, hermit and
//! guide still competed for the ceiling on equal terms with judge and
//! champion. This module weights admission by how much PR work is waiting.
//!
//! - **Ledger.** [`DemandLedger`] maps `(root, axis)` to the number of open
//!   PR rows last seen in that root's queue for the axis, and when. It is fed
//!   only by listings the role runner already makes — the Phase 1 queue-gate
//!   listing for judge (`loom:review-requested`) and doctor
//!   (`loom:changes-requested`), plus **one** ETag-cached `loom:pr` listing
//!   per *admitted* champion run ([`record_merge_debt`]). No forge query is
//!   added per tick per repo, and reads ([`DemandLedger::host_debt`]) are a
//!   mutex read with no I/O, safe in the synchronous walk. A failed listing
//!   records nothing, so its entry ages out (`staleSecs`) rather than reading
//!   as `0`; an axis with no fresh entry is `None` (**unobserved**).
//! - **Width.** For a PR role with debt axis `d`,
//!   `width = clamp(ceil(debt / perRun), 1, min(max, phase1_budget))`, and an
//!   unobserved axis gives exactly the Phase 1 budget ([`width`]). Judge and
//!   doctor use it as their effective budget; champion's budget is never
//!   lowered (it also promotes `loom:curated` issues), so its width feeds only
//!   the reservation.
//! - **Reservation.** Each PR role `p` wants
//!   `min(width(p), roots_with_debt(p))` slots (0 when unobserved or debt-free);
//!   a role cannot use more, since Phase 1 allows one run per `(repo, role)`.
//!   Admitting role `r` holds back the unfilled wants of every PR role that
//!   outranks it (champion > judge > doctor > every other role), capped at
//!   `ceiling − nonPrFloor`, and the check runs under the same lock as the
//!   insert ([`RoleRunGuard::admit_with_demand`]).
//!
//! **Fairness.** A reservation refusal is host-wide, like a ceiling refusal:
//! the walk stops and keeps the refused root at the front of `pending`, and
//! the round-robin cursor only moves on an admission, so a refused root is the
//! first one decided next time. Champion is never refused by a reservation,
//! judge only by champion's unfilled want, and non-PR roles always keep
//! `nonPrFloor` slots, so no role is shut out and every repo with debt is
//! still reached within `ceil(roots / admitted-per-tick)` ticks.
//!
//! `autonomous.roleRunner.demandWidth.enabled: false` restores Phase 1
//! exactly: no ledger reads, no reservation, no champion listing.

use super::*;
use crate::forge_listing::RestIssue;
use std::sync::atomic::AtomicUsize;

/// `forge_call_stats` caller name for champion's count-only `loom:pr` listing.
pub const DEMAND_CALLER: &str = "role_demand";

/// The PR roles in reservation priority order: champion releases the most
/// finished work for the least cost, so it goes first.
pub const PR_ROLE_PRIORITY: [&str; 3] = ["champion", "judge", "doctor"];

/// One PR queue whose depth steers admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DebtAxis {
    /// `loom:review-requested` — judge's queue.
    Review,
    /// `loom:changes-requested` — doctor's queue.
    Changes,
    /// `loom:pr` — champion's merge queue.
    Merge,
}

impl DebtAxis {
    /// The label whose open PR rows this axis counts.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Review => "loom:review-requested",
            Self::Changes => "loom:changes-requested",
            Self::Merge => "loom:pr",
        }
    }

    /// The axis a queue label feeds, if any.
    #[must_use]
    pub fn for_label(label: &str) -> Option<Self> {
        [Self::Review, Self::Changes, Self::Merge]
            .into_iter()
            .find(|a| a.label() == label)
    }

    /// The axis `role`'s width follows. Judge keys on review requests only:
    /// a `loom:changes-requested` PR is doctor's work until doctor returns it.
    #[must_use]
    pub fn for_role(role: &str) -> Option<Self> {
        match role {
            "judge" => Some(Self::Review),
            "doctor" => Some(Self::Changes),
            "champion" => Some(Self::Merge),
            _ => None,
        }
    }
}

/// `autonomous.roleRunner.demandWidth`, resolved per root and re-read every
/// tick like `roleMaxConcurrent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DemandConfig {
    /// `enabled` — `false` is exactly Phase 1 admission.
    pub enabled: bool,
    /// `perRun` (`k`) — debt items per role run in the width formula.
    pub per_run: usize,
    /// `max` — upper clamp on PR-role width (still capped by the budget).
    pub max: usize,
    /// `reserve` — Champion-first ceiling reservation on/off.
    pub reserve: bool,
    /// `nonPrFloor` — ceiling slots the reservation always leaves free.
    pub non_pr_floor: usize,
    /// `staleSecs` — ledger entries older than this are unobserved.
    pub stale_secs: u64,
}

impl Default for DemandConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            per_run: 3,
            max: 4,
            reserve: true,
            non_pr_floor: 1,
            stale_secs: 1800,
        }
    }
}

impl DemandConfig {
    /// `staleSecs` as a duration.
    #[must_use]
    pub fn stale(&self) -> Duration {
        Duration::from_secs(self.stale_secs)
    }
}

/// Parse `demandWidth` out of an `autonomous.roleRunner` block. Each key
/// falls back to its default on its own: a zero, negative or non-integer
/// number (or a non-bool flag) drops only that key — the
/// `parse_role_max_concurrent` discipline. No env tier.
#[must_use]
pub fn parse_demand_config(role_runner_block: &serde_json::Value) -> DemandConfig {
    let d = DemandConfig::default();
    let Some(obj) = role_runner_block
        .get("demandWidth")
        .and_then(serde_json::Value::as_object)
    else {
        return d;
    };
    let positive = |key: &str| {
        obj.get(key)
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0)
    };
    let count = |key: &str, default: usize| {
        positive(key)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(default)
    };
    let flag = |key: &str, default: bool| {
        obj.get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(default)
    };
    DemandConfig {
        enabled: flag("enabled", d.enabled),
        per_run: count("perRun", d.per_run),
        max: count("max", d.max),
        reserve: flag("reserve", d.reserve),
        non_pr_floor: count("nonPrFloor", d.non_pr_floor),
        stale_secs: positive("staleSecs").unwrap_or(d.stale_secs),
    }
}

/// Read `root`'s own `demandWidth` config.
#[must_use]
pub fn read_demand_config(root: &Path) -> DemandConfig {
    parse_demand_config(&concurrent_dispatch::role_runner_block(root))
}

/// One axis summed across every root with a fresh ledger entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AxisDebt {
    /// Open PR rows across the fresh entries.
    pub total: usize,
    /// Fresh entries with a nonzero count.
    pub roots_with_debt: usize,
}

/// The host-wide debt the ledger saw; `None` is an unobserved axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostDebt {
    /// `loom:review-requested`.
    pub review: Option<AxisDebt>,
    /// `loom:changes-requested`.
    pub changes: Option<AxisDebt>,
    /// `loom:pr`.
    pub merge: Option<AxisDebt>,
}

impl HostDebt {
    /// The debt on `axis`.
    #[must_use]
    pub fn axis(&self, axis: DebtAxis) -> Option<AxisDebt> {
        match axis {
            DebtAxis::Review => self.review,
            DebtAxis::Changes => self.changes,
            DebtAxis::Merge => self.merge,
        }
    }

    fn axis_mut(&mut self, axis: DebtAxis) -> &mut Option<AxisDebt> {
        match axis {
            DebtAxis::Review => &mut self.review,
            DebtAxis::Changes => &mut self.changes,
            DebtAxis::Merge => &mut self.merge,
        }
    }
}

type LedgerEntries = HashMap<(PathBuf, DebtAxis), (usize, Instant)>;

/// The process-wide demand ledger (see the module doc). Tests build their own
/// with [`DemandLedger::default`] so they never share state.
#[derive(Debug, Default)]
pub struct DemandLedger {
    entries: Mutex<LedgerEntries>,
    /// The last `(budget, width, planned reservation)` logged per role, so the
    /// width line is edge-triggered.
    logged: Mutex<HashMap<&'static str, (usize, usize, usize)>>,
    /// Host-debt reads, so a test can assert `enabled: false` never reads.
    reads: AtomicUsize,
}

impl DemandLedger {
    /// Record `count` open PR rows on `axis` for `root`, observed now.
    pub fn record(&self, root: &Path, axis: DebtAxis, count: usize) {
        self.record_at(root, axis, count, Instant::now());
    }

    /// [`Self::record`] with an explicit observation time.
    pub fn record_at(&self, root: &Path, axis: DebtAxis, count: usize, at: Instant) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.insert((root.to_path_buf(), axis), (count, at));
    }

    /// The host aggregate over entries no older than `stale`.
    #[must_use]
    pub fn host_debt(&self, stale: Duration) -> HostDebt {
        self.host_debt_at(Instant::now(), stale)
    }

    /// [`Self::host_debt`] as of `now`.
    #[must_use]
    pub fn host_debt_at(&self, now: Instant, stale: Duration) -> HostDebt {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut debt = HostDebt::default();
        for ((_, axis), (count, at)) in entries.iter() {
            if now.saturating_duration_since(*at) > stale {
                continue;
            }
            let slot = debt.axis_mut(*axis).get_or_insert_with(AxisDebt::default);
            slot.total += count;
            slot.roots_with_debt += usize::from(*count > 0);
        }
        debt
    }

    /// Drop the entries of roots no longer in the registry, so a departed
    /// root's debt stops holding slots before it would go stale.
    pub fn retain_roots(&self, roots: &[PathBuf]) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|(root, _), _| roots.contains(root));
    }

    /// How many times the host debt has been read.
    #[must_use]
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    /// Whether `decision` differs from the last one logged for its role (and
    /// remember it). Keeps the width line to one per change, not per tick.
    fn changed(&self, decision: &DemandDecision) -> bool {
        let key = (decision.budget, decision.width.unwrap_or(0), decision.plan.planned());
        let mut logged = self.logged.lock().unwrap_or_else(PoisonError::into_inner);
        logged.insert(decision.role, key) != Some(key)
    }
}

/// The ledger production admission reads and the forge listings feed.
#[must_use]
pub fn global() -> &'static DemandLedger {
    static LEDGER: OnceLock<DemandLedger> = OnceLock::new();
    LEDGER.get_or_init(DemandLedger::default)
}

/// Open pull requests in a REST issue listing (which also returns issues).
#[must_use]
pub fn count_pr_rows(rows: &[RestIssue]) -> usize {
    rows.iter()
        .filter(|r| r.is_pull_request && r.state.eq_ignore_ascii_case("open"))
        .count()
}

/// Record a queue listing the role runner already made for `root`: a no-op
/// for a label that feeds no axis, or when `root` has demand width disabled.
pub fn record_listing(ledger: &DemandLedger, root: &Path, label: &str, rows: &[RestIssue]) {
    if let Some(axis) = DebtAxis::for_label(label) {
        if read_demand_config(root).enabled {
            ledger.record(root, axis, count_pr_rows(rows));
        }
    }
}

/// Counts `root`'s open `loom:pr` rows. `Err` records nothing.
pub type DemandProbe = Arc<dyn Fn(&Path) -> Result<usize, String> + Send + Sync>;

/// The production merge-debt probe: one ETag-cached `loom:pr` listing (a free
/// `304` when nothing changed), under caller [`DEMAND_CALLER`].
#[must_use]
pub fn forge_merge_probe() -> DemandProbe {
    Arc::new(|root| {
        let gh_bin = std::env::var("LOOM_GH_BIN")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map_or_else(|| PathBuf::from("gh"), PathBuf::from);
        crate::forge_listing::list_issues_cached_as(
            DEMAND_CALLER,
            &gh_bin,
            Some(root),
            None,
            DebtAxis::Merge.label(),
            "open",
        )
        .map(|rows| count_pr_rows(&rows))
        .map_err(|e| e.to_string())
    })
}

/// A probe that never lists (the test-dispatcher default).
#[must_use]
pub fn no_merge_probe() -> DemandProbe {
    Arc::new(|_| Err("no demand probe configured".to_string()))
}

/// After an admitted champion run: count `root`'s `loom:pr` queue once and
/// record it. Never gates champion — the run has already happened.
pub fn record_merge_debt(probe: &DemandProbe, ledger: &DemandLedger, root: &Path) {
    if !read_demand_config(root).enabled {
        return;
    }
    match probe(root) {
        Ok(count) => ledger.record(root, DebtAxis::Merge, count),
        Err(e) => log::debug!(
            "role_runner: champion merge-debt listing for {} failed ({e}) — nothing recorded; \
             the axis ages out (#9392)",
            root.display()
        ),
    }
}

/// `clamp(ceil(debt / perRun), 1, min(max, phase1_budget))`; an unobserved
/// axis is exactly the Phase 1 budget.
#[must_use]
pub fn width(debt: Option<usize>, cfg: &DemandConfig, phase1_budget: usize) -> usize {
    let upper = cfg.max.min(phase1_budget).max(1);
    debt.map_or(phase1_budget, |d| d.div_ceil(cfg.per_run.max(1)).clamp(1, upper))
}

/// The slots a PR role wants held: `min(width, roots_with_debt)`, or 0 when
/// its axis is unobserved or debt-free.
#[must_use]
pub fn want(debt: Option<AxisDebt>, cfg: &DemandConfig, phase1_budget: usize) -> usize {
    match debt {
        Some(d) if d.total > 0 => width(Some(d.total), cfg, phase1_budget).min(d.roots_with_debt),
        _ => 0,
    }
}

/// Which PR roles a reservation is held for, in [`PR_ROLE_PRIORITY`] order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeldFor(pub [bool; 3]);

impl std::fmt::Display for HeldFor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = PR_ROLE_PRIORITY
            .iter()
            .zip(self.0)
            .filter_map(|(name, held)| held.then_some(*name))
            .collect();
        f.write_str(&names.join("+"))
    }
}

/// The reservation one admission is checked against: the wants of the PR
/// roles that outrank the role being admitted, and the `ceiling − nonPrFloor`
/// cap. The unfilled part is computed under the admission lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReservationPlan {
    /// `want(p)` per [`PR_ROLE_PRIORITY`] entry; 0 for roles that do not
    /// outrank the admitted one.
    pub wants: [usize; 3],
    /// `ceiling − nonPrFloor`.
    pub cap: usize,
}

impl ReservationPlan {
    /// No reservation (champion, `reserve: false`, or demand disabled).
    pub const NONE: Self = Self {
        wants: [0; 3],
        cap: 0,
    };

    /// The plan for admitting `role`: every PR role ranked above it.
    #[must_use]
    pub fn for_role(
        role: &str,
        host: &HostDebt,
        cfg: &DemandConfig,
        budget_of: &dyn Fn(&str) -> usize,
        ceiling: usize,
    ) -> Self {
        let rank = PR_ROLE_PRIORITY
            .iter()
            .position(|p| *p == role)
            .unwrap_or(PR_ROLE_PRIORITY.len());
        let mut wants = [0; 3];
        for (i, p) in PR_ROLE_PRIORITY.iter().enumerate().take(rank) {
            let axis = DebtAxis::for_role(p)
                .map(|a| host.axis(a))
                .unwrap_or_default();
            wants[i] = want(axis, cfg, budget_of(p));
        }
        Self {
            wants,
            cap: ceiling.saturating_sub(cfg.non_pr_floor),
        }
    }

    /// The reservation if no PR role were running: `min(Σ want, cap)`.
    #[must_use]
    pub fn planned(&self) -> usize {
        self.wants.iter().sum::<usize>().min(self.cap)
    }

    /// `min(Σ max(0, want(p) − active(p)), cap)` and the roles with an unfilled
    /// want, given each PR role's in-flight count.
    #[must_use]
    pub fn reserved(&self, active_of: impl Fn(&str) -> usize) -> (usize, HeldFor) {
        let mut held = [false; 3];
        let mut sum = 0;
        for (i, p) in PR_ROLE_PRIORITY.iter().enumerate() {
            let deficit = self.wants[i].saturating_sub(active_of(p));
            held[i] = deficit > 0;
            sum += deficit;
        }
        (sum.min(self.cap), HeldFor(held))
    }
}

/// What demand makes of one `(role, root)` admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DemandDecision {
    /// The role being admitted.
    pub role: &'static str,
    /// Its Phase 1 budget (`roleMaxConcurrent` or the default).
    pub phase1_budget: usize,
    /// The debt on its own axis (`None`: unobserved, or not a PR role).
    pub debt: Option<usize>,
    /// Its width, for a PR role.
    pub width: Option<usize>,
    /// The effective budget: width for judge/doctor, else the Phase 1 budget.
    pub budget: usize,
    /// The ceiling reservation it must leave free.
    pub plan: ReservationPlan,
}

/// Decide `role`'s effective budget and reservation from the host debt.
#[must_use]
pub fn decide(
    role: &'static str,
    budgets: &BTreeMap<String, usize>,
    ceiling: usize,
    host: &HostDebt,
    cfg: &DemandConfig,
) -> DemandDecision {
    let budget_of = |r: &str| concurrent_dispatch::resolve_role_max_concurrent(budgets, r, ceiling);
    let phase1_budget = budget_of(role);
    let axis = DebtAxis::for_role(role);
    let debt = axis.and_then(|a| host.axis(a)).map(|d| d.total);
    let width = axis.map(|_| width(debt, cfg, phase1_budget));
    let budget = match role {
        // Queue-gated, so width only limits how many non-empty repos run.
        "judge" | "doctor" => width.unwrap_or(phase1_budget),
        // Champion is ungated and also promotes issues: never throttled.
        _ => phase1_budget,
    };
    let plan = if cfg.reserve {
        ReservationPlan::for_role(role, host, cfg, &budget_of, ceiling)
    } else {
        ReservationPlan::NONE
    };
    DemandDecision {
        role,
        phase1_budget,
        debt,
        width,
        budget,
        plan,
    }
}

/// Log `decision` at INFO when its `(budget, width, planned reservation)`
/// differs from the last one logged for the role.
pub fn log_if_changed(ledger: &DemandLedger, decision: &DemandDecision, cfg: &DemandConfig) {
    if !ledger.changed(decision) {
        return;
    }
    let [champion, judge, doctor] = decision.plan.wants;
    log::info!(
        "role_runner: {} demand admission — debt {} → width {}, effective budget {} (perRun \
         {}, max {}, phase-1 budget {}); ceiling reservation {} for higher-priority PR roles \
         (wants champion={champion} judge={judge} doctor={doctor}, cap {} = ceiling − \
         nonPrFloor {}) (#9392)",
        decision.role,
        decision
            .debt
            .map_or_else(|| "unobserved".to_string(), |d| d.to_string()),
        decision
            .width
            .map_or_else(|| "n/a".to_string(), |w| w.to_string()),
        decision.budget,
        cfg.per_run,
        cfg.max,
        decision.phase1_budget,
        decision.plan.planned(),
        decision.plan.cap,
        cfg.non_pr_floor,
    );
}

impl RoleRunGuard {
    /// [`Self::admit_with_role_budget`] plus the Champion-first reservation
    /// (#9392), all under **one** lock: `InProgress`, `CeilingReached`,
    /// `RoleBudgetReached` against `budget`, then `ReservationHeld` when
    /// `active + reserved ≥ ceiling`. The PR roles' in-flight counts are read
    /// from the same guard set the insert lands in (the #6102 race argument).
    #[must_use]
    pub fn admit_with_demand(
        set: InProgressGuard,
        root: PathBuf,
        role: &'static str,
        ceiling: usize,
        budget: usize,
        plan: &ReservationPlan,
    ) -> RoleAdmission {
        let key = (root, role);
        {
            let mut guard = set.lock().unwrap_or_else(PoisonError::into_inner);
            if guard.contains(&key) {
                return RoleAdmission::InProgress;
            }
            let active = guard.len();
            if active >= ceiling {
                return RoleAdmission::CeilingReached { active, ceiling };
            }
            let active_of = |r: &str| guard.iter().filter(|(_, x)| *x == r).count();
            let role_active = active_of(role);
            if role_active >= budget {
                return RoleAdmission::RoleBudgetReached {
                    role,
                    active: role_active,
                    budget,
                };
            }
            let (reserved, held_for) = plan.reserved(active_of);
            if reserved > 0 && active + reserved >= ceiling {
                return RoleAdmission::ReservationHeld {
                    active,
                    ceiling,
                    reserved,
                    held_for,
                };
            }
            guard.insert(key.clone());
        }
        ROLE_RUN_START_GENERATION.fetch_add(1, Ordering::Relaxed);
        RoleAdmission::Admitted(Self { set, key })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tests/demand.rs"]
mod tests;
