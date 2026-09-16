//! The role-runner roster section of [`super::DaemonStatusReport`] (Issue
//! #7690 / #7691, #6704), split out of `types.rs` (#7852).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One roster member's row for `status` (Issue #7690) —
/// [`crate::role_shard::roster::RosterMemberView`] flattened for the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RosterMemberStatus {
    /// The member's opaque host id.
    pub host: String,
    /// Whether this member is currently live.
    pub fresh: bool,
    /// Seconds since this member's last heartbeat.
    pub last_beat_secs_ago: i64,
    /// How many workspace shard keys this member currently serves.
    pub serves_count: usize,
    /// Whether this row is the host `status` is running on.
    pub is_this_host: bool,
}

/// The role-runner roster section for `status`/`--json` (Issue #7690 / #7691,
/// #6704) — [`crate::role_shard::roster::RosterStatusView`] flattened for the
/// wire. With `roster.enabled` (opt-in) this roster is also what
/// [`super::RoleRunnerShardPosture::index`]/[`super::RoleRunnerShardPosture::count`] are
/// derived from, and [`Self::fence`] says whether this host is currently
/// admitted under it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RosterStatus {
    /// `owner/repo#N` of the roster issue.
    pub issue: String,
    /// How many members are currently live.
    pub live_count: usize,
    /// How many roster comments exist at all, live or expired.
    pub seen_count: usize,
    /// The current membership generation, `None` for an empty roster.
    #[serde(default)]
    pub generation: Option<DateTime<Utc>>,
    /// Seconds since [`Self::generation`], `None` alongside it.
    #[serde(default)]
    pub settled_secs: Option<i64>,
    /// The roster **admission fence**'s current verdict on this host (Issue
    /// #7691), as a one-line human string — `None` from a pre-#7691 daemon.
    ///
    /// Load-bearing for operators: a host that is *yielding* runs no role
    /// ticks at all, which is otherwise indistinguishable in `status` from a
    /// host that simply owns no slice. The design's whole fail-safe direction
    /// is "yield when in doubt", so the doubt has to be visible.
    #[serde(default)]
    pub fence: Option<String>,
    /// Per-member rows, sorted by host id. An expired member stays in this
    /// list (`fresh: false`) rather than being dropped — silence about a dead
    /// host is exactly how the pre-#6374 `LOOM_ROLE_RUNNER=0` mitigation
    /// became invisible.
    pub members: Vec<RosterMemberStatus>,
}
