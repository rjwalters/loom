//! Time in stage and the no-progress watchdog (#9244 liveness item 3). Pure,
//! with the clock passed in.
//!
//! **Forward progress** is any change in an issue's [`fingerprint`]: its
//! labels (every workflow transition is a label change), its PR (number,
//! labels, `updated_at`, which a push, a review or a CI-driven relabel
//! moves), or the `updated_at` of its sweep checkpoint on this host. A stage
//! change is progress too. An agent-owned stage whose fingerprint has not
//! changed for the watchdog window escalates with what Loom last saw.
//!
//! `blocked-by` is exempt: the blocker carries the inherited star and is
//! watched in its own row. So is `needs-operator`, which has already
//! escalated.
//!
//! The clock is in memory: a daemon restart restarts it, which can only make
//! an escalation later, never spurious.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::types::{AskKind, LandingStage, OperatorAsk};

/// A progress fingerprint: every forward-progress signal, flattened.
#[must_use]
pub fn fingerprint(
    issue_labels: &[String],
    pr: Option<(u32, &[String], Option<&str>)>,
    checkpoint: Option<&str>,
) -> String {
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
    if let Some(cp) = checkpoint {
        fp.push_str(&format!(";checkpoint={cp}"));
    }
    fp
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
    /// Record this pass's stage and fingerprint for (`repo`, `issue`).
    pub fn observe(
        &mut self,
        repo: &str,
        issue: u32,
        stage: LandingStage,
        fingerprint: &str,
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
        Observation {
            stage_since: e.stage_since,
            progress_at: e.progress_at,
        }
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
    Some(OperatorAsk {
        kind: AskKind::NoProgress,
        key: format!(
            "{}:{}:{}",
            AskKind::NoProgress.as_str(),
            w.stage.as_str(),
            short_hash(w.fingerprint)
        ),
        text: format!(
            "{repo}#{issue} is starred but has made no forward progress for {minutes} min: it \
             has been `{}` since {} (next actor: {actor}; last seen: {}). Check why the \
             {actor} is not moving it.",
            w.stage.as_str(),
            w.progress_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            w.fingerprint,
        ),
    })
}
