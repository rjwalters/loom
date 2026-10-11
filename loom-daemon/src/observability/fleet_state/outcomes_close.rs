//! Stays an issue's close or a repo's removal from the fleet ends (#11398),
//! beside [`super::diff`]. Each ends in exactly one `eta.stage_outcome`.
//!
//! - **`ready_wait`**: a departure the issue's history shows as its close
//!   takes the issue-close classification ([`IssueFacts::exit`]) at that
//!   close instant. A failed issue read waits as a pending ready departure,
//!   with the same retries and cap as one the forge does not show yet.
//! - **`triage_wait` / `approval_wait`**: an issue gone from a repo's
//!   complete `loom:triage` or `loom:curated` listing, in neither listing nor
//!   any row now, is read once (`issues/{n}`). A close ends the pre-ready
//!   stay its history shows open at the close (an approved issue had none);
//!   an open issue was relabelled, which is no exit. A failed read is
//!   retried up to [`PENDING_ISSUE_MAX_PASSES`] passes, then ends as
//!   `unknown` at the pass that first saw it gone.
//! - **Repo removal**: a repo in the previous pass's managed set and not in
//!   this one keeps its stays (vanished rows, pending reads, its last
//!   curated / triage listings) until it has been out of a non-empty managed
//!   set for [`REPO_REMOVAL_CONFIRM_PASSES`] passes. An empty managed set
//!   is an enumeration that failed, never a removal, and a repo back before
//!   then was a blip: its stays return to the ordinary diff. Once the removal
//!   holds, each stay is read once (`issues/{n}`): a merge or known-unmerged
//!   close is `landed` / `abandoned` at the forge's `closed_at`; anything
//!   else (still open, unresolved close, failed read) is `unknown` at the
//!   pass that first saw it gone, with no forge instant.

use super::*;

/// Passes a repo must stay out of a non-empty managed set before its stays
/// end (about fifteen minutes at the collector's cadence), so a slug that
/// failed to resolve for a pass or two is not a removal.
pub const REPO_REMOVAL_CONFIRM_PASSES: u32 = 3;

/// The label of an issue awaiting curation.
pub const TRIAGE_LABEL: &str = "loom:triage";

/// An issue gone from a pre-ready listing, its exit not yet read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPreReady {
    /// The stage the listing it left implies, used when no history reads.
    pub stage: FleetStage,
    /// Its creation instant, from the listing.
    pub created: Option<DateTime<Utc>>,
    /// The pass that first saw it gone.
    pub since: DateTime<Utc>,
    /// Failed reads so far.
    pub passes: u32,
}

/// One stay of a repo gone from the fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoneStay {
    /// A `fleet.state` row.
    Row {
        row: Box<FleetStateRow>,
        source: Option<Source>,
        /// The pass that first saw it gone.
        since: DateTime<Utc>,
    },
    /// A curated or triage listing entry.
    PreReady(PendingPreReady),
}

/// A repo gone from the managed set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AbsentRepo {
    /// Passes it has been out of a non-empty managed set.
    pub passes: u32,
    /// The stays it held, by issue, not yet ended.
    pub stays: BTreeMap<u32, GoneStay>,
}

/// The stays this pass found of repos out of the fleet: vanished rows, and
/// the pending maps holding entries of such repos.
pub(super) struct Waiting<'a> {
    pub rows: Vec<(String, u32, FleetStateRow, Option<Source>)>,
    pub ready: &'a mut BTreeMap<(String, u32), PendingReady>,
    pub issue: &'a mut BTreeMap<(String, u32), PendingIssue>,
}

