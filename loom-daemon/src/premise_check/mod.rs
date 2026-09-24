//! The premise gate (#8396): does this issue's premise survive contact with
//! the codebase, and does changing the reported behaviour need a human ruling?
//!
//! # Why this exists, one stage before Curator
//!
//! Loom's automated lifecycle begins at Curator, whose job is *enrichment* —
//! it is structurally biased toward making whatever is filed buildable. No
//! stage before it asks "should this be built at all, and is the premise
//! true?".
//!
//! On #7855 that gap cost a full pass. The issue proposed reversing a
//! documented safety posture — no automatic kill/restart of a wedged-but-alive
//! daemon, stated in `loom-daemon/src/watchdog/mod.rs`'s CONFIRMED-branch hang
//! report ("No automatic kill/restart is attempted (#4398 — there is no
//! provably-safe unattended remediation for a wedged-but-alive process)"),
//! elaborated in `loom-daemon/src/watchdog/help.txt`, and asserted as intended
//! by `defaults/scripts/tests/test-loom-daemon-watchdog.sh`. A Curator pass
//! noticed the reversal, wrote "the filing is the ruling" in its own rescoping
//! comment, and enriched it anyway. Champion caught it one stage later.
//!
//! `curator.md` already carried a "Checking Operator-Only Premises" section,
//! and #8309 has since added an explicit "an autonomous filing is never
//! operator approval" rule to it. Neither is a mechanism: the #7855 Curator
//! had the first one in the same file it was reading and reasoned past it.
//!
//! # What is mechanical here, and what deliberately is not
//!
//! Deciding whether a behaviour is deliberate requires reading code. That is
//! judgement, and this module does not pretend to automate it. What it
//! mechanises is the part that failed on #7855:
//!
//! 1. **Scope** ([`scope`]) — is this issue in the gated population? Fully
//!    mechanical: labels, an incident-report heading vocabulary, and an
//!    explicit reversal-claim vocabulary.
//! 2. **The record** ([`record`]) — a `<!-- loom:premise-check … -->` marker
//!    whose *internal consistency* is enforced. The load-bearing rule is that
//!    `deliberate=yes reversal=yes` **cannot** carry `verdict=clear`: an agent
//!    may not write down "this is deliberate and I am reversing it" and also
//!    "proceed to enrichment". #7855's own reasoning is unexpressible.
//! 3. **Evidence candidates** ([`evidence`]) — an advisory scan that hands the
//!    agent the files where an intent assertion co-occurs with the issue's own
//!    anchors, so "I searched and found nothing" is a claim made against a
//!    list rather than against a blank page.
//!
//! The scan is tuned for recall and is explicitly **not** the enforcement
//! (#7979: literal-text matching over prose is brittle — it fails on
//! rewording and on relocation). The enforcement is (1) and (2), which are
//! structural and do not read prose at all.
//!
//! # Asymmetry of the two error directions
//!
//! A false negative reproduces #7855: a documented decision is reversed by an
//! automated pipeline with no human in the loop, and the reversal is only
//! visible after it ships. A false positive costs one comment recording a
//! premise check that concludes "nothing deliberate here, proceed". Those
//! costs are nowhere near symmetric, so the vocabularies below keep genuinely
//! ambiguous phrasings and the gate fails **closed** on every error path.

pub mod cli;
pub mod evidence;
pub mod record;
pub mod scope;

pub use record::{Outcome, Record};
pub use scope::Trigger;

