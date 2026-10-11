//! Resuming a role run a daemon roll paused (issue #10832; design
//! `docs/design/daemon-roll-pause-resume.md` §4 "Role run", §9).
//!
//! A role run (Champion, Curator, Judge, Auditor, Guide, Doctor) is paused and
//! resumed like a sweep (operator decision Q7). It has no registry entry and
//! no claim lock, so the three things a sweep's lock carries are seeded here
//! instead, **before** the run is relaunched:
//!
//! 1. **The in-progress guard.** The resumed run takes `(root, role, lane 0)`
//!    in the role runner's shared [`InProgressGuard`] through the same
//!    admission a live tick uses ([`RoleRunGuard::try_acquire`]), so no
//!    interval or idle tick starts a second run of the role beside it. Lane
//!    `0` is the one classic run per `(root, role)`; a resumed run is always
//!    that one (only doctor's per-repository width opens lanes above it, and
//!    those are admitted by the live dispatcher, never by a resume).
//! 2. **The issue-creation mutex**, when the run held it
//!    ([`crate::issue_creation_mutex::seed_hold`]).
//! 3. **Its claim breadcrumb**, carried to the new run's item dir so a later
//!    requeue still releases the claim it took
//!    ([`crate::roll_pause::claim_breadcrumb`]).
//!
//! The relaunch itself is an ordinary role invocation through
//! [`ScriptRoleInvocationRunner`] (write-scope gate, runtime admission, model
//! resolution, log, process group, provenance), with two differences: it
//! resumes the saved session instead of starting `/loom:<role>`, and its
//! timeout is what the run had left when it was paused, so the roll does not
//! count against the run's budget.
//!
//! [`release_claim`] is the requeue half: a role run that cannot be resumed
//! gives back the claim label its breadcrumb names.

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc;

use super::*;
use crate::roll_pause::claim_breadcrumb::{self, ClaimBreadcrumb};
use crate::sweep_registry::resume_handle::{DispatchSession, RollResumeLaunch};
use crate::sweep_registry::roll_resume::{RollResumeRefusal, REASON_RESUME_FAILED};

/// Requeue reason: the role is no longer enabled for its root.
pub(crate) const REASON_ROLE_DISABLED: &str = "role-disabled";

/// The least time a resumed run gets, whatever it had left.
const MIN_RESUME_TIMEOUT: Duration = Duration::from_secs(60);

/// What [`ScriptRoleInvocationRunner`] carries for a roll resume.
#[derive(Debug, Clone)]
pub(crate) struct RoleRollResume {
    pub(crate) launch: RollResumeLaunch,
    /// Set when H5 gave up waiting for this launch: it must not spawn.
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl RoleRollResume {
    /// Why this resume must not launch on `admission`, if it must not.
    pub(super) fn refuse(
        &self,
        admission: Option<&crate::runtime_admission::ResolvedRuntime>,
    ) -> Option<String> {
        if self.cancelled.load(AtomicOrdering::SeqCst) {
            return Some("roll resume cancelled before it was spawned (#10832)".to_string());
        }
        let admitted = admission.map_or("claude", |a| a.runtime.as_str());
        (admitted != self.launch.runtime).then(|| {
            format!(
                "roll resume refused: the saved session is a {} session but the role now \
                 resolves to {admitted} (#10832)",
                self.launch.runtime
            )
        })
    }

    /// The resumed run's pause-and-roll identity, with the paused run's claim
    /// breadcrumb carried to its item dir.
    pub(super) fn session(&self, item: &str, workspace_root: &Path) -> Option<DispatchSession> {
        let session = DispatchSession::resumed(item, workspace_root, &self.launch)?;
        claim_breadcrumb::carry(
            &crate::roll_pause::item_dir(&session.pause_root, &self.launch.resume_of),
            &session.item_dir(),
        );
        Some(session)
    }
}

/// One paused role run to resume.
#[derive(Debug, Clone)]
pub(crate) struct RoleResumeSpec {
    pub(crate) root: PathBuf,
    pub(crate) role: String,
    pub(crate) launch: RollResumeLaunch,
    /// What the run had left of its timeout when it was paused.
    pub(crate) timeout_remaining: Option<Duration>,
    pub(crate) holds_issue_creation_mutex: bool,
    /// Spawn binary and `gh` overrides (tests only).
    pub(crate) spawn_bin: Option<PathBuf>,
    pub(crate) gh_bin: Option<PathBuf>,
}

/// How a resumed role run is doing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RoleResumeState {
    /// Its process is running.
    Running,
    /// It ran to completion.
    Succeeded,
    /// It ended without success; the string says how.
    Failed(String),
}

