//! `eta.stage_outcome` and `pr.resolved` from two consecutive `fleet.state`
//! passes (Issue #11126, #10196 R5). No ETA state is read.
//!
//! - **`eta.stage_outcome`**: a row whose stage changed between the previous
//!   view and this one, or that left the view, is one record.
//! - **`pr.resolved`**: a PR that left a repo's review listings between two
//!   complete listings of that repo is read once (`pulls/{n}`). A merged or
//!   closed PR is one record; a PR still open (its labels were removed) is
//!   none.
//!
//! Every host emits what it observes; nothing is elected. Duplicates across
//! hosts are expected, and `loom.fact_id` collapses them. The fact id is
//! keyed on forge instants only: a PR's `merged_at` / `closed_at`, and for a
//! review-label move the new label's `labeled` event (one events read per
//! move, at most [`LABEL_READ_BUDGET`] per pass). A move with no forge
//! instant (a sweep stage, an exhausted budget, a failed read) is still
//! emitted, without a fact id, at the observing pass with `resolution_sec`
//! set to the pass interval.
//!
//! A `ready_wait` row that leaves the view is not a record unless the repo's
//! ready listing was whole in both passes (never before #11139): the work
//! finder's listing can drop a row that is still ready. A row of a repo no
//! longer managed is not a record either.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::{FleetView, RepoListing, Source, MERGE_HOLD_LABELS, REVIEW_LABELS};
use crate::telemetry::kinds::fleet_state::{FleetStage, FleetStateRow};
use crate::telemetry::kinds::pr_resolved::{PrResolution, PrResolvedRecord};
use crate::telemetry::kinds::stage_outcome::{StageExit, StageOutcomeRecord, FLEET_STATE_EVENT};
use crate::telemetry::provenance::Provenance;
use crate::telemetry::TelemetryRecord;

/// Events reads for label-move instants, per pass.
pub const LABEL_READ_BUDGET: usize = 30;

/// A PR's state from one `pulls/{n}` read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullFacts {
    pub merged_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
}

/// The per-item forge reads, injected so the diff is testable offline.
pub trait ForgeReads {
    /// PR `number` in `repo`, or `None` when the read failed.
    fn pull(&mut self, repo: &str, number: u32) -> Option<PullFacts>;
    /// The latest `labeled` instant per label on `number`, or `None` when the
    /// read failed.
    fn label_times(&mut self, repo: &str, number: u32) -> Option<BTreeMap<String, DateTime<Utc>>>;
}

/// Open PR number → the issue its body links, per completely listed repo.
pub type Listed = BTreeMap<String, BTreeMap<u32, Option<u32>>>;

/// The listed PRs of this pass's complete listings.
#[must_use]
pub fn listed(listings: &[RepoListing]) -> Listed {
    listings
        .iter()
        .map(|l| {
            let prs = l.prs.iter().map(|pr| (pr.number, pr.issue)).collect();
            (l.repo.clone(), prs)
        })
        .collect()
}

/// What the previous pass left for the next diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Memory {
    /// Each repo's last complete review listing.
    pub listed: Listed,
    /// Departed PRs whose `pulls/{n}` read failed, per repo (number → issue),
    /// retried each pass until read.
    pub unread: Listed,
    /// PRs whose terminal stage outcome was emitted while a source still
    /// supplied their row (repo, PR number); the row's later stage changes or
    /// departure add no second one. Pruned once no row carries the PR.
    pub settled: BTreeSet<(String, u32)>,
    /// The previous view.
    pub view: Option<FleetView>,
    /// When the previous pass ran.
    pub at: Option<DateTime<Utc>>,
}

/// One pass's input to the diff.
pub struct Pass<'a> {
    pub view: &'a FleetView,
    /// This pass's complete review listings.
    pub listed: &'a Listed,
    /// Every managed repo.
    pub managed: &'a BTreeSet<String>,
    pub now: DateTime<Utc>,
}

