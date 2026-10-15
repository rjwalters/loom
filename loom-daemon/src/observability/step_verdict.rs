//! A role's per-item verdicts as `pass.verdict` facts (#12027): what a role
//! step or role tick decided on each item, read from what the run already
//! wrote, emitted by the daemon when it drains the run's pick journal.
//!
//! # Capture and emit
//!
//! Nothing here calls the forge. The agent side journals the writes it was
//! making anyway ([`super::pick_journal`]): the gh front its `merged` /
//! `promoted` acts, `loom-daemon forge comment` the markers a comment opens
//! with (`JournalEntry::Marker`), and `park-record apply` /
//! `operator-decision apply` their own writes (`JournalEntry::Applied`).
//! The daemon drains the journal where the role tick's `pick.decision` does
//! (`pick_decision::emit_role_tick`), for a role-runner tick and for a step's
//! end alike; a step also has its session log, whose last `CHAMPION-STEP`
//! line names the step's outcome.
//!
//! # Projections
//!
//! A role registers a projection ([`projector`]): the evidence about one
//! item to a closed, versioned `(class, reason)`: `champion` ([`champion`],
//! #12027) and `judge` ([`judge`], #12029).
//!
//! # The ledger's view, record-only
//!
//! A step record also carries the outcome ledger's own classification
//! (`ledger_class` / `ledger_reason`): `outcome_ledger::parse_outcome_line`
//! and `classify`, both pure, are called here and nowhere else in
//! production. Nothing the ledger *does* is applied: no effect, no cooldown,
//! no broadcast. The two can disagree (a red-CI rejection is `refused
//! ci_failing` here and `failed unclassified` to the ledger, which reads an
//! unchanged item with a verdict line as a failure), and the record shows
//! both. The post-state comes from the run's own journaled writes (moved =
//! any write on the item); with no readable journal, or writes the front
//! could not see, it is `None` (`failed probe_failed`), never a default.
//! `closed` and `claim_held` are not observable from the journal and read
//! `false`.
//!
//! # Volume
//!
//! A step end emits one record, no-op included. A role-runner tick emits one
//! record per item it wrote a projected marker or act on, nothing else: a
//! duplicate refusal posts no comment, so a tick's records are change-only.
//!
//! # Leakage
//!
//! A record carries only what was knowable at `evaluated_at` (the step's end,
//! or the time of the write it projects); `first_refused_at` is never later.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::pick_journal::{Journal, JournalEntry};
use crate::step_dispatch::outcome_ledger;
use crate::step_role::StepRole;
use crate::telemetry::kinds::pass::{PassMode, PassVerdictRecord, StepFacts};
use crate::telemetry::provenance::Provenance;

pub mod champion;
pub mod judge;
pub mod refusals;

use refusals::{Observed, RefusalStore};

/// A step session's own section of its log is read up to this many bytes
/// from its end.
const MAX_LOG_TAIL: usize = 256 * 1024;

/// One thing the run wrote on an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A comment marker (`forge comment`).
    Marker {
        at: DateTime<Utc>,
        marker: String,
        head_sha: Option<String>,
    },
    /// A `gh` write the agent front journaled.
    Act { at: DateTime<Utc>, action: String },
    /// A daemon subcommand's own write.
    Applied {
        at: DateTime<Utc>,
        action: String,
        by: String,
    },
}

/// Everything known about one item of one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evidence {
    pub number: u32,
    pub is_pr: bool,
    /// In journal order.
    pub events: Vec<Event>,
    /// The step's outcome word, verbatim (step path only).
    pub outcome: Option<String>,
    /// A step end (always projected), not a role-runner tick.
    pub step_path: bool,
    /// The item's labels as the run's listings served them.
    pub pre_labels: Vec<String>,
}

/// What the projection asks of the refusal store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StoreOp {
    #[default]
    None,
    /// A refusal marker: record its identity.
    Observe,
    /// A refusal with no marker this run: the standing one names it.
    Lookup,
    /// The refusal no longer stands (merged, head moved).
    Clear,
}

/// One item's projected verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Projection {
    pub class: &'static str,
    pub reason: String,
    pub reason_set: &'static str,
    pub criterion_key: Option<String>,
    pub reason_key: Option<String>,
    pub head_sha: Option<String>,
    pub body_hash: Option<String>,
    /// When the projected write was made.
    pub at: Option<DateTime<Utc>>,
    /// The verdict rests on a write that landed.
    pub wrote: bool,
    pub store: StoreOp,
}

