//! Doctor hand-back as one conditional, add-first, verified label transition
//! (`loom-daemon forge doctor-handback`, #9388).
//!
//! A Doctor pushed its fix, the stale-verdict guard saw the new head and put
//! the PR back on `loom:review-requested`, a Judge approved it, and only then
//! did the Doctor write its own `changes-requested -> review-requested`
//! transition from labels it had read before pushing. The prompt already said
//! "re-read the labels first", but a read and a write made as two model
//! round-trips leave the whole tool-call gap as the race window, and the
//! prompt treated "already re-queued" (the normal result of a head move) as an
//! error.
//!
//! This module is the logic; the CLI only supplies a [`Forge`]:
//!
//! - [`decide`] — a pure function over the PR's live labels and head versus
//!   the head the Doctor pushed. "Already advanced" is a success, not an abort.
//! - [`run`] — reads, decides, writes add-first (a failed add removes nothing,
//!   so no interleaving of these calls leaves the PR with no lifecycle label),
//!   then re-reads; a verdict or review label that appeared during the write
//!   makes it withdraw its own `loom:review-requested` rather than leave a
//!   dual-label state.
//!
//! GitHub has no label compare-and-swap, so this narrows the window to
//! consecutive API calls in one process and verifies afterwards; it does not
//! close it.

use serde_json::Value;

/// The first token of every answer, so a caller accepts only a positive signal.
pub const SENTINEL: &str = "LOOM-DOCTOR-HANDBACK";
/// The Doctor's own claim.
pub const CLAIM: &str = "loom:treating";
/// The review queue label the hand-back adds.
pub const QUEUE: &str = "loom:review-requested";
/// The label the hand-back clears.
pub const CHANGES: &str = "loom:changes-requested";
/// Any of these present means a reviewer already has (or resolved) the PR.
pub const ADVANCED: &[&str] = &[QUEUE, "loom:reviewing", "loom:pr"];
/// Labels whose appearance during the write means a review raced the add.
pub const RIVALS: &[&str] = &["loom:reviewing", "loom:pr", CHANGES];

/// Exit code: the PR was already advanced; only the claim was released.
pub const EXIT_ALREADY_ADVANCED: i32 = 10;
/// Exit code: `loom:treating` was gone; nothing written.
pub const EXIT_CLAIM_LOST: i32 = 11;
/// Exit code: the head moved past the pushed SHA; only the claim released.
pub const EXIT_HEAD_MOVED: i32 = 12;
/// Exit code: a verdict/review label landed during the write; own add withdrawn.
pub const EXIT_RACED: i32 = 13;
/// Exit code: unreadable state or a write that did not hold.
pub const EXIT_FAILED: i32 = 1;

/// What one read of the PR returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Label names.
    pub labels: Vec<String>,
    /// The PR's current head SHA.
    pub head_sha: String,
}

impl Snapshot {
    fn has(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
}

/// Labels and head from a REST `pulls/{n}` document. `None` when either is
/// missing (fail closed).
#[must_use]
pub fn snapshot_from_pull(doc: &Value) -> Option<Snapshot> {
    let head_sha = doc.pointer("/head/sha")?.as_str()?.trim().to_string();
    if head_sha.is_empty() {
        return None;
    }
    let labels = crate::verdict_gate::label_names(doc)?;
    Some(Snapshot { labels, head_sha })
}

/// What the pre-write read says to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Add `loom:review-requested`, then remove `loom:changes-requested` and
    /// `loom:treating`.
    HandBack,
    /// Remove only `loom:treating`; these advanced labels are already on.
    AlreadyAdvanced(Vec<String>),
    /// `loom:treating` is gone: write nothing.
    ClaimLost,
    /// The head (this SHA) is not the pushed one: write no state label,
    /// remove only the claim.
    HeadMoved(String),
    /// Labels or head could not be read: write nothing.
    Unreadable,
}

fn same_sha(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    !a.is_empty() && a.eq_ignore_ascii_case(b)
}

/// Decide the hand-back from one read. Pure.
#[must_use]
pub fn decide(read: Option<&Snapshot>, expected_head: &str) -> Plan {
    let Some(snap) = read else {
        return Plan::Unreadable;
    };
    if !snap.has(CLAIM) {
        return Plan::ClaimLost;
    }
    if !same_sha(&snap.head_sha, expected_head) {
        return Plan::HeadMoved(snap.head_sha.clone());
    }
    let advanced: Vec<String> = ADVANCED
        .iter()
        .filter(|l| snap.has(l))
        .map(|l| (*l).to_string())
        .collect();
    if advanced.is_empty() {
        Plan::HandBack
    } else {
        Plan::AlreadyAdvanced(advanced)
    }
}

/// The forge calls [`run`] makes. Each returns whether the call succeeded.
pub trait Forge {
    /// A fresh (uncached) read of the PR's labels and head.
    fn read(&mut self) -> Option<Snapshot>;
    /// Add one label.
    fn add(&mut self, label: &str) -> bool;
    /// Remove one label (absent is not an error the caller relies on).
    fn remove(&mut self, label: &str) -> bool;
}

