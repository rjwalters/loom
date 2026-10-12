//! Merge-queue controls — **dormant** (#10255, Phase A of epic #9978).
//!
//! What lands here, and nothing more:
//!
//! - [`mode`] — the one per-repo setting, `champion.mergeMode`
//!   (`direct` | `queue`, default `direct`, unknown values rejected).
//! - [`ops`] — typed, idempotent enqueue / dequeue / status against the
//!   [`ops::QueueApi`] seam, with the approved-head precondition.
//! - [`github`] — the GitHub [`ops::QueueApi`] (GraphQL via the counted `gh`
//!   facade).
//! - [`preflight`] — capability preflight with distinct failure kinds.
//! - [`authz`] — the fail-closed authorization protocol (#10256, Phase B1);
//!   [`group_authz`] extends it to whole merge groups, concludes last, and
//!   re-fails passed groups on revocation (#10256, Phase B4; not wired yet).
//! - [`lifecycle`] (+ [`forge`], [`grants`], [`removal`], [`events`],
//!   [`gh_lifecycle`], [`lifecycle_cli`]) — the guard-preserving handoff,
//!   drop/merge reconciliation, revocation before Loom-owned transitions and
//!   deduplicated telemetry (#10256, Phase B2). Called by `merge-pr.sh`, the
//!   daemon's claim-reconciliation pass and the verdict-invalidation paths,
//!   all of which return before any forge call in `direct` mode.
//! - [`handle`] — `loom-daemon forge merge-queue …`, for operators and tests.
//!
//! # Dormant by construction
//!
//! [`QUEUE_EXECUTION_ENABLED`] is a compile-time `false`. Until #9978's later
//! phases install the lifecycle safety contract and the required merge-group
//! checks, every mutating entry point refuses with `EXECUTION_DORMANT` even
//! when `champion.mergeMode=queue` — and under `direct` (the default) it
//! refuses with `NOT_QUEUE_MODE` before any forge call. The daemon tick
//! and the stale-verdict paths call the lifecycle (no-ops in direct mode); no
//! role prompt or `merge-pr.sh` invokes the handoff yet (honest gap, #10256). The
//! read-only `status`, `preflight`, and `mode` verbs are always available.
//!
//! # CLI exit codes
//!
//! | Exit | Meaning |
//! |---|---|
//! | `0` | success (including idempotent no-ops; preflight: capable) |
//! | `1` | the operation was answered and failed (preflight: not capable) |
//! | `2` | invalid `champion.mergeMode` / usage |
//! | `3` | could not determine (rate limit, unreadable config, transport) |
//! | `4` | refused before any forge call (`NOT_QUEUE_MODE` / `EXECUTION_DORMANT`) |

pub mod authz;
pub mod events;
pub mod forge;
pub mod gh_lifecycle;
pub mod github;
pub mod grants;
pub mod group_authz;
pub mod group_github;
pub mod group_run;
pub mod lifecycle;
pub mod lifecycle_cli;
pub mod mode;
pub mod ops;
pub mod preflight;
pub mod removal;

use std::path::PathBuf;

use crate::forge_cmd::{detect_forge, gh_bin, repo_nwo, ForgeType};
use mode::{resolve_merge_mode, ResolvedMergeMode};
use ops::{DequeueOutcome, EnqueueOutcome, QueueError};
use preflight::CapabilityError;

/// Phase A gate. Flipped only by the #9978 phase that installs the safety
/// contract — never by config.
pub const QUEUE_EXECUTION_ENABLED: bool = false;

const PREFIX: &str = "merge-queue:";

/// One `forge merge-queue` verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeQueueCmd {
    Mode,
    Preflight {
        repo: Option<String>,
        branch: Option<String>,
    },
    Status {
        pr: u32,
        repo: Option<String>,
    },
    Enqueue {
        pr: u32,
        approved_sha: String,
        repo: Option<String>,
    },
    Dequeue {
        pr: u32,
        repo: Option<String>,
    },
    /// #10256: reconcile one PR (`merge-pr.sh`) or every pending one.
    Reconcile {
        pr: Option<u32>,
        repo: Option<String>,
        mode_only: bool,
    },
    /// #10256: authorize and enqueue after every direct guard passed.
    Handoff {
        pr: u32,
        approved_sha: String,
        repo: Option<String>,
    },
    /// #10256: the Champion's single call: reconcile, then hand off.
    Step {
        pr: u32,
        approved_sha: String,
        repo: Option<String>,
    },
    /// #10256: revoke + dequeue before a Loom-owned transition.
    Revoke {
        pr: u32,
        reason: String,
        repo: Option<String>,
    },
    /// #10256: evaluate and post `loom/merge-authorization` for the merge
    /// group built at `commit` (every member, concluded last).
    GroupCheck {
        commit: String,
        repo: Option<String>,
    },
    /// #10256: body of the required `loom/merge-authorization` check.
    AuthorizeCheck {
        pr: u32,
        pr_head: String,
        repo: Option<String>,
    },
}

