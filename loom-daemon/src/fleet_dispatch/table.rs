//! The captain's assignment table and its Phase 1 policy (#12449).
//!
//! Pure: no I/O, no clock reads, no randomness. Every call takes `now` from
//! the caller's **monotonic** clock ([`Mono`]), so expiry never depends on a
//! worker timestamp and tests drive time by hand.
//!
//! # Rows
//!
//! One row per `(repo, item)`, in one of three states:
//!
//! - [`RowState::Granted`]: the captain granted it; the worker has not yet
//!   listed it in a heartbeat.
//! - [`RowState::Live`]: a heartbeat lists it. A row is created `Live` for a
//!   step the captain never granted (a fallback or attended step), which is
//!   how the table is rebuilt after a restart.
//! - [`RowState::Held`]: a granted item lost the forge order (`refuse` with
//!   `claim_lost`). Someone outside fleet mode owns it; nobody is assigned it
//!   until the forge lease could have lapsed.
//!
//! Granted and live rows live [`Policy::assignment_ttl`] (forge lease TTL
//! plus a margin) from the grant or the last heartbeat that listed them; held
//! rows live [`Policy::hold_ttl`] (the forge lease TTL). A row a host stops
//! listing without a `release` or `refuse` is **not** dropped early: it ends
//! with its TTL, so the captain never re-assigns an item before a forge lease
//! posted for it could have lapsed.
//!
//! # Policy
//!
//! A claim is refused, in this order:
//!
//! 1. `grace` during the start-up window: [`Policy::grace`] after start, or
//!    until every expected host (the roster hosts whose fleet config turns
//!    fleet mode on, [`Table::set_expected`]) has heartbeated, whichever is
//!    first;
//! 2. `held` when another host's row (or a held row) holds the
//!    `(repo, item)`, or when any other row holds the repo's
//!    `champion_merge` step;
//! 3. `not_eligible` when the step's role is not in the host's last reported
//!    `roles_ok` (Phase 1 decides eligibility by `roles_ok` only; a step name
//!    this build does not know has no role, so it is `not_eligible` too);
//! 4. `cap` when the host's granted and live rows reach
//!    `min(free_slots + live, maxConcurrent)`: the last heartbeat's counts,
//!    clamped by the fleet config, so a wrong report cannot hoard. A host
//!    that has not heartbeated this epoch is bounded by `maxConcurrent`
//!    alone, and refused when that is unknown too.
//!
//! # A claim on an item the host already holds
//!
//! Only a duplicate claim for the **same** `(item, step)` (spec §5.3) is an
//! idempotent re-grant: the same assignment id, the row renewed, no policy
//! check (the row already passed it). A claim for a **different** step on an
//! item the host holds is a new assignment: it runs the whole policy path
//! above (the one-`champion_merge`-per-repo lock, `roles_ok` and the cap,
//! counting the replaced row out of the cap), and only when granted replaces
//! the old row under a fresh assignment id. A refusal leaves the old row as
//! it was. The step on a row is never rewritten by a claim, so a host whose
//! `release` was lost cannot turn a stale `judge` row into a `champion_merge`
//! grant that skipped both the merge lock and its own `roles_ok`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::proto::{HeartbeatRequest, LiveStep, RefusalReason, REFUSE_CLAIM_LOST};
use crate::step_role::StepRole;

/// A reading of the server's monotonic clock: time since an arbitrary fixed
/// origin (the server's start).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Mono(pub Duration);

impl Mono {
    /// `self + d`.
    #[must_use]
    pub fn plus(self, d: Duration) -> Mono {
        Mono(self.0.saturating_add(d))
    }

    /// Whole seconds from `self` until `later`, at least `min`.
    #[must_use]
    pub fn secs_until(self, later: Mono, min: u64) -> u64 {
        later.0.saturating_sub(self.0).as_secs().max(min)
    }
}

