//! Status surface for the forge event-feed consumer (ADR-0021, Epic #8764
//! Phase 1 — issue #8765).
//!
//! A sibling module of [`super`] rather than more lines in `types.rs`, per
//! the file-size policy: the wire shape of one subsystem's status belongs in
//! one small file, and `types.rs` is an over-threshold ledger entry that may
//! not grow.
//!
//! The rule this type exists to enforce: **the feed always answers.** Before
//! it, the only evidence a daemon's cursor was advancing would have been the
//! absence of a warning in `daemon.log` — the same inference-from-absence
//! defect #5083 fixed for telemetry export. Every state below is a positive
//! answer to "why is (or is not) my cursor moving", including the deliberate
//! `disabled`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The one-word answer to "is this daemon consuming its forge event feed?"
///
/// Serialized `snake_case` so a watch loop can assert on it directly:
/// `loom-daemon status --json | jq -e '.forge_events.state == "healthy"'`.
/// An unknown variant from a *newer* daemon deserializes as
/// [`Self::Unrecognized`] rather than failing the whole status parse — the
/// same forward-compatibility posture as
/// [`super::ObservabilityExportState`].
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForgeEventsState {
    /// `forgeEvents.enabled` is `false`, or the block is absent entirely.
    /// Nothing is polled, no directory is created, no key is read, and no
    /// socket is opened — the deliberate default, and byte-identical in
    /// external behaviour to a pre-ADR-0021 daemon. **Never** reported for an
    /// opted-in daemon that failed to resolve its config: that is
    /// [`Self::Misconfigured`].
    #[default]
    Disabled,
    /// `enabled: true` but a required piece of provisioning could not be
    /// resolved: no `endpoint`, no `hostId`, an endpoint that is a reserved
    /// placeholder domain (RFC 2606/6761 — the event key must never be sent
    /// to `example.com`), or an unreadable/empty `eventKeyFile`. The poll
    /// loop never started. [`ForgeEventsStatus::last_error_detail`] names the
    /// missing piece.
    Misconfigured,
    /// The poll loop is running and has not completed a successful poll yet,
    /// and has not failed either — the first few seconds of a daemon's life.
    Connecting,
    /// The most recent poll failed for a reason that is neither an auth nor a
    /// host-identity problem: a transport error, a non-2xx that is not
    /// 401/403/404, a response over the read cap, a body that is not a valid
    /// feed page, or a `cursor` that moved backwards. The cursor was not
    /// advanced.
    Failing,
    /// The feed answered 401/403: this host's event key is wrong, revoked, or
    /// not yet minted. Distinct from [`Self::Failing`] because the fix is
    /// provisioning, not patience.
    AuthFailed,
    /// The feed answered 404 for our host, or answered 200 while echoing a
    /// **different** `host_id` than this daemon polls under. Cursors from
    /// such a response are never applied (ADR-0014 invariant 3) — a feed that
    /// is not ours cannot move our cursor.
    HostMismatch,
    /// Any of the three error classes above, sustained for
    /// [`crate::forge_events::BACKOFF_FAILURE_STREAK`] consecutive polls: the
    /// poll cadence has been stretched from the configured interval to
    /// [`crate::forge_events::BACKOFF_POLL_INTERVAL_SECS`].
    /// [`ForgeEventsStatus::last_error`] still carries the underlying class,
    /// so `backoff` narrows the cadence question without erasing the cause.
    /// A single successful poll returns both the state and the cadence.
    Backoff,
    /// The most recent poll succeeded (including a legitimately empty page —
    /// "no events since your cursor" is a successful poll, not a fault).
    Healthy,
    /// A state name this build does not know — a newer daemon reporting to an
    /// older client. Never produced by this daemon.
    #[serde(other)]
    Unrecognized,
}

impl ForgeEventsState {
    /// The short token the human-readable renderer leads with.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ForgeEventsState::Disabled => "disabled",
            ForgeEventsState::Misconfigured => "MISCONFIGURED",
            ForgeEventsState::Connecting => "connecting",
            ForgeEventsState::Failing => "FAILING",
            ForgeEventsState::AuthFailed => "AUTH FAILED",
            ForgeEventsState::HostMismatch => "HOST MISMATCH",
            ForgeEventsState::Backoff => "BACKOFF",
            ForgeEventsState::Healthy => "OK",
            ForgeEventsState::Unrecognized => "unrecognized",
        }
    }

    /// Whether this state is a *problem* an operator should act on.
    /// `disabled`, `connecting`, and `healthy` are not; the rest are.
    ///
    /// Note that even a "problem" here is never a correctness fault: the feed
    /// is additive prompt pressure over a polling floor that keeps running
    /// regardless (ADR-0014 invariant 2). It is a latency and provisioning
    /// signal, not an outage.
    #[must_use]
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            ForgeEventsState::Misconfigured
                | ForgeEventsState::Failing
                | ForgeEventsState::AuthFailed
                | ForgeEventsState::HostMismatch
                | ForgeEventsState::Backoff
        )
    }
}

