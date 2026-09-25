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

/// The `runtime` value that selects the Codex rollout-store reader (Issue
/// #8594).
///
/// Codex is a **legacy-adapter** runtime (`spawn-codex.sh`), not a native
/// harness, so it writes no `# LOOM_LAUNCH` record and this value never
/// appears in one. It reaches [`UsageSource::for_runtime`] through
/// [`usage_runtime_fallback`] instead — see that function for why that path
/// exists and why it cannot perturb any other runtime.
pub const CODEX_RUNTIME: &str = "codex";

/// The `runtime` value that selects the Pi event-stream reader (Issue #8594,
/// the Pi half). Pi is a native harness, so this is the value
/// `worker_spawn::run` writes into its `# LOOM_LAUNCH` record.
pub const PI_RUNTIME: &str = "pi";

/// The store a given runtime's per-model token usage is read from.
///
/// Deliberately an enum rather than a bare boolean: each runtime with a store
/// of its own gets a variant, and a new variant here is the one place that has
/// to change when the next one lands.
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
    /// Codex's own rollout JSONL session store ([`crate::codex_usage`]).
    CodexSessionRollouts,
    /// Pi's `--mode json` event stream as captured in the launch's own log
    /// ([`crate::pi_usage`]) — not Pi's session store; see that module's doc.
    PiLaunchStream,
}

