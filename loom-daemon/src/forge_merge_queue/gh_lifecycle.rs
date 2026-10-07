//! GitHub [`LifecycleForge`] and the production wiring of the queue
//! lifecycle (#10256, Phase B2).
//!
//! Calls go through the counted [`GhInvocation`] facade, same as
//! [`super::github`]. Comment listings are filtered by
//! [`crate::comment_trust::TrustPolicy`] before any marker is read.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::events::FileEventSink;
use super::forge::{LifecycleForge, PrSnapshot, RemovalEvent};
use super::github::{safe_detail, GhQueueApi};
use super::group_github::{group_transition_line, revoke_for_transition_groups};
use super::lifecycle::{sweep, Ctx, Reconciled};
use super::mode::{resolve_merge_mode, MergeMode};
use super::ops::PrState;
use crate::cmd_out::CmdOutcome;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

pub const REMOVALS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      timelineItems(last: 20, itemTypes: [REMOVED_FROM_MERGE_QUEUE_EVENT]) {
        nodes { ... on RemovedFromMergeQueueEvent { reason createdAt } }
      }
    }
  }
}";

/// GitHub-backed [`LifecycleForge`] for one repository.
pub struct GhLifecycleForge {
    gh: String,
    root: PathBuf,
    owner: String,
    name: String,
}

impl GhLifecycleForge {
    /// # Errors
    ///
    /// When `nwo` is not `owner/repo`.
    pub fn new(gh: &str, root: &Path, nwo: &str) -> Result<Self, String> {
        match GhTarget::repo(nwo) {
            Ok(GhTarget::Repo { owner, repo }) => Ok(Self {
                gh: gh.to_string(),
                root: root.to_path_buf(),
                owner,
                name: repo,
            }),
            _ => Err("repository must be an `owner/repo` slug".to_string()),
        }
    }

    pub(super) fn owner(&self) -> &str {
        &self.owner
    }

    pub(super) fn repo_name(&self) -> &str {
        &self.name
    }

    pub(super) fn nwo(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    fn call(&self, op: &'static str, intent: AccessIntent, args: &[String]) -> CmdOutcome {
        let target = GhTarget::Repo {
            owner: self.owner.clone(),
            repo: self.name.clone(),
        };
        GhInvocation::new(Operation::new(op), intent, target, CALL_TIMEOUT)
            .program(&self.gh)
            .current_dir(&self.root)
            .args(args)
            .run()
    }

    pub(super) fn ok(
        &self,
        op: &'static str,
        intent: AccessIntent,
        args: &[String],
    ) -> Result<String, String> {
        let out = self.call(op, intent, args);
        if out.succeeded() {
            Ok(out.stdout_trimmed())
        } else {
            Err(format!(
                "{op}: {}",
                safe_detail(&format!("{}\n{}", out.stderr_trimmed(), out.stdout_trimmed()))
            ))
        }
    }
}

fn s(v: &str) -> String {
    v.to_string()
}

/// Decode `GET repos/<nwo>/pulls/<n>`. Pure.
///
/// # Errors
///
/// A shape it cannot read.
pub fn parse_snapshot(pr: u32, text: &str) -> Result<PrSnapshot, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("unparseable PR: {e}"))?;
    let head = v
        .pointer("/head/sha")
        .and_then(serde_json::Value::as_str)
        .ok_or("PR has no head sha")?;
    let merged = v.get("merged").and_then(serde_json::Value::as_bool) == Some(true);
    let state = match (merged, v.get("state").and_then(serde_json::Value::as_str)) {
        (true, _) => PrState::Merged,
        (false, Some("open")) => PrState::Open,
        (false, Some("closed")) => PrState::Closed,
        (_, other) => return Err(format!("unknown PR state {other:?}")),
    };
    let labels = v
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .ok_or("PR has no labels array")?
        .iter()
        .filter_map(|l| l.get("name").and_then(serde_json::Value::as_str).map(s))
        .collect();
    Ok(PrSnapshot {
        number: pr,
        state,
        head_sha: head.to_string(),
        labels,
        merged_at: v
            .get("merged_at")
            .and_then(serde_json::Value::as_str)
            .map(s),
    })
}