/// Always-present state of this daemon's forge event-feed consumer.
///
/// Carried on [`super::DaemonStatusReport::forge_events`], which is `Some`
/// for any daemon of this vintage — `null` there means a pre-ADR-0021 binary,
/// never "silent because nothing happened".
///
/// **The event key never appears here.** Only the key *file path* is ever
/// named, in [`Self::last_error_detail`], exactly as the observability
/// exporter treats its ingest key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgeEventsStatus {
    /// The state as classified when this snapshot was taken. Unlike
    /// [`super::ObservabilityExportStatus`] there is no time-dependent grace
    /// window to re-derive across, so the daemon-stamped value is the answer.
    #[serde(default)]
    pub state: ForgeEventsState,
    /// The configured feed base URL, so an operator can confirm *where* the
    /// daemon is polling without opening the config. Present even when
    /// [`ForgeEventsState::Misconfigured`], when it is whatever did resolve.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// The `host_id` this daemon polls its feed under (`forgeEvents.hostId`).
    /// This is the identity the operator minted the key against — not
    /// necessarily [`crate::sweep_registry::host_identity`].
    #[serde(default)]
    pub host_id: Option<String>,
    /// The last durably-persisted feed cursor. `0` means "from the start of
    /// the feed's retention window" — the value a fresh, or a torn, state
    /// file reads as.
    #[serde(default)]
    pub cursor: u64,
    /// Non-empty pages observed this daemon process.
    #[serde(default)]
    pub pages_observed: u64,
    /// Events observed this daemon process, across those pages.
    #[serde(default)]
    pub events_observed: u64,
    /// When the poll loop started this daemon process. `None` when it never
    /// did (disabled / misconfigured).
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    /// When a poll last completed — successfully or not.
    #[serde(default)]
    pub last_poll_at: Option<DateTime<Utc>>,
    /// When a **non-empty** page was last applied. `None` alongside a `Some`
    /// `started_at` and a `healthy` state is the ordinary quiet-feed case,
    /// not a fault.
    #[serde(default)]
    pub last_page_at: Option<DateTime<Utc>>,
    /// The error *class* token of the most recent failure — `transport`,
    /// `protocol`, `auth_failed`, or `host_mismatch`. Retained under
    /// [`ForgeEventsState::Backoff`] (which describes cadence, not cause) and
    /// cleared by the next successful poll.
    #[serde(default)]
    pub last_error: Option<String>,
    /// Human-readable detail for that failure: an HTTP status, a transport
    /// error, the echoed vs expected `host_id`, or the offending config/key
    /// **path**. Never the key itself.
    #[serde(default)]
    pub last_error_detail: Option<String>,
    /// When that failure was observed.
    #[serde(default)]
    pub last_error_at: Option<DateTime<Utc>>,
    /// Consecutive failed polls. Reset to `0` by any successful poll; at
    /// [`crate::forge_events::BACKOFF_FAILURE_STREAK`] the cadence stretches
    /// and the state is promoted to [`ForgeEventsState::Backoff`].
    #[serde(default)]
    pub consecutive_failures: u32,
    /// The cadence currently in effect, in seconds — the configured interval
    /// normally, [`crate::forge_events::BACKOFF_POLL_INTERVAL_SECS`] under
    /// backoff. `0` when the loop is not running.
    #[serde(default)]
    pub poll_interval_secs: u64,
    /// One entry per **armed** Phase 2 early-tick consumer
    /// (`forgeEvents.events.*`), in the order the loops spawned. Empty is the
    /// default and the positive answer "no consumer is armed on this host" —
    /// a disarmed consumer registers nothing, so this is never "armed but
    /// silent". Absent on a pre-#8995 daemon, which deserializes as empty.
    #[serde(default)]
    pub wakes: Vec<ForgeEventsWakeStatus>,
    /// Event-gated polling state (`forgeEvents.pollGating`, #9255). `None`
    /// when gating is off (the default) or on a pre-#9255 daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_gating: Option<PollGatingStatus>,
}

