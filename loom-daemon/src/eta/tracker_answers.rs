//! Answer states counted once per tracker pass (#10233).
//!
//! A refusal is emitted when its reason first appears and is never refreshed
//! ([`crate::eta::emit`]), while an answer is refreshed every few minutes. An
//! answer rate counted as "answered rows over all rows" is therefore inflated
//! by however many refreshes an answer earns while a refusal sits silent.
//!
//! Here each series carries its last emitted state forward: on every **full**
//! estimate pass (`Tracker::estimate(None, …)`), each live `(item, kind)`
//! contributes one [`PassAnswers`] naming every heuristic's current state —
//! newly emitted or carried over. One pass, one vote per series, whatever
//! the emit policy let out. A series that has never emitted (an hourly cap on
//! its very first try) has no state and is left out, never read as either.
//!
//! The caller folds these into the shadow ledger as paired answer-rate
//! observations ([`crate::eta::shadow::ShadowLedger::record_answers`]).

use super::{ItemKey, Tracker};
use crate::eta::Kind;

/// Every heuristic's answer state for one live `(item, kind)` in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassAnswers {
    /// What the heuristics predict.
    pub kind: Kind,
    /// `(heuristic id, answered)` for each heuristic with an emitted state.
    pub states: Vec<(String, bool)>,
}

impl Tracker {
    /// Record `key`'s `kind` answer states for this pass.
    pub(super) fn tally_pass(&mut self, key: &ItemKey, kind: Kind) {
        let Some(item) = self.items.get(key) else {
            return;
        };
        let states: Vec<(String, bool)> = item
            .answered
            .iter()
            .filter(|((k, _), _)| *k == kind)
            .map(|((_, heuristic), answered)| (heuristic.clone(), *answered))
            .collect();
        if !states.is_empty() {
            self.answers.push(PassAnswers { kind, states });
        }
    }

    /// The answer states tallied since the last call, and reset.
    pub fn drain_answers(&mut self) -> Vec<PassAnswers> {
        std::mem::take(&mut self.answers)
    }
}