/// Decode the removals query's `data`. Pure.
///
/// # Errors
///
/// A shape it cannot read.
pub fn parse_removals(data: &serde_json::Value) -> Result<Vec<RemovalEvent>, String> {
    let nodes = data
        .pointer("/data/repository/pullRequest/timelineItems/nodes")
        .and_then(serde_json::Value::as_array)
        .ok_or("unexpected timeline shape")?;
    Ok(nodes
        .iter()
        .filter_map(|n| {
            Some(RemovalEvent {
                reason: n.get("reason").and_then(serde_json::Value::as_str).map(s),
                created_at: n.get("createdAt")?.as_str()?.to_string(),
            })
        })
        .collect())
}

impl LifecycleForge for GhLifecycleForge {
    fn snapshot(&self, pr: u32) -> Result<PrSnapshot, String> {
        let text = self.ok(
            "merge_queue.pr_snapshot",
            AccessIntent::Read,
            &[s("api"), format!("repos/{}/pulls/{pr}", self.nwo())],
        )?;
        parse_snapshot(pr, &text)
    }

    fn trusted_comments(&self, pr: u32) -> Result<Vec<String>, String> {
        let text = self.ok(
            "merge_queue.pr_comments",
            AccessIntent::Read,
            &[
                s("api"),
                format!("repos/{}/issues/{pr}/comments?per_page=100", self.nwo()),
                s("--paginate"),
            ],
        )?;
        crate::comment_trust::TrustPolicy::for_root(&self.root)
            .trusted_bodies(text.as_bytes())
            .ok_or_else(|| "comment listing could not be parsed".to_string())
    }

    fn post_comment(&self, pr: u32, body: &str) -> Result<(), String> {
        self.ok(
            "merge_queue.pr_comment",
            AccessIntent::Write,
            &[
                s("api"),
                s("-X"),
                s("POST"),
                format!("repos/{}/issues/{pr}/comments", self.nwo()),
                s("-f"),
                format!("body={body}"),
            ],
        )
        .map(|_| ())
    }