/// Exit codes. These are contract: `defaults/scripts/premise-check.sh` is a
/// thin stub, and `curator.md` / `sweep-wave-lifecycle.md` branch on them.
pub mod exit {
    /// Out of the gated population, or a consistent `verdict=clear` record.
    /// Curator proceeds exactly as it does today.
    pub const PROCEED: i32 = 0;
    /// Usage / precondition failure. **Fails closed**: a caller must treat
    /// this like [`RECORD_REQUIRED`], never like [`PROCEED`] — "the gate could
    /// not run" is not "the premise checks out".
    pub const ERROR: i32 = 1;
    /// In scope, no record. Enrichment must not start: perform the premise
    /// check and post the record first.
    pub const RECORD_REQUIRED: i32 = 10;
    /// A consistent record routing the issue to a human:
    /// `loom:operator-only` + `loom:operator-decision`, no enrichment pass.
    pub const ROUTE_OPERATOR: i32 = 11;
    /// A record exists but fails a structural rule. Same action as
    /// [`RECORD_REQUIRED`] — an inconsistent record is not a record.
    pub const RECORD_MALFORMED: i32 = 12;
    /// The record says the reported behaviour does not exist as described.
    /// Curator closes or rescopes (CLAUDE.md § "Issues Are Suggestions")
    /// rather than enriching a false premise.
    pub const PREMISE_FALSE: i32 = 13;
}

/// Everything the gate reads about one issue, already fetched.
///
/// Kept separate from the forge so every decision is testable without one —
/// the hermetic mode of the CLI builds this straight from files.
#[derive(Debug, Default)]
pub struct Inputs {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    /// Comment bodies, oldest first. Each is searched for the record marker
    /// independently: companion `premise-evidence:` lines must live in the
    /// *same* comment as the marker they support.
    pub comments: Vec<String>,
}

/// The gate's whole answer for one issue.
#[derive(Debug)]
pub struct Decision {
    pub trigger: Option<Trigger>,
    pub record: Option<Record>,
    pub outcome: Outcome,
    pub exit_code: i32,
}

/// Run stages 1 and 2. The evidence scan is the caller's, because it needs a
/// repo root and is skippable (`--no-scan`).
///
/// `content_root` is the **working tree the citations are resolved against**,
/// not the shared clone root: a citation is a claim about file content, which
/// differs per worktree (issue #8499). See
/// [`crate::repo_root::find_worktree_root`].
#[must_use]
pub fn decide(inputs: &Inputs, content_root: &std::path::Path) -> Decision {
    let Some(trigger) = scope::trigger(&inputs.title, &inputs.body, &inputs.labels) else {
        return Decision {
            trigger: None,
            record: None,
            outcome: Outcome::OutOfScope,
            exit_code: exit::PROCEED,
        };
    };

    // Body first, then comments oldest-first; the LAST chunk carrying a marker
    // wins. An issue accumulates passes, and the current premise check is the
    // most recent one — taking the first would re-litigate a superseded read.
    let mut chunks: Vec<&str> = Vec::with_capacity(inputs.comments.len() + 1);
    chunks.push(inputs.body.as_str());
    chunks.extend(inputs.comments.iter().map(String::as_str));

    let Some(chunk) = record::last_chunk_with_marker(&chunks) else {
        return Decision {
            trigger: Some(trigger),
            record: None,
            outcome: Outcome::RecordRequired,
            exit_code: exit::RECORD_REQUIRED,
        };
    };

    match record::parse(chunk) {
        Err(why) => Decision {
            trigger: Some(trigger),
            record: None,
            outcome: Outcome::Malformed(why),
            exit_code: exit::RECORD_MALFORMED,
        },
        Ok(rec) => {
            let outcome = record::check(&rec, content_root);
            let exit_code = match outcome {
                Outcome::Proceed => exit::PROCEED,
                Outcome::RouteOperator => exit::ROUTE_OPERATOR,
                Outcome::PremiseFalse => exit::PREMISE_FALSE,
                Outcome::Malformed(_) => exit::RECORD_MALFORMED,
                // Unreachable here (scope already matched), but mapped rather
                // than panicking: a gate that aborts is a gate that fails open
                // in every caller that only checks for a zero exit.
                Outcome::OutOfScope | Outcome::RecordRequired => exit::RECORD_REQUIRED,
            };
            Decision {
                trigger: Some(trigger),
                record: Some(rec),
                outcome,
                exit_code,
            }
        }
    }
}
