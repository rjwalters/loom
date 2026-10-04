//! Typed, idempotent merge-queue operations (#10255).
//!
//! The decisions live here, against the [`QueueApi`] trait, so every branch is
//! unit-testable with a scripted fake; [`super::github::GhQueueApi`] is the
//! only implementation that talks to a forge.
//!
//! # Contracts
//!
//! - **Enqueue is pinned to the approved head.** The local status read must
//!   show `headRefOid == approved_sha` before anything is sent, and the
//!   mutation itself always carries `expectedHeadOid = approved_sha` so a push
//!   landing between the read and the write is rejected forge-side. A head
//!   mismatch from either check is [`QueueError::HeadMismatch`] and is **never
//!   retried with the refreshed SHA** — re-approval is the caller's problem.
//! - **Idempotent.** Enqueueing a PR already queued at the approved head is
//!   [`EnqueueOutcome::AlreadyQueued`]; dequeueing a PR that is not queued is
//!   [`DequeueOutcome::NotQueued`]. Neither sends a mutation.
//! - **Dequeue cannot be head-pinned** (GitHub's `DequeuePullRequestInput`
//!   has no expected-head field), so status is re-read afterwards and a PR
//!   that merged in the meantime is reported as
//!   [`DequeueOutcome::AlreadyMerged`], not as success.
//! - **Direct mode never touches the queue API** — [`guarded_enqueue`] /
//!   [`guarded_dequeue`] refuse before the first call.

use std::fmt;

use super::mode::MergeMode;

/// Lifecycle state of the PR itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

impl PrState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::Closed => "closed",
            PrState::Merged => "merged",
        }
    }
}

/// The PR's merge-queue entry, when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    /// Forge entry state (GitHub: `QUEUED`, `AWAITING_CHECKS`, `MERGEABLE`,
    /// `UNMERGEABLE`, `LOCKED`).
    pub state: String,
    pub position: Option<u32>,
    /// The head commit the entry was queued at.
    pub head_oid: Option<String>,
}

/// One status snapshot: everything enqueue/dequeue decide on comes from a
/// single read, so the head and the queue state cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrQueueStatus {
    pub number: u32,
    /// Forge node id (what the mutations take; GitHub wants it, not the number).
    pub node_id: String,
    pub state: PrState,
    pub head_oid: String,
    pub entry: Option<QueueEntry>,
}

/// What the enqueue mutation reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueAck {
    Enqueued {
        position: Option<u32>,
    },
    /// The forge said the PR is already queued.
    AlreadyQueued,
}

/// What the dequeue mutation reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DequeueAck {
    Dequeued,
    /// The forge said the PR is not in the queue.
    NotQueued,
}

/// The forge seam. Implementations classify their own failures into
/// [`QueueError`] and must not put credential values in any message.
pub trait QueueApi {
    /// # Errors
    ///
    /// Any classified forge failure.
    fn status(&self, pr: u32) -> Result<PrQueueStatus, QueueError>;
    /// # Errors
    ///
    /// Any classified forge failure (a head mismatch is
    /// [`QueueError::HeadMismatch`]).
    fn enqueue(
        &self,
        pr: u32,
        node_id: &str,
        expected_head_oid: &str,
    ) -> Result<EnqueueAck, QueueError>;
    /// # Errors
    ///
    /// Any classified forge failure.
    fn dequeue(&self, pr: u32, node_id: &str) -> Result<DequeueAck, QueueError>;
}

/// Every way a queue operation can fail. Each variant has a stable
/// [`QueueError::code`] and an actionable [`fmt::Display`]; none carries a
/// credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    /// `champion.mergeMode` is `direct` — the queue API was not called.
    NotQueueMode,
    /// Queue execution is compiled dormant (#9978 Phase A) — not called.
    ExecutionDormant,
    /// The forge has no merge queue (Gitea).
    UnsupportedForge {
        forge: String,
    },
    /// `approved_sha` is not a full 40-hex commit id.
    InvalidSha {
        value: String,
    },
    NotFound {
        pr: u32,
    },
    PrNotOpen {
        pr: u32,
        state: PrState,
    },
    /// The PR head is not the approved head (or moved during the call).
    HeadMismatch {
        pr: u32,
        approved: String,
        actual: Option<String>,
    },
    /// Already queued, but at a head other than the approved one.
    QueuedAtOtherHead {
        pr: u32,
        approved: String,
        queued: String,
    },
    /// The credential may not perform this operation.
    Denied {
        detail: String,
    },
    /// The forge's API budget is exhausted — not a capability verdict.
    RateLimited {
        detail: String,
    },
    /// The forge says the queue is not available for this PR/branch.
    QueueUnavailable {
        detail: String,
    },
    /// Anything else (transport, malformed response, unclassified error).
    Forge {
        detail: String,
    },
}

