//! CLI rendering for the lifecycle verbs (#10256, Phase B2):
//! `reconcile`, `handoff`, `revoke`, `authorize-check`.
//!
//! # Sentinels and exit codes (the `merge-pr.sh` contract)
//!
//! The first stdout line is always a `LOOM-MERGE-QUEUE-*` /
//! `LOOM-MERGE-AUTHORIZATION` sentinel, so a caller can tell "direct mode"
//! from "a binary that predates the verb" (no sentinel at all).
//!
//! | Sentinel | Exit | Meaning for `merge-pr.sh` |
//! |---|---|---|
//! | `LOOM-MERGE-QUEUE-DIRECT` | 0 | direct mode: proceed exactly as before |
//! | `LOOM-MERGE-QUEUE-MODE` | 0 | (`--mode-only`) queue mode, nothing read |
//! | `LOOM-MERGE-QUEUE-CONTINUE` | 0 | queue mode: run the guards, then `handoff` (`step` does both) |
//! | `LOOM-MERGE-QUEUE-MERGED` | 0 | GitHub confirmed the merge |
//! | `LOOM-MERGE-QUEUE-QUEUED` | 7 | still queued and authorized: not merged, nothing wrong |
//! | `LOOM-MERGE-QUEUE-DROPPED` | 7 | GitHub dropped it; reason commented and routed |
//! | `LOOM-MERGE-QUEUE-HANDED-OFF` | 7 | enqueued (not merged) |
//! | `LOOM-MERGE-QUEUE-UNDETERMINED` | 3 | a needed read/write failed: do not merge |
//! | `LOOM-MERGE-QUEUE-REFUSED` | 1/3/4 | handoff refused (4 = dormant / not queue mode) |

use super::authz::{CheckConclusion, HandoffError};
use super::events::FileEventSink;
use super::gh_lifecycle::GhLifecycleForge;
use super::github::GhQueueApi;
use super::lifecycle::{
    authorize_check, handoff, reconcile_pr, revoke_for_transition, sweep, transition_line, Ctx,
    HandoffFailure, Reconciled,
};
use super::mode::MergeMode;
use super::ops::{self, EnqueueOutcome};
use super::{queue_exit_code, Env, MergeQueueCmd, Report};
use crate::forge_cmd::ForgeType;

/// `merge-pr.sh`'s "not merged this pass, nothing wrong" code.
pub const EXIT_QUEUED: i32 = 7;

fn lines(code: i32, out: Vec<String>) -> Report {
    Report {
        stdout: out,
        stderr: Vec::new(),
        code,
    }
}

fn direct() -> Report {
    lines(0, vec!["LOOM-MERGE-QUEUE-DIRECT".to_string()])
}

fn undetermined(why: &str) -> Report {
    lines(
        3,
        vec![
            "LOOM-MERGE-QUEUE-UNDETERMINED".to_string(),
            format!("merge-queue: {why} — not merging or enqueuing on this pass (fail closed)"),
        ],
    )
}

/// Render one per-PR reconcile outcome.
#[must_use]
pub fn render_reconciled(pr: u32, r: &Reconciled) -> Report {
    match r {
        Reconciled::Direct => direct(),
        Reconciled::Continue(note) => lines(
            0,
            vec![format!(
                "LOOM-MERGE-QUEUE-CONTINUE pr={pr}{}",
                note.as_ref().map_or_else(String::new, |n| format!(" note={n}"))
            )],
        ),
        Reconciled::Merged {
            recorded,
            after_revocation,
        } => lines(
            0,
            vec![format!(
                "LOOM-MERGE-QUEUE-MERGED pr={pr} recorded={recorded} after_revocation={after_revocation}"
            )],
        ),
        Reconciled::StillQueued { head } => lines(
            EXIT_QUEUED,
            vec![
                format!("LOOM-MERGE-QUEUE-QUEUED pr={pr} head={head}"),
                format!(
                    "PR #{pr} is in the merge queue at {head} and still authorized; not merged \
                     yet — GitHub merges it once the required checks (including \
                     loom/merge-authorization) pass."
                ),
            ],
        ),
        Reconciled::Dropped {
            kind,
            raw,
            route_error,
        } => {
            let mut out = vec![
                format!("LOOM-MERGE-QUEUE-DROPPED pr={pr} reason={}", kind.as_str()),
                format!(
                    "GitHub removed PR #{pr} from the merge queue (reason: {}); commented and \
                     routed.",
                    raw.as_deref().unwrap_or("none given")
                ),
            ];
            match route_error {
                None => lines(EXIT_QUEUED, out),
                Some(e) => {
                    out.push(format!(
                        "merge-queue: the drop was recorded but its label route failed: {e}"
                    ));
                    lines(1, out)
                }
            }
        }
        Reconciled::Undetermined(why) => undetermined(why),
    }
}

