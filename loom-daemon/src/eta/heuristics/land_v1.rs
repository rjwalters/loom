//! `land-v1`: time until the issue's PR merges (or the issue closes as
//! completed), from the current stage.
//!
//! Reads in-sweep phase durations and the ETA tracker's own stage-sample
//! journal (the external Judge and merge paths). An approved path always
//! ends with `merge_wait`.
//!
//! For an issue not yet started (#9326) the path starts at `ready_wait`: the
//! same queue wait `start-v1` simulates, then the post-dispatch chain from
//! `sweep.curator`, in one resampling pass. Every started item's output is
//! unchanged.

use super::{estimate_path, PathRules};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind};

/// The id. Immutable once shipped.
pub const LAND_V1: &str = "land-v1";

/// `land-v1`.
#[derive(Debug, Clone, Copy)]
pub struct LandV1;

impl Heuristic for LandV1 {
    fn id(&self) -> &'static str {
        LAND_V1
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        estimate_path(
            PathRules {
                id: LAND_V1,
                kind: Kind::Land,
                sources: &[SampleSource::SweepOutcome, SampleSource::StageJournal],
                always_merge: true,
                censoring: false,
                adjust: None,
            },
            input,
            history,
        )
    }
}
