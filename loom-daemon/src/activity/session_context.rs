//! Join keys for one transcript's `session.summary` record (Issue #9445).
//!
//! `session.summary` shipped in #8757 with the counters per-issue effort
//! analysis needs (`loom.tokens.*`, `loom.turns`, `loom.wall_ms`,
//! `loom.tool_errors`) and none of the keys needed to *find* them: `loom.repo`
//! was the cwd basename (so a worktree named `wood-reward` or
//! `agent-afb133cdd702752a5` reported that as its repo), `loom.issue` was set
//! only when the session's own first user message was a `/loom:<role> <N>`
//! slash command, and with no issue the #8908 trace join never fired either —
//! measured on live SigNoz over 2026-09-26..29: `loom.issue` present on 1 of
//! 4,121 rows, `trace_id` empty on all of them.
//!
//! This module resolves the keys instead of guessing them:
//!
//! | Key | Source |
//! |---|---|
//! | `repo` | the `owner/name` slug of the session workspace's `origin` remote — **omitted**, never a directory name, when there is no remote to read |
//! | `issue` | the slash-command argument, else an `issue-<N>` worktree in the cwd, else a `feature/issue-<N>` branch |
//! | `pr_number` | the issue's own sweep checkpoint, when it was written at or after the session started |
//! | `kind` | [`SessionKind`] — why a row with no `issue` has none |
//!
//! **No forge round trip.** Everything above is the transcript's own fields
//! plus one memoised local `git remote get-url origin` and one local
//! checkpoint read. `visibility` therefore stays at its fail-closed `Private`
//! default: resolving it truthfully needs the `gh` probe this pass
//! deliberately does not make.
//!
//! The split between [`SessionContext::derive`] (pure, transcript-only) and
//! [`SessionContext::resolve`] (adds the two filesystem reads) exists so every
//! attribution rule is testable without a checkout, and so the filesystem work
//! happens once per transcript in the ingest pass rather than inside the
//! record-building derivation.

use std::path::Path;

use crate::activity::transcript_parse::ParsedTranscript;
use crate::telemetry::{RepoVisibility, SessionKind};

/// The resolved join keys for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionContext {
    /// `owner/name` forge slug, or `None` when it could not be resolved from a
    /// remote. Never a directory name — that was the #9445 defect.
    pub repo: Option<String>,
    /// Visibility tag for `repo`; always the fail-closed default (see the
    /// module doc — this pass makes no forge call).
    pub visibility: RepoVisibility,
    /// The issue this session worked, when any of the three sources names one.
    pub issue: Option<u32>,
    /// The issue's PR, when its checkpoint named one during this session.
    pub pr_number: Option<u32>,
    /// Why `issue` is set — or deliberately is not.
    pub kind: SessionKind,
}

impl SessionContext {
    /// Everything derivable from the transcript alone: `issue` and `kind`.
    ///
    /// Pure — no filesystem, no subprocess. `repo`/`pr_number` are left
    /// unresolved (`None`), which is also exactly what a caller with no
    /// workspace on this host should report.
    #[must_use]
    pub fn derive(parsed: &ParsedTranscript) -> Self {
        let issue = attributed_issue(parsed);
        let kind = match (issue, parsed.role.as_deref()) {
            (Some(_), _) => SessionKind::Sweep,
            (None, Some(_)) => SessionKind::Role,
            (None, None) => SessionKind::Interactive,
        };
        SessionContext {
            repo: None,
            visibility: RepoVisibility::Private,
            issue,
            pr_number: None,
            kind,
        }
    }

    /// [`Self::derive`], plus the two local reads: the workspace's `origin`
    /// remote (for `repo`) and the issue's sweep checkpoint (for
    /// `pr_number`).
    #[must_use]
    pub fn resolve(parsed: &ParsedTranscript) -> Self {
        let mut ctx = Self::derive(parsed);
        let Some(cwd) = parsed.cwd.as_deref() else {
            return ctx;
        };
        ctx.repo = repo_slug_for_cwd(Path::new(cwd));
        ctx.pr_number = ctx
            .issue
            .and_then(|issue| checkpoint_pr_number(Path::new(cwd), issue, parsed.first_timestamp));
        ctx
    }
}

/// The issue a session worked, in precedence order (Issue #9445):
///
/// 1. the `/loom:<role> <N>` slash command's own argument
///    ([`crate::activity::transcript_parse::attribute_issue`]) — the session
///    said so itself;
/// 2. an `issue-<N>` worktree directory in the cwd — a Builder/Doctor session
///    dispatched into `.loom/worktrees/issue-<N>` (or the `.claude/worktrees/`
///    layout some repos use);
/// 3. a `feature/issue-<N>` branch — a session that moved into the worktree's
///    branch without its cwd saying so (or a hand-run one on that branch).
///
/// A negative or out-of-`u32`-range number from source 1 is dropped rather
/// than coerced.
#[must_use]
fn attributed_issue(parsed: &ParsedTranscript) -> Option<u32> {
    parsed
        .issue
        .and_then(|i| u32::try_from(i).ok())
        .or_else(|| parsed.cwd.as_deref().and_then(issue_from_cwd))
        .or_else(|| parsed.branch.as_deref().and_then(issue_from_branch))
}

