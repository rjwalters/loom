//! Shadow-record schemas (#9787). Capture records are immutable and
//! deterministic; scheduling exposure is always recorded so nothing can
//! claim enforcement.

use serde::{Deserialize, Serialize};

pub const CAPTURE_SCHEMA_VERSION: u32 = 1;

pub use super::PairFeatures;
pub use crate::collision_evidence::records::SchedulingExposure;

/// Unordered pair identity shared with #9786's evidence records.
pub fn unordered_pair_id(a: u32, b: u32) -> String {
    crate::collision_evidence::records::unordered_pair_id(a, b)
}

/// One captured candidate pair at a dispatch tick — recorded **before** any
/// outcome exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaptureRecord {
    /// Deterministic id (computed by `capture_id`).
    pub id: String,
    pub schema_version: u32,
    pub repo: String,
    pub unordered_pair_id: String,
    /// Dispatch tick / admission episode.
    pub tick_id: String,
    pub admission_policy: String,
    pub captured_at: String,
    /// Issues in canonical (ascending) order.
    pub issue_a: u32,
    pub issue_b: u32,
    /// Claim states at capture, `a/b` formatted.
    pub claim_state: String,
    /// Both sides dispatch-eligible this tick.
    pub dispatch_eligible: bool,
    /// Which side already had implementation/diff evidence at capture
    /// (cohort disclosure; format `a=<bool>/b=<bool>`).
    pub implementation_evidence_disclosed: String,
    /// Advisory features with explicit per-feature availability.
    pub features: PairFeatures,
    /// Always advisory for shadow records.
    pub exposure: SchedulingExposure,
    /// Budget/sampling notes (overruns, oversampling disclosure).
    #[serde(default)]
    pub notes: Vec<String>,
}