/// The pre-ready stage `history` shows open at `at`, with its entry when
/// known: `triage_wait` from the latest reopen (else `created`) until a
/// `loom:curated` / `loom:issue` label, then `approval_wait` from the latest
/// `loom:curated` label. `None` once a `loom:issue` label approved it.
fn open_pre_ready_stay(
    history: &LabelHistory,
    created: Option<DateTime<Utc>>,
    at: DateTime<Utc>,
) -> Option<(FleetStage, Option<DateTime<Utc>>)> {
    let entry = history
        .latest(at, |e| *e == HistoryEvent::Reopened)
        .or(created);
    let since_entry = |label: &str| {
        history
            .latest(at, |e| labeled(e, &[label]))
            .filter(|t| entry.is_none_or(|entry| *t >= entry))
    };
    match (since_entry(CURATED_LABEL), since_entry(READY_LABEL)) {
        (curated, Some(ready)) if curated.is_none_or(|c| ready >= c) => None,
        (Some(curated), _) => Some((FleetStage::ApprovalWait, Some(curated))),
        (None, _) => Some((FleetStage::TriageWait, entry)),
    }
}

impl<F: ForgeReads> Diff<'_, F> {
    /// The record of a `ready_wait` row that left, a close classified like
    /// any vanished row at the history's close instant. `None` while the
    /// forge shows no departure yet, or a read failed (retried as pending).
    pub(super) fn ready_exit(
        &mut self,
        repo: &str,
        issue: u32,
        was: &FleetStateRow,
        source: Option<Source>,
    ) -> Option<StageOutcomeRecord> {
        let left = match self.ready_departure(repo, issue, was)? {
            ReadyLeft::Moved(leaving) => {
                return Some(self.record(repo, issue, was, source, leaving))
            }
            ReadyLeft::Closed(left) => left,
        };
        let (exit, known) = self.forge.issue(repo, issue)?.exit();
        let leaving = Leaving {
            exit,
            next_stage: None,
            forge_at: known.map(|_| left),
        };
        let mut record = self.record(repo, issue, was, source, leaving);
        record.left_at = left.min(self.now);
        record.dwell_sec = dwell(record.exit, record.entered_at, record.left_at);
        Some(record)
    }

    /// The terminal record of pre-ready `stay`, ended by the close `facts`
    /// reads (`None`: no close known, so `unknown` at `stay.since`). `Err`
    /// when the history read failed and `retry`; `Ok(None)` when the history
    /// shows no pre-ready stay open (it was approved).
    fn pre_ready_end(
        &mut self,
        (repo, issue): (&str, u32),
        stay: &PendingPreReady,
        facts: Option<IssueFacts>,
        retry: bool,
    ) -> Result<Option<StageOutcomeRecord>, ()> {
        let (exit, forge_at) = facts.map_or((StageExit::Unknown, None), IssueFacts::exit);
        let now = self.now;
        let left_at = forge_at.unwrap_or(stay.since).min(now);
        let (stage, entered) = match self.history(repo, issue) {
            Some(history) => match open_pre_ready_stay(history, stay.created, left_at) {
                Some(open) => open,
                None => return Ok(None),
            },
            None if retry => return Err(()),
            None if stay.stage == FleetStage::TriageWait => (stay.stage, stay.created),
            None => (stay.stage, None),
        };
        let failed = matches!(self.histories.get(&(repo.to_string(), issue)), Some(None));
        Ok(Some(StageOutcomeRecord {
            repo: repo.to_string(),
            issue,
            pr_number: None,
            stage,
            entered_at: entered,
            entered_at_source: Some(if entered.is_some() {
                EnteredAtSource::Forge
            } else {
                EnteredAtSource::Unknown
            }),
            left_at,
            dwell_sec: dwell(exit, entered, left_at),
            exit,
            next_stage: None,
            event: FLEET_STATE_EVENT.to_string(),
            observed_at: now,
            resolution_sec: Some(if forge_at.is_some() {
                0
            } else {
                self.interval()
            }),
            forge_transition_at: forge_at,
            sweep_id: None,
            fact_id_missing_reason: forge_at.is_none().then_some(if failed {
                FactIdMissingReason::ForgeReadFailed
            } else {
                FactIdMissingReason::NoForgeInstant
            }),
            loom: self.loom.clone(),
        }))
    }

    /// The terminal record of `stay` of a repo whose removal holds: its
    /// issue's close when one reads, else `unknown` at the pass that first
    /// saw it gone.
    fn removed_exit(
        &mut self,
        repo: &str,
        issue: u32,
        stay: GoneStay,
    ) -> Option<StageOutcomeRecord> {
        let facts = self
            .forge
            .issue(repo, issue)
            .filter(|facts| *facts != IssueFacts::Open);
        match stay {
            GoneStay::Row { row, source, since } => {
                let (exit, forge_at) = facts.map_or((StageExit::Unknown, None), IssueFacts::exit);
                let leaving = Leaving {
                    exit,
                    next_stage: None,
                    forge_at,
                };
                let mut record = self.record(repo, issue, &row, source, leaving);
                if forge_at.is_none() {
                    record.left_at = since;
                    record.dwell_sec = dwell(record.exit, record.entered_at, since);
                }
                Some(record)
            }
            GoneStay::PreReady(stay) => self
                .pre_ready_end((repo, issue), &stay, facts, false)
                .ok()
                .flatten(),
        }
    }
}