/// The labels whose `labeled` event moves an open PR into `stage`.
fn stage_labels(stage: FleetStage) -> Vec<&'static str> {
    match stage {
        FleetStage::ReviewWait => vec![REVIEW_LABELS[0]],
        FleetStage::Doctor => vec![REVIEW_LABELS[1]],
        FleetStage::MergeWait => vec![REVIEW_LABELS[2]],
        FleetStage::MergeHold => MERGE_HOLD_LABELS.iter().copied().collect(),
        _ => Vec::new(),
    }
}

/// The terminal exit and forge instant a resolved PR gives its row, if the PR
/// merged or closed.
fn resolution_exit(facts: &PullFacts) -> Option<(StageExit, DateTime<Utc>)> {
    match (facts.merged_at, facts.closed_at) {
        (Some(at), _) => Some((StageExit::Landed, at)),
        (None, Some(at)) => Some((StageExit::CutShort, at)),
        (None, None) => None,
    }
}

/// How a row left its stage.
struct Leaving {
    exit: StageExit,
    next_stage: Option<FleetStage>,
    /// The forge's instant for the move, when known.
    forge_at: Option<DateTime<Utc>>,
}

struct Diff<'a, F> {
    forge: &'a mut F,
    loom: &'a Provenance,
    now: DateTime<Utc>,
    prev_at: DateTime<Utc>,
    budget: usize,
}

impl<F: ForgeReads> Diff<'_, F> {
    fn interval(&self) -> i64 {
        (self.now - self.prev_at).num_seconds().max(0)
    }

    /// The forge instant of `pr` entering `stage`, when it falls after the
    /// previous observation and no later than this one: the previous view
    /// had the row elsewhere, so an older `labeled` event (an earlier visit
    /// to the stage, even one inside the preceding interval) is not this move.
    fn label_instant(&mut self, repo: &str, pr: u32, stage: FleetStage) -> Option<DateTime<Utc>> {
        let labels = stage_labels(stage);
        if labels.is_empty() || self.budget == 0 {
            return None;
        }
        self.budget -= 1;
        let times = self.forge.label_times(repo, pr)?;
        let at = labels.iter().filter_map(|l| times.get(*l)).max().copied()?;
        (at > self.prev_at && at <= self.now).then_some(at)
    }

    fn record(
        &self,
        repo: &str,
        issue: u32,
        was: &FleetStateRow,
        source: Option<Source>,
        leaving: Leaving,
    ) -> StageOutcomeRecord {
        let Leaving {
            exit,
            next_stage,
            forge_at,
        } = leaving;
        let left_at = forge_at.unwrap_or(self.now).min(self.now);
        // Only a sweep's own checkpoint dates its entry. A polled row's
        // `entered_at` is the pass that first saw it: the entry fell somewhere
        // between the previous pass and that one, so it is not an instant.
        let entered_at =
            (source == Some(Source::Held) && !was.entered_at_lower_bound).then_some(was.entered_at);
        let completed = !matches!(exit, StageExit::CutShort | StageExit::Unknown);
        let dwell_sec = entered_at
            .filter(|_| completed)
            .map(|at| (left_at - at).num_seconds())
            .filter(|sec| *sec >= 0);
        StageOutcomeRecord {
            repo: repo.to_string(),
            issue,
            pr_number: was.pr,
            stage: was.stage,
            entered_at,
            left_at,
            dwell_sec,
            exit,
            next_stage,
            event: FLEET_STATE_EVENT.to_string(),
            observed_at: self.now,
            resolution_sec: Some(if forge_at.is_some() {
                0
            } else {
                self.interval()
            }),
            forge_transition_at: forge_at,
            loom: self.loom.clone(),
        }
    }
}