/// Exit code for a [`QueueError`].
#[must_use]
pub fn queue_exit_code(e: &QueueError) -> i32 {
    match e {
        QueueError::NotQueueMode | QueueError::ExecutionDormant => 4,
        QueueError::InvalidSha { .. } => 2,
        QueueError::RateLimited { .. } | QueueError::Forge { .. } => 3,
        _ => 1,
    }
}

/// Exit code for a [`CapabilityError`].
#[must_use]
pub fn capability_exit_code(e: &CapabilityError) -> i32 {
    match e {
        CapabilityError::ConfigInaccessible { .. } | CapabilityError::RateLimited { .. } => 3,
        _ => 1,
    }
}

/// The process's output and exit code, computed without printing so the CLI
/// path is testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub code: i32,
}

impl Report {
    fn ok(line: String) -> Self {
        Self {
            stdout: vec![line],
            stderr: Vec::new(),
            code: 0,
        }
    }
    fn err(code: &str, msg: impl std::fmt::Display, exit: i32) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: vec![format!("{PREFIX} [{code}] {msg}")],
            code: exit,
        }
    }
    fn queue(e: &QueueError) -> Self {
        Self::err(e.code(), e, queue_exit_code(e))
    }
}

/// Where the CLI gets its environment from — real in production, injected
/// in tests.
pub struct Env {
    pub forge: ForgeType,
    pub gh: String,
    pub default_repo: Option<String>,
    pub mode: Result<ResolvedMergeMode, mode::MergeModeError>,
    pub execution_enabled: bool,
    /// Workspace root (comment trust policy, telemetry log).
    pub root: PathBuf,
}

impl Env {
    /// The live environment for the current working directory.
    #[must_use]
    pub fn current() -> Self {
        let gh = gh_bin();
        let root = crate::repo_root::find_repo_root_from_cwd()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            forge: detect_forge(None),
            default_repo: None,
            mode: resolve_merge_mode(&root),
            execution_enabled: QUEUE_EXECUTION_ENABLED,
            gh,
            root,
        }
    }

    fn nwo(&self, repo: Option<&String>) -> Option<String> {
        repo.cloned()
            .or_else(|| self.default_repo.clone())
            .or_else(|| repo_nwo(&self.gh))
    }
}

fn unresolved_repo() -> Report {
    Report::err(
        "CONFIG_INACCESSIBLE",
        "no forge repository could be resolved here; pass --repo OWNER/REPO",
        3,
    )
}