/// A resumed role run H5 is still watching.
#[derive(Debug)]
pub(crate) struct RoleResumeHandle {
    /// The resumed run's own item id.
    pub(crate) item_id: String,
    /// Its process, when one was seen.
    pub(crate) pid: Option<u32>,
    outcome: mpsc::Receiver<RoleTickOutcome>,
    finished: Option<RoleTickOutcome>,
}

impl RoleResumeHandle {
    /// The run's state now.
    pub(crate) fn state(&mut self) -> RoleResumeState {
        if self.finished.is_none() {
            self.finished = self.outcome.try_recv().ok();
        }
        match &self.finished {
            None => RoleResumeState::Running,
            Some(RoleTickOutcome::Success) => RoleResumeState::Succeeded,
            Some(other) => RoleResumeState::Failed(format!("{other:?}")),
        }
    }
}

/// The role's static name, when the role is still enabled for `root`.
///
/// # Errors
/// `role-disabled` when the role runner is off for the root, the role is not
/// among its interval or on-idle roles, or the role's own config gate is shut.
pub(crate) fn enabled_role(
    root: &Path,
    role: &str,
) -> std::result::Result<&'static str, RollResumeRefusal> {
    let disabled = |why: String| Err(RollResumeRefusal::new(REASON_ROLE_DISABLED, why));
    let config = read_role_runner_config(root);
    if !resolve_enabled(&config) {
        return disabled(format!("the role runner is disabled for {}", root.display()));
    }
    let Some(spec) = resolve_roles(&config)
        .into_iter()
        .chain(resolve_on_idle_roles(&config))
        .find(|spec| spec.name.eq_ignore_ascii_case(role))
    else {
        return disabled(format!("`{role}` is not an enabled role for {}", root.display()));
    };
    Ok(spec.name)
}

