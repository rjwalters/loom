//! A non-Builder step's role-keyed lease (#11906): the pre-claim, its
//! renewal while the step runs, and its reap when the step ends.
//!
//! All three run the daemon's own `lease pr` verbs, so the lease a step holds
//! is byte-for-byte the one its prompt's `lease pr claim` / `fence` /
//! `release` read: `<!-- loom:pr-lease role=<role> host=… sweep=<step id>
//! step=<step> -->`.
//!
//! - **Pre-claim**, before the spawn, with no `--watch-pid` (no session to
//!   watch yet): claim-then-verify-order, a yield on loss. The won lease is
//!   listed in the step's lease file (`pr_lease_reap`).
//! - **Renewal**, once the child exists: `lease pr attach --watch-pid
//!   <child>`, renew-only. It re-attaches to the step's own live lease and
//!   starts the detached renewer, which lives as long as the step's session,
//!   never the daemon (#6129). A lease already released or reaped stays
//!   gone: `attach` never posts one. The issue path's renewal ran
//!   only for issue sweeps (`dispatch.rs`), while `child_env_markers` told
//!   every step it was renewed; a Builder step now takes that path, every
//!   other step this one.
//! - **Reap**, when the step ends: `lease pr reap` for every lease still
//!   listed, as the role runner does for a timed-out run (#11931), so a peer
//!   need not wait out the TTL.

use super::*;
use crate::pr_lease_reap::{self, Reaper};

/// `lease pr claim` exits (`cli/pr_lease.rs`).
const EXIT_HELD: i32 = 0;
const EXIT_PEER: i32 = 4;
const EXIT_BACKSTOP: i32 = 6;
const EXIT_ACTIVE: i32 = 7;

/// The deadline for one `lease pr claim`: bounded reads and one post, each
/// with its own `gh` deadline (as `pr_lease_reap::REAP_TIMEOUT`).
const CLAIM_TIMEOUT: Duration = pr_lease_reap::REAP_TIMEOUT;

/// The `lease pr` argv (after the binary) for a step's lease: the pre-claim
/// (`claim`, no `watch_pid`), or the renewal start (`attach --watch-pid`,
/// renew-only: it never posts a lease). Pure.
#[must_use]
pub(crate) fn claim_args(
    root: &Path,
    item: u32,
    role: StepRole,
    sweep_id: &str,
    watch_pid: Option<u32>,
) -> Vec<String> {
    let verb = if watch_pid.is_some() {
        "attach"
    } else {
        "claim"
    };
    let mut args = vec![
        "lease".to_string(),
        "pr".to_string(),
        verb.to_string(),
        item.to_string(),
        "--role".to_string(),
        role.role_name().to_string(),
        "--step".to_string(),
        role.as_str().to_string(),
        "--sweep-id".to_string(),
        sweep_id.to_string(),
    ];
    if let Some(pid) = watch_pid {
        args.extend(["--watch-pid".to_string(), pid.to_string()]);
    }
    args.extend([
        "--workspace".to_string(),
        root.to_string_lossy().into_owned(),
    ]);
    args
}

/// What a pre-claim's exit says (`None`: it ran past its deadline). Pure.
pub(crate) fn claim_verdict(code: Option<i32>) -> std::result::Result<(), StepRefusal> {
    match code {
        Some(EXIT_HELD) => Ok(()),
        Some(EXIT_PEER) => Err(StepRefusal::new("claim_lost", "a peer holds the live lease")),
        Some(EXIT_BACKSTOP) => {
            Err(StepRefusal::new("backstop_hold", "the Judge no-verdict backstop holds this PR"))
        }
        // #12105: another actor (a live lease of another kind, or a recent
        // push by another login) is working the PR; a Doctor stands down.
        Some(EXIT_ACTIVE) => Err(StepRefusal::new(
            "peer_active",
            "another actor is active on this PR (a live lease or a recent push)",
        )),
        Some(code) => {
            Err(StepRefusal::new("claim_unverified", format!("`lease pr claim` exited {code}")))
        }
        None => Err(StepRefusal::new("claim_unverified", "`lease pr claim` timed out")),
    }
}

