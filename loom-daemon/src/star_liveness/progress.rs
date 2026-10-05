//! Time in stage and the no-progress watchdog (#9244 liveness item 3). Pure,
//! with the clock passed in.
//!
//! **Forward progress** is built only from forge-visible facts every host
//! managing the repo reads identically, never from host-local state (a sweep
//! checkpoint exists only on the host running the sweep, so keying on it
//! made each host report the same stall under its own key, and left a
//! non-owning host with strictly less signal than the owner):
//!
//! - a change in the issue's [`fingerprint`]: its labels (every workflow
//!   transition is a label change) and its PR (number, labels, `updated_at`,
//!   which a push, a review or a CI-driven relabel moves; a push moving the
//!   head SHA always moves `updated_at` too);
//! - a stage change;
//! - for a `no-capacity` row the work finder queued (#10214), its position in
//!   the host's starred queue moving forward: work ahead was dispatched (or
//!   unstarred), so the queue is draining, not stalled. A queued row that does
//!   escalate names its deferral reason, its position, the cap and what limits
//!   the cap, never just "the work-finder";
//! - a trusted comment on the issue, including the sweep's lease comment,
//!   whose forge-assigned `updated_at` a live sweep renews every ~5 minutes
//!   ([`latest_comment_activity`]). So a long Builder phase with a live lease
//!   is progress on every host, not only the one running it. Comments are
//!   read only when a row is about to trip, and never for a key already
//!   escalated.
//!
//! An agent-owned stage with no progress for the watchdog window escalates
//! with what Loom last saw. The dedupe key hashes **only** the fingerprint,
//! not the stage: for an undispatched row the stage can come from host-local
//! capacity (`no-capacity` on one host, `ready` on another), so every host
//! names one stall with one key and the forge marker lets one comment
//! through.
//!
//! `blocked-by` is exempt: the blocker carries the inherited star and is
//! watched in its own row. So is `needs-operator`, which has already
//! escalated.
//!
//! The fingerprint clock is in memory: a daemon restart restarts it, which
//! can only make an escalation later, never spurious. Hosts may trip at
//! different moments; they still share the key.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::types::{AskKind, CapacityWait, LandingStage, OperatorAsk};

/// A progress fingerprint: the forge-visible state of the issue and its PR,
/// flattened. Identical on every host that reads the same forge.
#[must_use]
pub fn fingerprint(issue_labels: &[String], pr: Option<(u32, &[String], Option<&str>)>) -> String {
    let mut labels: Vec<&str> = issue_labels.iter().map(String::as_str).collect();
    labels.sort_unstable();
    let mut fp = format!("labels={}", labels.join(","));
    if let Some((n, pr_labels, updated)) = pr {
        let mut pl: Vec<&str> = pr_labels.iter().map(String::as_str).collect();
        pl.sort_unstable();
        fp.push_str(&format!(
            ";pr={n};pr_labels={};pr_updated={}",
            pl.join(","),
            updated.unwrap_or("")
        ));
    }
    fp
}

/// The latest trusted comment activity on an issue (created or edited, so a
/// lease renewal counts), ignoring the liveness pass's own escalation
/// comments (posting an ask is not progress).
#[must_use]
pub fn latest_comment_activity(
    comments: &[super::forge::ForgeComment],
    self_login: Option<&str>,
) -> Option<DateTime<Utc>> {
    comments
        .iter()
        .filter(|c| super::trust::trusted(c, self_login))
        .filter(|c| !c.body.contains(super::escalate::MARKER_PREFIX))
        .flat_map(|c| [c.created_at.as_deref(), c.updated_at.as_deref()])
        .flatten()
        .filter_map(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
        .max()
}

/// A short, stable hash of a fingerprint for the dedupe key (FNV-1a).
#[must_use]
pub fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", h & 0xffff_ffff)
}

#[derive(Debug, Clone)]
struct Entry {
    stage: LandingStage,
    stage_since: DateTime<Utc>,
    fingerprint: String,
    progress_at: DateTime<Utc>,
    /// The queue position last seen (#10214), for a queued row.
    position: Option<u32>,
}

/// What [`Tracker::observe`] returns for one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub stage_since: DateTime<Utc>,
    pub progress_at: DateTime<Utc>,
}

