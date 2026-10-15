//! `loom-daemon lease release` — hand back a lease taken by `lease ensure`
//! (#12103).
//!
//! ## What went wrong
//!
//! `lease ensure --watch-pid $PPID` run inside a Task subagent watches the
//! *parent session*, which outlives the subagent. The lease and its renewal
//! loop then kept blocking every later claim on the issue (`forge check-claim`
//! reported it held), and the only way out was to `kill` the loop and delete
//! the `loom:lease` comment by hand. `lease renewer release` could not help: it
//! matches on the sweep id, which an attended session never knows
//! (`session_sweep_id`).
//!
//! ## What this does
//!
//! 1. Reads the issue's trusted lease + lease-yield comments and takes the
//!    live (fresh, un-yielded) lease pairs.
//! 2. Refuses, naming the host, when any live lease belongs to another host:
//!    nothing is stopped or posted.
//! 3. Stops this host's renewer(s) for the issue whatever their sweep id
//!    (`lease_renewer::end_owners`).
//! 4. Posts a `loom:lease-yield` record per released pair — the existing
//!    "this claimant stood down" signal `forge check-claim` already honours —
//!    instead of deleting the lease comment, which would lose the audit trail.
//!
//! Idempotent: once yielded, a pair is no longer live, so a second run finds
//! nothing to post and exits 0.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use loom_daemon::comment_trust::TrustPolicy;
use loom_daemon::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use loom_daemon::host_identity::lease_host_identity;
use loom_daemon::sweep_registry::opaque_host_id;

use super::lease_co_occupancy::{live_pairs, read_rows, LeaseRow};
use super::lease_renewer::{self, Store};

const GH_TIMEOUT: Duration = Duration::from_secs(20);

/// Same TTL knob, and default, as `lease co-occupancy` and the publish script.
const TTL_ENV: &str = "LOOM_LEASE_TTL_MINUTES";
const DEFAULT_TTL_MINUTES: f64 = 15.0;

#[derive(clap::Args)]
pub(crate) struct LeaseReleaseArgs {
    /// The issue whose lease (taken on this host) should be handed back.
    #[arg(value_name = "ISSUE")]
    pub(crate) issue: u64,

    /// Repo checkout to operate in. Defaults to the current directory.
    #[arg(long, default_value = ".")]
    pub(crate) workspace: PathBuf,
}

/// What a release run decided to do, before doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Live leases held by this host: stop their renewers, post a yield each.
    pub(crate) own: Vec<(String, String)>,
    /// Live leases held by another host: the run is refused.
    pub(crate) foreign: Vec<(String, String)>,
}

/// Split the issue's live lease pairs by whether `mine` names their host. Pure.
pub(crate) fn plan(
    rows: &[LeaseRow],
    now: DateTime<Utc>,
    ttl: chrono::Duration,
    mine: &[String],
) -> Plan {
    let (own, foreign) = live_pairs(rows, now, ttl)
        .into_iter()
        .partition(|(host, _)| mine.contains(host));
    Plan { own, foreign }
}

/// The yield record that excuses `(host, sweep)` from `check-claim`.
pub(crate) fn yield_body(host: &str, sweep: &str) -> String {
    format!(
        "<!-- loom:lease-yield host={host} sweep={sweep} earliest_host=none earliest_sweep=none \
         -->\nThis lease (sweep `{sweep}` on host `{host}`) was released with `loom-daemon lease \
         release` (#12103): the claimant stood down and its renewal loop was stopped."
    )
}

/// What a release run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Another host holds a live lease; nothing was touched.
    Refused(Vec<(String, String)>),
    /// `renewers` stopped, `yielded` records posted.
    Released { renewers: usize, yielded: usize },
    /// A yield could not be posted; re-running is safe.
    PostFailed { renewers: usize, sweep: String },
}

