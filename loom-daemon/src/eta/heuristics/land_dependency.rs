//! `land-2026-10-06-tandem-wren` (#10510): `land-2026-10-04-twin-otter-b`
//! composed over the dependency graph ([`crate::eta::dependency`]).
//!
//! A blocked, stacked or sequenced item starts (or merges) after its
//! parents: per Monte Carlo draw, its ready time is the max of its own and
//! its parents' land draws, with common random numbers across the graph.
//! An item with no parent that applies at `as_of` gets the base's own
//! explanation, re-identified: the same numbers, bit for bit.
//!
//! The wrapper is generic over its base ([`DependencyComposition::new`]);
//! this id fixes the base at twin-otter-b. A composition over another base
//! is a new id (the shadow budget, #10525: wrappers are explicit).
//!
//! Ships **registered, not current**: promotion is
//! [`crate::eta::shadow`]'s two-gate rule.

use super::LandTwinOtterB;
use crate::eta::dependency;
use crate::eta::fit::CoefficientFile;
use crate::eta::history::StageSamples;
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind};
use std::sync::Arc;

/// The id. Immutable once shipped.
pub const LAND_TANDEM_WREN: &str = "land-2026-10-06-tandem-wren";

/// A dependency composition over `base`, registered as `id`.
pub struct DependencyComposition {
    id: &'static str,
    base: Box<dyn Heuristic>,
}

impl DependencyComposition {
    /// The composition of `base`, as heuristic `id`.
    #[must_use]
    pub fn new(id: &'static str, base: Box<dyn Heuristic>) -> Self {
        DependencyComposition { id, base }
    }

    /// `land-2026-10-06-tandem-wren`: over twin-otter-b with `fit`.
    #[must_use]
    pub fn tandem_wren(fit: Option<Arc<CoefficientFile>>) -> Self {
        Self::new(LAND_TANDEM_WREN, Box::new(LandTwinOtterB::new(fit)))
    }
}

impl Heuristic for DependencyComposition {
    fn id(&self) -> &'static str {
        self.id
    }

    fn kind(&self) -> Kind {
        self.base.kind()
    }

    /// The base's: the wrapper must read exactly the input its base reads,
    /// or a dependency-free item would not be bit-identical.
    fn models_hold(&self) -> bool {
        self.base.models_hold()
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        dependency::compose(self.id, self.base.as_ref(), input, history)
    }
}