/// A role's projection (#12027 registers Champion; #12029 adds Judge).
#[derive(Debug, Clone, Copy)]
pub struct Projector {
    /// The `step` value for an item of this kind.
    pub step_name: fn(is_pr: bool) -> &'static str,
    pub project: fn(&Evidence) -> Option<Projection>,
    /// The step's outcome word from its log text.
    pub outcome_word: fn(text: &str, is_pr: bool, item: u32) -> Option<String>,
}

/// The projection registered for `role`, if any.
#[must_use]
pub fn projector(role: &str) -> Option<Projector> {
    match role {
        "champion" => Some(champion::PROJECTOR),
        "judge" => Some(judge::PROJECTOR),
        _ => None,
    }
}

/// The step whose end drains a journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSource {
    pub sweep_id: String,
    pub step: StepRole,
    pub item: u32,
    /// The session's own section of its log, `None` when unreadable.
    pub log_text: Option<String>,
}

/// What a drained journal belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Step(StepSource),
    Tick { started_at: DateTime<Utc> },
}

/// A step staged for its end's drain: its log is read on the drain thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedStep {
    pub sweep_id: String,
    pub step: StepRole,
    pub item: u32,
    pub log: PathBuf,
}

impl StagedStep {
    /// Read the step's own section of its log: after its dispatch header
    /// (`sweep_id=<id> `), its last [`MAX_LOG_TAIL`] bytes.
    #[must_use]
    pub fn read(self) -> StepSource {
        let log_text = std::fs::read(&self.log).ok().and_then(|bytes| {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let start = text.rfind(&format!("sweep_id={} ", self.sweep_id))?;
            let section = &text[start..];
            let mut cut = section.len().saturating_sub(MAX_LOG_TAIL);
            while !section.is_char_boundary(cut) {
                cut += 1;
            }
            Some(section[cut..].to_string())
        });
        StepSource {
            sweep_id: self.sweep_id,
            step: self.step,
            item: self.item,
            log_text,
        }
    }
}

thread_local! {
    static STAGED: RefCell<Option<StagedStep>> = const { RefCell::new(None) };
}

/// Stage `step` for the drain about to run on this thread (a step's end).
pub fn stage_step(step: StagedStep) {
    STAGED.with(|s| *s.borrow_mut() = Some(step));
}

/// Take the step staged on this thread, if any.
pub fn take_staged_step() -> Option<StagedStep> {
    STAGED.with(|s| s.borrow_mut().take())
}

/// Where and when the records are being built.
#[derive(Debug, Clone)]
pub struct Context<'a> {
    pub role: &'a str,
    pub repo: &'a str,
    pub host: &'a str,
    pub now: DateTime<Utc>,
}

fn evaluating_rows(journal: &Journal) -> BTreeMap<u32, Vec<String>> {
    let mut labels = BTreeMap::new();
    for entry in &journal.entries {
        let rows = match entry {
            JournalEntry::Queue { rows, .. } | JournalEntry::Listing { rows, .. } => rows,
            _ => continue,
        };
        for row in rows {
            labels.insert(row.number, row.labels.clone());
        }
    }
    labels
}

/// `(number, is_pr when the entry says, event)` of each write entry.
fn events(journal: &Journal) -> Vec<(u32, Option<bool>, Event)> {
    journal
        .entries
        .iter()
        .filter_map(|entry| match entry {
            JournalEntry::Marker {
                at,
                number,
                is_pr,
                marker,
                head_sha,
            } => Some((
                *number,
                Some(*is_pr),
                Event::Marker {
                    at: *at,
                    marker: marker.clone(),
                    head_sha: head_sha.clone(),
                },
            )),
            JournalEntry::Act { at, number, action } => {
                let is_pr = match action.as_str() {
                    "merged" => Some(true),
                    "promoted" => Some(false),
                    _ => None,
                };
                Some((
                    *number,
                    is_pr,
                    Event::Act {
                        at: *at,
                        action: action.clone(),
                    },
                ))
            }
            JournalEntry::Applied {
                at,
                number,
                is_pr,
                action,
                by,
            } => Some((
                *number,
                Some(*is_pr),
                Event::Applied {
                    at: *at,
                    action: action.clone(),
                    by: by.clone(),
                },
            )),
            JournalEntry::Queue { .. } | JournalEntry::Listing { .. } => None,
        })
        .collect()
}

