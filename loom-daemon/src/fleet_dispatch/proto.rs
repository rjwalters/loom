//! The fleet-dispatch wire contract (Phase 1 of #12447), shared by the captain
//! server (#12449) and the worker client (#12450).
//!
//! HTTP/1.1 JSON over the tailnet. Every endpoint is `POST` with a JSON body,
//! and **every response carries `server_epoch`**, which changes on each
//! server start, so a worker can tell its table entries were rebuilt.
//!
//! | Endpoint | Request | Response |
//! |---|---|---|
//! | [`HEARTBEAT_PATH`] | [`HeartbeatRequest`] | [`HeartbeatResponse`] |
//! | [`CLAIM_PATH`] | [`ClaimRequest`] | [`ClaimResponse`] |
//! | [`REFUSE_PATH`] | [`RefuseRequest`] | [`AckResponse`] |
//! | [`RELEASE_PATH`] | [`ReleaseRequest`] | [`AckResponse`] |
//!
//! A `claim` refusal is an **answer** (HTTP 200, `granted: false`), never a
//! failure. Non-200 statuses carry an [`ErrorResponse`]: `400` malformed,
//! `403` caller not in the roster or `host_id` not the caller's, `404`/`405`
//! wrong route, `411`/`413` body length, `503` identity backend unavailable.
//!
//! Readers are tolerant: unknown fields are ignored, so a later phase can add
//! fields without breaking an older peer. `step` travels as the step wire
//! name ([`crate::step_role::StepRole::as_str`]) in a plain string so a live
//! entry from a newer worker cannot poison a whole heartbeat.

use serde::{Deserialize, Serialize};

/// `POST` path of the heartbeat.
pub const HEARTBEAT_PATH: &str = "/v1/heartbeat";
/// `POST` path of a claim.
pub const CLAIM_PATH: &str = "/v1/claim";
/// `POST` path of a refuse.
pub const REFUSE_PATH: &str = "/v1/refuse";
/// `POST` path of a release.
pub const RELEASE_PATH: &str = "/v1/release";

/// Every Phase 1 path.
pub const PATHS: [&str; 4] = [HEARTBEAT_PATH, CLAIM_PATH, REFUSE_PATH, RELEASE_PATH];

/// The `refuse` reason that holds the item for the forge lease TTL: someone
/// outside fleet mode won the forge order. Every other reason drops the row.
pub const REFUSE_CLAIM_LOST: &str = "claim_lost";

/// The `refuse` reasons the captain counts by name (spec §5.4): the worker's
/// existing refusal tokens. Any other token is counted as `other`, so a
/// worker cannot grow the refusals metric's label set.
pub const REFUSE_REASONS: [&str; 18] = [
    REFUSE_CLAIM_LOST,
    "already_closed",
    "already_landed",
    "claimed",
    "head_moved",
    "peer_active",
    "backstop_hold",
    "claim_unverified",
    "roll_paused",
    "spawn_failed",
    "label_flip_failed",
    "workspace_commands_missing",
    "token_selection_failed",
    "no_token_pool",
    "pool_exhausted",
    "runtime_rejected",
    "model_runtime_mismatch",
    "admission_refused",
];

#[cfg(test)]
mod refuse_reason_tests {
    use super::REFUSE_REASONS;
    use std::collections::BTreeSet;

    /// Every `StepRefusal::new("<token>", …)` literal in `src`.
    fn tokens(src: &str) -> impl Iterator<Item = String> + '_ {
        src.split("StepRefusal::new(").skip(1).filter_map(|rest| {
            let rest = rest.trim_start().strip_prefix('"')?;
            Some(rest.split('"').next()?.to_string())
        })
    }

    /// The worker client sends `StepRefusal::reason` on `/v1/refuse`
    /// (`step_spawn::give_back`), so every literal it is built with must be
    /// counted by name, not as `other`.
    #[test]
    fn every_client_refusal_token_is_a_refuse_reason() {
        let sources = [
            include_str!("../sweep_registry/step_spawn.rs"),
            include_str!("../sweep_registry/step_spawn/lease.rs"),
            include_str!("../sweep_registry/step_spawn/unlocked.rs"),
        ];
        let emitted: BTreeSet<String> = sources.iter().flat_map(|s| tokens(s)).collect();
        assert!(emitted.len() >= 10, "the scan found too few tokens: {emitted:?}");
        let counted: BTreeSet<String> = REFUSE_REASONS.iter().map(|r| (*r).to_string()).collect();
        let missing: Vec<_> = emitted.difference(&counted).collect();
        assert!(
            missing.is_empty(),
            "client refusal tokens missing from REFUSE_REASONS: {missing:?}"
        );
    }
}

/// One step the host runs, granted or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveStep {
    /// `owner/name`.
    pub repo: String,
    /// The issue or PR number the step acts on.
    pub item: u64,
    /// The step wire name (`builder`, `judge`, `champion_merge`, …).
    pub step: String,
    /// Present only for a step the captain granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<String>,
}

/// `POST /v1/heartbeat`: the host's live set, free slots and runnable roles.
/// Sent every 30 s; the server rebuilds its table from these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub host_id: String,
    /// Every step the host runs, including ones the captain never granted.
    pub live: Vec<LiveStep>,
    /// The worker's own count of free slots.
    pub free_slots: u32,
    /// Roles the host can run now (pool, brake and back-off already applied).
    pub roles_ok: Vec<String>,
}

/// The heartbeat's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub server_epoch: String,
    /// `true` while the server is in its start-up grace window and refuses
    /// claims with [`RefusalReason::Grace`].
    pub grace: bool,
}

