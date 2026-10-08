//! What a host remembers between workspace passes (#10718): the per-repo
//! verdict that lets an unchanged repo cost nothing beyond its line in the
//! head query, the per-repo backoff, the state of a network outage, and what
//! this process has already resynced.
//!
//! All of it is process memory. A restart (which is also how the running
//! version changes) starts from nothing, so the first pass after one checks
//! every workspace.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::{Alert, WState, WorkspaceReport, ALERT_AFTER, BACKOFF_CAP};

/// Longest a host stays off the network after passes in which no remote
/// answered.
pub const OUTAGE_HOLD_CAP: Duration = Duration::from_secs(30 * 60);

fn chrono_of(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX)
}

#[derive(Debug, Clone)]
pub(super) struct Backoff {
    pub(super) failures: u32,
    pub(super) next_attempt: DateTime<Utc>,
    pub(super) state: WState,
    pub(super) last_error: String,
}

/// What a workspace was found to be at one default-branch commit, for one
/// running version.
#[derive(Debug, Clone)]
pub(super) struct Verdict {
    pub(super) commit: String,
    pub(super) version: String,
    pub(super) branch: String,
    pub(super) state: WState,
    pub(super) installed: Option<String>,
    pub(super) requires_daemon: Option<String>,
    pub(super) reason: Option<String>,
}

impl Verdict {
    /// A stale workspace this host may resync.
    pub(super) fn stale(&self) -> bool {
        matches!(self.state, WState::W1 | WState::W3)
    }

    /// Fill `report` from this verdict.
    pub(super) fn fill(&self, report: &mut WorkspaceReport) {
        report.state = self.state;
        report.installed.clone_from(&self.installed);
        report.requires_daemon.clone_from(&self.requires_daemon);
        report.reason.clone_from(&self.reason);
    }
}

/// How a failure counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureKind {
    /// A rule on the remote refused the push. Retrying sooner cannot help.
    Protected,
    /// This repo's remote did not answer. Backed off, never alerted per
    /// repo: the pass reports unreachable remotes once, for the host.
    Unreachable,
    /// The forge or the remote answered and refused this repo: it was deleted
    /// or renamed, or the credential may not read it. This repo's failure
    /// alone, alerted as `repo-access`; never part of a network outage.
    Refused,
    /// Anything else.
    Other,
}

/// A network outage as the host sees it. It lasts from the first remote that
/// does not answer until every remote that failed has answered again.
#[derive(Debug, Default)]
struct Outage {
    /// Passes in which some remote did not answer, since the outage began.
    streak: u32,
    /// The alert for this outage has been published.
    alerted: bool,
    /// No remote answered at all: stay off the network until then.
    hold_until: Option<DateTime<Utc>>,
}

/// Per-repo state kept in process memory. It resets on restart: a restarted
/// host re-evaluates every workspace from scratch on its first H0 tick.
#[derive(Debug, Default)]
pub struct Memory {
    pub(super) backoff: HashMap<PathBuf, Backoff>,
    pub(super) verdicts: HashMap<PathBuf, Verdict>,
    /// Things said once per process.
    pub(super) noted: HashSet<String>,
    /// Where the next pass starts, so a pass that ran out of time does not
    /// starve the workspaces at the end of the list.
    pub(super) cursor: usize,
    /// The version and commit of this process's one resync of each repo.
    pub(super) resynced: HashMap<PathBuf, (String, String)>,
    /// Workspaces whose remote (or forge) did not answer when last asked.
    pub(super) down: HashSet<PathBuf>,
    /// Whether the payload differs from each checkout's installed files, per
    /// (HEAD, stamp): the checkout half of the dispatch hold (#10719).
    pub(super) checkout: HashMap<PathBuf, (String, bool)>,
    outage: Outage,
    /// Passes in a row whose head query failed, and whether that was alerted.
    head_query: (u32, bool),
    /// The rate-limit breaker was open at the last pass (said once per time
    /// it opens).
    breaker_open: bool,
}

impl Memory {
    /// The backoff delay after `failures` consecutive failures: one sync
    /// interval, doubling, capped at [`BACKOFF_CAP`].
    #[must_use]
    pub fn delay(interval: Duration, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(20);
        interval.saturating_mul(1u32 << doublings).min(BACKOFF_CAP)
    }