/// The ledger's post-state for `item` from the run's own writes: `None`
/// when the journal was not read or the agent's writes were not observable.
fn post_state(journal: Option<&Journal>, item: u32) -> Option<outcome_ledger::PostState> {
    let journal = journal.filter(|j| j.read)?;
    let all = events(journal);
    let observable =
        journal.front_expected || all.iter().any(|(_, _, e)| matches!(e, Event::Act { .. }));
    if !observable {
        return None;
    }
    Some(outcome_ledger::PostState {
        moved: all
            .iter()
            .any(|(n, _, e)| *n == item && !matches!(e, Event::Marker { .. })),
        closed: false,
        claim_held: false,
    })
}

fn derived(parts: &[&str]) -> String {
    crate::telemetry::trace::derived_hex(parts, 32)
}

/// The records for one drained run. Pure apart from `store`, which the
/// projections' refusal identities update.
#[must_use]
pub fn records_for(
    ctx: &Context<'_>,
    source: &Source,
    journal: Option<&Journal>,
    store: &mut RefusalStore,
) -> Vec<PassVerdictRecord> {
    let Some(projector) = projector(ctx.role) else {
        return Vec::new();
    };
    let empty = Journal::default();
    let read = journal.unwrap_or(&empty);
    let labels = evaluating_rows(read);
    let all = events(read);
    match source {
        Source::Step(step) => {
            if step.step.role_name() != ctx.role {
                return Vec::new();
            }
            let is_pr = step.step.acts_on_pr();
            let text = step.log_text.as_deref().unwrap_or_default();
            let evidence = Evidence {
                number: step.item,
                is_pr,
                events: all
                    .into_iter()
                    .filter(|(n, _, _)| *n == step.item)
                    .map(|(_, _, e)| e)
                    .collect(),
                outcome: (projector.outcome_word)(text, is_pr, step.item),
                step_path: true,
                pre_labels: labels.get(&step.item).cloned().unwrap_or_default(),
            };
            let line = outcome_ledger::parse_outcome_line(text, step.step, step.item);
            let ledger = outcome_ledger::classify(line.as_ref(), post_state(journal, step.item));
            let Some(projection) = (projector.project)(&evidence) else {
                return Vec::new();
            };
            let pass_id = derived(&["loom.pass.step", &step.sweep_id]);
            vec![record(
                ctx,
                &projector,
                &pass_id,
                &evidence,
                projection,
                Some(&ledger),
                ctx.now,
                store,
            )]
        }
        Source::Tick { started_at } => {
            let pass_id = derived(&[
                "loom.pass.tick",
                ctx.role,
                ctx.host,
                &crate::telemetry::trace::instant(*started_at),
            ]);
            let mut items: BTreeMap<u32, Evidence> = BTreeMap::new();
            for (number, is_pr, event) in all {
                let evidence = items.entry(number).or_insert_with(|| Evidence {
                    number,
                    pre_labels: labels.get(&number).cloned().unwrap_or_default(),
                    ..Evidence::default()
                });
                if let Some(is_pr) = is_pr {
                    evidence.is_pr = is_pr;
                }
                evidence.events.push(event);
            }
            items
                .into_values()
                .filter_map(|evidence| {
                    let projection = (projector.project)(&evidence)?;
                    let at = projection.at.unwrap_or(ctx.now);
                    Some(record(ctx, &projector, &pass_id, &evidence, projection, None, at, store))
                })
                .collect()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn record(
    ctx: &Context<'_>,
    projector: &Projector,
    pass_id: &str,
    evidence: &Evidence,
    mut p: Projection,
    ledger: Option<&outcome_ledger::StepOutcome>,
    evaluated_at: DateTime<Utc>,
    store: &mut RefusalStore,
) -> PassVerdictRecord {
    let number = u64::from(evidence.number);
    // The store is keyed by the lowercase slug, like `stage_items`' `repo`.
    let key = ctx.repo.to_ascii_lowercase();
    let mut first_refused_at = None;
    match p.store {
        StoreOp::None => {}
        StoreOp::Observe => {
            let standing = store.observe(
                &key,
                number,
                &Observed {
                    head_sha: p.head_sha.as_deref(),
                    criterion_key: p.criterion_key.as_deref().unwrap_or_default(),
                    reason_key: p.reason_key.as_deref().unwrap_or_default(),
                    reason: &p.reason,
                    at: evaluated_at,
                },
            );
            first_refused_at = Some(standing.first_refused_at);
            p.head_sha = p.head_sha.or(standing.head_sha);
        }
        StoreOp::Lookup => {
            if let Some(standing) = store.touch(&key, number, p.head_sha.as_deref(), evaluated_at) {
                p.reason = standing.reason;
                p.criterion_key = Some(standing.criterion_key);
                p.reason_key = Some(standing.reason_key);
                p.head_sha = p.head_sha.or(standing.head_sha);
                first_refused_at = Some(standing.first_refused_at);
            }
        }
        StoreOp::Clear => store.clear(&key, number),
    }
    let mechanism = format!("step:{}", ctx.role);
    let hour = evaluated_at.format("%Y-%m-%dT%H").to_string();
    let fact_id = derived(&[
        "loom.pass.verdict",
        &mechanism,
        ctx.repo,
        &number.to_string(),
        p.head_sha.as_deref().unwrap_or_default(),
        p.criterion_key.as_deref().unwrap_or_default(),
        p.reason_key.as_deref().unwrap_or_default(),
        &hour,
    ]);
    let proposal_label = (!evidence.is_pr)
        .then(|| {
            champion::PROPOSAL_LABELS
                .into_iter()
                .find(|l| evidence.pre_labels.iter().any(|x| x == l))
        })
        .flatten()
        .map(str::to_string);
    PassVerdictRecord {
        pass_id: pass_id.to_string(),
        mechanism,
        role: Some(ctx.role.to_string()),
        repo: ctx.repo.to_string(),
        number,
        artifact: if evidence.is_pr { "pr" } else { "issue" }.to_string(),
        verdict: p.class.to_string(),
        reason: Some(p.reason.clone()),
        detail: None,
        blockers: Vec::new(),
        labels_added: Vec::new(),
        labels_removed: Vec::new(),
        mode: PassMode::On,
        applied: p.wrote,
        at: evaluated_at,
        step: StepFacts {
            head_sha: p.head_sha,
            step: Some((projector.step_name)(evidence.is_pr).to_string()),
            class: Some(p.class.to_string()),
            reason_set: Some(p.reason_set.to_string()),
            ledger_class: ledger.map(|l| l.class.as_str().to_string()),
            ledger_reason: ledger.map(|l| l.reason.clone()),
            pre_labels: evidence.pre_labels.clone(),
            evaluated_at: Some(evaluated_at),
            first_refused_at,
            criterion_key: p.criterion_key,
            reason_key: p.reason_key,
            proposal_label,
            body_hash: p.body_hash,
            fact_id: Some(fact_id),
            loom: Some(Provenance::current()),
        },
    }
}

/// Build and emit one drained run's records (`pick_decision::emit_role_tick`).
/// A no-op for a role with no projection, or when records are not exported.
pub fn emit_drain(
    root: &Path,
    role: &str,
    repo: &str,
    started_at: DateTime<Utc>,
    staged: Option<StagedStep>,
    journal: Option<&Journal>,
) {
    if projector(role).is_none() || !super::ops::records_exported() {
        return;
    }
    let source = staged.map_or(Source::Tick { started_at }, |s| Source::Step(s.read()));
    let host = super::ops::global_ops_sink()
        .map_or_else(crate::sweep_registry::host_identity, |s| s.host_id().to_string());
    let ctx = Context {
        role,
        repo,
        host: &host,
        now: Utc::now(),
    };
    let records = refusals::with_store(Some(root), &repo.to_ascii_lowercase(), |store| {
        let before = store.clone();
        store.prune(ctx.now);
        let records = records_for(&ctx, &source, journal, store);
        let changed = *store != before;
        (records, changed)
    });
    for record in records {
        super::ops::emit_record(crate::telemetry::TelemetryRecord::PassVerdict(record));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "step_verdict_tests.rs"]
mod tests;
