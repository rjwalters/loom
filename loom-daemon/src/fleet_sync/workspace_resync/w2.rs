//! W2 of the workspace resync (#10718): the loop bound, the claim, and the
//! resync itself in a throwaway worktree. See the parent module for the
//! whole pass.

use std::path::Path;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};

use super::memory::FailureKind;
use super::{
    daemon, git, record_failure, refused, stamp, Alert, Candidate, Env, Memory, Scan, WState,
    WorkspaceReport,
};
use crate::fleet_store::resync_claim::{stale_after, Acquire, Claimant, ForgeUnreachable, Held};
use crate::init::payload::{gate_metadata, resync_workspace_with, Payload, ResyncOutcome};

// ============================================================================
// The loop bound
// ============================================================================

/// Whether the loop bound lets this host resync a repo now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Bound {
    /// Yes.
    Clear,
    /// This repo was already resynced to the running version and is stale
    /// again. Never resynced a second time at this version.
    Loop(String),
}

/// The loop bound for the default branch at `commit`, which the caller knows
/// to be stale. It holds whatever made the repo stale again: a host running
/// the same version with a different payload, a person or a tool undoing the
/// resync, or a defect in the diff.
///
/// * This process resynced the repo to the running version already: `Loop`.
/// * The branch's recent history carries a resync commit for the running
///   version, from any host: `Loop`.
///
/// So a repo gets at most one daemon resync commit per version fleet-wide.
/// That is the whole bound (#10987 removed a 15-minute wait between any two
/// resync commits): a resync to another version is not a loop, and a host
/// never installs a version older than the repo's. Local: it reads history
/// already in the clone.
pub(super) fn bound(env: &Env<'_>, root: &Path, commit: &str, memory: &Memory) -> Result<Bound> {
    let version = env.running.to_string();
    if let Some((_, earlier)) = memory.resynced.get(root).filter(|(v, _)| *v == version) {
        return Ok(Bound::Loop(format!(
            "this host already resynced it to v{version} in {} and it is stale again",
            short(earlier)
        )));
    }
    let past = git::past_resyncs(root, commit)?;
    if let Some(p) = past.iter().find(|p| p.version == version) {
        return Ok(Bound::Loop(format!(
            "{} already resynced it to v{version} in {} and it is stale again",
            p.host,
            short(&p.commit)
        )));
    }
    Ok(Bound::Clear)
}

/// Say that the loop bound refused `report`'s repo: in the report, in the
/// log, and (once per repo and version) as a `resync-loop` alert.
fn refuse_loop(
    env: &Env<'_>,
    report: &mut WorkspaceReport,
    detail: &str,
    memory: &mut Memory,
    alerts: &mut Vec<Alert>,
) {
    let version = env.running;
    report.reason = Some(format!("resync-loop: {detail}; not resynced again at v{version}"));
    if !memory
        .noted
        .insert(format!("loop:{}:{version}", report.root.display()))
    {
        return;
    }
    log::error!(
        "workspace_resync: {}: resync-loop: {detail}; this host will not resync it again at \
         v{version}",
        report.repo.as_deref().unwrap_or("unknown repo")
    );
    alerts.push(Alert {
        root: report.root.clone(),
        repo: report.repo.clone(),
        kind: "resync-loop",
        failures: 1,
        detail: detail.to_string(),
        next_attempt: (env.clock)(),
    });
}

// ============================================================================
// W2
// ============================================================================

/// How a resync under the claim ended.
enum Done {
    /// One commit is on the default branch.
    Pushed(String),
    /// Nothing left to do at this commit: another host or a person did it.
    AlreadyCurrent(String),
    /// The re-read found a state this host never writes to.
    NoWrite(WState, Option<String>),
    /// Stopped before the push, on purpose. Not a failure.
    Aborted(String),
    /// The loop bound refused it once the claim was held.
    Loop(String),
    /// The branch moved on both tries. Next tick.
    Moved,
    /// A rule on the remote refused the push.
    Protected(String),
}

/// Releases the claim when dropped, so a panic or an early return inside the
/// resync does not leave it held for the stale window.
struct ClaimGuard<'a> {
    claimant: &'a Claimant<'a>,
    held: &'a Held,
    clock: &'a dyn Fn() -> DateTime<Utc>,
    nwo: &'a str,
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.claimant.release(self.held, (self.clock)()) {
            log::warn!(
                "workspace_resync: {}: could not release the claim ({e:#}); it expires on its own",
                self.nwo
            );
        }
    }
}