/// Render a handoff result.
#[must_use]
pub fn render_handoff(pr: u32, sha: &str, r: &Result<EnqueueOutcome, HandoffFailure>) -> Report {
    match r {
        Ok(o) => lines(
            EXIT_QUEUED,
            vec![
                format!("LOOM-MERGE-QUEUE-HANDED-OFF pr={pr} head={sha}"),
                format!(
                    "PR #{pr} {} the merge queue at {sha}. This is NOT a merge: issue closure, \
                     branch/worktree cleanup and merge telemetry wait for GitHub to confirm it.",
                    if matches!(o, EnqueueOutcome::AlreadyQueued { .. }) {
                        "was already in"
                    } else {
                        "was added to"
                    }
                ),
            ],
        ),
        Err(e) => {
            let code = match e {
                HandoffFailure::Gate(q) => queue_exit_code(q),
                HandoffFailure::Preflight(_) => 3,
                HandoffFailure::Authz(HandoffError::Enqueue(q)) => queue_exit_code(q),
                HandoffFailure::Authz(HandoffError::Store(_)) => 3,
                _ => 1,
            };
            lines(
                code,
                vec![
                    format!("LOOM-MERGE-QUEUE-REFUSED pr={pr}"),
                    format!("merge-queue: handoff of PR #{pr} refused: {e}. Nothing fell back to a direct merge."),
                ],
            )
        }
    }
}

struct Live {
    queue: GhQueueApi,
    forge: GhLifecycleForge,
    events: FileEventSink,
}

fn live(env: &Env, repo: Option<&String>) -> Result<(Live, String), Report> {
    let Some(nwo) = env.nwo(repo) else {
        return Err(undetermined("no forge repository could be resolved; pass --repo OWNER/REPO"));
    };
    let queue = GhQueueApi::new(&env.gh, &nwo).map_err(|e| undetermined(&e.to_string()))?;
    let forge = GhLifecycleForge::new(&env.gh, &env.root, &nwo).map_err(|e| undetermined(&e))?;
    Ok((
        Live {
            queue,
            forge,
            events: FileEventSink::for_root(&env.root),
        },
        nwo,
    ))
}

fn ctx<'a>(env: &Env, mode: MergeMode, l: &'a Live) -> Ctx<'a> {
    Ctx {
        mode,
        execution_enabled: env.execution_enabled,
        queue: &l.queue,
        forge: &l.forge,
        events: &l.events,
        now: chrono::Utc::now(),
    }
}