/// Run one verb against `env`.
#[must_use]
pub fn run(cmd: &MergeQueueCmd, env: &Env) -> Report {
    let mode = match &env.mode {
        Ok(m) => *m,
        Err(e) => return Report::err("INVALID_MERGE_MODE", e, 2),
    };
    // #10256: the lifecycle verbs answer `direct` before the forge is even
    // identified, so a Gitea or direct-mode `merge-pr.sh` is unaffected.
    if let Some(report) = lifecycle_cli::run(cmd, env, mode.mode) {
        return report;
    }
    // Mutations are refused before the forge is even identified, so direct
    // mode provably never reaches a queue API.
    if matches!(cmd, MergeQueueCmd::Enqueue { .. } | MergeQueueCmd::Dequeue { .. }) {
        if let Err(e) = ops::execution_gate(mode.mode, env.execution_enabled) {
            return Report::queue(&e);
        }
    }
    if env.forge == ForgeType::Gitea && *cmd != MergeQueueCmd::Mode {
        if let MergeQueueCmd::Preflight { .. } = cmd {
            let e = CapabilityError::UnsupportedForge {
                forge: "Gitea".to_string(),
            };
            return Report::err(e.code(), &e, capability_exit_code(&e));
        }
        return Report::queue(&QueueError::UnsupportedForge {
            forge: "Gitea".to_string(),
        });
    }
    match cmd {
        MergeQueueCmd::Mode => Report::ok(format!(
            "{PREFIX} mode={} source={} execution={}",
            mode.mode.as_str(),
            mode.source.as_str(),
            if env.execution_enabled {
                "enabled"
            } else {
                "dormant"
            }
        )),
        MergeQueueCmd::Preflight { repo, branch } => {
            let Some(nwo) = env.nwo(repo.as_ref()) else {
                return unresolved_repo();
            };
            match preflight::github_preflight(&env.gh, &nwo, branch.as_deref()) {
                Ok(c) => Report::ok(format!(
                    "{PREFIX} OK {nwo} '{}': merge_queue rule in ruleset(s) {:?}; required checks: {}",
                    c.branch,
                    c.queue_rulesets,
                    c.required_checks.join(", ")
                )),
                Err(e) => Report::err(e.code(), &e, capability_exit_code(&e)),
            }
        }
        MergeQueueCmd::Status { pr, repo } => with_api(env, repo.as_ref(), |api| {
            ops::status(api, *pr).map(|s| {
                let entry = s.entry.as_ref().map_or_else(
                    || "not-queued".to_string(),
                    |e| {
                        format!(
                            "queued state={} position={} queued_head={}",
                            e.state,
                            e.position
                                .map_or_else(|| "?".to_string(), |p| p.to_string()),
                            e.head_oid.as_deref().unwrap_or("?")
                        )
                    },
                );
                format!(
                    "{PREFIX} PR #{} {} head={} {entry}",
                    s.number,
                    s.state.as_str(),
                    s.head_oid
                )
            })
        }),
        MergeQueueCmd::Enqueue {
            pr,
            approved_sha,
            repo,
        } => with_api(env, repo.as_ref(), |api| {
            ops::guarded_enqueue(mode.mode, env.execution_enabled, api, *pr, approved_sha).map(
                |o| match o {
                    EnqueueOutcome::Enqueued { position } => {
                        format!("{PREFIX} PR #{pr} enqueued at {approved_sha} (position {position:?})")
                    }
                    EnqueueOutcome::AlreadyQueued { position } => format!(
                        "{PREFIX} PR #{pr} already queued at {approved_sha} (position {position:?}); nothing sent"
                    ),
                },
            )
        }),
        MergeQueueCmd::Dequeue { pr, repo } => with_api(env, repo.as_ref(), |api| {
            ops::guarded_dequeue(mode.mode, env.execution_enabled, api, *pr).map(|o| match o {
                DequeueOutcome::Dequeued => format!("{PREFIX} PR #{pr} dequeued"),
                DequeueOutcome::NotQueued => {
                    format!("{PREFIX} PR #{pr} was not queued; nothing sent")
                }
                DequeueOutcome::AlreadyMerged => {
                    format!("{PREFIX} PR #{pr} already merged; nothing to dequeue")
                }
            })
        }),
        // Answered by `lifecycle_cli::run` above.
        MergeQueueCmd::Reconcile { .. }
        | MergeQueueCmd::Handoff { .. }
        | MergeQueueCmd::Step { .. }
        | MergeQueueCmd::Revoke { .. }
        | MergeQueueCmd::GroupCheck { .. }
        | MergeQueueCmd::AuthorizeCheck { .. } => {
            Report::err("INTERNAL", "lifecycle verb was not dispatched", 1)
        }
    }
}

fn with_api(
    env: &Env,
    repo: Option<&String>,
    f: impl FnOnce(&dyn ops::QueueApi) -> Result<String, QueueError>,
) -> Report {
    let Some(nwo) = env.nwo(repo) else {
        return unresolved_repo();
    };
    match github::GhQueueApi::new(&env.gh, &nwo).and_then(|api| f(&api)) {
        Ok(line) => Report::ok(line),
        Err(e) => Report::queue(&e),
    }
}

/// `loom-daemon forge merge-queue …`: print the report and exit with its code.
pub fn handle(cmd: &MergeQueueCmd) -> ! {
    let report = run(cmd, &Env::current());
    for line in &report.stdout {
        println!("{line}");
    }
    for line in &report.stderr {
        eprintln!("{line}");
    }
    std::process::exit(report.code)
}

#[cfg(test)]
mod authz_tests;
#[cfg(test)]
mod group_authz_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;