/// Run `cmd` to completion within `limit`; its exit code, `None` past the
/// deadline (killed) or when it could not run.
fn run_bounded(cmd: &mut Command, limit: Duration) -> Option<i32> {
    let mut child = cmd.spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(200)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

impl SweepRegistry {
    /// A `loom-daemon lease pr …` command for this workspace, its won leases
    /// listed in `sweep_id`'s lease file. `None` without a daemon binary.
    fn lease_command(&self, sweep_id: &str, args: &[String]) -> Option<Command> {
        let bin = match crate::daemon_bin_resolve::resolve_daemon_bin() {
            Ok(bin) => bin,
            Err(e) => {
                log::warn!("sweep_registry: no loom-daemon binary for a step lease ({e})");
                return None;
            }
        };
        let root = &self.config.workspace_root;
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .current_dir(root)
            .env(pr_lease_reap::LEASE_FILE_ENV, pr_lease_reap::path_for(root, sweep_id))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(gh) = &self.config.gh_bin {
            cmd.env("LOOM_GH_BIN", gh);
        }
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        Some(cmd)
    }

    /// B1: pre-take a non-Builder step's role-keyed lease, and only that.
    pub(super) fn pre_claim_step_lease(
        &self,
        item: u32,
        role: StepRole,
        sweep_id: &str,
    ) -> std::result::Result<(), StepRefusal> {
        run_step_claim(self.step_claim_command(item, role, sweep_id))
    }

    /// The pre-claim's `lease pr claim` command, built under the lock and
    /// run by [`run_step_claim`] with it released (#11909).
    pub(super) fn step_claim_command(
        &self,
        item: u32,
        role: StepRole,
        sweep_id: &str,
    ) -> Option<Command> {
        let root = &self.config.workspace_root;
        let _ = std::fs::remove_file(pr_lease_reap::path_for(root, sweep_id));
        let args = claim_args(root, item, role, sweep_id, None);
        self.lease_command(sweep_id, &args)
    }

    /// Start the renewal of a just-spawned step's lease, on a detached
    /// thread (the claim's forge reads must not hold the registry lock). A
    /// Builder step's issue lease takes the issue sweep's renewal loop.
    pub(super) fn start_step_lease_renewal(
        &self,
        item: u32,
        role: StepRole,
        sweep_id: &str,
        pid: u32,
        log_path: &Path,
    ) {
        if self.config.skip_label_flip {
            return;
        }
        if role == StepRole::Builder {
            drop(self.start_lease_renewal_loop(item, sweep_id, pid, log_path));
            return;
        }
        let args = claim_args(&self.config.workspace_root, item, role, sweep_id, Some(pid));
        let Some(mut cmd) = self.lease_command(sweep_id, &args) else {
            return;
        };
        let id = sweep_id.to_string();
        let spawned = std::thread::Builder::new()
            .name(format!("step-lease-renew-{item}"))
            .spawn(move || match run_bounded(&mut cmd, CLAIM_TIMEOUT) {
                Some(EXIT_HELD) => {}
                code => log::warn!(
                    "sweep_registry: {id}: lease renewal did not start (exit {code:?}); the \
                     lease ages out at the TTL"
                ),
            });
        if let Err(e) = spawned {
            log::warn!("sweep_registry: {sweep_id}: no lease-renewal thread ({e})");
        }
    }

    /// Reap every PR lease the step `sweep_id` still lists (#11931's reap),
    /// on a detached thread, recording `reason`. The lease file is removed
    /// afterwards. A no-op when the step listed none.
    pub(crate) fn reap_step_leases(&self, sweep_id: &str, reason: &'static str) {
        let reaper = Reaper::existing(&self.config.workspace_root, sweep_id);
        if reaper.held().is_empty() {
            return;
        }
        let id = sweep_id.to_string();
        let spawned = std::thread::Builder::new()
            .name("step-lease-reap".to_string())
            .spawn(move || {
                let reaped = reaper.reap_with_daemon(reason);
                log::info!("sweep_registry: {id}: reaped {reaped} lease(s) ({reason}, #11906)");
            });
        if let Err(e) = spawned {
            log::warn!("sweep_registry: {sweep_id}: no lease-reap thread ({e}); leases age out");
        }
    }
}

/// Run a pre-claim command ([`SweepRegistry::step_claim_command`]); no
/// registry state is read, so the caller holds no lock.
pub(super) fn run_step_claim(cmd: Option<Command>) -> std::result::Result<(), StepRefusal> {
    let Some(mut cmd) = cmd else {
        return Err(StepRefusal::new("claim_unverified", "no loom-daemon binary"));
    };
    claim_verdict(run_bounded(&mut cmd, CLAIM_TIMEOUT))
}
