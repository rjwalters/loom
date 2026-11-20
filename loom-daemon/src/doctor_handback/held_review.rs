//! Route a repaired Champion-held approval back to Judge. The trusted intent
//! makes the exact operation retryable while the hold remains in force.

use super::{same_sha, Snapshot, CHANGES, CLAIM, QUEUE};
use crate::champion_hold::{self, standing_hold, unreleased_hold_head, HoldKind};
use crate::claim_reconciliation::{extract_latest_verdict_sha, VerdictKind};
use serde_json::Value;

pub(super) const APPROVED: &str = "loom:pr";
const BLOCKED: &str = "loom:blocked";
const REVIEWING: &str = "loom:reviewing";
const PREFIX: &str = "<!-- loom:held-doctor-review ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Intent {
    from: String,
    to: String,
    hold: u64,
}

impl Intent {
    fn marker(&self) -> String {
        format!("{PREFIX}from={} to={} hold={} -->", self.from, self.to, self.hold)
    }

    pub(crate) fn body(&self) -> String {
        format!("{}\n\nDoctor verified this held repair for independent review. The existing Champion hold remains in force.", self.marker())
    }

    pub(crate) fn from(&self) -> &str {
        &self.from
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A live owner is required to record a new intent.
    Doctor,
    /// A prior intent is required. Recovery cannot act alongside a live role.
    Recover,
}

pub(crate) fn candidate(labels: &[String]) -> bool {
    labels.iter().any(|label| label == BLOCKED)
        && labels.iter().any(|label| label == APPROVED)
        && !labels.iter().any(|label| {
            (label != BLOCKED
                && crate::claim_reconciliation::VERDICT_HOLD_LABELS.contains(&label.as_str()))
                || matches!(label.as_str(), "loom:frozen" | CHANGES | REVIEWING)
        })
}

/// Reconstruct the currently admissible operation from fresh, trusted records.
/// A distinct hold episode, any intervening verdict, and release signals all
/// invalidate an earlier intent. The hold's recorded head must be the same
/// head as the still-standing approval.
fn classify(
    snapshot: &Snapshot,
    park_body: &str,
    comments: &[Value],
    expected: &str,
) -> Option<(Intent, bool)> {
    if !same_sha(&snapshot.head_sha, expected) || !candidate(&snapshot.labels) {
        return None;
    }
    let bodies = crate::comment_trust::records::bodies(comments);
    if bodies.len() != comments.len() || champion_hold::doctor_cycle_capped(&bodies) {
        return None;
    }
    let kind = standing_hold(&bodies)?;
    let records = crate::park_record::parse(park_body);
    if records.iter().any(|park| {
        park.blocker.is_some()
            || park.by.as_deref() != Some(champion_hold::HOLD_BY)
            || park.reason.as_deref() != Some(kind.reason())
    }) {
        return None;
    }
    let hold_marker = match kind {
        HoldKind::CriticalFile => champion_hold::CRITICAL_FILE_HOLD_MARKER,
        HoldKind::MergeRisk => champion_hold::MERGE_RISK_HOLD_MARKER,
        HoldKind::Ratification => champion_hold::ratification::HOLD_MARKER,
    };
    let from = unreleased_hold_head(&bodies)?;
    let approved = extract_latest_verdict_sha(&bodies, VerdictKind::Approved)?;
    if same_sha(&from, expected)
        || !same_sha(&from, &approved)
        || !full_sha(expected)
        || !full_sha(&from)
    {
        return None;
    }
    let index = bodies
        .iter()
        .rposition(|body| body.starts_with(hold_marker))?;
    let head = crate::merge_pr::hold_state::recorded_head(&bodies[index])?;
    if !same_sha(&head, &from) {
        return None;
    }
    let hold = comments[index].get("id")?.as_u64()?;
    let intent = Intent {
        from,
        to: expected.to_ascii_lowercase(),
        hold,
    };
    let marker = intent.marker();
    let recorded = bodies[index + 1..]
        .iter()
        .any(|body| body.lines().next() == Some(marker.as_str()));
    Some((intent, recorded))
}

fn full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) struct Observation {
    pub(crate) snapshot: Snapshot,
    pub(crate) park_body: String,
    pub(crate) trusted: Vec<Value>,
    /// Doctor mode owns the live lease, or recovery mode sees no live role.
    pub(crate) can_act: bool,
}