/// `POST /v1/claim`: "may I claim this item I drew?"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRequest {
    pub host_id: String,
    pub repo: String,
    pub item: u64,
    pub step: String,
}

/// Why a claim was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// Another live assignment holds the `(repo, item)`, or another host
    /// holds the repo's Champion merge step.
    Held,
    /// The server is in its start-up grace window.
    Grace,
    /// The host does not fit the item, or the step's role is not in its
    /// `roles_ok`.
    NotEligible,
    /// The host is at its cap.
    Cap,
    /// A no-progress hold on the `(repo, item, step)` (#12541), as opposed
    /// to [`Self::Held`], another host's live claim (#12551). A worker
    /// that predates it reads it as [`Self::Other`] and still refuses.
    Cooldown,
    /// A reason this build does not know (a newer server). Never sent by
    /// this server.
    #[serde(other)]
    Other,
}

impl RefusalReason {
    /// The wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Grace => "grace",
            Self::NotEligible => "not_eligible",
            Self::Cap => "cap",
            Self::Cooldown => "cooldown",
            Self::Other => "other",
        }
    }
}

/// The claim's answer: `{granted: true, assignment_id, ttl_s}` or
/// `{granted: false, reason, held_by?, retry_after_s}`, both with
/// `server_epoch`. One struct with optional halves, so a reader needs no
/// tagged-union support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimResponse {
    pub server_epoch: String,
    pub granted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<String>,
    /// Seconds the grant lives without a heartbeat renewing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RefusalReason>,
    /// The host holding the item, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_by: Option<String>,
    /// Seconds before asking about this item again is worthwhile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_s: Option<u64>,
}

impl ClaimResponse {
    /// A grant.
    #[must_use]
    pub fn granted(server_epoch: &str, assignment_id: String, ttl_s: u64) -> Self {
        Self {
            server_epoch: server_epoch.to_string(),
            granted: true,
            assignment_id: Some(assignment_id),
            ttl_s: Some(ttl_s),
            reason: None,
            held_by: None,
            retry_after_s: None,
        }
    }

    /// A refusal.
    #[must_use]
    pub fn refused(
        server_epoch: &str,
        reason: RefusalReason,
        held_by: Option<String>,
        retry_after_s: u64,
    ) -> Self {
        Self {
            server_epoch: server_epoch.to_string(),
            granted: false,
            assignment_id: None,
            ttl_s: None,
            reason: Some(reason),
            held_by,
            retry_after_s: Some(retry_after_s),
        }
    }
}

/// `POST /v1/refuse`: the worker's live pre-check or its forge claim refused
/// a granted item. `reason` is the existing refusal token
/// ([`REFUSE_CLAIM_LOST`], `already_closed`, `head_moved`, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefuseRequest {
    pub host_id: String,
    pub assignment_id: String,
    pub reason: String,
}

/// `POST /v1/release`: the step ended. `outcome` is the step's outcome class
/// (`verdict` / `noop` / `refused` / `failed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRequest {
    pub host_id: String,
    pub assignment_id: String,
    pub outcome: String,
}

/// The answer to `refuse` and `release`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckResponse {
    pub server_epoch: String,
    /// Whether the server had the assignment (for this host). `false` after a
    /// restart lost it, or for another host's id; never an error.
    #[serde(default)]
    pub known: bool,
}

/// The body of every non-200 response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub server_epoch: String,
    pub error: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn heartbeat_round_trips_the_contract_shape() {
        let wire = json!({
            "host_id": "worker-a",
            "live": [
                {"repo": "acme/w", "item": 123, "step": "judge", "assignment_id": "a-1"},
                {"repo": "acme/w", "item": 7, "step": "builder"}
            ],
            "free_slots": 12,
            "roles_ok": ["builder", "judge"]
        });
        let req: HeartbeatRequest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(req.live[0].assignment_id.as_deref(), Some("a-1"));
        assert_eq!(req.live[1].assignment_id, None);
        assert_eq!(serde_json::to_value(&req).unwrap(), wire);
    }

    #[test]
    fn a_grant_and_a_refusal_serialise_only_their_own_half() {
        let grant = serde_json::to_value(ClaimResponse::granted("e1", "a-9".into(), 1200)).unwrap();
        assert_eq!(
            grant,
            json!({"server_epoch": "e1", "granted": true, "assignment_id": "a-9", "ttl_s": 1200})
        );
        let refusal = serde_json::to_value(ClaimResponse::refused(
            "e1",
            RefusalReason::Held,
            Some("worker-b".into()),
            60,
        ))
        .unwrap();
        assert_eq!(
            refusal,
            json!({"server_epoch": "e1", "granted": false, "reason": "held",
                   "held_by": "worker-b", "retry_after_s": 60})
        );
    }

    #[test]
    fn every_reason_serialises_to_its_token_and_an_unknown_one_reads_as_other() {
        for r in [
            RefusalReason::Held,
            RefusalReason::Grace,
            RefusalReason::NotEligible,
            RefusalReason::Cap,
            RefusalReason::Cooldown,
        ] {
            assert_eq!(serde_json::to_value(r).unwrap(), json!(r.as_str()));
        }
        let r: RefusalReason = serde_json::from_value(json!("queue_full")).unwrap();
        assert_eq!(r, RefusalReason::Other);
    }

    #[test]
    fn readers_ignore_unknown_fields() {
        let req: ClaimRequest = serde_json::from_value(json!({
            "host_id": "h", "repo": "a/b", "item": 1, "step": "builder", "seed": 4
        }))
        .unwrap();
        assert_eq!(req.item, 1);
        let ack: AckResponse = serde_json::from_value(json!({"server_epoch": "e"})).unwrap();
        assert!(!ack.known);
    }
}