/// The whole decision, with the forge post and the process table injected.
#[allow(clippy::too_many_arguments)]
pub(crate) fn release_lease(
    store: &Store,
    repo: &str,
    issue: u64,
    plan: &Plan,
    mine: &[String],
    is_live: lease_renewer::LiveProbe<'_>,
    signal: &dyn Fn(u32),
    post: &dyn Fn(&str) -> bool,
) -> Result<Outcome> {
    if !plan.foreign.is_empty() {
        return Ok(Outcome::Refused(plan.foreign.clone()));
    }
    let mut renewers = 0;
    for host in mine {
        renewers +=
            lease_renewer::end_owners(store, repo, issue, Some(host), None, is_live, signal)?;
    }
    for (host, sweep) in &plan.own {
        if !post(&yield_body(host, sweep)) {
            return Ok(Outcome::PostFailed {
                renewers,
                sweep: sweep.clone(),
            });
        }
    }
    Ok(Outcome::Released {
        renewers,
        yielded: plan.own.len(),
    })
}

/// Every id this host's leases may carry: the opaque published id, and the raw
/// one when `LOOM_LEASE_PUBLISH_HOSTNAME` opted in.
fn my_host_ids() -> Vec<String> {
    let raw = lease_host_identity();
    vec![opaque_host_id(&raw), raw]
}

fn post_comment(gh_bin: &Path, root: &Path, issue: u64, body: &str) -> bool {
    use loom_daemon::forge_call_stats::ops;
    let nwo = match loom_daemon::write_scope::may_write_from(root, None) {
        loom_daemon::write_scope::Verdict::Allow(nwo) => nwo,
        loom_daemon::write_scope::Verdict::Deny(why) => {
            eprintln!("lease release: refusing to write here (#9548): {why}");
            return false;
        }
    };
    GhInvocation::new(
        Operation::new("lease.release_yield"),
        AccessIntent::Write,
        GhTarget::None,
        GH_TIMEOUT,
    )
    .forge_op(ops::COMMENT_CREATE)
    .program(gh_bin)
    .current_dir(root)
    .writer_identity()
    .args([
        "api",
        "-X",
        "POST",
        &format!("repos/{nwo}/issues/{issue}/comments"),
        "-f",
        &format!("body={body}"),
        "--silent",
    ])
    .run()
    .ok_output()
    .is_some()
}

impl LeaseReleaseArgs {
    pub(crate) fn run(self) -> Result<()> {
        let gh_bin = PathBuf::from(std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".into()));
        let ttl_minutes: f64 = std::env::var(TTL_ENV)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|m: &f64| m.is_finite() && *m >= 0.0)
            .unwrap_or(DEFAULT_TTL_MINUTES);
        let ttl = chrono::Duration::seconds((ttl_minutes * 60.0) as i64);
        let root = &self.workspace;
        let policy = TrustPolicy::for_root(root);
        let Some(rows) = read_rows(&gh_bin, root, self.issue, GH_TIMEOUT, &policy) else {
            anyhow::bail!(
                "lease release: could not read issue #{}'s lease comments; nothing changed",
                self.issue
            );
        };
        let mine = my_host_ids();
        let plan = plan(&rows, Utc::now(), ttl, &mine);
        let repo = lease_renewer::repo_identity(
            std::env::var("LOOM_REPO").ok().as_deref(),
            &std::fs::canonicalize(root).unwrap_or_else(|_| root.clone()),
        );
        let signal = |pid: u32| {
            if let Ok(pid) = i32::try_from(pid) {
                // SAFETY: plain kill(2) on a pid just re-verified by identity.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        };
        let outcome = release_lease(
            &Store::from_env(),
            &repo,
            self.issue,
            &plan,
            &mine,
            &lease_renewer::owner_is_live,
            &signal,
            &|body| post_comment(&gh_bin, root, self.issue, body),
        )?;
        match outcome {
            Outcome::Refused(foreign) => {
                let held: Vec<String> = foreign
                    .iter()
                    .map(|(h, s)| format!("host={h} sweep={s}"))
                    .collect();
                eprintln!(
                    "lease release: issue #{} has a live lease held by another host ({}); only \
                     the holder can release it — nothing stopped or posted",
                    self.issue,
                    held.join(", ")
                );
                std::process::exit(1);
            }
            Outcome::Released { renewers, yielded } => {
                eprintln!(
                    "lease release: issue #{}: stopped {renewers} renewer(s), posted {yielded} \
                     yield record(s)",
                    self.issue
                );
                Ok(())
            }
            Outcome::PostFailed { renewers, sweep } => {
                eprintln!(
                    "lease release: issue #{}: stopped {renewers} renewer(s) but could not post \
                     the yield for sweep {sweep}; re-run to retry",
                    self.issue
                );
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests;
