//! Item-level features (#10231, #10201 Slice B): the values an estimate takes
//! from data the daemon already holds in process, with no forge read.
//!
//! | Feature | Source | Observed |
//! |---|---|---|
//! | `tier`, `workspace_priority`, `issue_created_at`, `issue_age_sec` | the ready-queue row | the plan tick (`ReadyPlan::at`) |
//! | `sweep_runtime` | the `sweep.global.dispatch` payload | the dispatch |
//! | `sweep_model`, `sweep_effort` | the owning sweep registry's entry, read at the dispatch | the dispatch |
//! | `attempt` | dispatches of the issue this process observed | the dispatch |
//! | `judge_verdicts_so_far` | verdicts the tracker settled | the settlement, **not** the verdict's own instant |
//! | `repo_first_pass_approval_rate` | the loaded verdict history ([`StageSamples::verdicts`]) | its `observed_at` |
//! | `urgent` | none: deprecated | |
//!
//! # Point in time
//!
//! Every value is kept with the instant it was *observed* (`known_at`) and an
//! estimate at `as_of` reads only observations with `known_at < as_of`. A
//! later queue refresh, registry read or verdict therefore cannot change an
//! earlier instant's features, whatever its own event time was: a verdict
//! recorded at the end of the Judge phase is known when the next phase or
//! listing settles it, so that settlement is its `known_at`.
//!
//! # First-pass population
//!
//! [`StageSamples::verdicts`] has three feeders, and the rate counts the
//! `attempt == 1` verdicts of all of them: in-sweep verdicts from
//! `sweep.outcome`; external (`in_sweep: false`) Judge verdicts from the ETA
//! stage journal, i.e. label transitions the tracker saw outside a sweep and
//! `eta backfill` rows from PR label history; and, under fleet history scope,
//! the fleet snapshot's verdicts from forge label timelines. No sample carries
//! a source tag or PR id, so one first verdict can be counted more than once
//! (see `eta.md` → Item facts).
//!
//! # Omission reasons
//!
//! A feature that cannot apply carries a specific reason in [`reason`], never
//! `not_collected`.

use super::{Item, ItemKey, Tracker};
use crate::eta::explanation::{FeatureOmitted, Features};
use crate::eta::history::StageSamples;
use chrono::{DateTime, Utc};

/// Omission reasons this module assigns (`features_omitted[].reason`).
pub mod reason {
    /// `urgent`: `loom:urgent` no longer affects dispatch (#9244), so the
    /// field is a deprecated compatibility field and never populated.
    pub const DEPRECATED: &str = "deprecated";
    /// The issue has no `tier:*` label.
    pub const NO_TIER_LABEL: &str = "no_tier_label";
    /// The issue never appeared as a ready-queue row this process observed
    /// (it entered through a review listing, or the daemon restarted).
    pub const NEVER_IN_READY_QUEUE: &str = "never_in_ready_queue";
    /// The issue's queue row was first observed at or after `as_of`.
    pub const NOT_OBSERVED_YET: &str = "not_observed_yet";
    /// The row's `createdAt` was absent.
    pub const CREATED_AT_MISSING: &str = "issue_created_at_missing";
    /// The row's `createdAt` was not an RFC 3339 instant.
    pub const CREATED_AT_MALFORMED: &str = "issue_created_at_malformed";
    /// The issue's creation instant is after `as_of`: no age is invented.
    pub const CREATED_IN_FUTURE: &str = "issue_created_in_future";
    /// No sweep was dispatched for the item before `as_of`.
    pub const NO_SWEEP_YET: &str = "no_sweep_yet";
    /// A sweep is running or ran, but its dispatch was not observed by this
    /// process (adopted across a daemon restart).
    pub const SWEEP_NOT_OBSERVED: &str = "sweep_not_observed";
    /// The dispatch event named no admitted runtime.
    pub const RUNTIME_NOT_RECORDED: &str = "runtime_not_recorded";
    /// The owning registry had no entry for the sweep when its dispatch was
    /// observed, so its model and effort are unknown.
    pub const REGISTRY_UNAVAILABLE: &str = "registry_unavailable";
    /// The dispatch requested no explicit value: the runtime used its own
    /// default, which is not recorded in process.
    pub const RUNTIME_DEFAULT: &str = "runtime_default";
    /// The Judge's verdicts for this PR happened before this process observed
    /// the item, so "so far" is unknown rather than empty: no dispatch or
    /// verdict was observed at all, or the first observed dispatch came after
    /// the PR already existed (a rework re-dispatch after a restart).
    pub const VERDICT_HISTORY_UNOBSERVED: &str = "verdict_history_unobserved";
    /// No verdict of any feeder (any repo) was observed before `as_of`.
    pub const NO_VERDICT_HISTORY: &str = "no_verdict_history";
    /// The repo has no first-attempt verdict observed in the history window
    /// before `as_of`: an empty denominator.
    pub const NO_FIRST_VERDICTS: &str = "no_first_verdicts_before_as_of";
}

