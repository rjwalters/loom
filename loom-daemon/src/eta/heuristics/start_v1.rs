//! `start-v1` (#9326): time until a ready (`loom:issue`) issue's sweep is
//! dispatched, from its `dispatch_plan` position.
//!
//! The path is the `ready_wait` stage alone: one draw from the host's
//! slot-turnover grid per turnover the item still needs
//! ([`crate::eta::DispatchInput::turnovers`]), plus the fixed admission delay
//! ([`crate::eta::DispatchInput::admission_delay_sec`]). The turnover samples
//! live in the ETA stage journal only. No plan position is a
//! `no_dispatch_plan` refusal; too few turnover samples is
//! `insufficient_samples` — whatever the position, never a number made up
//! from the tick interval alone.

use super::{estimate_path, PathRules};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind};

/// The id. Immutable once shipped.
pub const START_V1: &str = "start-v1";

/// `start-v1`.
#[derive(Debug, Clone, Copy)]
pub struct StartV1;

impl Heuristic for StartV1 {
    fn id(&self) -> &'static str {
        START_V1
    }

    fn kind(&self) -> Kind {
        Kind::Start
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        estimate_path(
            PathRules {
                id: START_V1,
                kind: Kind::Start,
                sources: &[SampleSource::StageJournal],
                always_merge: false,
                censoring: false,
                adjust: None,
                models_hold: false,
                half_life_sec: None,
                stall_term: false,
                residual_tail: false,
            },
            input,
            history,
        )
    }
}