/// The forge lease TTL's default (`LOOM_LEASE_TTL_MINUTES`, 15 min); the
/// server resolves the live value with
/// [`crate::claim_reconciliation::resolve_lease_ttl_minutes`].
pub const FORGE_LEASE_TTL: Duration = Duration::from_secs(15 * 60);
/// The margin a captain row outlives the forge lease by.
pub const ASSIGNMENT_TTL_MARGIN: Duration = Duration::from_secs(5 * 60);
/// The default start-up grace window.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(90);

/// `retry_after_s` of a `held` refusal, at most.
const HELD_RETRY_MAX_SECS: u64 = 60;
/// `retry_after_s` of a `cap` refusal: about one heartbeat.
const CAP_RETRY_SECS: u64 = 30;
/// `retry_after_s` of a `not_eligible` refusal.
const NOT_ELIGIBLE_RETRY_SECS: u64 = 60;

/// The table's timing knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Lifetime of a granted or live row without renewal.
    pub assignment_ttl: Duration,
    /// Lifetime of a held (`claim_lost`) row.
    pub hold_ttl: Duration,
    /// The start-up grace window's upper bound.
    pub grace: Duration,
}

impl Policy {
    /// The policy for a forge lease TTL of `lease_ttl_minutes`: rows live the
    /// lease TTL plus [`ASSIGNMENT_TTL_MARGIN`], holds the lease TTL. A
    /// non-finite or non-positive value takes the default lease TTL.
    #[must_use]
    pub fn for_lease_ttl_minutes(lease_ttl_minutes: f64, grace: Duration) -> Self {
        let lease = if lease_ttl_minutes.is_finite() && lease_ttl_minutes > 0.0 {
            Duration::from_secs_f64((lease_ttl_minutes * 60.0).min(7.0 * 86_400.0))
        } else {
            FORGE_LEASE_TTL
        };
        Self {
            assignment_ttl: lease + ASSIGNMENT_TTL_MARGIN,
            hold_ttl: lease,
            grace,
        }
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            assignment_ttl: FORGE_LEASE_TTL + ASSIGNMENT_TTL_MARGIN,
            hold_ttl: FORGE_LEASE_TTL,
            grace: DEFAULT_GRACE,
        }
    }
}

/// A row's state. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Granted,
    Live,
    Held,
}

/// One `(repo, item)` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The host running or granted it; `None` for a held row (the owner is
    /// outside fleet mode).
    pub host: Option<String>,
    pub step: String,
    pub assignment_id: String,
    pub state: RowState,
    /// Whether the captain granted it (as opposed to learning it from a live
    /// set). A `claim_lost` on a granted row is the split-brain measure.
    pub granted: bool,
    pub deadline: Mono,
}

/// What the table last heard from a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostState {
    pub free_slots: u32,
    /// How many steps the last heartbeat listed.
    pub live_reported: u32,
    pub roles_ok: BTreeSet<String>,
    pub last_seen: Mono,
}

/// A claim's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimDecision {
    Granted {
        assignment_id: String,
        ttl_s: u64,
        /// `true` when this re-granted the host's own existing grant.
        renewed: bool,
    },
    Refused {
        reason: RefusalReason,
        held_by: Option<String>,
        retry_after_s: u64,
    },
}

/// What a `refuse` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseOutcome {
    /// `claim_lost`: the row is now held for the forge lease TTL. `granted`
    /// says whether the captain had granted it.
    Held { granted: bool },
    /// A stale reason: the row is gone.
    Dropped,
    /// No such assignment for this host.
    Unknown,
}

/// Rows removed by an expiry pass, by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Expired {
    /// Granted or live rows that hit their TTL.
    pub assignments: u32,
    /// Held rows whose forge-TTL hold ended.
    pub holds: u32,
}

impl Expired {
    /// Whether anything expired.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.assignments == 0 && self.holds == 0
    }
}

