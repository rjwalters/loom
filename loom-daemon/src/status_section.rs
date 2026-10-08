//! `loom-daemon status --json --section …`: the named top-level sections of
//! the status payload (Issue #10787).
//!
//! A full `status --json` builds the whole [`crate::types::DaemonStatusReport`]
//! — `O(registered roots)`, 10–21s across ~58 repos on a busy dispatcher — and
//! then runs several client-side collectors (a per-account token probe, a git
//! staleness check, a worktree disk walk). Callers such as fleet tooling read
//! two or three fields. [`StatusSection`] names every top-level block of the
//! payload so a caller can ask for just those, and [`SectionSet`] says which
//! build phases those blocks actually need, so the daemon **skips** the
//! per-root walk (and the CLI skips its collectors) for anything unrequested,
//! rather than building everything and filtering the output.
//!
//! [`StatusSection`] is the single list: clap's `--section` parser and its
//! `--help` "possible values" come from its `ValueEnum` derive, the wire
//! request (`Request::DaemonStatusSections`) carries it via serde, and the
//! renderer's key filter reads [`StatusSection::json_keys`]. A new top-level
//! key must register here (a test fails otherwise); a section that needs the
//! per-root walk must also be named in [`SectionSet::walks_roots`], and one
//! that reads a machine-level input (token pool, disk/RAM headroom, the
//! work-finder config) in the matching `needs_*` predicate.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// One top-level section of the `status --json` payload.
///
/// The name is the payload key it selects; a section that groups several
/// keys (`in_flight` + `in_flight_count`) is named for the base key, and
/// [`Self::json_keys`] lists them all. The serde and clap names are the same
/// `snake_case` string, so the wire form and the CLI form never diverge.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum StatusSection {
    InFlight,
    UnregisteredLocked,
    StaleSweeps,
    StuckWorktreeReclaims,
    PoolExhaustionHolds,
    CapacityBound,
    RoleAgents,
    PreflightAdvisory,
    Observability,
    ForgeEvents,
    DeepClean,
    DynamicCap,
    Capacity,
    MainHealthGate,
    WorkFinder,
    OperatorPriorityLanding,
    RoleRunner,
    CredentialPreflight,
    Drain,
    PerRepo,
    Worktrees,
    Pipeline,
    TokenUsage,
    SelfUpdate,
    DaemonBuild,
    TaskLiveness,
    SessionContainers,
    AutoUpdate,
    IdleExit,
    HostBreaker,
    AdmissionBrake,
    RateLimitBreaker,
    ForgeCalls,
    Safehouse,
    PeerClaims,
    Protection,
    JournalAdoptedAtStartup,
    FleetStore,
    PendingRestart,
    ForgeEgress,
}