/// Try the claim for `candidate` and, if won, resync. Returns whether this
/// pass is done with W2 (so the remaining candidates wait a tick).
pub(super) fn attempt(
    env: &Env<'_>,
    candidate: &Candidate,
    report: &mut WorkspaceReport,
    memory: &mut Memory,
    scan: &mut Scan,
) -> bool {
    let root = report.root.clone();
    // Loom writes only to repositories it manages and can write (#9548). A
    // refusal is this host's standing, not a failure: another host may hold
    // a credential that can, so there is no backoff and no alert.
    if let Err(why) = (env.may_write)(&root, &candidate.nwo) {
        if memory
            .noted
            .insert(format!("scope:{}:{why}", root.display()))
        {
            log::warn!(
                "workspace_resync: {}: not resynced from this host: {why} (reported once)",
                candidate.nwo
            );
        }
        report.reason = Some(format!("not written from this host: {why}"));
        return false;
    }
    // The loop bound, before any claim: a refusal costs no forge request.
    match bound(env, &root, &candidate.commit, memory) {
        Ok(Bound::Clear) => {}
        Ok(Bound::Loop(detail)) => {
            refuse_loop(env, report, &detail, memory, &mut scan.alerts);
            return false;
        }
        Err(e) => {
            let detail = format!("{e:#}");
            record_failure(env, report, FailureKind::Other, &detail, memory, &mut scan.alerts);
            return false;
        }
    }
    // The gate again, immediately before the claim: classification may have
    // taken a while, and a pause or a roll may have started since.
    if let Err(why) = (env.gate)() {
        report.reason = Some(format!("{} before the claim", why.host_note()));
        return true;
    }
    let forge = (env.forge)(&root, &candidate.nwo);
    let version = env.running.to_string();
    let claimant = Claimant {
        read: forge.as_read(),
        write: forge.as_write(),
        repo: &candidate.nwo,
        host: env.host,
        version: &version,
        stale_after: stale_after(env.interval),
    };
    let acquired = git::tree_of(&root, &candidate.commit)
        .and_then(|tree| claimant.acquire(&tree, (env.clock)()));
    let held = match acquired {
        Ok(Acquire::Won(held)) => held,
        Ok(Acquire::HeldBy { host, since }) => {
            log::debug!("workspace_resync: {}: claim held by {host}", candidate.nwo);
            report.reason = Some(format!("claim held by {host} since {}", stamp(since)));
            return false;
        }
        Ok(Acquire::Lost(why)) => {
            log::debug!("workspace_resync: {}: {why}", candidate.nwo);
            report.reason = Some(format!("claim not taken this tick: {why}"));
            return false;
        }
        Err(e) => {
            fail_attempt(env, &candidate.nwo, report, &e.context("claim"), memory, scan);
            return true;
        }
    };
    let outcome = {
        let _claim = ClaimGuard {
            claimant: &claimant,
            held: &held,
            clock: env.clock,
            nwo: &candidate.nwo,
        };
        resync_under_claim(env, candidate, &root, &claimant, &held, memory)
    };
    match outcome {
        Ok(Done::Pushed(commit)) => {
            memory.backoff.remove(&root);
            log::info!(
                "workspace_resync: {}: resynced installed Loom to v{version} ({commit})",
                candidate.nwo
            );
            report.state = WState::W0;
            report.installed = Some(version.clone());
            report.reason = None;
            settle_current(env, &root, &candidate.branch, &commit, report, memory);
            memory
                .resynced
                .insert(root.clone(), (version.clone(), commit.clone()));
            report.reason = Some(format!("resynced to v{version} in {}", short(&commit)));
        }
        Ok(Done::AlreadyCurrent(commit)) => {
            memory.backoff.remove(&root);
            report.state = WState::W0;
            report.reason = None;
            settle_current(env, &root, &candidate.branch, &commit, report, memory);
            report.reason = Some("already current once the claim was held; no commit".to_string());
        }
        Ok(Done::NoWrite(state, reason)) => (report.state, report.reason) = (state, reason),
        Ok(Done::Aborted(why)) => {
            log::info!("workspace_resync: {}: resync abandoned: {why}", candidate.nwo);
            report.reason = Some(why);
        }
        Ok(Done::Loop(detail)) => refuse_loop(env, report, &detail, memory, &mut scan.alerts),
        Ok(Done::Moved) => {
            report.reason = Some("the default branch moved twice; retrying next tick".to_string());
        }
        Ok(Done::Protected(rule)) => {
            let detail = format!("push refused by a branch rule: {rule}");
            record_failure(env, report, FailureKind::Protected, &detail, memory, &mut scan.alerts);
        }
        Err(e) => fail_attempt(env, &candidate.nwo, report, &e, memory, scan),
    }
    true
}

