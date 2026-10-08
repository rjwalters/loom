//! What a host remembers between workspace passes (#10718): the per-repo
//! verdict that lets an unchanged repo cost nothing, the per-repo backoff,
//! the state of a network outage, and what this process has already resynced.
//!
//! All of it is process memory. A restart (which is also how the running
//! version changes) starts from nothing, so the first pass after one checks
//! every workspace.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::{Alert, WState, WorkspaceReport, ALERT_AFTER, BACKOFF_CAP};

/// A settled workspace's remote head is looked at again after this many sync
/// intervals. See [`recheck_after`].
pub const RECHECK_TICKS: u32 = 15;

/// Longest a host stays off the network after passes in which no remote
/// answered.
pub const OUTAGE_HOLD_CAP: Duration = Duration::from_secs(30 * 60);

/// How long a settled verdict (`W0`, `W4`, `repo-ahead`, not installed) is
/// trusted before the remote head is looked at again: [`RECHECK_TICKS`] sync
/// intervals. A stale workspace (`W1`, `W3`) is looked at every tick.
#[must_use]
pub fn recheck_after(interval: Duration) -> Duration {
    interval.saturating_mul(RECHECK_TICKS)
}

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
    /// When the remote last confirmed `commit` is the head. `None` for a
    /// verdict read from the local remote-tracking ref only.
    pub(super) probed_at: Option<DateTime<Utc>>,
    /// The loop bound refused this workspace at this commit: nothing will be
    /// tried until the head moves, so it is rechecked like a settled one.
    pub(super) held: bool,
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

    /// Should the remote be asked for its head now?
    pub(super) fn probe_due(&self, now: DateTime<Utc>, interval: Duration) -> bool {
        if self.stale() && !self.held {
            // Another host may be resyncing it right now.
            return true;
        }
        self.probed_at
            .is_none_or(|at| now < at || now - at >= chrono_of(recheck_after(interval)))
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
    outage: Outage,
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
            FailureKind::Unreachable | FailureKind::Other => Self::delay(interval, failures),
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
            FailureKind::Other => failures >= ALERT_AFTER,
        };
        let alert = alerts.then(|| Alert {
            root: report.root.clone(),
            repo: report.repo.clone(),
            kind: match kind {
                FailureKind::Protected => "branch-protection",
                FailureKind::Unreachable | FailureKind::Other => "failure",
            },
            failures,
            detail: detail.to_string(),
            next_attempt,
        });
        (next_attempt, alert)
    }

    /// Remember what `root` is at `commit`. `probed` says the remote just
    /// confirmed that commit is the head.
    #[allow(clippy::too_many_arguments)] // one record; each argument is a field
    pub(super) fn settle(
        &mut self,
        root: &Path,
        branch: &str,
        commit: &str,
        version: &str,
        report: &WorkspaceReport,
        probed: bool,
        now: DateTime<Utc>,
        interval: Duration,
    ) {
        let before = self.verdicts.get(root);
        let probed_at = if probed {
            // The first confirmation is back-dated by a per-repo offset, so
            // sixty repos first seen in one pass are not all rechecked in
            // the same later pass.
            Some(match before.and_then(|v| v.probed_at) {
                Some(_) => now,
                None => now - chrono_of(spread(root, recheck_after(interval))),
            })
        } else {
            before
                .filter(|v| v.commit == commit)
                .and_then(|v| v.probed_at)
        };
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
                probed_at,
                held: false,
            },
        );
    }

    /// The remote confirmed `root`'s cached commit is still the head.
    pub(super) fn confirm(&mut self, root: &Path, now: DateTime<Utc>) {
        if let Some(v) = self.verdicts.get_mut(root) {
            v.probed_at = Some(now);
        }
    }

    /// The loop bound refused `root` at its cached commit.
    pub(super) fn hold(&mut self, root: &Path) {
        if let Some(v) = self.verdicts.get_mut(root) {
            v.held = true;
        }
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

/// A stable per-repo offset in `[0, window)`.
fn spread(root: &Path, window: Duration) -> Duration {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    let secs = window.as_secs().max(1);
    Duration::from_secs(hasher.finish() % secs)
}