impl StatusSection {
    /// Every section, in payload order.
    #[must_use]
    pub fn all() -> &'static [StatusSection] {
        <Self as clap::ValueEnum>::value_variants()
    }

    /// The section's name: its `--section` value, its wire form, and (for
    /// a single-key section) the payload key itself.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.spec().0
    }

    /// The top-level payload keys this section selects.
    #[must_use]
    pub fn json_keys(self) -> &'static [&'static str] {
        self.spec().1
    }

    #[allow(clippy::too_many_lines)]
    fn spec(self) -> (&'static str, &'static [&'static str]) {
        use StatusSection as S;
        match self {
            S::InFlight => ("in_flight", &["in_flight", "in_flight_count"]),
            S::UnregisteredLocked => {
                ("unregistered_locked", &["unregistered_locked", "unregistered_locked_count"])
            }
            S::StaleSweeps => ("stale_sweeps", &["stale_sweeps", "stale_sweeps_count"]),
            S::StuckWorktreeReclaims => (
                "stuck_worktree_reclaims",
                &["stuck_worktree_reclaims", "stuck_worktree_reclaims_count"],
            ),
            S::PoolExhaustionHolds => ("pool_exhaustion_holds", &["pool_exhaustion_holds"]),
            S::CapacityBound => ("capacity_bound", &["capacity_bound"]),
            S::RoleAgents => ("role_agents", &["role_agents"]),
            S::PreflightAdvisory => (
                "preflight_advisory",
                &[
                    "preflight_advisory_active",
                    "preflight_advisory_message",
                    "preflight_advisory_changed_at",
                ],
            ),
            S::Observability => (
                "observability",
                &[
                    "observability_host_id_mismatch",
                    "observability_export",
                    "observability_exports",
                ],
            ),
            S::ForgeEvents => ("forge_events", &["forge_events"]),
            S::DeepClean => ("deep_clean", &["deep_clean"]),
            S::DynamicCap => ("dynamic_cap", &["dynamic_cap"]),
            S::Capacity => ("capacity", &["capacity"]),
            S::MainHealthGate => ("main_health_gate", &["main_health_gate"]),
            S::WorkFinder => ("work_finder", &["work_finder"]),
            S::OperatorPriorityLanding => {
                ("operator_priority_landing", &["operator_priority_landing"])
            }
            S::RoleRunner => {
                ("role_runner", &["role_runner_host_env_override", "role_runner_shard"])
            }
            S::CredentialPreflight => ("credential_preflight", &["credential_preflight"]),
            S::Drain => ("drain", &["drain"]),
            S::PerRepo => ("per_repo", &["per_repo"]),
            S::Worktrees => ("worktrees", &["worktrees"]),
            S::Pipeline => ("pipeline", &["pipeline"]),
            S::TokenUsage => ("token_usage", &["token_usage"]),
            S::SelfUpdate => ("self_update", &["self_update"]),
            S::DaemonBuild => ("daemon_build", &["daemon_build"]),
            S::TaskLiveness => ("task_liveness", &["task_liveness"]),
            S::SessionContainers => ("session_containers", &["session_containers"]),
            S::AutoUpdate => ("auto_update", &["auto_update"]),
            S::IdleExit => ("idle_exit", &["idle_exit"]),
            S::HostBreaker => ("host_breaker", &["host_breaker"]),
            S::AdmissionBrake => ("admission_brake", &["admission_brake"]),
            S::RateLimitBreaker => ("rate_limit_breaker", &["rate_limit_breaker"]),
            S::ForgeCalls => ("forge_calls", &["forge_calls"]),
            S::Safehouse => ("safehouse", &["safehouse"]),
            S::PeerClaims => ("peer_claims", &["peer_claims"]),
            S::Protection => ("protection", &["protection"]),
            S::JournalAdoptedAtStartup => {
                ("journal_adopted_at_startup", &["journal_adopted_at_startup"])
            }
            S::FleetStore => ("fleet_store", &["fleet_store"]),
            S::PendingRestart => ("pending_restart", &["pending_restart"]),
            S::ForgeEgress => ("forge_egress", &["forge_egress"]),
        }
    }
}

impl std::fmt::Display for StatusSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for StatusSection {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::all()
            .iter()
            .copied()
            .find(|section| section.as_str() == s)
            .ok_or_else(|| {
                let valid: Vec<&str> = Self::all().iter().map(|x| x.as_str()).collect();
                format!("unknown status section '{s}'; valid sections: {}", valid.join(", "))
            })
    }
}

/// Which sections one status build serves: every section (a plain `status`,
/// and every `Request::DaemonStatus`) or only a named subset.
///
/// Answers two questions: which build phases to run (`walks_*`, `needs_*`)
/// and which payload keys to keep ([`Self::retain_keys`]). The full set
/// answers "yes" to everything and keeps every key, so a full build is
/// exactly the pre-#10787 build.
///
/// The value is **normalized**: order and duplicates do not matter, and a
/// selection naming every section is the full set ([`Self::only`] collapses
/// it). Two sets are therefore equal — and hash equally — exactly when they
/// ask for the same build, so the set can key a shared or cached build
/// directly (#10861), and an all-sections request is a full request.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SectionSet {
    /// `None` = every section. Never `Some` of every section.
    only: Option<BTreeSet<StatusSection>>,
}

impl SectionSet {
    /// Every section (the default, unsectioned build).
    #[must_use]
    pub fn all() -> Self {
        Self { only: None }
    }

    /// Only `sections`. Duplicates and order collapse, and naming every
    /// section yields [`Self::all`].
    #[must_use]
    pub fn only(sections: impl IntoIterator<Item = StatusSection>) -> Self {
        let set: BTreeSet<StatusSection> = sections.into_iter().collect();
        if set.len() == StatusSection::all().len() {
            return Self::all();
        }
        Self { only: Some(set) }
    }

    /// Whether this is the full, unsectioned set.
    #[must_use]
    pub fn is_all(&self) -> bool {
        self.only.is_none()
    }