/// Remember that `root` is current at `commit`, so the next tick asks nobody.
fn settle_current(
    env: &Env<'_>,
    root: &Path,
    branch: &str,
    commit: &str,
    report: &WorkspaceReport,
    memory: &mut Memory,
) {
    let version = env.running.to_string();
    memory.settle(root, branch, commit, &version, report);
}

/// Count a failed attempt: an unreachable remote or forge for the host (one
/// alert for the outage), anything else against the repo.
fn fail_attempt(
    env: &Env<'_>,
    nwo: &str,
    report: &mut WorkspaceReport,
    error: &anyhow::Error,
    memory: &mut Memory,
    scan: &mut Scan,
) {
    let detail = format!("{error:#}");
    let unreachable = error.chain().any(|cause| {
        cause.downcast_ref::<git::Unreachable>().is_some()
            || cause.downcast_ref::<ForgeUnreachable>().is_some()
    });
    let refused = error
        .chain()
        .any(|cause| cause.downcast_ref::<git::Refused>().is_some());
    let kind = if unreachable {
        scan.no_answer(&report.root, nwo, &detail, memory);
        FailureKind::Unreachable
    } else if refused {
        FailureKind::Refused
    } else {
        FailureKind::Other
    };
    record_failure(env, report, kind, &detail, memory, &mut scan.alerts);
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// W2, with the claim held: re-read, apply in a throwaway worktree, commit,
/// push. A moved branch is retried once.
fn resync_under_claim(
    env: &Env<'_>,
    candidate: &Candidate,
    root: &Path,
    claimant: &Claimant<'_>,
    held: &Held,
    memory: &Memory,
) -> Result<Done> {
    for _ in 0..2 {
        // Everything is read again now that the claim is ours: another host
        // or a person may have resynced it, or a newer host moved it ahead.
        let commit = git::fetch(root, &candidate.branch)?;
        let raw = git::metadata_at(root, &commit)?.ok_or_else(|| {
            anyhow!("the install metadata is gone from origin/{}", candidate.branch)
        })?;
        if let Err(refusal) = gate_metadata(&raw, &daemon(env)) {
            let (state, reason) = refused(&refusal);
            return Ok(Done::NoWrite(state, reason));
        }
        let payload = env.payload.get()?;
        git::clean_stale_worktrees(root);
        // Removed when it goes out of scope, on every path out of the block.
        let worktree = git::add_worktree(root, &commit)?;
        let step = Step {
            env,
            candidate,
            root,
            worktree: worktree.path(),
            commit: &commit,
            claimant,
            held,
            memory,
        };
        match step.run(payload)? {
            Done::Moved => {}
            other => return Ok(other),
        }
    }
    Ok(Done::Moved)
}

/// One try at W2 inside the throwaway worktree.
struct Step<'a, 'e> {
    env: &'a Env<'e>,
    candidate: &'a Candidate,
    root: &'a Path,
    worktree: &'a Path,
    commit: &'a str,
    claimant: &'a Claimant<'a>,
    held: &'a Held,
    memory: &'a Memory,
}

impl Step<'_, '_> {
    fn run(&self, payload: &Payload) -> Result<Done> {
        let (env, root, worktree) = (self.env, self.root, self.worktree);
        let written = match resync_workspace_with(payload, worktree)? {
            ResyncOutcome::Refused(refusal) => {
                let (state, reason) = refused(&refusal);
                return Ok(Done::NoWrite(state, reason));
            }
            ResyncOutcome::Unchanged => return Ok(Done::AlreadyCurrent(self.commit.to_string())),
            ResyncOutcome::Applied { written } => written,
        };
        let message = git::resync_message(env.host, &env.running.to_string());
        let Some(new) = git::commit(worktree, root, &written, &message)? else {
            return Ok(Done::AlreadyCurrent(self.commit.to_string()));
        };
        // There is something to push. The loop bound again, on the head that
        // was just read: another host may have resynced this version while
        // the claim was being taken.
        if let Bound::Loop(detail) = bound(env, root, self.commit, self.memory)? {
            return Ok(Done::Loop(detail));
        }
        // The gate (which carries `paused`, a roll and the floor) and the
        // fence, immediately before the push. A roll that started since the
        // claim sets the drain flag; a claim that was taken over is no longer
        // ours.
        if let Err(why) = (env.gate)() {
            return Ok(Done::Aborted(format!("{} before the push", why.host_note())));
        }
        if let Err(why) = self.claimant.fence(self.held, (env.clock)())? {
            return Ok(Done::Aborted(format!("{why}; not pushing")));
        }
        Ok(match git::push(worktree, root, &self.candidate.branch)? {
            git::Push::Accepted => Done::Pushed(new),
            git::Push::NonFastForward => Done::Moved,
            git::Push::Protected(rule) => Done::Protected(rule),
        })
    }
}
