//! `finish-v1`: remaining in-sweep time of a running sweep.
//!
//! Reads only in-sweep phase durations (`sweep.outcome`). The path ends at
//! the approving verdict, or after the in-sweep merge when at least half of
//! the history's successful sweeps merged themselves (`path.merge_share`).

use super::{estimate_path, PathRules};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind};

/// The id. Immutable once shipped.
pub const FINISH_V1: &str = "finish-v1";

/// `finish-v1`.
#[derive(Debug, Clone, Copy)]
pub struct FinishV1;

impl Heuristic for FinishV1 {
    fn id(&self) -> &'static str {
        FINISH_V1
    }

    fn kind(&self) -> Kind {
        Kind::Finish
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        estimate_path(
            PathRules {
                id: FINISH_V1,
                kind: Kind::Finish,
                sources: &[SampleSource::SweepOutcome],
                always_merge: false,
            },
            input,
            history,
        )
    }
}