impl UsageSource {
    /// Select the source for a launch's `runtime`, as read off its
    /// `# LOOM_LAUNCH` record (or, for a legacy adapter with a store of its
    /// own, off [`usage_runtime_fallback`]).
    ///
    /// `None` (a Claude or legacy-adapter spawn, which writes no launch
    /// record) and any unrecognised runtime both select
    /// [`Self::ClaudeTranscripts`] — the pre-#8507 behavior, unchanged.
    #[must_use]
    pub fn for_runtime(runtime: Option<&str>) -> Self {
        match runtime.map(str::trim) {
            Some(OPENCODE_RUNTIME) => Self::OpenCodeSessionDb,
            Some(KIMI_RUNTIME) => Self::KimiSessionStore,
            Some(CODEX_RUNTIME) => Self::CodexSessionRollouts,
            Some(PI_RUNTIME) => Self::PiLaunchStream,
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
            Self::CodexSessionRollouts => "codex-session-rollouts",
            Self::PiLaunchStream => "pi-launch-stream",
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

/// The runtime to read usage against, for a launch whose log wrote no
/// `# LOOM_LAUNCH` record (Issue #8594).
///
/// # The gap this closes
///
/// Only a **native harness** (`pi`, `opencode`, `kimi`) writes a
/// `# LOOM_LAUNCH` record. A legacy-adapter launch — `spawn-codex.sh`,
/// `spawn-claude.sh` — writes none, so the `runtime` every caller passes into
/// [`sweep_tokens_by_model`] is `None` for it. For Claude that is exactly
/// right (`None` already selects the Claude transcripts). For Codex it meant
/// the reader could never be reached at all: a `LOOM_RUNTIME=codex` sweep
/// published no `tokens_by_model` even with its own rollout store sitting on
/// disk, which is the second half of what #8594 reported.
///
/// # Why this cannot perturb Claude or OpenCode
///
/// Two independent guards, both asserted in this module's tests:
///
/// 1. `labelled.is_some()` short-circuits. An OpenCode/Pi launch always has a
///    launch record, so the marker is never even consulted for it.
/// 2. A marker value is returned **only** when it selects a source other than
///    [`UsageSource::ClaudeTranscripts`]. A Claude launch's marker says
///    `claude`, which maps to the Claude transcripts — the reader it was
///    already using — so this returns `None` and the caller's behavior, and
///    its payload, are byte-identical to pre-#8594.
///
/// This resolves the **usage source only**, never the published
/// `runtime`/`provider`/`profile` labels: #8507's omission contract is that a
/// launch which wrote no record publishes no label rather than a partly
/// fabricated one, and this marker carries no provider or profile to publish.
#[must_use]
pub fn usage_runtime_fallback(labelled: Option<&str>, log_contents: &str) -> Option<String> {
    if let Some(labelled) = labelled.map(str::trim).filter(|r| !r.is_empty()) {
        return Some(labelled.to_string());
    }
    let resolved = crate::launch_record::last_resolved_runtime(log_contents)?;
    (UsageSource::for_runtime(Some(&resolved)) != UsageSource::ClaudeTranscripts)
        .then_some(resolved)
}

/// [`usage_runtime_fallback`] for a **sweep**, reading the marker out of the
/// sweep's own per-issue log (`crate::launch_record::sweep_log_path`).
///
/// `labelled` is what the caller already resolved from the launch record, so a
/// native-harness sweep costs no extra read at all (guard 1 short-circuits
/// before the log is opened).
#[must_use]
pub fn sweep_usage_runtime(
    labelled: Option<&str>,
    workspace_root: &Path,
    issue: u32,
) -> Option<String> {
    if let Some(labelled) = labelled.map(str::trim).filter(|r| !r.is_empty()) {
        return Some(labelled.to_string());
    }
    let contents =
        std::fs::read_to_string(crate::launch_record::sweep_log_path(workspace_root, issue))
            .ok()?;
    usage_runtime_fallback(None, &contents)
}

/// [`usage_runtime_fallback`] for a **role tick**, reading the marker out of
/// the tick's own per-role log.
#[must_use]
pub fn role_tick_usage_runtime(labelled: Option<&str>, log_path: &Path) -> Option<String> {
    if let Some(labelled) = labelled.map(str::trim).filter(|r| !r.is_empty()) {
        return Some(labelled.to_string());
    }
    let contents = std::fs::read_to_string(log_path).ok()?;
    usage_runtime_fallback(None, &contents)
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
/// module doc), or — for a legacy adapter that writes no record but keeps a
/// usage store anyway — whatever [`sweep_usage_runtime`] resolved; `window` is
/// the sweep's wall-clock span. `None` — never `Some(vec![])` — when the
/// selected source found nothing attributable.
///
/// Pi reads the sweep's own per-issue log ([`crate::launch_record::sweep_log_path`]),
/// where its `--mode json` stream is captured; `window` then selects this
/// dispatch's messages out of every earlier one appended to the same file.
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
        UsageSource::CodexSessionRollouts => crate::codex_usage::tokens_by_model(
            &crate::codex_usage::SessionFilter::directories(&sweep_directories(
                workspace_root,
                issue,
            )),
            window,
            None,
        ),
        UsageSource::PiLaunchStream => crate::pi_usage::tokens_by_model(
            &crate::launch_record::sweep_log_path(workspace_root, issue),
            window,
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
///
/// `log_path` is the tick's own per-role log (`role_runner::role_log_path`):
/// the one store keyed by where the output was captured rather than by
/// directory is Pi's ([`crate::pi_usage`]), and a role tick's log is not
/// derivable from `root` alone.
#[must_use]
pub fn role_tick_tokens_by_model(
    runtime: Option<&str>,
    root: &Path,
    log_path: &Path,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Option<Vec<ModelUsageTotals>> {
    match UsageSource::for_runtime(runtime) {
        UsageSource::OpenCodeSessionDb => {
            crate::opencode_usage::tokens_by_model(&role_tick_directories(root), window, None)
        }
        UsageSource::KimiSessionStore => {
            crate::kimi_usage::tokens_by_model(&role_tick_directories(root), None, window, None)
        }
        UsageSource::CodexSessionRollouts => crate::codex_usage::tokens_by_model(
            &crate::codex_usage::SessionFilter::directories(&role_tick_directories(root)),
            window,
            None,
        ),
        UsageSource::PiLaunchStream => crate::pi_usage::tokens_by_model(log_path, window),
        UsageSource::ClaudeTranscripts => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn each_mapped_runtime_selects_its_own_store_and_everything_else_stays_on_transcripts() {
        assert_eq!(UsageSource::for_runtime(Some("opencode")), UsageSource::OpenCodeSessionDb);
        assert_eq!(UsageSource::for_runtime(Some("codex")), UsageSource::CodexSessionRollouts);
        assert_eq!(UsageSource::for_runtime(Some("pi")), UsageSource::PiLaunchStream);
        for (runtime, expected) in [
            (" opencode ", UsageSource::OpenCodeSessionDb),
            (" codex ", UsageSource::CodexSessionRollouts),
            (" pi ", UsageSource::PiLaunchStream),
        ] {
            assert_eq!(
                UsageSource::for_runtime(Some(runtime)),
                expected,
                "the launch record's value is trimmed before matching"
            );
        }
        // Matching is exact: `pi-coding-agent` (the npm package) is not a runtime.
        for other in [None, Some("claude"), Some("pi-coding-agent"), Some("")] {
            assert_eq!(
                UsageSource::for_runtime(other),
                UsageSource::ClaudeTranscripts,
                "an unmapped runtime ({other:?}) must keep the pre-#8507 reader"
            );
        }
    }

    /// The `# LOOM_RUNTIME_RESOLVED` line `worker_spawn::run` writes for every
    /// runtime, native or legacy adapter.
    fn resolved_log(runtime: &str) -> String {
        format!(
            "==== dispatch sweep_id=s1 ====\nspawn-worker: runtime={runtime} (from config)\n\
             {}{runtime}\n# LOOM_CLI_START runtime={runtime}\n",
            crate::launch_record::RUNTIME_RESOLVED_MARKER
        )
    }

    #[test]
    fn a_labelled_runtime_always_wins_and_never_reads_the_log() {
        // Guard 1: a native-harness launch has a record, so the marker is
        // irrelevant even when the log disagrees.
        assert_eq!(
            usage_runtime_fallback(Some("opencode"), &resolved_log("codex")).as_deref(),
            Some("opencode")
        );
        assert_eq!(
            usage_runtime_fallback(Some("  pi  "), "").as_deref(),
            Some("pi"),
            "trimmed, like `for_runtime`"
        );
    }

    #[test]
    fn the_marker_fallback_reaches_codex_and_leaves_every_other_runtime_alone() {
        // The whole point: a legacy-adapter Codex launch writes no
        // `# LOOM_LAUNCH` record, so this marker is the only place its runtime
        // is recorded.
        assert_eq!(usage_runtime_fallback(None, &resolved_log("codex")).as_deref(), Some("codex"));
        // A Pi launch normally has a record (guard 1); one that lost it — a
        // pre-#8401 binary — still reaches its own reader through the marker.
        assert_eq!(usage_runtime_fallback(None, &resolved_log("pi")).as_deref(), Some("pi"));
        // Guard 2: every runtime whose store is the Claude transcripts anyway
        // yields `None`, so the caller's behavior — and its payload — is
        // byte-identical to pre-#8594. `claude` is the one that matters: its
        // marker IS present in every Claude sweep log.
        for unchanged in ["claude", "something-new"] {
            assert_eq!(
                usage_runtime_fallback(None, &resolved_log(unchanged)),
                None,
                "{unchanged}'s marker must not become a usage-source decision"
            );
        }
        // A log with no marker at all (a pre-#8594 binary, a rotated log).
        assert_eq!(usage_runtime_fallback(None, "nothing here\n"), None);
    }

    #[test]
    fn a_blank_label_falls_through_to_the_marker_rather_than_selecting_nothing() {
        for blank in [Some(""), Some("  ")] {
            assert_eq!(
                usage_runtime_fallback(blank, &resolved_log("codex")).as_deref(),
                Some("codex")
            );
        }
    }

    #[test]
    fn source_names_cover_every_variant_and_stay_stable_for_logs() {
        for (source, name) in [
            (UsageSource::ClaudeTranscripts, "claude-transcripts"),
            (UsageSource::OpenCodeSessionDb, "opencode-session-db"),
            (UsageSource::KimiSessionStore, "kimi-session-store"),
            (UsageSource::CodexSessionRollouts, "codex-session-rollouts"),
            (UsageSource::PiLaunchStream, "pi-launch-stream"),
        ] {
            assert_eq!(source.as_str(), name);
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
        assert!(UsageSource::CodexSessionRollouts.is_native_store());
        assert!(UsageSource::PiLaunchStream.is_native_store());
        assert!(!UsageSource::ClaudeTranscripts.is_native_store());
    }

    #[test]
    fn a_role_ticks_claude_arm_is_the_callers_transcript_scan_not_this_dispatch() {
        // The Claude arm returns None on purpose: a Claude tick's tokens come
        // from a transcript scan that ALSO derives forge-mutating `actions`,
        // which only the caller keeps. See `role_tick_tokens_by_model`'s doc.
        let log = Path::new("/w/loom/.loom/logs/role-curator.log");
        assert!(role_tick_tokens_by_model(None, Path::new("/w/loom"), log, None).is_none());
        assert!(
            role_tick_tokens_by_model(Some("claude"), Path::new("/w/loom"), log, None).is_none()
        );
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
    fn sweep_usage_runtime_reads_the_sweeps_own_log_only_when_unlabelled() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let log = crate::launch_record::sweep_log_path(root, 8594);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, resolved_log("codex")).unwrap();
        assert_eq!(sweep_usage_runtime(None, root, 8594).as_deref(), Some("codex"));
        assert_eq!(sweep_usage_runtime(Some("opencode"), root, 8594).as_deref(), Some("opencode"));
        // A Claude sweep's log carries `runtime=claude`; nothing changes.
        std::fs::write(&log, resolved_log("claude")).unwrap();
        assert_eq!(sweep_usage_runtime(None, root, 8594), None);
        // A missing log is `None`, never a panic.
        assert_eq!(sweep_usage_runtime(None, root, 9999), None);
    }

    #[test]
    fn role_tick_usage_runtime_reads_the_per_role_log_only_when_unlabelled() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("role-curator.log");
        std::fs::write(&log, resolved_log("codex")).unwrap();
        assert_eq!(role_tick_usage_runtime(None, &log).as_deref(), Some("codex"));
        assert_eq!(role_tick_usage_runtime(Some("pi"), &log).as_deref(), Some("pi"));
        assert_eq!(role_tick_usage_runtime(None, &tmp.path().join("absent.log")), None);
    }

    /// Issue #8594 (review of PR #8641): a pool-selected Codex sweep runs with
    /// `CODEX_HOME=<~/.loom/codex-profiles/<name>>` in the CHILD's env only, so
    /// its rollouts are not under the daemon's own ambient home. The sweep's
    /// `tokens_by_model` must still find them through the profile root.
    #[serial_test::serial(codex_home_env)]
    #[test]
    fn a_codex_sweep_finds_rollouts_under_a_pooled_profile_home() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("repo");
        let profiles = tmp.path().join("codex-profiles");
        // The daemon's ambient home: exists, holds nothing for this sweep.
        let ambient = tmp.path().join("ambient-codex");
        std::fs::create_dir_all(ambient.join("sessions")).unwrap();
        let day = profiles.join("work-2").join("sessions/2026/09/20");
        std::fs::create_dir_all(&day).unwrap();
        let cwd = workspace.to_str().unwrap();
        let lines = [
            serde_json::json!({"type": "session_meta", "payload": {
                "session_id": "s-1", "timestamp": "2026-09-21T02:00:00Z",
                "cwd": cwd, "model_provider": "openai"}}),
            serde_json::json!({"type": "turn_context", "payload": {"cwd": cwd, "model": "gpt-5"}}),
            serde_json::json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 1_000, "cached_input_tokens": 400,
                    "output_tokens": 100, "reasoning_output_tokens": 10}}}}),
        ];
        std::fs::write(
            day.join("rollout-2026-09-20T19-00-00-s-1.jsonl"),
            lines.map(|l| l.to_string()).join("\n"),
        )
        .unwrap();

        std::env::remove_var(crate::codex_usage::CODEX_HOME_ENV);
        std::env::set_var(crate::codex_usage::CODEX_NATIVE_HOME_ENV, &ambient);
        std::env::set_var(crate::tokens_pool::paths::CODEX_PROFILE_ROOT_ENV, &profiles);
        let window = Some((
            "2026-09-21T01:00:00Z".parse().unwrap(),
            "2026-09-21T03:00:00Z".parse().unwrap(),
        ));
        let found = sweep_tokens_by_model(Some("codex"), &workspace, 8594, window);
        // Without the profile root the ambient home alone finds nothing.
        std::env::set_var(crate::tokens_pool::paths::CODEX_PROFILE_ROOT_ENV, "");
        let ambient_only = sweep_tokens_by_model(Some("codex"), &workspace, 8594, window);
        std::env::remove_var(crate::codex_usage::CODEX_NATIVE_HOME_ENV);
        std::env::remove_var(crate::tokens_pool::paths::CODEX_PROFILE_ROOT_ENV);

        let totals = found.expect("the pooled profile's rollout must be found");
        assert_eq!(totals.len(), 1, "{totals:?}");
        assert_eq!(totals[0].model, "gpt-5");
        assert_eq!(totals[0].input, 600);
        assert_eq!(totals[0].cache_read, 400);
        assert_eq!(totals[0].output, 100);
        assert_eq!(ambient_only, None);
    }

