//! Codex session containers in `loom-daemon status` and `health` (issue
//! #10600, Epic #10452 Phase 3).
//!
//! Until this section existed the only way to learn that a host's Codex seats
//! were down was SSH and `docker ps`, or spotting `posture=not-running` in a
//! role log. On 2026-10-05 a host ran ~21.5 h with every session container
//! stopped; on 2026-10-06 another ran with all five stopped after a Docker
//! Desktop restart. Both were found by a person reading role-failure counts.
//!
//! # What it reads, and what it never does
//!
//! It stays inside the status time budget: it makes **no docker call**. It
//! reads
//!
//! * the seats the session watch published (`observability::ops::
//!   codex_session::published_seats`, every registered root);
//! * the newest published container snapshot
//!   ([`session_state::newest`]), with the drift verdict the watch already
//!   computed ([`session_state::effective_drift`], the reconciler's own
//!   definition);
//! * each seat's operator hold and drift-removal record, from disk; and
//! * the reconciler's last action per account ([`record_pass`]), in memory.
//!
//! A snapshot older than [`session_state::LATEST_MAX_AGE`], or none at all,
//! reads `unavailable`, never as any container state.
//!
//! # Degraded
//!
//! A seat that is not running, whose mounts are stale, or that has a standing
//! removal record makes the section **degraded**, with a one-line reason
//! naming the accounts. So does an unobservable snapshot while seats exist:
//! selection is blind then. A seat held by an operator `stop` is listed but
//! does not degrade it: the operator took it down on purpose, and a section
//! that stays amber for a deliberate stop would teach people to ignore it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::session_reconcile::{AccountOutcome, DeferReason, Outcome};
use crate::tokens_pool::session_drift_removal::DriftRemoval;
use crate::tokens_pool::session_seats::Seat;
use crate::tokens_pool::session_state::{container_running, Observed, Snapshot, LATEST_MAX_AGE};

/// `loom-daemon status`'s session-container section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionContainersReport {
    /// `available` (a fresh snapshot docker answered) or `unavailable`.
    pub observation: String,
    /// Why the containers could not be observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// Age of the snapshot read, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_age_secs: Option<u64>,
    /// Whether this daemon runs the session reconciler (`None`: unknown).
    #[serde(default)]
    pub reconciler_enabled: Option<bool>,
    pub degraded: bool,
    /// One line naming what is wrong, when degraded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
    pub accounts: Vec<SeatStatus>,
}

/// One session-managed Codex account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatStatus {
    pub account: String,
    pub container: String,
    /// `running`, `stopped`, `missing`, `restarting`, or `unavailable`.
    pub state: String,
    pub mounts: SeatMounts,
    /// `host`, `private-clone` or `unverified` (not running, unlabelled or
    /// not hardened: `spawn-codex.sh`'s posture gate refuses it).
    pub posture: String,
    /// An operator `accounts session stop` holds it down.
    pub held: bool,
    /// The reconciler removed it for a denied mount and keeps it down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removal: Option<SeatRemoval>,
    /// The reconciler's last action on it in this daemon process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconcile: Option<LastReconcile>,
    /// This seat degrades the section.
    pub degraded: bool,
}

/// A seat's mounts against the registry and the denials.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatMounts {
    /// `ok`, `stale`, `unknown` (no verdict: the registry was unreadable, or
    /// the container has no workspace label), `n/a` (missing, or
    /// private-clone) or `unavailable`.
    pub verdict: String,
    /// Registered roots it does not mount.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<PathBuf>,
    /// Mounts no longer registered (not counting `denied`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<PathBuf>,
    /// Mounts `session start` refuses today (home, `firewall: true`): the
    /// containment case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied: Vec<PathBuf>,
}

/// A standing `.session-drift-removed.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatRemoval {
    pub workspace: PathBuf,
    pub denied: Vec<PathBuf>,
    pub reason: String,
    pub removed_at_unix_ms: u64,
}

/// The reconciler's last action on an account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastReconcile {
    pub action: String,
    pub at_unix: u64,
}

impl SeatMounts {
    /// `ok`, `stale (missing N, extra M, denied K)`, …
    #[must_use]
    pub fn describe(&self) -> String {
        if self.verdict == "stale" {
            format!(
                "stale (missing {}, extra {}, denied {})",
                self.missing.len(),
                self.extra.len(),
                self.denied.len()
            )
        } else {
            self.verdict.clone()
        }
    }
}

impl SeatRemoval {
    /// `removed (denied mount: <path>, …)`.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.denied.is_empty() {
            return format!("removed ({})", self.reason);
        }
        format!("removed (denied mount: {})", join(&self.denied))
    }
}