/// The records two consecutive passes give. Pure apart from `forge`.
#[must_use]
pub fn diff(
    memory: &mut Memory,
    pass: &Pass<'_>,
    forge: &mut impl ForgeReads,
    loom: &Provenance,
) -> Vec<TelemetryRecord> {
    let (Some(prev), Some(prev_at)) = (memory.view.clone(), memory.at) else {
        return Vec::new();
    };
    let prev = &prev;
    let mut diff = Diff {
        forge,
        loom,
        now: pass.now,
        prev_at,
        budget: LABEL_READ_BUDGET,
    };
    let mut out = Vec::new();

    let mut resolved: BTreeMap<(String, u32), PullFacts> = BTreeMap::new();
    let mut unread = std::mem::take(&mut memory.unread);
    unread.retain(|repo, _| pass.managed.contains(repo));
    for (repo, now_prs) in pass.listed {
        let Some(was_prs) = memory.listed.get(repo) else {
            continue;
        };
        // Departures of this pass, and earlier ones not yet read.
        let mut departed: BTreeMap<u32, Option<u32>> = unread.remove(repo).unwrap_or_default();
        departed.extend(was_prs.iter().map(|(&n, &issue)| (n, issue)));
        departed.retain(|n, _| !now_prs.contains_key(n));
        for (number, issue) in departed {
            let Some(facts) = diff.forge.pull(repo, number) else {
                log::debug!("fleet.state: pulls/{number} of {repo} unread; retrying next pass");
                unread
                    .entry(repo.clone())
                    .or_default()
                    .insert(number, issue);
                continue;
            };
            resolved.insert((repo.clone(), number), facts);
            let (state, at) = match (facts.merged_at, facts.closed_at) {
                (Some(merged), _) => (PrResolution::Merged, merged),
                (None, Some(closed)) => (PrResolution::Closed, closed),
                (None, None) => continue,
            };
            out.push(TelemetryRecord::PrResolved(PrResolvedRecord {
                repo: repo.clone(),
                pr_number: number,
                issue,
                state,
                resolved_at: at.min(pass.now),
                observed_at: pass.now,
                resolution_sec: 0,
                closed_at: facts.closed_at.or(facts.merged_at),
                loom: loom.clone(),
            }));
        }
    }
    memory.unread = unread;
    let mut settled = std::mem::take(&mut memory.settled);

    let empty = super::RepoView::default();
    for (repo, was_repo) in &prev.repos {
        let now_repo = pass.view.repos.get(repo).unwrap_or(&empty);
        for (&issue, was) in &was_repo.rows {
            if was
                .pr
                .is_some_and(|pr| settled.contains(&(repo.clone(), pr)))
            {
                continue;
            }
            // A PR that left the review listings and merged or closed ends
            // its row's stage even when another source (a held sweep, a stale
            // ready tick) still supplies the row: reconcile that before
            // treating the surviving row as an ordinary stage change.
            if let (Some(pr), true) = (was.pr, now_repo.rows.contains_key(&issue)) {
                if let Some((exit, at)) =
                    resolved.get(&(repo.clone(), pr)).and_then(resolution_exit)
                {
                    let leaving = Leaving {
                        exit,
                        next_stage: None,
                        forge_at: Some(at),
                    };
                    let source = was_repo.sources.get(&issue).copied();
                    out.push(TelemetryRecord::StageOutcome(
                        diff.record(repo, issue, was, source, leaving),
                    ));
                    settled.insert((repo.clone(), pr));
                    continue;
                }
            }
            match now_repo.rows.get(&issue) {
                Some(now) if now.stage == was.stage => {}
                Some(now) => {
                    // A hold release is the removal of the hold label; `loom:pr`
                    // stays applied, so no `labeled` event dates it.
                    let released =
                        was.stage == FleetStage::MergeHold && now.stage == FleetStage::MergeWait;
                    let forge_at = match (now_repo.sources.get(&issue), now.pr) {
                        (Some(Source::Review), Some(pr)) if !released => {
                            diff.label_instant(repo, pr, now.stage)
                        }
                        _ => None,
                    };
                    let leaving = Leaving {
                        exit: StageExit::between(was.stage, now.stage),
                        next_stage: Some(now.stage),
                        forge_at,
                    };
                    let source = was_repo.sources.get(&issue).copied();
                    out.push(TelemetryRecord::StageOutcome(
                        diff.record(repo, issue, was, source, leaving),
                    ));
                }
                None => {
                    if !pass.managed.contains(repo) {
                        continue;
                    }
                    if was.stage == FleetStage::ReadyWait
                        && !(prev.ready_complete(repo) && pass.view.ready_complete(repo))
                    {
                        continue;
                    }
                    let facts = was.pr.and_then(|pr| resolved.get(&(repo.clone(), pr)));
                    let (exit, forge_at) = match facts {
                        Some(PullFacts {
                            merged_at: Some(at),
                            ..
                        }) => (StageExit::Landed, Some(*at)),
                        Some(PullFacts {
                            closed_at: Some(at),
                            ..
                        }) => (StageExit::CutShort, Some(*at)),
                        Some(_) => (StageExit::CutShort, None),
                        None => (StageExit::Unknown, None),
                    };
                    let leaving = Leaving {
                        exit,
                        next_stage: None,
                        forge_at,
                    };
                    let source = was_repo.sources.get(&issue).copied();
                    out.push(TelemetryRecord::StageOutcome(
                        diff.record(repo, issue, was, source, leaving),
                    ));
                }
            }
        }
    }
    settled.retain(|(repo, pr)| {
        pass.view
            .repos
            .get(repo)
            .is_some_and(|v| v.rows.values().any(|row| row.pr == Some(*pr)))
    });
    memory.settled = settled;
    out
}

