//! Why one `loom:blocked` artifact could not be read (#12036): the typed cause
//! [`check-stale-blocked`](super) and `notify-cleared-blockers` report as
//! *unevaluated*, never as clear.
//!
//! Moved here from the retired release pass's `release_items` (#11301), which
//! was its only other user.

use serde::Serialize;

use super::budget::{Bucket, FloorStop};

/// Why an artifact went unevaluated, typed where the cause is created so no
/// consumer parses the text (#12036).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnevaluatedReason {
    /// The REST core budget floor.
    BudgetFloorCore,
    /// The GraphQL budget floor.
    BudgetFloorGraphql,
    /// A forge read failed (404, no state, comment or label-event read).
    ReadFailed,
    /// Anything else (no evidence gathered, body unknown).
    Other,
}

impl UnevaluatedReason {
    /// Every reason, in a stable order.
    pub const ALL: [Self; 4] = [
        Self::BudgetFloorCore,
        Self::BudgetFloorGraphql,
        Self::ReadFailed,
        Self::Other,
    ];

    /// The exported snake_case key.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BudgetFloorCore => "budget_floor_core",
            Self::BudgetFloorGraphql => "budget_floor_graphql",
            Self::ReadFailed => "read_failed",
            Self::Other => "other",
        }
    }
}

/// The budget reading that stopped an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BudgetReading {
    pub bucket: Bucket,
    pub remaining: u64,
    pub floor: u64,
}

/// Why one artifact could not be read: the typed cause plus the text the
/// pass has always reported (`Display` is exactly `text`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub reason: UnevaluatedReason,
    pub budget: Option<BudgetReading>,
    pub text: String,
}

impl Unreadable {
    /// A forge read that failed.
    #[must_use]
    pub fn read_failed(text: impl Into<String>) -> Self {
        Self {
            reason: UnevaluatedReason::ReadFailed,
            budget: None,
            text: text.into(),
        }
    }

    /// Neither a floor nor a failed read.
    #[must_use]
    pub fn other(text: impl Into<String>) -> Self {
        Self {
            reason: UnevaluatedReason::Other,
            budget: None,
            text: text.into(),
        }
    }

    /// A floor stop with its own `text` (the phase-1 probe words it itself).
    #[must_use]
    pub fn floor(stop: &FloorStop, text: impl Into<String>) -> Self {
        Self {
            reason: match stop.bucket {
                Bucket::Core => UnevaluatedReason::BudgetFloorCore,
                Bucket::Graphql => UnevaluatedReason::BudgetFloorGraphql,
            },
            budget: Some(BudgetReading {
                bucket: stop.bucket,
                remaining: stop.remaining,
                floor: stop.floor,
            }),
            text: text.into(),
        }
    }

    /// The same cause under a longer `text`.
    #[must_use]
    pub fn with_text(mut self, text: String) -> Self {
        self.text = text;
        self
    }
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl From<FloorStop> for Unreadable {
    fn from(stop: FloorStop) -> Self {
        let text = stop.to_string();
        Self::floor(&stop, text)
    }
}

/// A bare message is a failed read: every `String` error the gatherer
/// produces is one.
impl From<String> for Unreadable {
    fn from(text: String) -> Self {
        Self::read_failed(text)
    }
}