/// Relaunch a paused role run from its saved session. Seeds the in-progress
/// guard and (when the run held it) the issue-creation mutex first, then runs
/// the invocation on its own thread, which holds both until the run ends.
///
/// Returns once the run's process exists, or the run has already ended, or
/// `wait` has passed (the launch is then cancelled and reported as failed).
///
/// # Errors
/// With a design §9 reason: `role-disabled`, `guard-refused:role-in-progress`,
/// or `session-resume-failed`.
pub(crate) fn launch(
    spec: &RoleResumeSpec,
    in_progress: &InProgressGuard,
    wait: Duration,
) -> std::result::Result<RoleResumeHandle, RollResumeRefusal> {
    let name = enabled_role(&spec.root, &spec.role)?;
    let Some(guard) = RoleRunGuard::try_acquire(in_progress.clone(), spec.root.clone(), name)
    else {
        return Err(RollResumeRefusal::guard(
            "role-in-progress",
            format!("a `{name}` run is already in progress for {}", spec.root.display()),
        ));
    };
    let seeded = spec
        .holds_issue_creation_mutex
        .then(|| crate::issue_creation_mutex::seed_hold(&spec.root, name))
        .flatten();
    let cancelled = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let old_item = spec.launch.resume_of.clone();
    let timeout = spec
        .timeout_remaining
        .unwrap_or(DEFAULT_ROLE_TIMEOUT)
        .max(MIN_RESUME_TIMEOUT);
    let mut runner = ScriptRoleInvocationRunner::new(spec.root.clone()).with_timeout(timeout);
    if let Some(bin) = &spec.spawn_bin {
        runner = runner.with_spawn_bin(bin.clone());
    }
    if let Some(gh) = &spec.gh_bin {
        runner = runner.with_gh_bin(gh.clone());
    }
    runner.roll_resume = Some(RoleRollResume {
        launch: spec.launch.clone(),
        cancelled: cancelled.clone(),
    });
    let root = spec.root.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("role-resume-{name}"))
        .spawn(move || {
            // Held for the whole resumed run, exactly as a live tick holds them.
            let (_guard, _seeded) = (guard, seeded);
            // The prompt is unused: a resume passes the saved session instead.
            let outcome = runner.invoke(name, "");
            record_role_tick(name, &root, &outcome);
            let _ = tx.send(outcome);
        });
    if let Err(e) = spawned {
        return Err(RollResumeRefusal::new(
            REASON_RESUME_FAILED,
            format!("could not start the resume thread: {e}"),
        ));
    }
    let mut handle = RoleResumeHandle {
        item_id: String::new(),
        pid: None,
        outcome: rx,
        finished: None,
    };
    let deadline = Instant::now() + wait;
    loop {
        let live = crate::roll_pause::live_runs::snapshot()
            .into_iter()
            .find(|run| run.resume.as_ref().is_some_and(|l| l.resume_of == old_item));
        if let Some(run) = live {
            handle.item_id = run.item_id;
            handle.pid = Some(run.pid);
            return Ok(handle);
        }
        match handle.state() {
            RoleResumeState::Running => {}
            // It started, did its work and ended before this poll saw it.
            RoleResumeState::Succeeded => return Ok(handle),
            RoleResumeState::Failed(how) => {
                return Err(RollResumeRefusal::new(REASON_RESUME_FAILED, how))
            }
        }
        if Instant::now() >= deadline {
            cancelled.store(true, AtomicOrdering::SeqCst);
            return Err(RollResumeRefusal::new(
                REASON_RESUME_FAILED,
                format!("the resumed `{name}` run had not spawned after {}s", wait.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Requeue half for a role run (design §9): release the claim label its
/// breadcrumb names, and say why on the PR or issue it was on. Refused where
/// this installation may not write (#9548), like every other role write.
///
/// # Errors
/// When the write scope refuses, or a `gh` call fails. The caller leaves the
/// item for the role's own staleness rule, which releases the same claim.
pub(crate) fn release_claim(
    root: &Path,
    gh: &Path,
    claim: &ClaimBreadcrumb,
    comment: &str,
) -> std::result::Result<(), String> {
    use crate::claim_reconciliation::gh_call;
    if !crate::write_scope::gate_root_with(root, gh, "the roll requeue of a role claim") {
        return Err("write scope refused (#9548)".to_string());
    }
    let (kind, number) = (claim.on.as_str(), claim.number.to_string());
    let repo_flag = gh_call::loom_repo_flag();
    let run = |op: &'static str, args: Vec<&str>| {
        let inv = gh_call::write(op, gh, root)
            .args(args)
            .args(repo_flag.iter().map(String::as_str));
        match gh_call::output(inv) {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => {
                Err(format!("{op} exited {:?}: {}", out.status.code(), gh_call::stderr(&out)))
            }
            Err(e) => Err(format!("{op}: {e:#}")),
        }
    };
    run(
        "roll.role_claim_release",
        vec![kind, "edit", &number, "--remove-label", &claim.label],
    )?;
    run("roll.role_requeue_comment", vec![kind, "comment", &number, "--body", comment])
}

/// How far the forge's clock may run ahead of this host's before a `labeled`
/// event counts as later than the paused run's stop.
const RECLAIM_SLACK_SECS: i64 = 30;

/// Whether `claim` is still the paused role run's to give back: the sweep
/// path's "still ours" re-check (`SweepRegistry::roll_claim_still_ours`) for a
/// role claim.
///
/// A role's claim label is the claim itself. While the run was paused, the
/// role's staleness rule may have released the label, and another run (on
/// this host or another) may have claimed the same PR or issue with the same
/// label. Removing it then would strip the new owner's claim. So the requeue
/// releases the label only when its latest `labeled` event on the forge is no
/// later than the run's stop (`stopped_at`, else the time the breadcrumb was
/// recorded).
///
/// # Errors
/// Who-owns-it-now, when the label was applied again after the stop. Like the
/// sweep check's lease probe, it fails open: an unreadable timeline, or no
/// stop time to compare with, leaves the claim this run's.
pub(crate) fn claim_still_ours(
    root: &Path,
    gh: &Path,
    claim: &ClaimBreadcrumb,
    stopped_at: Option<chrono::DateTime<chrono::Utc>>,
) -> std::result::Result<(), String> {
    let cutoff = stopped_at.or_else(|| {
        claim
            .at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&chrono::Utc))
    });
    let Some(cutoff) = cutoff else {
        return Ok(());
    };
    let labeled = crate::claim_reconciliation::forge::fetch_claim_labeled_at(
        gh,
        root,
        claim.number,
        &claim.label,
    );
    relabeled_since(claim, labeled, cutoff)
}

/// [`claim_still_ours`]'s decision.
fn relabeled_since(
    claim: &ClaimBreadcrumb,
    labeled: Option<chrono::DateTime<chrono::Utc>>,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<(), String> {
    match labeled {
        Some(at) if at > cutoff + chrono::Duration::seconds(RECLAIM_SLACK_SECS) => Err(format!(
            "{} was applied to {} #{} again at {} after the paused run stopped at {}",
            claim.label,
            claim.on,
            claim.number,
            at.to_rfc3339(),
            cutoff.to_rfc3339()
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "roll_resume_tests.rs"]
mod tests;