/// The assignment table. See the module docs.
#[derive(Debug, Clone)]
pub struct Table {
    epoch: String,
    policy: Policy,
    started: Mono,
    grace_closed: bool,
    expected: BTreeSet<String>,
    rows: BTreeMap<(String, u64), Row>,
    by_id: BTreeMap<String, (String, u64)>,
    hosts: BTreeMap<String, HostState>,
    next_id: u64,
}

impl Table {
    /// An empty table for a server that started at `started` under `epoch`,
    /// with `expected` the host ids (fleet mode on) whose heartbeats end the
    /// grace window early.
    #[must_use]
    pub fn new(
        epoch: impl Into<String>,
        policy: Policy,
        started: Mono,
        expected: BTreeSet<String>,
    ) -> Self {
        Self {
            epoch: epoch.into(),
            policy,
            started,
            grace_closed: false,
            expected,
            rows: BTreeMap::new(),
            by_id: BTreeMap::new(),
            hosts: BTreeMap::new(),
            next_id: 0,
        }
    }

    /// This server's epoch.
    #[must_use]
    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// The table's policy.
    #[must_use]
    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// Replace the hosts expected to heartbeat (a fleet-store reload).
    pub fn set_expected(&mut self, expected: BTreeSet<String>) {
        self.expected = expected;
    }

    /// The row for `(repo, item)`.
    #[must_use]
    pub fn row(&self, repo: &str, item: u64) -> Option<&Row> {
        self.rows.get(&(repo.to_string(), item))
    }

    /// Every row, keyed by `(repo, item)`.
    #[must_use]
    pub fn rows(&self) -> &BTreeMap<(String, u64), Row> {
        &self.rows
    }

    /// The `(repo, item)` and row `assignment_id` names.
    #[must_use]
    pub fn row_by_id(&self, assignment_id: &str) -> Option<(&(String, u64), &Row)> {
        let key = self.by_id.get(assignment_id)?;
        self.rows.get_key_value(key)
    }

    /// What the table last heard from `host`.
    #[must_use]
    pub fn host(&self, host: &str) -> Option<&HostState> {
        self.hosts.get(host)
    }

    /// Whether claims are still refused with `grace` at `now`. Closes the
    /// window for good once it has ended.
    pub fn in_grace(&mut self, now: Mono) -> bool {
        if self.grace_closed {
            return false;
        }
        let timed_out = now >= self.started.plus(self.policy.grace);
        let all_reported =
            !self.expected.is_empty() && self.expected.iter().all(|h| self.hosts.contains_key(h));
        if timed_out || all_reported {
            self.grace_closed = true;
            return false;
        }
        true
    }