/// Per-(repo, issue) stage and progress clocks.
#[derive(Debug, Default)]
pub struct Tracker {
    entries: HashMap<(String, u32), Entry>,
}

impl Tracker {
    /// Record this pass's stage, fingerprint and (for a queued row) queue
    /// position for (`repo`, `issue`). A position that moved forward is
    /// progress (#10214).
    pub fn observe(
        &mut self,
        repo: &str,
        issue: u32,
        stage: LandingStage,
        fingerprint: &str,
        position: Option<u32>,
        now: DateTime<Utc>,
    ) -> Observation {
        let e = self
            .entries
            .entry((repo.to_string(), issue))
            .or_insert_with(|| Entry {
                stage,
                stage_since: now,
                fingerprint: fingerprint.to_string(),
                progress_at: now,
                position,
            });
        if e.stage != stage {
            e.stage = stage;
            e.stage_since = now;
            e.progress_at = now;
        }
        if e.fingerprint != fingerprint {
            e.fingerprint = fingerprint.to_string();
            e.progress_at = now;
        }
        if let (Some(was), Some(is)) = (e.position, position) {
            if is < was {
                e.progress_at = now;
            }
        }
        e.position = position;
        Observation {
            stage_since: e.stage_since,
            progress_at: e.progress_at,
        }
    }

    /// Move (`repo`, `issue`)'s progress clock forward to `at` (forge-seen
    /// activity newer than the last fingerprint change). Never backwards.
    pub fn note_progress(
        &mut self,
        repo: &str,
        issue: u32,
        at: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let e = self.entries.get_mut(&(repo.to_string(), issue))?;
        if at > e.progress_at {
            e.progress_at = at;
        }
        Some(e.progress_at)
    }

    /// Forget every row not in `live` (unstarred, closed, or no longer an
    /// inheriting blocker), except rows of a repo in `unread` (its read
    /// failed this pass, so its clocks are kept rather than restarted).
    pub fn retain(&mut self, live: &HashSet<(String, u32)>, unread: &[String]) {
        self.entries
            .retain(|k, _| live.contains(k) || unread.contains(&k.0));
    }
}

/// One row as the watchdog sees it.
#[derive(Debug, Clone, Copy)]
pub struct Watched<'a> {
    pub repo: &'a str,
    pub issue: u32,
    pub stage: LandingStage,
    pub next_actor: &'a str,
    pub fingerprint: &'a str,
    pub progress_at: DateTime<Utc>,
    /// A `no-capacity` row's structured wait (#10214).
    pub wait: Option<&'a CapacityWait>,
}

/// The watchdog verdict for one row: an escalation when an agent-owned
/// stage has made no progress for `window`.
#[must_use]
pub fn watchdog(w: &Watched<'_>, now: DateTime<Utc>, window: Duration) -> Option<OperatorAsk> {
    if matches!(w.stage, LandingStage::NeedsOperator | LandingStage::BlockedBy) {
        return None;
    }
    let idle = now.signed_duration_since(w.progress_at).to_std().ok()?;
    if idle < window {
        return None;
    }
    let minutes = idle.as_secs() / 60;
    let (repo, issue, actor) = (w.repo, w.issue, w.next_actor);
    let key = format!("{}:{}", AskKind::NoProgress.as_str(), short_hash(w.fingerprint));
    let since = w
        .progress_at
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    if let Some(wait) = w.wait.filter(|_| w.stage == LandingStage::NoCapacity) {
        let moving = if wait.queued() {
            format!("the queue ahead of it has not advanced since {since}")
        } else {
            format!("no host has taken it since {since}")
        };
        return Some(OperatorAsk {
            kind: AskKind::NoProgress,
            key,
            text: format!(
                "{repo}#{issue} is starred but has waited {minutes} min for a slot: it is {} (deferred: {}), and {moving}. {} (last seen: {}).",
                wait.summary(),
                wait.limit_phrase(),
                super::queue::advice(wait),
                w.fingerprint,
            ),
        });
    }
    Some(OperatorAsk {
        kind: AskKind::NoProgress,
        key,
        text: format!(
            "{repo}#{issue} is starred but has made no forward progress for {minutes} min: it \
             has been `{}` since {} (next actor: {actor}; last seen: {}). Check why the \
             {actor} is not moving it.",
            w.stage.as_str(),
            since,
            w.fingerprint,
        ),
    })
}