    /// Whether `section` is served.
    #[must_use]
    pub fn has(&self, section: StatusSection) -> bool {
        self.only.as_ref().is_none_or(|set| set.contains(&section))
    }

    fn any(&self, sections: &[StatusSection]) -> bool {
        sections.iter().any(|s| self.has(*s))
    }

    /// The selected sections, in payload order; `None` for the full set.
    #[must_use]
    pub fn sections(&self) -> Option<Vec<StatusSection>> {
        self.only.as_ref().map(|set| set.iter().copied().collect())
    }

    /// Whether the build must walk every registered root's sweep registry
    /// (the `O(roots)` registry-lock/list phase, 45–55% of the per-root loop
    /// on a busy dispatcher). Needed by everything derived from the live
    /// sweep list: the in-flight union and its count (which `capacity_bound`,
    /// `role_agents.total_with_sweeps` and the drain roll projection read),
    /// unregistered locks, stale sweeps, and the per-repo rows.
    #[must_use]
    pub fn walks_roots(&self) -> bool {
        use StatusSection as S;
        self.any(&[
            S::InFlight,
            S::UnregisteredLocked,
            S::StaleSweeps,
            S::CapacityBound,
            S::RoleAgents,
            S::Drain,
        ]) || self.walks_root_detail()
    }

    /// Whether the build must also run the per-root detail phases (role-runner
    /// config, the `role_shard::decide` walk, token pool, stash summary,
    /// sweep-command check) that only the per-repo rows carry. `worktrees`
    /// and `pipeline` are collected client-side from those rows' roots.
    #[must_use]
    pub fn walks_root_detail(&self) -> bool {
        use StatusSection as S;
        self.any(&[S::PerRepo, S::Worktrees, S::Pipeline])
    }

    /// Whether the daemon refreshes its CPU-idle sample (~1s `iostat` on
    /// macOS) before building — only `dynamic_cap` reports it.
    #[must_use]
    pub fn needs_cpu_sample(&self) -> bool {
        self.has(StatusSection::DynamicCap)
    }

    /// Whether the build resolves the daemon's token pool (directory, size,
    /// `.ranking`): `dynamic_cap` and `capacity` report it, and the CLI's
    /// token probe (`token_usage`, `capacity`) runs against that directory.
    #[must_use]
    pub fn needs_token_pool(&self) -> bool {
        use StatusSection as S;
        self.any(&[S::DynamicCap, S::Capacity, S::TokenUsage])
    }

    /// Whether the build measures disk and RAM headroom (a `df` and a
    /// `vm_stat` subprocess on macOS — seconds each on a saturated host):
    /// the cap terms `dynamic_cap` reports and `capacity_bound` compares.
    #[must_use]
    pub fn needs_host_headroom(&self) -> bool {
        use StatusSection as S;
        self.any(&[S::DynamicCap, S::CapacityBound])
    }

    /// Whether the build reads the work-finder config: the configured
    /// ceiling (`dynamic_cap`, `capacity_bound`), `work_finder.enabled`, and
    /// `protection.autonomy_mismatch`, which is derived from it.
    #[must_use]
    pub fn needs_work_finder_config(&self) -> bool {
        use StatusSection as S;
        self.any(&[
            S::DynamicCap,
            S::CapacityBound,
            S::WorkFinder,
            S::Protection,
        ])
    }

    /// `f()` when `section` is served, `None` otherwise — for a report field
    /// that only one section reads.
    pub fn when<T>(&self, section: StatusSection, f: impl FnOnce() -> T) -> Option<T> {
        self.has(section).then(f)
    }

    /// Whether the CLI runs its per-account token probe (a network call per
    /// account): `token_usage`, and `capacity`, whose figures prefer it.
    #[must_use]
    pub fn needs_token_probe(&self) -> bool {
        self.any(&[StatusSection::TokenUsage, StatusSection::Capacity])
    }

    /// Drop every top-level key of `value` that no selected section names.
    /// A no-op for the full set, so the default payload is untouched.
    pub fn retain_keys(&self, value: &mut serde_json::Value) {
        let (Some(set), Some(obj)) = (self.only.as_ref(), value.as_object_mut()) else {
            return;
        };
        obj.retain(|key, _| set.iter().any(|s| s.json_keys().contains(&key.as_str())));
    }
}

#[cfg(test)]
mod tests;