/// The result of one hand-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `loom:review-requested` on; `loom:changes-requested`/`loom:treating` off.
    HandedBack,
    /// The PR was already advanced; only the claim was released.
    AlreadyAdvanced(Vec<String>),
    /// The claim was gone; nothing written.
    ClaimLost,
    /// The head moved to this SHA; only the claim (if any) was released.
    HeadMoved(String),
    /// These labels landed during the write; the own add was withdrawn.
    Raced(Vec<String>),
    /// Fail closed, with why.
    Failed(String),
}

impl Outcome {
    /// The sentinel line and the exit code.
    #[must_use]
    pub fn render(&self) -> (String, i32) {
        match self {
            Self::HandedBack => (
                format!("{SENTINEL} HANDED-BACK {QUEUE} added; {CHANGES} and {CLAIM} removed"),
                0,
            ),
            Self::AlreadyAdvanced(on) => (
                format!(
                    "{SENTINEL} ALREADY-ADVANCED PR already carries {}; released {CLAIM} only, \
                     nothing else to do",
                    on.join(",")
                ),
                EXIT_ALREADY_ADVANCED,
            ),
            Self::ClaimLost => {
                (format!("{SENTINEL} CLAIM-LOST {CLAIM} is gone; wrote nothing"), EXIT_CLAIM_LOST)
            }
            Self::HeadMoved(sha) => (
                format!(
                    "{SENTINEL} HEAD-MOVED head is now {sha}, not the SHA you pushed; wrote no \
                     state label, released {CLAIM}"
                ),
                EXIT_HEAD_MOVED,
            ),
            Self::Raced(rivals) => (
                format!(
                    "{SENTINEL} RACED {} landed during the hand-back; withdrew own {QUEUE}",
                    rivals.join(",")
                ),
                EXIT_RACED,
            ),
            Self::Failed(why) => (format!("{SENTINEL} FAILED {why}"), EXIT_FAILED),
        }
    }
}

/// Remove only the claim, then confirm it is gone.
fn release_claim(forge: &mut impl Forge) -> Result<(), String> {
    forge.remove(CLAIM);
    match forge.read() {
        Some(s) if s.has(CLAIM) => Err(format!("{CLAIM} could not be removed")),
        Some(_) => Ok(()),
        None => Err(format!("the labels could not be re-read to confirm {CLAIM} was removed")),
    }
}

/// Read, decide, write add-first, re-read and verify.
pub fn run(forge: &mut impl Forge, expected_head: &str) -> Outcome {
    let pre = forge.read();
    match decide(pre.as_ref(), expected_head) {
        Plan::Unreadable => {
            Outcome::Failed("the PR's labels or head could not be read; wrote nothing".into())
        }
        Plan::ClaimLost => Outcome::ClaimLost,
        Plan::HeadMoved(current) => match release_claim(forge) {
            Ok(()) => Outcome::HeadMoved(current),
            Err(why) => Outcome::Failed(why),
        },
        Plan::AlreadyAdvanced(on) => match release_claim(forge) {
            Ok(()) => Outcome::AlreadyAdvanced(on),
            Err(why) => Outcome::Failed(why),
        },
        Plan::HandBack => {
            let Some(pre) = pre else {
                return Outcome::Failed("unreachable: no pre-read".into());
            };
            hand_back(forge, &pre)
        }
    }
}

fn hand_back(forge: &mut impl Forge, pre: &Snapshot) -> Outcome {
    // Add first: if it fails nothing is removed, so the PR keeps its
    // changes-requested/treating state instead of dropping out of every queue.
    if !forge.add(QUEUE) {
        return Outcome::Failed(format!(
            "{QUEUE} could not be added; {CHANGES} and {CLAIM} were left in place"
        ));
    }
    if pre.has(CHANGES) {
        forge.remove(CHANGES);
    }
    forge.remove(CLAIM);
    let Some(post) = forge.read() else {
        return Outcome::Failed("the labels could not be re-read to verify the hand-back".into());
    };
    let rivals: Vec<String> = RIVALS
        .iter()
        .filter(|l| !pre.has(l) && post.has(l))
        .map(|l| (*l).to_string())
        .collect();
    if !rivals.is_empty() && post.has(QUEUE) {
        // A review resolved or claimed the PR while we wrote: its label is the
        // lifecycle label, so withdrawing our add never leaves the PR empty.
        if !forge.remove(QUEUE) {
            return Outcome::Failed(format!(
                "{} landed during the hand-back and the own {QUEUE} could not be withdrawn",
                rivals.join(",")
            ));
        }
        return Outcome::Raced(rivals);
    }
    let mut problems = Vec::new();
    if !post.has(QUEUE) && rivals.is_empty() {
        problems.push(format!("missing {QUEUE}"));
    }
    for l in [CHANGES, CLAIM] {
        if post.has(l) && !rivals.iter().any(|r| r == l) {
            problems.push(format!("still carries {l}"));
        }
    }
    if !problems.is_empty() {
        return Outcome::Failed(format!("the hand-back did not hold: {}", problems.join("; ")));
    }
    if rivals.is_empty() {
        Outcome::HandedBack
    } else {
        // A rival landed and already displaced our add itself.
        Outcome::Raced(rivals)
    }
}

#[cfg(test)]
mod tests;