/// Event-gated polling counters and per-workspace gated state (#9255).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollGatingStatus {
    #[serde(default)]
    pub enabled: bool,
    /// Gating is in effect right now: enabled AND the feed is `healthy`.
    /// `false` means every workspace is on its base cadence.
    #[serde(default)]
    pub gating_active: bool,
    /// The effective hard maximum staleness, in seconds.
    #[serde(default)]
    pub hard_cap_secs: u64,
    /// Discovery polls skipped because no feed event named the repo.
    #[serde(default)]
    pub polls_skipped: u64,
    /// Re-polls triggered by a feed event.
    #[serde(default)]
    pub event_repolls: u64,
    /// Re-polls forced by the hard cap expiring.
    #[serde(default)]
    pub hard_cap_repolls: u64,
    /// Hard-cap re-polls that found a change the feed never reported.
    #[serde(default)]
    pub lossy_repolls: u64,
    #[serde(default)]
    pub workspaces: Vec<PollGatingWorkspace>,
}

impl PollGatingStatus {
    /// Lossy-feed rate: `lossy_repolls / hard_cap_repolls`, `None` before the
    /// first hard-cap re-poll (nothing observed yet).
    #[must_use]
    pub fn lossy_rate(&self) -> Option<f64> {
        (self.hard_cap_repolls > 0)
            .then(|| self.lossy_repolls as f64 / self.hard_cap_repolls as f64)
    }
}

/// One workspace's gated state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollGatingWorkspace {
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub repo: String,
    /// The last decision skipped this workspace's poll.
    #[serde(default)]
    pub gated: bool,
    #[serde(default)]
    pub last_poll_age_secs: Option<u64>,
}

/// One armed early-tick consumer's live counters (ADR-0021 Phase 2, #8995).
///
/// The measurement surface ADR-0021's "run it and measure before defaulting
/// on" plan needs: an operator who arms a consumer can see whether prompts are
/// arriving (`prompts`), whether they are moving the loop (`early_ticks`), and
/// whether the spacing floor is absorbing them (`throttled` — under a burst a
/// healthy number, coalescing working rather than a fault).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgeEventsWakeStatus {
    /// Human-readable consumer name, e.g. `claim-reconcile wake`.
    #[serde(default)]
    pub consumer: String,
    /// The `forgeEvents.events.*` key that armed it, e.g.
    /// `claimReconcileWake` — the token an operator greps their config for.
    #[serde(default)]
    pub config_key: String,
    /// The loop's own cadence, in seconds.
    #[serde(default)]
    pub cadence_secs: u64,
    /// The spacing floor in effect (`forgeEvents.events.minSpacingSecs`).
    #[serde(default)]
    pub min_spacing_secs: u64,
    /// Qualifying `forge.event` prompts observed this daemon process.
    #[serde(default)]
    pub prompts: u64,
    /// Ticks taken early because of one.
    #[serde(default)]
    pub early_ticks: u64,
    /// Prompts dropped by the spacing floor.
    #[serde(default)]
    pub throttled: u64,
}

impl ForgeEventsWakeStatus {
    /// The documented early-tick ceiling for this loop: `cadence_secs /
    /// min_spacing_secs`, floored at `1`. Reporting arithmetic only.
    #[must_use]
    pub fn max_multiplier(&self) -> u64 {
        (self.cadence_secs / self.min_spacing_secs.max(1)).max(1)
    }
}

impl ForgeEventsStatus {
    /// The status of a daemon that is deliberately not consuming a feed.
    #[must_use]
    pub fn disabled() -> Self {
        ForgeEventsStatus::default()
    }

    /// The status of an opted-in daemon whose provisioning could not be
    /// resolved — see [`ForgeEventsState::Misconfigured`]. `endpoint` is
    /// whatever *did* resolve (possibly `None`); `detail` names the missing
    /// piece and is surfaced verbatim on `status`.
    #[must_use]
    pub fn misconfigured(endpoint: Option<String>, detail: String) -> Self {
        ForgeEventsStatus {
            state: ForgeEventsState::Misconfigured,
            endpoint,
            last_error: Some("misconfigured".to_string()),
            last_error_detail: Some(detail),
            last_error_at: Some(Utc::now()),
            ..ForgeEventsStatus::default()
        }
    }

    /// Age of the most recent completed poll, in seconds, as of `now`.
    /// `None` before the first poll completes. Clamped at zero so a clock
    /// skew between the daemon and the reading CLI cannot render as a
    /// negative age.
    #[must_use]
    pub fn last_poll_age_secs(&self, now: DateTime<Utc>) -> Option<u64> {
        self.last_poll_at
            .map(|at| u64::try_from((now - at).num_seconds()).unwrap_or(0))
    }
}