/// End the pre-ready stays a close took out of this pass's complete curated
/// and triage listings, and retry earlier failed reads. Entries of a repo
/// out of the fleet are kept for [`track_removed`].
pub(super) fn pre_ready_departures<F: ForgeReads>(
    memory: &mut Memory,
    pass: &Pass<'_>,
    diff: &mut Diff<'_, F>,
    out: &mut Vec<TelemetryRecord>,
) {
    let curated_now = memory.curated_now.clone().unwrap_or_default();
    let triage_now = memory.triage_now.clone().unwrap_or_default();
    let mut gone = std::mem::take(&mut memory.pre_ready_gone);
    let lists = [
        (FleetStage::ApprovalWait, &memory.curated, &curated_now),
        (FleetStage::TriageWait, &memory.triage, &triage_now),
    ];
    for (stage, was, now) in lists {
        // Only a repo complete in both passes: a failed listing is no
        // evidence an issue left.
        for (repo, now_items) in now {
            let Some(was_items) = was.get(repo) else {
                continue;
            };
            for (&issue, &created) in was_items.iter().filter(|(n, _)| !now_items.contains_key(n)) {
                gone.entry((repo.clone(), issue))
                    .or_insert(PendingPreReady {
                        stage,
                        created,
                        since: pass.now,
                        passes: 0,
                    });
            }
        }
    }
    let listed = |repo: &str, issue: u32| {
        [&curated_now, &triage_now]
            .iter()
            .any(|l| l.get(repo).is_some_and(|items| items.contains_key(&issue)))
    };
    let row = |repo: &str, issue: u32| {
        pass.view
            .repos
            .get(repo)
            .is_some_and(|r| r.rows.contains_key(&issue))
    };
    let mut kept = BTreeMap::new();
    for ((repo, issue), stay) in gone {
        if !pass.managed.contains(&repo) {
            kept.insert((repo, issue), stay);
            continue;
        }
        if listed(&repo, issue) || row(&repo, issue) {
            continue;
        }
        let passes = stay.passes + 1;
        let last = passes >= PENDING_ISSUE_MAX_PASSES;
        let facts = match diff.forge.issue(&repo, issue) {
            Some(IssueFacts::Open) => continue,
            Some(facts) => Some(facts),
            None if !last => {
                kept.insert((repo, issue), PendingPreReady { passes, ..stay });
                continue;
            }
            None => None,
        };
        match diff.pre_ready_end((&repo, issue), &stay, facts, !last) {
            Err(()) => {
                kept.insert((repo, issue), PendingPreReady { passes, ..stay });
            }
            Ok(Some(record)) => {
                let key = (repo, issue, record.stage, record.left_at);
                if memory.pre_ready_done.insert(key) {
                    out.push(TelemetryRecord::StageOutcome(record));
                }
            }
            Ok(None) => {}
        }
    }
    memory.pre_ready_gone = kept;
}