    /// Remove every row whose deadline has passed at `now`.
    pub fn expire(&mut self, now: Mono) -> Expired {
        let mut expired = Expired::default();
        let dead: Vec<(String, u64)> = self
            .rows
            .iter()
            .filter(|(_, row)| row.deadline <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in dead {
            if let Some(row) = self.remove(&key) {
                match row.state {
                    RowState::Held => expired.holds += 1,
                    RowState::Granted | RowState::Live => expired.assignments += 1,
                }
            }
        }
        expired
    }

    /// Apply `host`'s heartbeat at `now`: record its slots and roles, and
    /// renew (or create) a live row for every step it lists. Returns whether
    /// the server is still in its grace window after this heartbeat.
    pub fn heartbeat(&mut self, now: Mono, host: &str, hb: &HeartbeatRequest) -> bool {
        self.hosts.insert(
            host.to_string(),
            HostState {
                free_slots: hb.free_slots,
                live_reported: u32::try_from(hb.live.len()).unwrap_or(u32::MAX),
                roles_ok: hb.roles_ok.iter().cloned().collect(),
                last_seen: now,
            },
        );
        let deadline = now.plus(self.policy.assignment_ttl);
        for live in &hb.live {
            self.apply_live(host, live, deadline);
        }
        self.in_grace(now)
    }

    fn apply_live(&mut self, host: &str, live: &LiveStep, deadline: Mono) {
        let key = (live.repo.clone(), live.item);
        if let Some(row) = self.rows.get_mut(&key) {
            match row.host.as_deref() {
                // A heartbeat naming another assignment, or an id-less step
                // that differs from a fresh grant's, was read before this
                // row was replaced (a step change through `claim`): it still
                // names the old step and must not flip the row back. The
                // next heartbeat carries the new id.
                Some(h) if h == host && is_stale_for(row, live) => {}
                Some(h) if h == host => {
                    row.state = RowState::Live;
                    row.deadline = deadline;
                    row.step.clone_from(&live.step);
                }
                // A held item this host now runs: it won the forge order,
                // so the row is its own again.
                None => {
                    row.host = Some(host.to_string());
                    row.state = RowState::Live;
                    row.deadline = deadline;
                    row.step.clone_from(&live.step);
                }
                // Another host's row: the forge decides that race; keep the
                // first holder.
                Some(_) => {}
            }
            return;
        }
        // A new row. Keep the worker's assignment id (a grant from before a
        // restart) so its later `release` / `refuse` still finds the row,
        // unless that id already names another row.
        let id = match &live.assignment_id {
            Some(id) if !id.is_empty() && !self.by_id.contains_key(id) => id.clone(),
            _ => self.new_id("l"),
        };
        self.insert(
            key,
            Row {
                host: Some(host.to_string()),
                step: live.step.clone(),
                assignment_id: id,
                state: RowState::Live,
                granted: live.assignment_id.is_some(),
                deadline,
            },
        );
    }

    /// Decide `host`'s claim of `(repo, item)` for `step` at `now`.
    /// `max_concurrent` is the host's `maxConcurrent` from the fleet config.
    pub fn claim(
        &mut self,
        now: Mono,
        host: &str,
        repo: &str,
        item: u64,
        step: &str,
        max_concurrent: Option<u32>,
    ) -> ClaimDecision {
        self.expire(now);
        if self.in_grace(now) {
            let end = self.started.plus(self.policy.grace);
            return refused(RefusalReason::Grace, None, now.secs_until(end, 1));
        }
        let key = (repo.to_string(), item);
        // Whether this claim replaces the host's own row on the item with a
        // different step (see the module docs).
        let mut replaces_own = false;
        if let Some(row) = self.rows.get_mut(&key) {
            let own = row.host.as_deref() == Some(host) && row.state != RowState::Held;
            if own && row.step == step {
                // Idempotent: the same host asking again for the same
                // (item, step) gets the same assignment id back, renewed.
                row.deadline = row.deadline.max(now.plus(self.policy.assignment_ttl));
                return ClaimDecision::Granted {
                    assignment_id: row.assignment_id.clone(),
                    ttl_s: self.policy.assignment_ttl.as_secs(),
                    renewed: true,
                };
            }
            if !own {
                let retry = now.secs_until(row.deadline, 1).min(HELD_RETRY_MAX_SECS);
                return refused(RefusalReason::Held, row.host.clone(), retry);
            }
            replaces_own = true;
        }
        let Ok(parsed) = step.parse::<StepRole>() else {
            return refused(RefusalReason::NotEligible, None, NOT_ELIGIBLE_RETRY_SECS);
        };
        if parsed == StepRole::ChampionMerge {
            let merge = StepRole::ChampionMerge.as_str();
            if let Some(row) = self
                .rows
                .iter()
                .find(|(k, row)| k.0 == repo && row.step == merge && **k != key)
                .map(|(_, row)| row)
            {
                let retry = now.secs_until(row.deadline, 1).min(HELD_RETRY_MAX_SECS);
                return refused(RefusalReason::Held, row.host.clone(), retry);
            }
        }
        let state = self.hosts.get(host);
        if let Some(state) = state {
            if !state.roles_ok.contains(parsed.role_name()) {
                return refused(RefusalReason::NotEligible, None, NOT_ELIGIBLE_RETRY_SECS);
            }
        }
        let reported = state.map(|s| s.free_slots.saturating_add(s.live_reported));
        let limit = match (reported, max_concurrent) {
            (Some(r), Some(m)) => Some(r.min(m)),
            (Some(r), None) => Some(r),
            (None, m) => m,
        };
        // The row being replaced frees its slot for the new step.
        let held = self.held_by(host).saturating_sub(u32::from(replaces_own));
        if limit.is_none_or(|l| held >= l) {
            return refused(RefusalReason::Cap, None, CAP_RETRY_SECS);
        }
        if replaces_own {
            self.remove(&key);
        }
        let id = self.new_id("a");
        self.insert(
            key,
            Row {
                host: Some(host.to_string()),
                step: step.to_string(),
                assignment_id: id.clone(),
                state: RowState::Granted,
                granted: true,
                deadline: now.plus(self.policy.assignment_ttl),
            },
        );
        ClaimDecision::Granted {
            assignment_id: id,
            ttl_s: self.policy.assignment_ttl.as_secs(),
            renewed: false,
        }
    }

    /// Granted and live rows `host` holds.
    #[must_use]
    pub fn held_by(&self, host: &str) -> u32 {
        let n = self
            .rows
            .values()
            .filter(|r| r.state != RowState::Held && r.host.as_deref() == Some(host))
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Apply `host`'s `refuse` of `assignment_id` for `reason` at `now`.
    pub fn refuse(
        &mut self,
        now: Mono,
        host: &str,
        assignment_id: &str,
        reason: &str,
    ) -> RefuseOutcome {
        let Some(key) = self.own_key(host, assignment_id) else {
            return RefuseOutcome::Unknown;
        };
        if reason == REFUSE_CLAIM_LOST {
            let hold_ttl = self.policy.hold_ttl;
            if let Some(row) = self.rows.get_mut(&key) {
                let granted = row.granted;
                row.host = None;
                row.state = RowState::Held;
                row.deadline = now.plus(hold_ttl);
                return RefuseOutcome::Held { granted };
            }
            return RefuseOutcome::Unknown;
        }
        self.remove(&key);
        RefuseOutcome::Dropped
    }

    /// Apply `host`'s `release` of `assignment_id`. `true` when the row was
    /// this host's and is now gone.
    pub fn release(&mut self, host: &str, assignment_id: &str) -> bool {
        match self.own_key(host, assignment_id) {
            Some(key) => self.remove(&key).is_some(),
            None => false,
        }
    }

    fn own_key(&self, host: &str, assignment_id: &str) -> Option<(String, u64)> {
        let key = self.by_id.get(assignment_id)?;
        let row = self.rows.get(key)?;
        (row.host.as_deref() == Some(host) && row.state != RowState::Held).then(|| key.clone())
    }

    fn new_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        let epoch: String = self.epoch.chars().take(8).collect();
        format!("{prefix}-{epoch}-{}", self.next_id)
    }

    fn insert(&mut self, key: (String, u64), row: Row) {
        self.by_id.insert(row.assignment_id.clone(), key.clone());
        self.rows.insert(key, row);
    }

    fn remove(&mut self, key: &(String, u64)) -> Option<Row> {
        let row = self.rows.remove(key)?;
        self.by_id.remove(&row.assignment_id);
        Some(row)
    }
}

fn refused(reason: RefusalReason, held_by: Option<String>, retry_after_s: u64) -> ClaimDecision {
    ClaimDecision::Refused {
        reason,
        held_by,
        retry_after_s,
    }
}

#[cfg(test)]
#[path = "table_tests.rs"]
mod tests;

/// Whether `live` predates `row`'s grant: it carries a different assignment
/// id, or none while the row is a fresh grant for another step.
fn is_stale_for(row: &Row, live: &LiveStep) -> bool {
    row.granted
        && match live.assignment_id.as_deref() {
            Some(id) => id != row.assignment_id,
            None => row.state == RowState::Granted && live.step != row.step,
        }
}