/// Run one lifecycle verb. `None` for any other verb.
#[must_use]
pub fn run(cmd: &MergeQueueCmd, env: &Env, mode: MergeMode) -> Option<Report> {
    let queue_on_gitea = mode == MergeMode::Queue && env.forge == ForgeType::Gitea;
    Some(match cmd {
        MergeQueueCmd::Reconcile {
            pr,
            repo,
            mode_only,
        } => {
            if mode == MergeMode::Direct {
                return Some(direct());
            }
            if queue_on_gitea {
                return Some(undetermined(
                    "champion.mergeMode=queue, but Gitea has no merge queue",
                ));
            }
            if *mode_only {
                return Some(lines(
                    0,
                    vec![format!(
                        "LOOM-MERGE-QUEUE-MODE queue execution={}",
                        if env.execution_enabled {
                            "enabled"
                        } else {
                            "dormant"
                        }
                    )],
                ));
            }
            // #9548: queue-mode reconcile writes (comments, labels, dequeue).
            if let crate::write_scope::Verdict::Deny(why) =
                crate::write_scope::may_write_from(&env.root, repo.as_deref())
            {
                return Some(undetermined(&format!("refusing the write (#9548): {why}")));
            }
            let (l, _) = match live(env, repo.as_ref()) {
                Ok(x) => x,
                Err(r) => return Some(r),
            };
            let c = ctx(env, mode, &l);
            match pr {
                Some(pr) => render_reconciled(*pr, &reconcile_pr(&c, *pr)),
                None => match sweep(&c) {
                    Ok(rows) => {
                        let mut out =
                            vec![format!("LOOM-MERGE-QUEUE-SWEEP pending={}", rows.len())];
                        let mut code = 0;
                        for (pr, r) in &rows {
                            let rep = render_reconciled(*pr, r);
                            if rep.code != 0 && rep.code != EXIT_QUEUED {
                                code = rep.code;
                            }
                            out.extend(rep.stdout);
                        }
                        lines(code, out)
                    }
                    Err(e) => undetermined(&format!("event log: {e}")),
                },
            }
        }
        MergeQueueCmd::Handoff {
            pr,
            approved_sha,
            repo,
        } => {
            // Refuse before resolving anything when direct or dormant.
            if let Err(e) = ops::execution_gate(mode, env.execution_enabled) {
                return Some(render_handoff(*pr, approved_sha, &Err(HandoffFailure::Gate(e))));
            }
            if queue_on_gitea {
                return Some(undetermined(
                    "champion.mergeMode=queue, but Gitea has no merge queue",
                ));
            }
            let (l, nwo) = match live(env, repo.as_ref()) {
                Ok(x) => x,
                Err(r) => return Some(r),
            };
            let gh = env.gh.clone();
            let required = move || {
                super::preflight::github_preflight(&gh, &nwo, None)
                    .map(|c| c.required_checks)
                    .map_err(|e| format!("[{}] {e}", e.code()))
            };
            let r = handoff(&ctx(env, mode, &l), *pr, approved_sha, &required);
            render_handoff(*pr, approved_sha, &r)
        }
        MergeQueueCmd::Step {
            pr,
            approved_sha,
            repo,
        } => {
            if mode == MergeMode::Direct {
                return Some(direct());
            }
            let rec = run(
                &MergeQueueCmd::Reconcile {
                    pr: Some(*pr),
                    repo: repo.clone(),
                    mode_only: false,
                },
                env,
                mode,
            )?;
            // Hand off only on the explicit CONTINUE verdict; every other
            // outcome (queued, dropped, merged, undetermined) is final here.
            if rec
                .stdout
                .first()
                .is_some_and(|l| l.starts_with("LOOM-MERGE-QUEUE-CONTINUE"))
            {
                return run(
                    &MergeQueueCmd::Handoff {
                        pr: *pr,
                        approved_sha: approved_sha.clone(),
                        repo: repo.clone(),
                    },
                    env,
                    mode,
                );
            }
            rec
        }
        MergeQueueCmd::Revoke { pr, reason, repo } => {
            if mode == MergeMode::Direct {
                return Some(lines(
                    0,
                    vec!["LOOM-MERGE-QUEUE-DIRECT nothing to revoke".to_string()],
                ));
            }
            let (l, _) = match live(env, repo.as_ref()) {
                Ok(x) => x,
                Err(r) => return Some(r),
            };
            match revoke_for_transition(&ctx(env, mode, &l), *pr, reason) {
                None => direct(),
                Some(rev) => lines(
                    if rev.safe_to_transition() { 0 } else { 1 },
                    vec![
                        format!(
                            "LOOM-MERGE-QUEUE-REVOKED pr={pr} safe_to_transition={}",
                            rev.safe_to_transition()
                        ),
                        transition_line(&rev),
                    ],
                ),
            }
        }
        MergeQueueCmd::AuthorizeCheck { pr, pr_head, repo } => {
            let (l, _) = match live(env, repo.as_ref()) {
                Ok(x) => x,
                Err(r) => return Some(r),
            };
            match authorize_check(&l.forge, *pr, pr_head, chrono::Utc::now()) {
                CheckConclusion::Success => lines(
                    0,
                    vec![format!(
                        "LOOM-MERGE-AUTHORIZATION success pr={pr} head={pr_head}"
                    )],
                ),
                CheckConclusion::Failure(why) => lines(
                    1,
                    vec![
                        format!("LOOM-MERGE-AUTHORIZATION failure pr={pr} head={pr_head}"),
                        why.iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("; "),
                    ],
                ),
            }
        }
        _ => return None,
    })
}