const MAX_ISSUE_OBS: usize = 8;
const MAX_DISPATCH_OBS: usize = 16;
const MAX_VERDICT_OBS: usize = 32;

/// The issue facts of a ready-queue row (`ReadyQueueRow`'s fields, as
/// carried into ETA ingestion).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueRow {
    /// The owning workspace's priority tier (lower dispatches first).
    pub workspace_priority: u32,
    /// The issue's `createdAt` as the listing supplied it.
    pub created_at: Option<String>,
    /// The issue's `tier:*` label.
    pub tier: Option<String>,
}

/// What the dispatch path knew about a sweep, besides the event itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchMeta {
    /// The admitted runtime from the `sweep.global.dispatch` payload.
    pub runtime: Option<String>,
    /// The sweep's registry entry, when the owning registry had one.
    pub registry: Option<RegistryMeta>,
}

/// A sweep registry entry's dispatch parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryMeta {
    /// Model requested at dispatch; `None` = the runtime's default.
    pub model: Option<String>,
    /// Effort requested at dispatch; `None` = the runtime's default.
    pub effort: Option<String>,
    /// When the sweep was spawned. Informational: the observation's
    /// `known_at` is when the dispatch was seen, not this.
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Created {
    Missing,
    Malformed,
    At(DateTime<Utc>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IssueObs {
    known_at: DateTime<Utc>,
    workspace_priority: u32,
    tier: Option<String>,
    created: Created,
}

#[derive(Debug, Clone)]
struct DispatchObs {
    sweep_id: String,
    known_at: DateTime<Utc>,
    meta: DispatchMeta,
}

#[derive(Debug, Clone)]
struct VerdictObs {
    attempt: u32,
    pass: bool,
    known_at: DateTime<Utc>,
}

/// The observations one item keeps, each with its `known_at`.
#[derive(Debug, Clone, Default)]
pub(super) struct ItemFacts {
    issue: Vec<IssueObs>,
    dispatches: Vec<DispatchObs>,
    verdicts: Vec<VerdictObs>,
    /// The first dispatch observed came after the PR existed, with no verdict
    /// observed: the verdicts before it are unknown.
    verdicts_before_unobserved: bool,
}

fn push_capped<T>(list: &mut Vec<T>, value: T, cap: usize) {
    list.push(value);
    if list.len() > cap {
        list.remove(0);
    }
}

fn parse_created(raw: Option<&str>) -> Created {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Created::Missing,
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map_or(Created::Malformed, |t| Created::At(t.with_timezone(&Utc))),
    }
}

fn omission(name: &str, why: &str) -> FeatureOmitted {
    FeatureOmitted {
        name: name.to_string(),
        reason: why.to_string(),
    }
}

impl ItemFacts {
    /// A ready-queue row observed at `known_at`. A repeat of the latest
    /// value keeps its first `known_at`.
    pub(super) fn note_issue(&mut self, row: &IssueRow, known_at: DateTime<Utc>) {
        let obs = IssueObs {
            known_at,
            workspace_priority: row.workspace_priority,
            tier: row.tier.clone(),
            created: parse_created(row.created_at.as_deref()),
        };
        if self.issue.last().is_some_and(|last| {
            last.known_at <= known_at
                && (last.workspace_priority, &last.tier, &last.created)
                    == (obs.workspace_priority, &obs.tier, &obs.created)
        }) {
            return;
        }
        push_capped(&mut self.issue, obs, MAX_ISSUE_OBS);
        self.issue.sort_by_key(|o| o.known_at);
    }

    /// A verdict settled at `known_at`. One per attempt: a repeat is dropped.
    pub(super) fn note_verdict(&mut self, attempt: u32, pass: bool, known_at: DateTime<Utc>) {
        if self.verdicts.iter().any(|v| v.attempt == attempt) {
            return;
        }
        push_capped(
            &mut self.verdicts,
            VerdictObs {
                attempt,
                pass,
                known_at,
            },
            MAX_VERDICT_OBS,
        );
    }

    fn issue_at(&self, as_of: DateTime<Utc>) -> Option<&IssueObs> {
        self.issue.iter().rev().find(|o| o.known_at < as_of)
    }

    fn write_issue(
        &self,
        as_of: DateTime<Utc>,
        features: &mut Features,
        omitted: &mut Vec<FeatureOmitted>,
    ) {
        let Some(obs) = self.issue_at(as_of) else {
            let why = if self.issue.is_empty() {
                reason::NEVER_IN_READY_QUEUE
            } else {
                reason::NOT_OBSERVED_YET
            };
            for name in [
                "tier",
                "workspace_priority",
                "issue_created_at",
                "issue_age_sec",
            ] {
                omitted.push(omission(name, why));
            }
            return;
        };
        features.workspace_priority = Some(i64::from(obs.workspace_priority));
        features.tier.clone_from(&obs.tier);
        if obs.tier.is_none() {
            omitted.push(omission("tier", reason::NO_TIER_LABEL));
        }
        match obs.created {
            Created::Missing => {
                omitted.push(omission("issue_created_at", reason::CREATED_AT_MISSING));
                omitted.push(omission("issue_age_sec", reason::CREATED_AT_MISSING));
            }
            Created::Malformed => {
                omitted.push(omission("issue_created_at", reason::CREATED_AT_MALFORMED));
                omitted.push(omission("issue_age_sec", reason::CREATED_AT_MALFORMED));
            }
            Created::At(created) => {
                features.issue_created_at = Some(created);
                if created > as_of {
                    omitted.push(omission("issue_age_sec", reason::CREATED_IN_FUTURE));
                } else {
                    features.issue_age_sec = Some((as_of - created).num_seconds());
                }
            }
        }
    }

    fn write_sweep(
        &self,
        adopted: bool,
        as_of: DateTime<Utc>,
        features: &mut Features,
        omitted: &mut Vec<FeatureOmitted>,
    ) {
        let seen: Vec<&DispatchObs> = self
            .dispatches
            .iter()
            .filter(|d| d.known_at < as_of)
            .collect();
        let Some(latest) = seen.last() else {
            let why = if adopted && self.dispatches.is_empty() {
                reason::SWEEP_NOT_OBSERVED
            } else {
                reason::NO_SWEEP_YET
            };
            for name in ["sweep_runtime", "sweep_model", "sweep_effort", "attempt"] {
                omitted.push(omission(name, why));
            }
            return;
        };
        features.attempt = u32::try_from(seen.len()).ok();
        features.sweep_runtime.clone_from(&latest.meta.runtime);
        if latest.meta.runtime.is_none() {
            omitted.push(omission("sweep_runtime", reason::RUNTIME_NOT_RECORDED));
        }
        match &latest.meta.registry {
            None => {
                omitted.push(omission("sweep_model", reason::REGISTRY_UNAVAILABLE));
                omitted.push(omission("sweep_effort", reason::REGISTRY_UNAVAILABLE));
            }
            Some(entry) => {
                features.sweep_model.clone_from(&entry.model);
                features.sweep_effort.clone_from(&entry.effort);
                if entry.model.is_none() {
                    omitted.push(omission("sweep_model", reason::RUNTIME_DEFAULT));
                }
                if entry.effort.is_none() {
                    omitted.push(omission("sweep_effort", reason::RUNTIME_DEFAULT));
                }
            }
        }
    }

    fn write_verdicts(
        &self,
        has_pr: bool,
        as_of: DateTime<Utc>,
        features: &mut Features,
        omitted: &mut Vec<FeatureOmitted>,
    ) {
        let nothing_seen = self.dispatches.is_empty() && self.verdicts.is_empty();
        if has_pr && (nothing_seen || self.verdicts_before_unobserved) {
            omitted.push(omission("judge_verdicts_so_far", reason::VERDICT_HISTORY_UNOBSERVED));
            return;
        }
        let mut known: Vec<&VerdictObs> = self
            .verdicts
            .iter()
            .filter(|v| v.known_at < as_of)
            .collect();
        known.sort_by_key(|v| (v.attempt, v.known_at));
        features.judge_verdicts_so_far = Some(
            known
                .iter()
                .map(|v| if v.pass { "pass" } else { "fail" }.to_string())
                .collect(),
        );
    }
}

/// Write the repo's first-pass approval rate at `as_of`.
fn write_first_pass_rate(
    repo: &str,
    history: &StageSamples,
    as_of: DateTime<Utc>,
    features: &mut Features,
    omitted: &mut Vec<FeatureOmitted>,
) {
    let name = "repo_first_pass_approval_rate";
    // Only samples observed before `as_of` pick the reason, so even the
    // reason text cannot depend on later data.
    if !history.verdicts.iter().any(|v| v.observed_at < as_of) {
        omitted.push(omission(name, reason::NO_VERDICT_HISTORY));
        return;
    }
    match history.first_pass_approval(repo, as_of) {
        Some((n, approved)) if n > 0 => {
            #[allow(clippy::cast_precision_loss)]
            let rate = approved as f64 / n as f64;
            features.repo_first_pass_approval_rate = Some(rate);
        }
        _ => omitted.push(omission(name, reason::NO_FIRST_VERDICTS)),
    }
}

impl Tracker {
    /// `sweep.global.dispatch` for an issue sweep, with what the dispatch
    /// path knew beside it. Call it right after [`Tracker::on_dispatch`].
    ///
    /// The observation's instant is `at`, when this process saw the dispatch
    /// and read the registry: never the registry entry's earlier
    /// `started_at`, which is an event time, not when the value was known
    /// here. A repeat of `sweep_id` is dropped.
    pub fn on_sweep_dispatch(
        &mut self,
        repo: &str,
        issue: u32,
        sweep_id: &str,
        meta: DispatchMeta,
        at: DateTime<Utc>,
    ) {
        let known_at = at;
        let item = self.item(repo, issue);
        let has_pr = item.pr_number.is_some();
        let facts = &mut item.facts;
        if facts.dispatches.iter().any(|d| d.sweep_id == sweep_id) {
            return;
        }
        if has_pr && facts.dispatches.is_empty() && facts.verdicts.is_empty() {
            facts.verdicts_before_unobserved = true;
        }
        push_capped(
            &mut facts.dispatches,
            DispatchObs {
                sweep_id: sweep_id.to_string(),
                known_at,
                meta,
            },
            MAX_DISPATCH_OBS,
        );
        facts.dispatches.sort_by_key(|d| d.known_at);
    }

    /// The Slice B features of `item` at `as_of`, and why each null one is
    /// null.
    pub(super) fn item_features(
        &self,
        key: &ItemKey,
        item: &Item,
        history: &StageSamples,
        as_of: DateTime<Utc>,
        features: &mut Features,
        omitted: &mut Vec<FeatureOmitted>,
    ) {
        omitted.push(omission("urgent", reason::DEPRECATED));
        item.facts.write_issue(as_of, features, omitted);
        let adopted = item.sweep_running || item.sweep_id.is_some();
        item.facts.write_sweep(adopted, as_of, features, omitted);
        item.facts
            .write_verdicts(item.pr_number.is_some(), as_of, features, omitted);
        write_first_pass_rate(&key.repo, history, as_of, features, omitted);
    }
}
