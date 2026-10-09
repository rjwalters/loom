//! The start paths of `SessionLifecycle`: the shared `start_inner`, and the
//! session reconciler's hold-respecting `start_unless_held`, which closes its
//! race with an operator `stop` (issues #10453, #10661). A child module for
//! the file-size ratchet (#7711).
//!
//! # Why the hold is checked again after `docker start`/`run` returns
//!
//! The reconciler checks the hold, then runs `docker start` or `docker run`.
//! `docker run` can take minutes (a first image pull), and nothing locks it
//! against `stop`. A `stop` that writes the hold while it is in flight sees
//! no container on either of its inspects, reports success, and the
//! container then appears running **and** held, which no later pass touches.
//! No fixed number of extra inspects in `stop` closes that.
//!
//! So the reconciler checks the hold once more after its `docker start`/`run`
//! has returned, and if it is now held it undoes its own start. Let `stop`
//! write the hold at T1, and the reconciler re-check at T2:
//!
//! * T2 after T1: the re-check sees the hold and the reconciler stops and
//!   removes the container it just started ([`SessionLifecycle::undo_for_hold`]).
//! * T2 before T1: the `docker start`/`run` returned before T2, so before T1.
//!   The container already existed when `stop` wrote the hold, and `stop`'s
//!   inspect after its write sees it and stops and removes it.
//!
//! Either way one side sees the other; no lock is needed for that. The undo
//! is still a stop, so it keeps `stop`'s safety rules: it takes the
//! container's dispatch lock exclusively (waiting briefly, since a concurrent
//! operator `stop` holds it until it returns), and it does not stop a
//! container with an in-flight exec. In those two cases it leaves the
//! container running, and says so in a WARN naming the command that finishes
//! the stop.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};

use super::{
    container_name, ensure_profile_controls, find_codex_account, mark_session_managed,
    session_hold, ContainerRunner, SessionLifecycle, SessionStatus, STOP_GRACE,
};
use crate::tokens_pool::session_dispatch_lock::{self, Exclusive};

/// How long the undo waits for the dispatch lock: long enough for a
/// concurrent operator `stop` to return (it holds the lock until then).
const UNDO_LOCK_WAIT: Duration = Duration::from_secs(30);

/// Where the dispatch lock comes from if the reconciler's start has to be
/// undone for a hold (#10661).
#[derive(Debug, Clone, Copy)]
pub enum UndoLock<'a> {
    /// The caller already holds the container's lock exclusively (the drift
    /// teardown keeps it across its recreate).
    HeldByCaller,
    /// Take it in this lock directory (`None`: none resolves; then, like an
    /// operator `stop`, the `docker top` check alone guards the undo).
    Take(Option<&'a Path>),
}

/// A hold check for the reconciler's start, and the lock its undo uses.
pub(super) type HeldCheck<'a> = (&'a dyn Fn() -> bool, UndoLock<'a>);

impl<R: ContainerRunner> SessionLifecycle<R> {
    /// The automated start the session reconciler uses (issue #10453): like
    /// [`Self::start_with_workspace`], but it never lifts a hold or records
    /// an operator choice. It checks `is_held` after inspecting the container
    /// and before any `docker start`/`run`, and again after it returns,
    /// undoing its own start if a `stop` wrote its hold meanwhile (#10661,
    /// see this module's docs). Both fail with [`session_hold::OperatorHeld`],
    /// so a `stop` racing this is never undone.
    pub fn start_unless_held(
        &self,
        name: &str,
        workspace: Option<&Path>,
        is_held: &dyn Fn() -> bool,
        undo_lock: UndoLock<'_>,
    ) -> Result<SessionStatus> {
        self.start_inner(name, workspace, Some((is_held, undo_lock)))
    }

    pub(super) fn start_inner(
        &self,
        name: &str,
        workspace: Option<&Path>,
        held: Option<HeldCheck<'_>>,
    ) -> Result<SessionStatus> {
        let account = find_codex_account(&self.workspace, name)?;
        let name = account.id.name.as_str();
        let profile = account.credential_reference;
        let container = container_name(name);
        let requested_workspace = workspace
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.workspace.clone());
        let is_held = || held.is_some_and(|(held, _)| held());
        match self.runner.inspect(&container)? {
            Some(state) if state.running => {
                // Already running: reuse it (idempotent `start`).
                Self::check_workspace_match(name, &state, &requested_workspace)?;
            }
            Some(state) if state.restarting => bail!(
                "session {name:?} ({container}) is restarting — Docker is backing off before \
                 restarting a crashed container, so it is not reused. Recreate it: \
                 `loom-daemon accounts session stop {name} --force`, then start it again \
                 (issue #10453)"
            ),
            Some(state) => {
                Self::check_workspace_match(name, &state, &requested_workspace)?;
                if is_held() {
                    return Err(session_hold::OperatorHeld.into());
                }
                self.runner.start_existing(&container)?;
                self.recheck_hold_after_start(name, &container, held)?;
            }
            None => {
                if is_held() {
                    return Err(session_hold::OperatorHeld.into());
                }
                ensure_profile_controls(&profile)?;
                self.runner.create(
                    &container,
                    &self.image,
                    &profile,
                    &requested_workspace,
                    &self.workspace,
                )?;
                self.recheck_hold_after_start(name, &container, held)?;
            }
        }
        mark_session_managed(&profile, &container)?;
        self.status(name)
    }

    /// After this call's own `docker start`/`run` returned: if a hold landed
    /// meanwhile, undo the start and fail with `OperatorHeld`.
    fn recheck_hold_after_start(
        &self,
        name: &str,
        container: &str,
        held: Option<HeldCheck<'_>>,
    ) -> Result<()> {
        let Some((is_held, undo_lock)) = held else {
            return Ok(());
        };
        if !is_held() {
            return Ok(());
        }
        if let Err(error) = self.undo_for_hold(name, container, undo_lock) {
            log::warn!(
                "session_reconcile: {container}: an operator stop held {name} while this pass \
                 was starting it, and undoing the start failed ({error:#}); it may be running \
                 and held. Finish the stop: `loom-daemon accounts session stop {name}`"
            );
        }
        Err(session_hold::OperatorHeld.into())
    }

    /// Stop and remove the container this pass just started, under `stop`'s
    /// rules (the dispatch lock, then the in-flight-exec check).
    pub(super) fn undo_for_hold(
        &self,
        name: &str,
        container: &str,
        undo_lock: UndoLock<'_>,
    ) -> Result<()> {
        let _lock = match undo_lock {
            UndoLock::HeldByCaller => None,
            UndoLock::Take(dir) => {
                match session_dispatch_lock::exclusive_within(dir, container, UNDO_LOCK_WAIT) {
                    Exclusive::Acquired(lock) => Some(lock),
                    Exclusive::Unknown(_) => None,
                    Exclusive::Busy => {
                        bail!("a dispatch holds its lock, so it was left running")
                    }
                }
            }
        };
        let Some(state) = self.runner.inspect(container)? else {
            return Ok(());
        };
        if state.running && self.runner.has_active_exec(container)? {
            bail!("an exec is in flight in it, so it was left running ({name})");
        }
        self.runner.stop_and_remove(container, STOP_GRACE)
    }
}