impl SeatStatus {
    /// What is wrong with it, for the degraded reason; `None` when serving
    /// (or held).
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        if !self.degraded {
            return None;
        }
        Some(match &self.removal {
            Some(removal) => removal.describe(),
            None if self.state == "running" => format!("mounts {}", self.mounts.describe()),
            None => self.state.clone(),
        })
    }

    /// The `status` row after the account name.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = vec![self.state.clone()];
        if self.mounts.verdict != "n/a" && self.mounts.verdict != "unavailable" {
            parts.push(format!("mounts {}", self.mounts.describe()));
        }
        parts.push(format!("posture {}", self.posture));
        if self.held {
            parts.push("held (operator stop)".into());
        }
        if let Some(removal) = &self.removal {
            parts.push(removal.describe());
        }
        parts.push(format!(
            "last reconcile: {}",
            self.last_reconcile
                .as_ref()
                .map_or("none", |last| last.action.as_str())
        ));
        parts.join(", ")
    }
}

fn join(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

// ============================================================================
// The reconciler's last action per account
// ============================================================================

static ACTIONS: Mutex<BTreeMap<String, LastReconcile>> = Mutex::new(BTreeMap::new());
/// 0 unknown, 1 enabled, 2 disabled.
static RECONCILER: AtomicU8 = AtomicU8::new(0);

/// Record whether this daemon runs the reconciler (`spawn_from_config`).
pub fn set_reconciler_enabled(enabled: bool) {
    RECONCILER.store(if enabled { 1 } else { 2 }, Ordering::Relaxed);
}

fn reconciler_enabled() -> Option<bool> {
    match RECONCILER.load(Ordering::Relaxed) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

fn utc(unix: u64) -> String {
    i64::try_from(unix)
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map_or_else(|| unix.to_string(), |at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    let mut out: String = line.chars().take(160).collect();
    if line.chars().count() > 160 {
        out.push('…');
    }
    out
}

/// The action an outcome records; `None` when the pass did nothing worth
/// remembering (a healthy or held container keeps the previous action).
#[must_use]
pub fn action_of(outcome: &Outcome) -> Option<String> {
    Some(match outcome {
        Outcome::Held | Outcome::Running => return None,
        Outcome::Restarting => "waiting (Docker is restarting it)".into(),
        Outcome::CrashLoop => "crash loop reported (not reused)".into(),
        Outcome::Resumed => "started".into(),
        Outcome::Recreated { .. } => "recreated".into(),
        Outcome::PrivateCloneSkipped => "skipped (private-clone)".into(),
        Outcome::DriftRecreated { .. } => "recreated (mount drift)".into(),
        Outcome::DriftDeferred { reason } => format!(
            "deferred ({})",
            match reason {
                DeferReason::Busy => "in-flight",
                DeferReason::LockUnknown => "dispatch lock unknown",
                DeferReason::Changed => "container changed",
                DeferReason::NothingIntended => "registry lists nothing under its workspace",
                DeferReason::RootUnavailable => "a registered root is unavailable",
                DeferReason::InputsUnreadable => "registry or roster unreadable",
            }
        ),
        Outcome::DriftUnachievable => "drift not actionable (see the daemon log)".into(),
        Outcome::DriftRemoved { .. } => "removed (denied mount)".into(),
        Outcome::DriftRemovalStands => "kept down (removal stands)".into(),
        Outcome::BackingOff { retry_at } => format!("backoff until {}", utc(*retry_at)),
        Outcome::DockerUnavailable { retry_at, .. } => {
            format!("docker unavailable; backoff until {}", utc(*retry_at))
        }
        Outcome::Failed { error, retry_at } => {
            format!("failed; backoff until {}: {}", utc(*retry_at), first_line(error))
        }
    })
}

/// Fold one pass's outcomes into `actions`. A `BackingOff` keeps a previous
/// action that already names the backoff (it would only hide its cause).
pub fn fold_pass(
    actions: &mut BTreeMap<String, LastReconcile>,
    outcomes: &[AccountOutcome],
    now: u64,
) {
    for outcome in outcomes {
        let Some(action) = action_of(&outcome.outcome) else {
            continue;
        };
        let keep = matches!(outcome.outcome, Outcome::BackingOff { .. })
            && actions
                .get(&outcome.name)
                .is_some_and(|last| last.action.contains("backoff until"));
        if !keep {
            actions.insert(
                outcome.name.clone(),
                LastReconcile {
                    action,
                    at_unix: now,
                },
            );
        }
    }
}

/// Record one reconcile pass (`session_reconcile::run_tick`).
pub fn record_pass(outcomes: &[AccountOutcome], now: u64) {
    let mut actions = ACTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    fold_pass(&mut actions, outcomes, now);
}

// ============================================================================
// Building the section
// ============================================================================

/// Everything the section is built from. Pure inputs, so it is tested
/// without docker or a daemon.
pub struct Inputs<'a> {
    pub seats: &'a [Seat],
    /// The newest snapshot and its age.
    pub snapshot: Option<(Duration, &'a Snapshot)>,
    pub actions: &'a BTreeMap<String, LastReconcile>,
    pub reconciler_enabled: Option<bool>,
    /// Whether any of an account's profiles holds an operator hold.
    pub held: &'a dyn Fn(&[PathBuf]) -> bool,
    /// The removal record standing in any of an account's profiles.
    pub removal: &'a dyn Fn(&[PathBuf]) -> Option<DriftRemoval>,
}

fn posture(inspect: &serde_json::Value) -> &'static str {
    use crate::session_exec::posture::{classify, Posture};
    match classify(inspect) {
        Posture::Host => "host",
        Posture::PrivateClone => "private-clone",
        _ => "unverified",
    }
}

fn observed_seat(observed: Option<&Observed>) -> (String, SeatMounts, String) {
    let Some(observed) = observed else {
        let mounts = SeatMounts {
            verdict: "n/a".into(),
            ..SeatMounts::default()
        };
        return ("missing".into(), mounts, "unverified".into());
    };
    let inspect = &observed.inspect;
    let state = if inspect["State"]["Restarting"] == serde_json::Value::Bool(true) {
        "restarting"
    } else if container_running(inspect) {
        "running"
    } else {
        "stopped"
    };
    let private = crate::tokens_pool::session_state::is_private_clone(inspect);
    let mounts = match &observed.drift {
        _ if private => SeatMounts {
            verdict: "n/a".into(),
            ..SeatMounts::default()
        },
        None => SeatMounts {
            verdict: "unknown".into(),
            ..SeatMounts::default()
        },
        Some(drift) => SeatMounts {
            verdict: if drift.is_empty() { "ok" } else { "stale" }.into(),
            missing: drift.drift.missing.clone(),
            extra: drift.extra_not_denied(),
            denied: drift.denied.clone(),
        },
    };
    (state.into(), mounts, posture(inspect).into())
}

/// The section, or `None` when this host has no session-managed seat (so a
/// host without one renders exactly as before).
#[must_use]
pub fn build(inputs: &Inputs<'_>) -> Option<SessionContainersReport> {
    if inputs.seats.is_empty() {
        return None;
    }
    let (map, unavailable) = match inputs.snapshot {
        None => (None, Some("no container snapshot has been published yet".to_string())),
        Some((age, _)) if age > LATEST_MAX_AGE => (
            None,
            Some(format!(
                "the newest container snapshot is {}s old (over {}s): the session watch has not \
                 published since",
                age.as_secs(),
                LATEST_MAX_AGE.as_secs()
            )),
        ),
        Some((_, Snapshot::Unavailable(reason))) => {
            (None, Some(format!("docker could not be queried: {reason}")))
        }
        Some((_, Snapshot::Available(map))) => (Some(map), None),
    };
    let accounts: Vec<SeatStatus> = inputs
        .seats
        .iter()
        .map(|seat| {
            let (state, mounts, posture) = match map {
                Some(map) => observed_seat(map.get(&seat.container)),
                None => (
                    "unavailable".to_string(),
                    SeatMounts {
                        verdict: "unavailable".into(),
                        ..SeatMounts::default()
                    },
                    "unverified".to_string(),
                ),
            };
            let held = (inputs.held)(&seat.profiles);
            let removal = (inputs.removal)(&seat.profiles).map(|r| SeatRemoval {
                workspace: r.workspace,
                denied: r.denied,
                reason: r.reason,
                removed_at_unix_ms: r.removed_at_unix_ms,
            });
            let degraded = !held
                && (removal.is_some()
                    || (map.is_some() && (state != "running" || mounts.verdict == "stale")));
            SeatStatus {
                account: seat.account.clone(),
                container: seat.container.clone(),
                state,
                mounts,
                posture,
                held,
                removal,
                last_reconcile: inputs.actions.get(&seat.account).cloned(),
                degraded,
            }
        })
        .collect();
    let problems: Vec<String> = accounts
        .iter()
        .filter_map(|seat| Some(format!("{} {}", seat.account, seat.problem()?)))
        .collect();
    let degraded_reason = match (&unavailable, problems.is_empty()) {
        (Some(why), true) => Some(format!("session containers unobservable: {why}")),
        (Some(why), false) => {
            Some(format!("session containers unobservable: {why}; {}", problems.join("; ")))
        }
        (None, true) => None,
        (None, false) => Some(format!(
            "{} of {} session seat(s) not serving: {}",
            problems.len(),
            accounts.len(),
            problems.join("; ")
        )),
    };
    Some(SessionContainersReport {
        observation: if unavailable.is_some() {
            "unavailable"
        } else {
            "available"
        }
        .into(),
        unavailable_reason: unavailable,
        snapshot_age_secs: inputs.snapshot.map(|(age, _)| age.as_secs()),
        reconciler_enabled: inputs.reconciler_enabled,
        degraded: degraded_reason.is_some(),
        degraded_reason,
        accounts,
    })
}

/// The section as this daemon sees it now. No docker call.
#[must_use]
pub fn report() -> Option<SessionContainersReport> {
    let seats: Arc<Vec<Seat>> = crate::observability::ops::codex_session::published_seats()?;
    let newest = crate::tokens_pool::session_state::newest();
    let actions = ACTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    build(&Inputs {
        seats: &seats,
        snapshot: newest
            .as_ref()
            .map(|(age, snapshot)| (*age, snapshot.as_ref())),
        actions: &actions,
        reconciler_enabled: reconciler_enabled(),
        held: &crate::tokens_pool::session_hold::held_across,
        removal: &crate::tokens_pool::session_drift_removal::read,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "session_status_tests.rs"]
mod tests;