impl QueueError {
    /// Stable, grep-able kind token.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            QueueError::NotQueueMode => "NOT_QUEUE_MODE",
            QueueError::ExecutionDormant => "EXECUTION_DORMANT",
            QueueError::UnsupportedForge { .. } => "UNSUPPORTED_FORGE",
            QueueError::InvalidSha { .. } => "INVALID_SHA",
            QueueError::NotFound { .. } => "PR_NOT_FOUND",
            QueueError::PrNotOpen { .. } => "PR_NOT_OPEN",
            QueueError::HeadMismatch { .. } => "HEAD_MISMATCH",
            QueueError::QueuedAtOtherHead { .. } => "QUEUED_AT_OTHER_HEAD",
            QueueError::Denied { .. } => "DENIED",
            QueueError::RateLimited { .. } => "RATE_LIMITED",
            QueueError::QueueUnavailable { .. } => "QUEUE_UNAVAILABLE",
            QueueError::Forge { .. } => "FORGE_ERROR",
        }
    }
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueError::NotQueueMode => write!(
                f,
                "champion.mergeMode is \"direct\"; the merge-queue API was not called. \
                 Set it to \"queue\" only once #9978's later phases have landed."
            ),
            QueueError::ExecutionDormant => write!(
                f,
                "merge-queue execution is dormant in this build (#9978 Phase A): the lifecycle \
                 safety contract and required merge-group checks are not installed yet. Nothing \
                 was sent, and nothing fell back to a direct merge."
            ),
            QueueError::UnsupportedForge { forge } => write!(
                f,
                "{forge} has no merge queue; use champion.mergeMode=direct for this repository."
            ),
            QueueError::InvalidSha { value } => write!(
                f,
                "approved head {value:?} is not a full 40-character commit SHA; pass the exact \
                 head the Judge approved."
            ),
            QueueError::NotFound { pr } => write!(f, "PR #{pr} was not found in this repository."),
            QueueError::PrNotOpen { pr, state } => {
                write!(f, "PR #{pr} is {}; only an open PR can be queued.", state.as_str())
            }
            QueueError::HeadMismatch {
                pr,
                approved,
                actual,
            } => write!(
                f,
                "PR #{pr} head is {} but the approved head is {approved}; the verdict does not \
                 cover the current head. Re-review is required — the refreshed SHA is never \
                 retried automatically.",
                actual.as_deref().unwrap_or("(moved during the request)")
            ),
            QueueError::QueuedAtOtherHead {
                pr,
                approved,
                queued,
            } => write!(
                f,
                "PR #{pr} is already queued at {queued}, not at the approved head {approved}. \
                 Dequeue it and re-review before queueing again."
            ),
            QueueError::Denied { detail } => write!(
                f,
                "the forge denied the operation ({detail}); the credential needs write access \
                 to pull requests and the merge queue."
            ),
            QueueError::RateLimited { detail } => write!(
                f,
                "the forge API is rate-limited ({detail}); retry after the reset. This is not a \
                 capability verdict."
            ),
            QueueError::QueueUnavailable { detail } => write!(
                f,
                "the merge queue is not available for this PR ({detail}); run \
                 `loom-daemon forge merge-queue preflight` for the reason."
            ),
            QueueError::Forge { detail } => write!(f, "forge request failed: {detail}"),
        }
    }
}

impl std::error::Error for QueueError {}

/// Result of a successful [`enqueue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Enqueued {
        position: Option<u32>,
    },
    /// Already queued at the approved head; nothing was sent.
    AlreadyQueued {
        position: Option<u32>,
    },
}

/// Result of a successful [`dequeue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DequeueOutcome {
    Dequeued,
    /// Not queued; nothing was sent.
    NotQueued,
    /// It merged before (or while) the dequeue ran.
    AlreadyMerged,
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read the PR's queue status.
///
/// # Errors
///
/// Whatever the API reports.
pub fn status(api: &dyn QueueApi, pr: u32) -> Result<PrQueueStatus, QueueError> {
    api.status(pr)
}

