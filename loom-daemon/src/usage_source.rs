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

/// The `runtime` value that selects the Kimi Code CLI session-store reader
/// (Issue #8564). Matches `defaults/runtimes/kimi.json`'s own `runtime` key,
/// which is what `worker_spawn::run` writes into the launch record.
pub const KIMI_RUNTIME: &str = "kimi";

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
    /// Kimi Code CLI's own `session_index.jsonl` + per-agent `wire.jsonl`
    /// event logs ([`crate::kimi_usage`]).
    KimiSessionStore,
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
            Some(KIMI_RUNTIME) => Self::KimiSessionStore,
            _ => Self::ClaudeTranscripts,
        }
    }

    /// Stable identifier for logs and tests.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeTranscripts => "claude-transcripts",
            Self::OpenCodeSessionDb => "opencode-session-db",
            Self::KimiSessionStore => "kimi-session-store",
        }
    }

    /// Whether this source is a native harness's own session store rather than
    /// Claude Code's JSONL transcripts.
    ///
    /// Callers that derive *more* than tokens from a Claude transcript (the
    /// role-tick scanner's forge-mutating `actions`, for instance) need this
    /// to decide that the extra signal is **unmeasured**, not an observed
    /// zero. Expressed as a predicate on the enum rather than as an equality
    /// test against one variant so that adding the next native store does not
    /// silently route it back onto the Claude reader — the exact bug a
    /// `== OpenCodeSessionDb` test would have introduced for Kimi (#8564).
    #[must_use]
    pub fn is_native_store(self) -> bool {
        !matches!(self, Self::ClaudeTranscripts)
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
        UsageSource::KimiSessionStore => crate::kimi_usage::tokens_by_model(
            &sweep_directories(workspace_root, issue),
            // No consumer captures a launch's Kimi session id yet (it rides on
            // the `session.resume_hint` stdout line — see
            // `crate::kimi_usage`'s module doc), so attribution uses the same
            // directory+window key every other reader behind this seam uses.
            None,
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

/// Per-`(model, speed, service_tier)` token totals for one **role tick**, read
/// from whichever store `runtime` selects.
///
/// The role-tick counterpart of [`sweep_tokens_by_model`], and the reason it
/// exists separately: a tick runs in the main checkout with no worktree of its
/// own ([`role_tick_directories`]), and the Claude arm of the decision is not
/// reachable from here at all — a Claude tick's tokens come from a transcript
/// SCAN that also derives forge-mutating `actions`, which the caller keeps.
/// So this returns `None` for [`UsageSource::ClaudeTranscripts`] and the
/// caller uses [`UsageSource::is_native_store`] to pick between the two paths.
#[must_use]
pub fn role_tick_tokens_by_model(
    runtime: Option<&str>,
    root: &Path,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Option<Vec<ModelUsageTotals>> {
    match UsageSource::for_runtime(runtime) {
        UsageSource::OpenCodeSessionDb => {
            crate::opencode_usage::tokens_by_model(&role_tick_directories(root), window, None)
        }
        UsageSource::KimiSessionStore => {
            crate::kimi_usage::tokens_by_model(&role_tick_directories(root), None, window, None)
        }
        UsageSource::ClaudeTranscripts => None,
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
    fn kimi_selects_its_own_session_store_and_nothing_else_moves() {
        // Issue #8564: `defaults/runtimes/kimi.json`'s `runtime` value is what
        // `worker_spawn::run` writes into the `# LOOM_LAUNCH` record.
        assert_eq!(UsageSource::for_runtime(Some("kimi")), UsageSource::KimiSessionStore);
        assert_eq!(
            UsageSource::for_runtime(Some(" kimi ")),
            UsageSource::KimiSessionStore,
            "the launch record's value is trimmed before matching"
        );
        assert_eq!(
            UsageSource::for_runtime(Some("kimi-code")),
            UsageSource::ClaudeTranscripts,
            "matching is exact: the CLI's package name is not the runtime value"
        );
        // The pre-#8564 mappings are untouched.
        assert_eq!(UsageSource::for_runtime(Some("opencode")), UsageSource::OpenCodeSessionDb);
        assert_eq!(UsageSource::for_runtime(None), UsageSource::ClaudeTranscripts);
    }

    /// Scope step 3 of #8564, which asks for *verification* rather than new
    /// code: `runtime`/`provider`/`profile` must reach the outcome records
    /// from a Kimi launch with nothing Kimi-specific in the path. Pinned as a
    /// test because "nothing Kimi-specific is needed" is only true while
    /// `parse_launch_runtime` stays runtime-agnostic — a future `match` on
    /// known runtimes there would break Kimi silently, with no other failure.
    #[test]
    fn a_kimi_launch_record_yields_its_labels_and_its_store_with_no_special_casing() {
        let record = serde_json::json!({
            "schema": 1,
            "runtime": "kimi",
            "provider": "moonshot",
            "model": "kimi-k2.7-code",
            "profile": "example-kimi-subscription",
        })
        .to_string();
        let attribution = crate::launch_record::parse_launch_runtime(&record).unwrap();
        assert_eq!(attribution.runtime, "kimi");
        assert_eq!(attribution.provider.as_deref(), Some("moonshot"));
        assert_eq!(attribution.profile.as_deref(), Some("example-kimi-subscription"));
        // …and the SAME `runtime` string is what selects the store, so a
        // completion's labels and its numbers can never describe two runtimes.
        assert_eq!(
            UsageSource::for_runtime(Some(attribution.runtime.as_str())),
            UsageSource::KimiSessionStore
        );
    }

    #[test]
    fn every_native_store_is_distinguished_from_the_claude_transcript_reader() {
        // `is_native_store` exists so a caller that derives MORE than tokens
        // from a Claude transcript cannot silently route a new native runtime
        // back onto the Claude reader (the bug an `== OpenCodeSessionDb` test
        // would have shipped for Kimi).
        assert!(UsageSource::OpenCodeSessionDb.is_native_store());
        assert!(UsageSource::KimiSessionStore.is_native_store());
        assert!(!UsageSource::ClaudeTranscripts.is_native_store());
    }

    #[test]
    fn a_role_ticks_claude_arm_is_the_callers_transcript_scan_not_this_dispatch() {
        // The Claude arm returns None on purpose: a Claude tick's tokens come
        // from a transcript scan that ALSO derives forge-mutating `actions`,
        // which only the caller keeps. See `role_tick_tokens_by_model`'s doc.
        assert!(role_tick_tokens_by_model(None, Path::new("/w/loom"), None).is_none());
        assert!(role_tick_tokens_by_model(Some("claude"), Path::new("/w/loom"), None).is_none());
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
        assert_eq!(UsageSource::KimiSessionStore.as_str(), "kimi-session-store");
    }
}