/// The next pass's memory: this pass's view and time, and each repo's latest
/// complete listing (a repo whose listing failed keeps its earlier one).
#[must_use]
pub fn remember(memory: Memory, pass: &Pass<'_>) -> Memory {
    let mut listed = memory.listed;
    listed.retain(|repo, _| pass.managed.contains(repo));
    listed.extend(pass.listed.iter().map(|(k, v)| (k.clone(), v.clone())));
    Memory {
        listed,
        unread: memory.unread,
        settled: memory.settled,
        view: Some(pass.view.clone()),
        at: Some(pass.now),
    }
}

/// Offer each record with valid provenance to `sink`; returns how many were
/// offered.
pub fn offer(records: Vec<TelemetryRecord>, sink: &super::FleetStateSink) -> usize {
    let mut offered = 0;
    for record in records {
        let valid = match &record {
            TelemetryRecord::PrResolved(r) => r.has_provenance(),
            TelemetryRecord::StageOutcome(r) => r.has_provenance(),
            _ => false,
        };
        if !valid {
            log::warn!("fleet.state: dropped a {} record: invalid provenance", record.kind());
            continue;
        }
        log::debug!("fleet.state: emit {}", record.kind());
        sink.offer(record);
        offered += 1;
    }
    offered
}

/// The live reads, through `gh api` in each repo's own checkout.
pub struct GhForgeReads {
    pub roots: BTreeMap<String, PathBuf>,
}

fn parse_instant(raw: Option<&str>) -> Option<DateTime<Utc>> {
    raw.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

impl GhForgeReads {
    fn root(&self, repo: &str) -> Option<&Path> {
        self.roots.get(repo).map(PathBuf::as_path)
    }
}

impl ForgeReads for GhForgeReads {
    fn pull(&mut self, repo: &str, number: u32) -> Option<PullFacts> {
        let pull = crate::observability::ops::stage_dwell::gh_json(
            self.root(repo)?,
            &format!("repos/{repo}/pulls/{number}"),
        )?;
        Some(PullFacts {
            merged_at: parse_instant(pull["merged_at"].as_str()),
            closed_at: parse_instant(pull["closed_at"].as_str()),
        })
    }

    fn label_times(&mut self, repo: &str, number: u32) -> Option<BTreeMap<String, DateTime<Utc>>> {
        let events = crate::observability::ops::stage_dwell::gh_json(
            self.root(repo)?,
            &format!("repos/{repo}/issues/{number}/events?per_page=100"),
        )?;
        Some(crate::observability::ops::stage_dwell::parse_label_times(&events))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "outcomes_tests.rs"]
mod tests;
