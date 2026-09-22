//! Which on-disk store a launch's per-model token usage must be read from
//! (Issue #8507).
//!
//! # Why a seam exists at all
//!
//! Per-model token attribution used to be one hard-wired reader:
//! [`crate::transcript_tokens`], which sums Claude Code's own JSONL
//! transcripts under `${CLAUDE_CONFIG_DIR:-~/.claude}/projects/`. No other
//! runtime writes that file, so a sweep or role tick dispatched on OpenCode
//! produced **no** `tokens_by_model` at all — and downstream, no model badge
//! on the public fleet feed, which is what #8507 reported for the GLM-5.3
//! trial's 46 merged PRs.
//!
//! Three call sites need the same decision — the sweep outcome journal, the
//! role-tick journal, and safehouse's `completion-v1` narration — so the
//! decision lives here once rather than as three copies of a
//! `runtime == "opencode"` test that could drift apart.
//!
//! # The contract every source keeps
//!
//! - **Unknown is not zero.** A source that finds nothing returns `None`, not
//!   `Some(vec![])`; the consumer then omits the field rather than publishing
//!   a fabricated zero-token breakdown.
//! - **Never guess a model name.** A source only ever reports a model id it
//!   actually read off disk (see `reconcile.ts`'s equivalent rule downstream).
//! - **The runtime comes from the launch, not from config.** Every caller
//!   resolves it from the launch's own `# LOOM_LAUNCH` record
//!   ([`crate::launch_record::RuntimeAttribution`]) — the ground truth of what
//!   ran — never from the dispatch-time configuration, which records only what
//!   was *requested*.
//! - **An unrecognised runtime falls back to the Claude reader.** That keeps
//!   every pre-#8507 payload byte-identical: a Claude/legacy spawn writes no
//!   launch record at all, so its `runtime` is `None`, which selects exactly
//!   the reader it has always used.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::script_helpers::sweep_experiment::ModelUsageTotals;

/// The `runtime` value (as written to the `# LOOM_LAUNCH` record by
/// `worker_spawn::run`) that selects the OpenCode session-store reader.
pub const OPENCODE_RUNTIME: &str = "opencode";

/// The store a given runtime's per-model token usage is read from.
///
/// Deliberately an enum rather than a bare boolean: Pi and Codex each have
/// their own usage store that #8507 leaves to follow-up work, and a new
/// variant here is the one place that has to change when either lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    /// Claude Code's on-disk JSONL transcripts ([`crate::transcript_tokens`]).
    /// The default for every runtime not explicitly mapped below, including an
    /// absent runtime.
    ClaudeTranscripts,
    /// OpenCode's own SQLite session store ([`crate::opencode_usage`]).
    OpenCodeSessionDb,
}

impl UsageSource {
    /// Select the source for a launch's `runtime`, as read off its
    /// `# LOOM_LAUNCH` record.
    ///
    /// `None` (a Claude or legacy-adapter spawn, which writes no launch
    /// record) and any unrecognised runtime both select
    /// [`Self::ClaudeTranscripts`] — the pre-#8507 behavior, unchanged.
    #[must_use]
    pub fn for_runtime(runtime: Option<&str>) -> Self {
        match runtime.map(str::trim) {
            Some(OPENCODE_RUNTIME) => Self::OpenCodeSessionDb,
            _ => Self::ClaudeTranscripts,
        }
    }

    /// Stable identifier for logs and tests.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeTranscripts => "claude-transcripts",
            Self::OpenCodeSessionDb => "opencode-session-db",
        }
    }
}

/// The working directories a **sweep's** OpenCode sessions can legitimately
/// have been opened in: the workspace root itself (the Curator/Judge/Champion
/// phases and the orchestrator run there) plus this issue's own worktree (where
/// the Builder and Doctor phases run).
///
/// A *sibling* issue's worktree is deliberately excluded, so two concurrent
/// sweeps in one workspace never fold each other's tokens in — the window
/// filter alone could not separate them, because concurrent sweeps overlap in
/// time by construction.
#[must_use]
pub fn sweep_directories(workspace_root: &Path, issue: u32) -> Vec<PathBuf> {
    vec![
        workspace_root.to_path_buf(),
        crate::worktree_root::worktree_root(workspace_root).join(format!("issue-{issue}")),
    ]
}

/// The working directories a **role tick's** OpenCode sessions can have been
/// opened in: the workspace root alone. A scheduled role tick runs in the main
/// checkout and never gets a worktree of its own.
#[must_use]
pub fn role_tick_directories(root: &Path) -> Vec<PathBuf> {
    vec![root.to_path_buf()]
}

/// Per-`(model, speed, service_tier)` token totals for one sweep, read from
/// whichever store `runtime` selects.
///
/// `runtime` is the launch's own `# LOOM_LAUNCH` `runtime` value (see the
/// module doc); `window` is the sweep's wall-clock span. `None` — never
/// `Some(vec![])` — when the selected source found nothing attributable.
#[must_use]
pub fn sweep_tokens_by_model(
    runtime: Option<&str>,
    workspace_root: &Path,
    issue: u32,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Option<Vec<ModelUsageTotals>> {
    match UsageSource::for_runtime(runtime) {
        UsageSource::OpenCodeSessionDb => crate::opencode_usage::tokens_by_model(
            &sweep_directories(workspace_root, issue),
            window,
            None,
        ),
        UsageSource::ClaudeTranscripts => {
            let projects_dir = crate::transcript_tokens::claude_projects_dir()?;
            crate::transcript_tokens::sum_sweep_tokens_by_model(
                &projects_dir,
                workspace_root,
                issue,
                window,
            )
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn opencode_selects_the_session_db_and_everything_else_stays_on_transcripts() {
        assert_eq!(UsageSource::for_runtime(Some("opencode")), UsageSource::OpenCodeSessionDb);
        assert_eq!(
            UsageSource::for_runtime(Some(" opencode ")),
            UsageSource::OpenCodeSessionDb,
            "the launch record's value is trimmed before matching"
        );
        for other in [None, Some("claude"), Some("pi"), Some("codex"), Some("")] {
            assert_eq!(
                UsageSource::for_runtime(other),
                UsageSource::ClaudeTranscripts,
                "an unmapped runtime ({other:?}) must keep the pre-#8507 reader"
            );
        }
    }

    #[test]
    fn a_sweeps_directory_set_is_the_root_plus_its_own_worktree_only() {
        let root = Path::new("/w/loom");
        let dirs = sweep_directories(root, 8507);
        // Exactly two entries, and the second is resolved through
        // `worktree_root` so an operator's external-volume override is honored
        // (hence a name assertion rather than a hardcoded default path).
        assert_eq!(dirs.len(), 2, "{dirs:?}");
        assert_eq!(dirs[0], PathBuf::from("/w/loom"));
        assert_eq!(dirs[1].file_name().unwrap(), "issue-8507");
        assert!(
            !dirs
                .iter()
                .any(|d| d.file_name().is_some_and(|n| n == "issue-8508")),
            "a concurrent sibling sweep's worktree must never be attributed here"
        );
    }

    #[test]
    fn a_role_ticks_directory_set_is_the_root_alone() {
        assert_eq!(role_tick_directories(Path::new("/w/loom")), vec![PathBuf::from("/w/loom")]);
    }

    #[test]
    fn source_names_are_stable_for_logs() {
        assert_eq!(UsageSource::ClaudeTranscripts.as_str(), "claude-transcripts");
        assert_eq!(UsageSource::OpenCodeSessionDb.as_str(), "opencode-session-db");
    }
}