/// The issue number of an `issue-<N>` worktree in `cwd`, if any.
///
/// Matches an `issue-<digits>` path component that directly follows a
/// `worktrees` component, which covers both layouts Loom's guards recognise:
/// `<root>/.loom/worktrees/issue-42` (what `worktree.sh` creates) and
/// `<root>/.claude/worktrees/issue-42`. A cwd *below* the worktree root
/// (`…/issue-42/dashboard/src`) still matches — the scan looks at every
/// component, not just the last.
#[must_use]
pub fn issue_from_cwd(cwd: &str) -> Option<u32> {
    let mut previous: Option<&str> = None;
    for component in cwd.split('/') {
        if previous == Some("worktrees") {
            if let Some(issue) = component
                .strip_prefix("issue-")
                .and_then(|n| n.parse::<u32>().ok())
            {
                return Some(issue);
            }
        }
        if !component.is_empty() {
            previous = Some(component);
        }
    }
    None
}

/// The issue number a `feature/issue-<N>` branch names, if any.
///
/// Trailing text after the number is tolerated (`feature/issue-9447-install-merge`
/// is a real branch on this repo), as is a prefix segment, so a namespaced
/// spelling (`user/feature/issue-42`) resolves too. `issue-42abc` does not
/// match: the digits must end the token or be followed by `-`/`_`/`/`/`.`.
#[must_use]
pub fn issue_from_branch(branch: &str) -> Option<u32> {
    let mut previous: Option<&str> = None;
    for segment in branch.split('/') {
        if previous == Some("feature") {
            if let Some(rest) = segment.strip_prefix("issue-") {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                let boundary_ok = rest[digits.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| matches!(c, '-' | '_' | '.'));
                if boundary_ok {
                    return digits.parse::<u32>().ok();
                }
            }
        }
        if !segment.is_empty() {
            previous = Some(segment);
        }
    }
    None
}

/// The `owner/name` slug for a session whose cwd was `cwd`, or `None`.
///
/// Two attempts, both local `git remote get-url origin` reads memoised per
/// directory by [`crate::forge_etag_store::remote_identity`] (the daemon's
/// existing per-root remote resolver — reused rather than re-forked so a busy
/// ingest pass costs at most one `git` call per directory for the process's
/// lifetime):
///
/// 1. `cwd` itself. Works from any directory inside any checkout — including
///    a worktree whose directory name has nothing to do with the repo, which
///    is the whole point of resolving through the remote.
/// 2. the enclosing workspace root ([`workspace_of`]) when `cwd` is gone.
///    An issue worktree is removed on merge, and the summaries of the sweep
///    that merged it are frequently re-read by a later ingest pass — the
///    primary clone survives, and it is the same repo.
///
/// `None` when neither answers: a cwd outside any checkout, or one whose
/// remote is unparseable. Deliberately not a basename fallback.
#[must_use]
fn repo_slug_for_cwd(cwd: &Path) -> Option<String> {
    let owner_repo =
        |dir: &Path| crate::forge_etag_store::remote_identity(dir).map(|(_host, nwo)| nwo);
    owner_repo(cwd).or_else(|| {
        let workspace = crate::observability::runtime_usage::join::workspace_of(cwd);
        (workspace != cwd).then(|| owner_repo(&workspace)).flatten()
    })
}

/// The PR number `issue`'s sweep checkpoint records, when that checkpoint
/// belongs to this session's run.
///
/// The checkpoint (`<workspace>/.loom/sweep-checkpoint/issue-<N>.json`) is the
/// one local artifact that already knows a sweep's PR — `/loom:sweep` writes
/// `pr_number` there at the `builder-done` boundary — so this reads it rather
/// than adding a forge lookup. It persists across dispatches, so the same
/// freshness rule the registry's live-phase overlay uses applies: a checkpoint
/// last written *before* the session began describes an earlier dispatch, and
/// its PR is not this session's.
#[must_use]
fn checkpoint_pr_number(
    cwd: &Path,
    issue: u32,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<u32> {
    let started_at = started_at?;
    let checkpoint = crate::observability::runtime_usage::join::workspace_of(cwd)
        .join(".loom/sweep-checkpoint")
        .join(format!("issue-{issue}.json"));
    crate::sweep_registry::reaper::checkpoint_written_by_run(&checkpoint, started_at)
        .then(|| crate::sweep_registry::reaper::read_checkpoint_pr_number(&checkpoint))
        .flatten()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests;
