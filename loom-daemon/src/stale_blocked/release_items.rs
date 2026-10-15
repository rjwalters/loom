//! Per-artifact verdicts of the `loom:blocked` release pass (#10752).
//!
//! [`Report`] used to keep counts only: `skipped{no-park-record=3}` named no
//! artifact, and `still_blocked` named neither the artifact nor the blocker
//! holding it. Each artifact the pass lists now also gets exactly **one**
//! [`Item`]: its verdict, the closed-set reason, every declared blocker with
//! the state this pass read for it, and the labels a release changes. This is
//! what `pass.verdict` exports ([`super::release_telemetry`]) and what
//! `release-stale-blocked --json` lists under `items`.
//!
//! The pass records what it needs as it goes: [`Report::listed`] once per
//! listed row (kind and declared blockers, from the body it already holds) and
//! [`Report::blocker_read`] once per blocker state it reads. Neither makes a
//! forge call.

use std::collections::HashMap;

use serde::Serialize;

use super::batch::RefState;
use super::budget::{Bucket, FloorStop};
use super::release::{Acted, Report, Unread};
use crate::forge_listing::RestIssue;
use crate::park_record::apply::BLOCKED_LABEL;
use crate::park_record::{blockers, BlockerRef};

/// At most this many blockers are kept per item.
const MAX_BLOCKERS: usize = 20;
/// `forge_call_stats::sanitize` caps a field at this many characters; a
/// detail that reaches it was cut (#12036).
const SANITIZE_CAP: usize = 96;

/// Why an artifact went [`ItemVerdict::Unevaluated`], typed where the cause
/// is created so no consumer parses `detail` (#12036).
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

/// What the pass decided for one artifact. Every listed artifact gets exactly
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemVerdict {
    /// Every declared blocker resolved: `loom:blocked` removed (planned,
    /// under dry-run).
    Released,
    /// Some declared blockers resolved: their records dropped from the body.
    Reparked,
    /// No declared blocker has resolved.
    StillBlocked,
    /// Left alone for the [`super::release::Skip`] in `reason`.
    Skipped,
    /// Could not be evaluated this pass (a read failed, the budget floor).
    Unevaluated,
    /// A release or re-park write failed.
    Failed,
}

impl ItemVerdict {
    /// Every verdict, in a stable order.
    pub const ALL: [Self; 6] = [
        Self::Released,
        Self::Reparked,
        Self::StillBlocked,
        Self::Skipped,
        Self::Unevaluated,
        Self::Failed,
    ];

    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Released => "released",
            Self::Reparked => "reparked",
            Self::StillBlocked => "still_blocked",
            Self::Skipped => "skipped",
            Self::Unevaluated => "unevaluated",
            Self::Failed => "failed",
        }
    }
}

/// The state a blocker was read in. `not_read` when this pass never read it
/// (a body-only skip, a cross-repo blocker, a read cut short).
pub mod blocker_state {
    pub const OPEN: &str = "open";
    pub const CLOSED: &str = "closed";
    pub const MERGED: &str = "merged";
    /// A PR closed without merging: never a resolved blocker.
    pub const CLOSED_UNMERGED: &str = "closed_unmerged";
    /// The read failed.
    pub const UNREAD: &str = "unread";
    pub const NOT_READ: &str = "not_read";
}

/// One declared blocker and its state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockerCheck {
    /// `#12`, or `owner/repo#12`.
    #[serde(rename = "ref")]
    pub reference: String,
    /// One of [`blocker_state`].
    pub state: &'static str,
}

/// One artifact's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub number: u64,
    /// `issue` or `pr`.
    pub artifact: &'static str,
    pub verdict: ItemVerdict,
    /// The skip reason ([`super::release::Skip::key`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Why it went unevaluated, or why a write failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Why it went unevaluated (#12036).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unevaluated_reason: Option<UnevaluatedReason>,
    /// The budget reading behind a floor stop (#12036).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetReading>,
    pub blockers: Vec<BlockerCheck>,
    /// Labels added (a release's restored lane label).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels_added: Vec<String>,
    /// Labels removed (a release's `loom:blocked`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels_removed: Vec<String>,
    /// Every write landed (always `false` under dry-run).
    pub applied: bool,
}

/// What the pass learned about each artifact before deciding it. Not
/// serialized: [`Item`] carries the result.
#[derive(Debug, Clone, Default)]
pub struct ItemContext {
    listed: HashMap<u64, Listed>,
    states: HashMap<u64, &'static str>,
}

