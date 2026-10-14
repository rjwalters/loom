//! The Curator's backlog level at dispatch (#11819).
//!
//! A repo that sets `curator.backlogTarget` (any config tier, through
//! [`crate::config_resolver::curator_backlog_target`]) has its Curator run a
//! keep-or-close admission step, stricter as the repo's open-issue count
//! passes that target. The daemon, not the agent, computes the level: on the
//! dispatch's blocking thread it counts the repo's open issues (one REST
//! search, PRs excluded, search quota rather than GraphQL, #10039), derives
//! the level from `r = open / target`
//! ([`BacklogLevel::for_counts`]), appends it to the slash command the same
//! way `--max-proposals` reaches the Architect
//! (`/loom:curator --backlog-level strict --backlog-open 412
//! --backlog-target 140`), and exports one `backlog.level` record.
//!
//! - **No target ⇒ nothing**: no forge call, no flags, no record; the prompt
//!   is byte-identical to a pre-#11819 dispatch.
//! - **The count fails ⇒ `normal`** plus a warning, and `--backlog-open` is
//!   omitted. Never stricter than intended, never skipped.
//! - Only the current count is read; nothing keeps a history.
//!
//! The prompt builder ([`super::resolve_role_prompt`]) stays pure: this runs
//! inside [`super::invoke_with_collision_probe`], which every dispatch path
//! (interval tick, idle edge) already calls on a blocking thread.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::telemetry::kinds::backlog_level::{BacklogLevel, BacklogLevelRecord};
use crate::telemetry::provenance::Provenance;

/// The only role this applies to.
pub const CURATOR_ROLE: &str = "curator";

/// Upper bound on the one count request.
const COUNT_TIMEOUT: Duration = Duration::from_secs(30);

/// What one Curator dispatch is told about the backlog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklogDispatch {
    /// The level passed as `--backlog-level`.
    pub level: BacklogLevel,
    /// The open count, when the count succeeded.
    pub open: Option<u64>,
    /// The resolved `curator.backlogTarget`.
    pub target: u64,
    /// Why the count failed, when it did.
    pub count_error: Option<String>,
}

impl BacklogDispatch {
    /// The level for a resolved `target` and the count's result.
    #[must_use]
    pub fn from_count(target: u64, count: Result<u64, String>) -> Self {
        match count {
            Ok(open) => Self {
                level: BacklogLevel::for_counts(open, target),
                open: Some(open),
                target,
                count_error: None,
            },
            Err(e) => Self {
                level: BacklogLevel::Normal,
                open: None,
                target,
                count_error: Some(e),
            },
        }
    }

    /// The slash-command arguments, with a leading space.
    #[must_use]
    pub fn args(&self) -> String {
        let open = self
            .open
            .map(|n| format!(" --backlog-open {n}"))
            .unwrap_or_default();
        format!(
            " --backlog-level {}{open} --backlog-target {}",
            self.level.as_str(),
            self.target
        )
    }

    /// The `backlog.level` record for this dispatch.
    #[must_use]
    pub fn record(&self, repo: &str, at: DateTime<Utc>) -> BacklogLevelRecord {
        BacklogLevelRecord {
            repo: repo.to_ascii_lowercase(),
            open: self.open,
            target: self.target,
            level: self.level,
            dispatch_id: crate::observability::lifecycle::role_execution_id(CURATOR_ROLE, at),
            count_error: self.count_error.clone(),
            observed_at: at,
            loom: Provenance::current(),
        }
    }
}

/// Resolve the backlog level for one dispatch of `role` on `root`, counting
/// with `count`. `None` for every role but the Curator, and for a repo with no
/// target (in which case `count` is never called). A failed count is logged
/// as a warning and resolves to `normal`.
pub fn resolve_with(
    root: &Path,
    role: &str,
    count: impl FnOnce(&Path) -> Result<u64, String>,
) -> Option<BacklogDispatch> {
    if role != CURATOR_ROLE {
        return None;
    }
    let target = crate::config_resolver::curator_backlog_target(root)?;
    let dispatch = BacklogDispatch::from_count(target, count(root));
    if let Some(e) = &dispatch.count_error {
        log::warn!(
            "role_runner: curator backlog count for {} failed ({e}); dispatching with \
             --backlog-level normal against curator.backlogTarget={target} (#11819)",
            root.display()
        );
    }
    Some(dispatch)
}

/// `prompt` plus the backlog flags for a Curator dispatch on a repo with a
/// target, exporting one `backlog.level` record; `prompt` unchanged otherwise.
/// **Blocking**: may make one forge call.
#[must_use]
pub fn with_backlog_args(root: &Path, role: &str, prompt: &str) -> String {
    let Some(dispatch) = resolve_with(root, role, count_open_issues) else {
        return prompt.to_string();
    };
    let repo = crate::release_resolve::host::repo_slug(root).unwrap_or_default();
    crate::observability::ops::emit_record(crate::telemetry::TelemetryRecord::BacklogLevel(
        dispatch.record(&repo, Utc::now()),
    ));
    format!("{prompt}{}", dispatch.args())
}

/// The REST path counting `slug`'s open issues: first page of one item, so
/// only `total_count` matters.
#[must_use]
pub fn count_path(slug: &str) -> String {
    format!(
        "search/issues?q=repo%3A{}%20is%3Aissue%20is%3Aopen&per_page=1",
        slug.replace('/', "%2F")
    )
}

/// `total_count` from a search answer.
///
/// # Errors
/// The answer is not JSON or carries no unsigned `total_count`.
pub fn parse_total_count(body: &str) -> Result<u64, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("the answer was not JSON ({e})"))?;
    value
        .get("total_count")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "the answer carried no `total_count`".to_string())
}

/// Count `root`'s open issues (PRs excluded) with one REST search.
fn count_open_issues(root: &Path) -> Result<u64, String> {
    use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    let slug = crate::release_resolve::host::repo_slug(root)
        .ok_or_else(|| "no owner/repo from the origin remote".to_string())?;
    let outcome = GhInvocation::new(
        Operation::new("role_runner.curator_backlog_count"),
        AccessIntent::Read,
        GhTarget::None,
        COUNT_TIMEOUT,
    )
    .forge_op(crate::forge_call_stats::ops::ISSUE_SEARCH)
    .identity_scope(None, Some(&slug))
    .current_dir(root)
    .args(["api", &count_path(&slug)])
    .run();
    match outcome {
        crate::cmd_out::CmdOutcome::Ran(out) if out.status.success() => {
            parse_total_count(&String::from_utf8_lossy(&out.stdout))
        }
        crate::cmd_out::CmdOutcome::Ran(out) => Err(format!(
            "gh exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        other => Err(format!("gh did not run ({other:?})")),
    }
}

#[cfg(test)]
#[path = "backlog_tests.rs"]
mod tests;