/// Hold the stays of repos out of the fleet, return a blip's stays to the
/// diff, and end each stay of a repo whose removal holds exactly once.
pub(super) fn track_removed<F: ForgeReads>(
    memory: &mut Memory,
    pass: &Pass<'_>,
    diff: &mut Diff<'_, F>,
    waiting: Waiting<'_>,
    out: &mut Vec<TelemetryRecord>,
) {
    let Waiting { rows, ready, issue } = waiting;
    let mut absent = std::mem::take(&mut memory.absent);
    // A repo back in the fleet was a blip: a stay whose row is back goes on,
    // any other waits for its exit like a row that vanished this pass.
    let back: Vec<String> = absent
        .keys()
        .filter(|repo| pass.managed.contains(*repo))
        .cloned()
        .collect();
    for repo in back {
        let Some(blip) = absent.remove(&repo) else {
            continue;
        };
        for (number, stay) in blip.stays {
            let key = (repo.clone(), number);
            let shown = pass
                .view
                .repos
                .get(&repo)
                .is_some_and(|r| r.rows.contains_key(&number));
            match stay {
                GoneStay::Row { .. } if shown => {}
                GoneStay::Row { row, source, since } if row.stage == FleetStage::ReadyWait => {
                    let next = None;
                    ready.insert(
                        key,
                        PendingReady {
                            row: *row,
                            source,
                            next,
                            since,
                            passes: 0,
                        },
                    );
                }
                GoneStay::Row { row, source, since } => {
                    issue.insert(
                        key,
                        PendingIssue {
                            row: *row,
                            source,
                            since,
                            passes: 0,
                        },
                    );
                }
                GoneStay::PreReady(stay) => {
                    memory.pre_ready_gone.insert(key, stay);
                }
            }
        }
    }
    let new: Vec<String> = memory.managed.difference(pass.managed).cloned().collect();
    for repo in &new {
        absent.entry(repo.clone()).or_default();
    }
    let absorb = |absent: &mut BTreeMap<String, AbsentRepo>, repo: &str, n: u32, stay: GoneStay| {
        if let Some(a) = absent.get_mut(repo) {
            a.stays.entry(n).or_insert(stay);
        }
    };
    for (repo, n, row, source) in rows {
        let since = pass.now;
        let row = Box::new(row);
        absorb(&mut absent, &repo, n, GoneStay::Row { row, source, since });
    }
    // Pending entries of a repo out of the fleet move to its stays; one of
    // a repo never in the fleet is dropped, as before.
    ready.retain(|(repo, n), w| {
        if pass.managed.contains(repo) {
            return true;
        }
        let (row, source, since) = (Box::new(w.row.clone()), w.source, w.since);
        absorb(&mut absent, repo, *n, GoneStay::Row { row, source, since });
        false
    });
    issue.retain(|(repo, n), w| {
        if pass.managed.contains(repo) {
            return true;
        }
        let (row, source, since) = (Box::new(w.row.clone()), w.source, w.since);
        absorb(&mut absent, repo, *n, GoneStay::Row { row, source, since });
        false
    });
    memory.pre_ready_gone.retain(|(repo, n), w| {
        pass.managed.contains(repo) || {
            absorb(&mut absent, repo, *n, GoneStay::PreReady(w.clone()));
            false
        }
    });
    // The pre-ready stays its last complete listings held.
    for repo in &new {
        let lists = [
            (FleetStage::ApprovalWait, &memory.curated),
            (FleetStage::TriageWait, &memory.triage),
        ];
        for (stage, listing) in lists {
            for (&n, &created) in listing.get(repo).into_iter().flatten() {
                let since = pass.now;
                let stay = PendingPreReady {
                    stage,
                    created,
                    since,
                    passes: 0,
                };
                absorb(&mut absent, repo, n, GoneStay::PreReady(stay));
            }
        }
    }
    for (repo, mut gone) in absent {
        if !pass.managed.is_empty() {
            gone.passes = gone.passes.saturating_add(1);
        }
        if gone.passes >= REPO_REMOVAL_CONFIRM_PASSES {
            for (n, stay) in std::mem::take(&mut gone.stays) {
                if let Some(record) = diff.removed_exit(&repo, n, stay) {
                    out.push(TelemetryRecord::StageOutcome(record));
                }
            }
        }
        // Kept after it ends its stays, so a row that vanishes later ends too.
        memory.absent.insert(repo, gone);
    }
}