    /// One Pi 0.85.1 assistant `message_end` event (shape: see
    /// `crate::pi_usage`'s "Schema provenance").
    fn pi_message_end(model: &str, input: i64, output: i64, at: &str) -> String {
        let ms = at.parse::<DateTime<Utc>>().unwrap().timestamp_millis();
        serde_json::json!({"type": "message_end", "message": {
            "role": "assistant", "api": "openai-completions", "provider": "friendli",
            "model": model, "stopReason": "stop", "timestamp": ms,
            "usage": {"input": input, "output": output, "cacheRead": 10, "cacheWrite": 0,
                "totalTokens": input + output + 10,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}}}})
        .to_string()
    }

    /// Issue #8594's Pi half, end to end through the seam: a `pi` sweep reads
    /// its OWN per-issue log, and only this dispatch's window of it.
    #[test]
    fn a_pi_sweep_reads_its_own_logs_stream_within_its_window() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let log = crate::launch_record::sweep_log_path(root, 8594);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let contents = [
            "==== dispatch sweep_id=old ====".to_string(),
            pi_message_end("zai-org/GLM-5.3", 9_000, 900, "2026-09-24T02:00:00Z"),
            "==== dispatch sweep_id=new ====".to_string(),
            r#"# LOOM_LAUNCH {"schema":1,"runtime":"pi","provider":"friendli"}"#.to_string(),
            r#"{"type":"session","version":3,"id":"s-new","timestamp":"2026-09-25T02:00:00.000Z","cwd":"/w"}"#.to_string(),
            pi_message_end("zai-org/GLM-5.3", 1_000, 100, "2026-09-25T02:01:00Z"),
            pi_message_end("zai-org/GLM-5.3", 500, 50, "2026-09-25T02:02:00Z"),
        ]
        .join("\n");
        std::fs::write(&log, contents).unwrap();
        // A sibling issue's log is never read.
        std::fs::write(
            crate::launch_record::sweep_log_path(root, 8595),
            pi_message_end("other", 7, 7, "2026-09-25T02:01:00Z"),
        )
        .unwrap();
        let window = Some((
            "2026-09-25T02:00:00Z".parse().unwrap(),
            "2026-09-25T03:00:00Z".parse().unwrap(),
        ));
        let totals = sweep_tokens_by_model(Some("pi"), root, 8594, window).unwrap();
        assert_eq!(totals.len(), 1, "{totals:?}");
        assert_eq!(totals[0].model, "zai-org/GLM-5.3");
        assert_eq!((totals[0].input, totals[0].output, totals[0].cache_read), (1_500, 150, 20));
        // No log for the issue at all: unknown, not zero.
        assert_eq!(sweep_tokens_by_model(Some("pi"), root, 1, window), None);
    }

    #[test]
    fn a_pi_role_tick_reads_its_own_role_log() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let log = crate::role_runner::role_log_path(&root.join(".loom").join("logs"), "curator");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, pi_message_end("m", 30, 3, "2026-09-25T02:01:00Z")).unwrap();
        let window = Some((
            "2026-09-25T02:00:00Z".parse().unwrap(),
            "2026-09-25T02:05:00Z".parse().unwrap(),
        ));
        let totals = role_tick_tokens_by_model(Some("pi"), root, &log, window).unwrap();
        assert_eq!((totals[0].input, totals[0].output), (30, 3));
    }
}