    /// Record a failure against `report`'s repo. Returns when it is tried
    /// again, and the alert to publish if this failure is one.
    ///
    /// A protection rule alerts once per repo for the life of the process and
    /// is only logged after that; an unreachable remote never alerts here.
    pub(super) fn fail(
        &mut self,
        report: &WorkspaceReport,
        kind: FailureKind,
        detail: &str,
        interval: Duration,
        now: DateTime<Utc>,
    ) -> (DateTime<Utc>, Option<Alert>) {
        let failures = self
            .backoff
            .get(&report.root)
            .map_or(0, |b| b.failures)
            .saturating_add(1);
        let delay = match kind {
            FailureKind::Protected => BACKOFF_CAP,
            FailureKind::Unreachable | FailureKind::Refused | FailureKind::Other => {
                Self::delay(interval, failures)
            }
        };
        let next_attempt = now + chrono_of(delay);
        self.backoff.insert(
            report.root.clone(),
            Backoff {
                failures,
                next_attempt,
                state: report.state,
                last_error: detail.to_string(),
            },
        );
        let alerts = match kind {
            FailureKind::Protected => self
                .noted
                .insert(format!("protected:{}", report.root.display())),
            FailureKind::Unreachable => false,
            FailureKind::Refused | FailureKind::Other => failures >= ALERT_AFTER,
        };
        let alert = alerts.then(|| Alert {
            root: report.root.clone(),
            repo: report.repo.clone(),
            kind: match kind {
                FailureKind::Protected => "branch-protection",
                FailureKind::Refused => "repo-access",
                FailureKind::Unreachable | FailureKind::Other => "failure",
            },
            failures,
            detail: detail.to_string(),
            next_attempt,
        });
        (next_attempt, alert)
    }

    /// Remember what `root` is at `commit`.
    pub(super) fn settle(
        &mut self,
        root: &Path,
        branch: &str,
        commit: &str,
        version: &str,
        report: &WorkspaceReport,
    ) {
        self.verdicts.insert(
            root.to_path_buf(),
            Verdict {
                commit: commit.to_string(),
                version: version.to_string(),
                branch: branch.to_string(),
                state: report.state,
                installed: report.installed.clone(),
                requires_daemon: report.requires_daemon.clone(),
                reason: report.reason.clone(),
            },
        );
    }

    /// Note whether the rate-limit breaker is open this pass. Returns whether
    /// that is news: it just opened, or just closed.
    pub(super) fn note_breaker(&mut self, open: bool) -> bool {
        std::mem::replace(&mut self.breaker_open, open) != open
    }

    /// Fold one pass's head query into its record. `failure` is why it got
    /// no usable answer while the network was otherwise there; `failing`
    /// says it failed at all (during an outage it did, and that is the
    /// outage's to report: the run neither grows nor ends). Returns whether
    /// this is the first failure of a run (worth a warning), and the one
    /// alert a run of [`ALERT_AFTER`] failing passes gets.
    pub(super) fn note_head_query(
        &mut self,
        failure: Option<&str>,
        failing: bool,
        interval: Duration,
        now: DateTime<Utc>,
    ) -> (bool, Option<Alert>) {
        let Some(detail) = failure else {
            if !failing {
                self.head_query = (0, false);
            }
            return (false, None);
        };
        let (streak, alerted) = &mut self.head_query;
        *streak = streak.saturating_add(1);
        if *streak == 1 {
            return (true, None);
        }
        if *streak < ALERT_AFTER || *alerted {
            return (false, None);
        }
        *alerted = true;
        let alert = Alert {
            root: PathBuf::new(),
            repo: None,
            kind: "head-query",
            failures: *streak,
            detail: format!(
                "the batched default-branch head query has failed in {streak} passes in a row, \
                 so every repo's remote is asked with git ls-remote instead: {detail}"
            ),
            next_attempt: now + chrono_of(interval),
        };
        (false, Some(alert))
    }

    /// Is the host staying off the network after an outage?
    pub(super) fn outage_hold(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.outage.hold_until.filter(|until| now < *until)
    }

    /// Fold one pass's network result into the outage state. `reached` and
    /// `unreachable` count the remotes (and forges) that did and did not
    /// answer. Returns the one alert an outage gets.
    pub(super) fn note_network(
        &mut self,
        reached: u32,
        unreachable: u32,
        first_error: Option<&str>,
        interval: Duration,
        now: DateTime<Utc>,
    ) -> Option<Alert> {
        if unreachable == 0 {
            if self.down.is_empty() {
                self.outage = Outage::default();
            }
            return None;
        }
        self.outage.streak = self.outage.streak.saturating_add(1);
        let next_attempt = if reached == 0 {
            let hold = Self::delay(interval, self.outage.streak).min(OUTAGE_HOLD_CAP);
            let until = now + chrono_of(hold);
            self.outage.hold_until = Some(until);
            until
        } else {
            self.outage.hold_until = None;
            now + chrono_of(interval)
        };
        if self.outage.streak < ALERT_AFTER || self.outage.alerted {
            return None;
        }
        self.outage.alerted = true;
        Some(Alert {
            root: PathBuf::new(),
            repo: None,
            kind: "network",
            failures: self.outage.streak,
            detail: format!(
                "{} workspace remote(s) are not answering ({unreachable} asked and {reached} \
                 reached in this pass, {} failing passes so far): {}",
                self.down.len().max(1),
                self.outage.streak,
                first_error.unwrap_or("no detail")
            ),
            next_attempt,
        })
    }
}