#[derive(Debug, Clone)]
struct Listed {
    pr: bool,
    declared: Vec<BlockerRef>,
}

/// The [`blocker_state`] a [`RefState`] read means.
#[must_use]
pub fn state_of<E>(read: &Result<RefState, E>) -> &'static str {
    match read {
        Err(_) => blocker_state::UNREAD,
        Ok(s) => match (s.state.as_str(), s.is_pr) {
            ("MERGED", _) => blocker_state::MERGED,
            ("CLOSED", true) => blocker_state::CLOSED_UNMERGED,
            ("CLOSED", false) => blocker_state::CLOSED,
            _ => blocker_state::OPEN,
        },
    }
}

/// The text as `forge_call_stats::sanitize` leaves it, but never cut
/// mid-word: a cut detail backs off to the last whitespace and ends `...`.
fn detail(why: &str) -> Option<String> {
    let d = crate::forge_call_stats::sanitize(why)?;
    let ascii = why
        .trim()
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .count();
    let n = d.chars().count();
    if n + 1 < SANITIZE_CAP || ascii <= n {
        return Some(d);
    }
    Some(match d.rfind(' ') {
        Some(i) => format!("{}...", d[..i].trim_end()),
        None => format!("{d}..."),
    })
}

/// [`detail`] for the telemetry tests.
#[cfg(test)]
pub(super) fn detail_for_test(why: &str) -> Option<String> {
    detail(why)
}

impl Report {
    /// Note a listed row's kind and declared blockers (#10752).
    pub(super) fn listed(&mut self, row: &RestIssue) {
        let body = row.body.as_deref().unwrap_or_default();
        self.context.listed.insert(
            u64::from(row.number),
            Listed {
                pr: row.is_pull_request,
                declared: blockers(body),
            },
        );
    }

    /// Note the state read for declared blocker `number`.
    pub(super) fn blocker_read<E>(&mut self, number: u64, read: &Result<RefState, E>) {
        self.context.states.insert(number, state_of(read));
    }

    /// Record `number`'s one verdict.
    pub(super) fn decide(
        &mut self,
        number: u64,
        verdict: ItemVerdict,
        reason: Option<String>,
        why: Option<&str>,
    ) {
        let listed = self.context.listed.get(&number);
        let blockers = listed
            .map(|l| {
                l.declared
                    .iter()
                    .take(MAX_BLOCKERS)
                    .map(|b| match &b.repo {
                        None => BlockerCheck {
                            reference: format!("#{}", b.number),
                            state: self
                                .context
                                .states
                                .get(&b.number)
                                .copied()
                                .unwrap_or(blocker_state::NOT_READ),
                        },
                        Some(repo) => BlockerCheck {
                            reference: format!("{repo}#{}", b.number),
                            state: blocker_state::NOT_READ,
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.items.push(Item {
            number,
            artifact: if listed.is_some_and(|l| l.pr) {
                "pr"
            } else {
                "issue"
            },
            verdict,
            reason,
            detail: why.and_then(detail),
            unevaluated_reason: None,
            budget: None,
            blockers,
            labels_added: Vec::new(),
            labels_removed: Vec::new(),
            applied: false,
        });
    }

    /// `number` has no resolved declared blocker.
    pub(super) fn hold(&mut self, number: u64) {
        self.still_blocked += 1;
        self.decide(number, ItemVerdict::StillBlocked, None, None);
    }

    /// A release (`release`) or re-park was planned or written.
    pub(super) fn acted(&mut self, release: bool, acted: Acted) {
        let verdict = if release {
            ItemVerdict::Released
        } else {
            ItemVerdict::Reparked
        };
        self.decide(acted.number, verdict, None, None);
        if let Some(item) = self.items.last_mut() {
            item.applied = acted.applied;
            if release {
                item.labels_added = acted.restored.iter().cloned().collect();
                item.labels_removed = vec![BLOCKED_LABEL.to_string()];
            }
        }
        if release {
            self.released.push(acted);
        } else {
            self.reparked.push(acted);
        }
    }

    /// A write for `number` failed.
    pub(super) fn fail(&mut self, number: u64, why: String) {
        self.decide(number, ItemVerdict::Failed, None, Some(&why));
        self.failed.push(Unread { number, why });
    }
}