/// Enqueue `pr` pinned to `approved_sha`. See the module docs for the
/// contract.
///
/// # Errors
///
/// [`QueueError`] — never a silent direct merge.
pub fn enqueue(
    api: &dyn QueueApi,
    pr: u32,
    approved_sha: &str,
) -> Result<EnqueueOutcome, QueueError> {
    let approved = approved_sha.trim().to_ascii_lowercase();
    if !is_full_sha(&approved) {
        return Err(QueueError::InvalidSha {
            value: approved_sha.to_string(),
        });
    }
    let st = api.status(pr)?;
    if st.state != PrState::Open {
        return Err(QueueError::PrNotOpen {
            pr,
            state: st.state,
        });
    }
    if let Some(entry) = &st.entry {
        return match entry.head_oid.as_deref() {
            Some(queued) if !queued.eq_ignore_ascii_case(&approved) => {
                Err(QueueError::QueuedAtOtherHead {
                    pr,
                    approved,
                    queued: queued.to_string(),
                })
            }
            _ if !st.head_oid.eq_ignore_ascii_case(&approved) => Err(QueueError::HeadMismatch {
                pr,
                approved,
                actual: Some(st.head_oid.clone()),
            }),
            _ => Ok(EnqueueOutcome::AlreadyQueued {
                position: entry.position,
            }),
        };
    }
    if !st.head_oid.eq_ignore_ascii_case(&approved) {
        return Err(QueueError::HeadMismatch {
            pr,
            approved,
            actual: Some(st.head_oid),
        });
    }
    match api.enqueue(pr, &st.node_id, &approved)? {
        EnqueueAck::Enqueued { position } => Ok(EnqueueOutcome::Enqueued { position }),
        EnqueueAck::AlreadyQueued => {
            // A concurrent enqueue won the race; confirm it is OUR head.
            let again = api.status(pr)?;
            match again.entry {
                Some(QueueEntry {
                    head_oid: Some(ref q),
                    ..
                }) if !q.eq_ignore_ascii_case(&approved) => Err(QueueError::QueuedAtOtherHead {
                    pr,
                    approved,
                    queued: q.clone(),
                }),
                Some(entry) => Ok(EnqueueOutcome::AlreadyQueued {
                    position: entry.position,
                }),
                None => Err(QueueError::Forge {
                    detail: format!(
                        "the forge reported PR #{pr} already queued, but a re-read shows no queue entry"
                    ),
                }),
            }
        }
    }
}

/// Remove `pr` from the queue (idempotent; see module docs for the race).
///
/// # Errors
///
/// [`QueueError`].
pub fn dequeue(api: &dyn QueueApi, pr: u32) -> Result<DequeueOutcome, QueueError> {
    let st = api.status(pr)?;
    if st.state == PrState::Merged {
        return Ok(DequeueOutcome::AlreadyMerged);
    }
    if st.entry.is_none() {
        return Ok(DequeueOutcome::NotQueued);
    }
    let ack = api.dequeue(pr, &st.node_id)?;
    let after = api.status(pr)?;
    if after.state == PrState::Merged {
        return Ok(DequeueOutcome::AlreadyMerged);
    }
    if after.entry.is_some() {
        return Err(QueueError::Forge {
            detail: format!("dequeue of PR #{pr} was accepted but the PR is still queued"),
        });
    }
    Ok(match ack {
        DequeueAck::Dequeued => DequeueOutcome::Dequeued,
        DequeueAck::NotQueued => DequeueOutcome::NotQueued,
    })
}

/// The refusal every mutating entry point applies first: direct mode, then
/// the dormant execution gate. Pure, and checked **before** any API call.
///
/// # Errors
///
/// [`QueueError::NotQueueMode`] / [`QueueError::ExecutionDormant`].
pub fn execution_gate(mode: MergeMode, execution_enabled: bool) -> Result<(), QueueError> {
    match mode {
        MergeMode::Direct => Err(QueueError::NotQueueMode),
        MergeMode::Queue if !execution_enabled => Err(QueueError::ExecutionDormant),
        MergeMode::Queue => Ok(()),
    }
}

/// [`enqueue`] behind [`execution_gate`].
///
/// # Errors
///
/// The gate's refusal, else [`enqueue`]'s.
pub fn guarded_enqueue(
    mode: MergeMode,
    execution_enabled: bool,
    api: &dyn QueueApi,
    pr: u32,
    approved_sha: &str,
) -> Result<EnqueueOutcome, QueueError> {
    execution_gate(mode, execution_enabled)?;
    enqueue(api, pr, approved_sha)
}

/// [`dequeue`] behind [`execution_gate`].
///
/// # Errors
///
/// The gate's refusal, else [`dequeue`]'s.
pub fn guarded_dequeue(
    mode: MergeMode,
    execution_enabled: bool,
    api: &dyn QueueApi,
    pr: u32,
) -> Result<DequeueOutcome, QueueError> {
    execution_gate(mode, execution_enabled)?;
    dequeue(api, pr)
}