pub(crate) trait Forge {
    fn observe(&mut self, mode: Mode) -> Option<Observation>;
    fn content_changed(&mut self, intent: &Intent, expected: &str) -> Option<bool>;
    fn record(&mut self, intent: &Intent) -> bool;
    fn ensure_idle(&mut self) -> bool;
    fn add(&mut self, label: &str) -> bool;
    fn remove(&mut self, label: &str) -> bool;
    fn snapshot(&mut self) -> Option<Snapshot>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ineligible,
    Interrupted,
    Requeued,
}

fn eligible(obs: Observation, mode: Mode, expected: &str) -> Option<(Intent, bool, Snapshot)> {
    let (intent, recorded) = classify(&obs.snapshot, &obs.park_body, &obs.trusted, expected)?;
    if !obs.can_act || (mode == Mode::Doctor && !obs.snapshot.has(CLAIM)) {
        return None;
    }
    Some((intent, recorded, obs.snapshot))
}

fn read(forge: &mut impl Forge, mode: Mode, expected: &str) -> Option<(Intent, bool, Snapshot)> {
    eligible(forge.observe(mode)?, mode, expected)
}

/// Record the intent before touching labels. The old approval stays available
/// to the reconciliation listing until the queue label is safely added. The
/// blocked label is never written. Check the exact intent and current claims
/// again before each phase, including after the queue-label write.
pub(crate) fn route(forge: &mut impl Forge, mode: Mode, expected: &str) -> Outcome {
    let Some(obs) = forge.observe(mode) else {
        return if mode == Mode::Doctor {
            Outcome::Interrupted
        } else {
            Outcome::Ineligible
        };
    };
    let Some((intent, recorded, _)) = eligible(obs, mode, expected) else {
        return Outcome::Ineligible;
    };
    if !recorded {
        if mode == Mode::Recover {
            return Outcome::Ineligible;
        }
        match forge.content_changed(&intent, expected) {
            Some(true) => {}
            Some(false) => return Outcome::Ineligible,
            None => return Outcome::Interrupted,
        }
        if !matches!(read(forge, mode, expected), Some((current, _, _)) if current == intent)
            || !forge.record(&intent)
        {
            return Outcome::Interrupted;
        }
    }
    if !forge.ensure_idle() {
        return Outcome::Interrupted;
    }
    let Some((_, _, snapshot)) = read(forge, mode, expected)
        .filter(|(current, recorded, _)| current == &intent && *recorded)
    else {
        return Outcome::Interrupted;
    };
    if !snapshot.has(QUEUE) && !forge.add(QUEUE) {
        return Outcome::Interrupted;
    }
    if !forge.ensure_idle()
        || !matches!(read(forge, mode, expected), Some((current, true, live)) if current == intent && live.has(QUEUE))
        || !forge.remove(APPROVED)
    {
        return Outcome::Interrupted;
    }
    if mode == Mode::Doctor {
        // The originating Doctor owns this claim. The recovery runner must
        // leave any later claim for the ordinary lease/claim reconciliation.
        let _ = forge.remove(CLAIM);
    }
    if forge.snapshot().is_some_and(|snap| {
        same_sha(&snap.head_sha, expected)
            && snap.has(BLOCKED)
            && (snap.has(QUEUE) || snap.has(REVIEWING))
            && !snap.has(APPROVED)
            && (mode == Mode::Recover || !snap.has(CLAIM))
    }) {
        Outcome::Requeued
    } else {
        Outcome::Interrupted
    }
}

#[cfg(test)]
mod tests;