    fn edit_labels(&self, pr: u32, add: &[&str], remove: &[&str]) -> Result<(), String> {
        if !add.is_empty() {
            let mut args = vec![
                s("api"),
                s("-X"),
                s("POST"),
                format!("repos/{}/issues/{pr}/labels", self.nwo()),
            ];
            for l in add {
                args.push(s("-f"));
                args.push(format!("labels[]={l}"));
            }
            self.ok("merge_queue.pr_add_labels", AccessIntent::Write, &args)?;
        }
        for l in remove {
            let path = format!("repos/{}/issues/{pr}/labels/{l}", self.nwo());
            if let Err(e) = self.ok(
                "merge_queue.pr_remove_label",
                AccessIntent::Write,
                &[s("api"), s("-X"), s("DELETE"), path],
            ) {
                // Already absent is the desired end state.
                if !e.contains("404") && !e.to_ascii_lowercase().contains("does not exist") {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn removals(&self, pr: u32) -> Result<Vec<RemovalEvent>, String> {
        let text = self.ok(
            "merge_queue.removals",
            AccessIntent::Read,
            &[
                s("api"),
                s("graphql"),
                s("-f"),
                format!("query={REMOVALS_QUERY}"),
                s("-f"),
                format!("owner={}", self.owner),
                s("-f"),
                format!("name={}", self.name),
                s("-F"),
                format!("number={pr}"),
            ],
        )?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("unparseable timeline: {e}"))?;
        if let Some(errs) = v.get("errors").filter(|e| !e.is_null()) {
            return Err(safe_detail(&errs.to_string()));
        }
        parse_removals(&v)
    }
}

/// `owner/repo` of the checkout at `root`.
fn nwo_at(gh: &str, root: &Path) -> Option<String> {
    let out = GhInvocation::new(
        Operation::new("merge_queue.repo_nwo"),
        AccessIntent::Read,
        GhTarget::None,
        CALL_TIMEOUT,
    )
    .program(gh)
    .current_dir(root)
    .args([
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "-q",
        ".nameWithOwner",
    ])
    .run();
    let nwo = out.stdout_trimmed();
    (out.succeeded() && nwo.contains('/')).then_some(nwo)
}

/// Outcome of resolving the live seams for a checkout.
enum Live<R> {
    /// Direct mode: nothing read beyond config, no forge call.
    Direct,
    /// Not confirmed direct, yet the live seams could not be built (mode,
    /// repository or client unresolved). Unknown facts: callers fail closed.
    Unresolved(String),
    Ran(R),
}

/// Build the live seams for `root` and run `f`.
fn with_live<R>(
    gh: &Path,
    root: &Path,
    f: impl FnOnce(&Ctx<'_>, &GhLifecycleForge) -> R,
) -> Live<R> {
    let mode = match resolve_merge_mode(root) {
        Ok(m) => m.mode,
        Err(e) => {
            log::warn!("merge-queue: {}: {e}", root.display());
            return Live::Unresolved(format!("merge mode unresolvable: {e}"));
        }
    };
    if mode == MergeMode::Direct {
        return Live::Direct;
    }
    let gh = gh.to_string_lossy().to_string();
    let Some(nwo) = nwo_at(&gh, root) else {
        log::warn!("merge-queue: could not resolve the repository at {}", root.display());
        return Live::Unresolved("repository unresolvable".to_string());
    };
    let (Ok(queue), Ok(forge)) =
        (GhQueueApi::new(&gh, &nwo), GhLifecycleForge::new(&gh, root, &nwo))
    else {
        return Live::Unresolved("forge client could not be built".to_string());
    };
    let events = FileEventSink::for_root(root);
    let ctx = Ctx {
        mode,
        execution_enabled: super::QUEUE_EXECUTION_ENABLED,
        queue: &queue,
        forge: &forge,
        events: &events,
        now: chrono::Utc::now(),
    };
    Live::Ran(f(&ctx, &forge))
}

/// The daemon's periodic pass: reconcile every pending queued PR so a drop or
/// a merge is seen within one successful tick. No-op in direct mode.
pub fn daemon_tick(gh: &Path, root: &Path) {
    let Live::Ran(res) = with_live(gh, root, |c, _| sweep(c)) else {
        return;
    };
    match res {
        Ok(rows) => {
            for (pr, r) in rows {
                match r {
                    Reconciled::Undetermined(why) => {
                        log::warn!("merge-queue: PR #{pr} in {}: {why}", root.display());
                    }
                    other => log::info!("merge-queue: PR #{pr} in {}: {other:?}", root.display()),
                }
            }
        }
        Err(e) => log::warn!("merge-queue: event log in {}: {e}", root.display()),
    }
}

/// Audit line used when queue mode could not be ruled out but the revocation
/// could not be attempted. Never silent: the caller's comment carries it.
#[must_use]
pub fn unresolved_line(why: &str) -> String {
    format!(
        "- **Merge-queue revocation NOT confirmed** ({why}): this repository is not confirmed to \
         be in direct mode, so a queued entry may remain. The label flip still denies the \
         `loom/merge-authorization` check, but the queue entry was not revoked or dequeued."
    )
}

/// Revoke + dequeue before a Loom-owned transition on `pr` in `root`.
/// Returns the audit line; `None` only when direct mode is confirmed.
#[must_use]
pub fn revoke_for_root(gh: &Path, root: &Path, pr: u32, reason: &str) -> Option<String> {
    match with_live(gh, root, |ctx, forge| {
        revoke_for_transition_groups(ctx, forge, pr, reason).map(|rev| group_transition_line(&rev))
    }) {
        Live::Direct => None,
        Live::Unresolved(why) => Some(unresolved_line(&why)),
        Live::Ran(line) => line,
    }
}
