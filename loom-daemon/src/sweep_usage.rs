//! One sweep's token usage, plus the [`TokensStatus`] that explains it
//! (Issue #9440).
//!
//! # The hole this closes
//!
//! `sweep.outcome` already carried `tokens_in`/`tokens_out`/`tokens_by_model`,
//! but only 10.3% of the records in the fleet's D1 store carried them
//! (2,707 of 26,260 over 2026-08-15..09-29) — flat across every host and every
//! week, so not a version cliff. Failed and cancelled attempts were almost
//! never measured, which made a per-issue "total tokens to land" sum collapse
//! onto the single landing sweep: Spearman 0.996 against the landing sweep's
//! own tokens, and 96% of landed issues at a ratio of exactly 1.0. That is an
//! artifact of the measurement, not a property of the work — on the 20
//! multi-attempt issues that *did* have complete tokens, the lifecycle cost was
//! a median 1.5x the landing sweep.
//!
//! Two independent causes, both fixed by routing every construction site
//! through [`resolve`]:
//!
//! 1. The live event-bus collector (`observability::collector`) hard-coded
//!    `tokens_in: None` on every terminal record it mapped, because
//!    `map_event_to_records` is a pure function with no workspace root in
//!    scope. That path emits the *majority* of the fleet's `sweep.outcome`
//!    records.
//! 2. Both paths spelled "never spawned, so truly zero" and "spawned, but the
//!    transcript is gone" the same way: an absent key.
//!
//! # What this module is not
//!
//! It is **not** a new usage scraper. Every count still comes from
//! [`crate::usage_source::sweep_tokens_by_model`] (which dispatches per runtime
//! to the OpenCode / Kimi / Codex / Pi stores or the Claude transcripts) and
//! [`crate::transcript_tokens::sum_sweep_tokens_split`]. This module adds
//! exactly one thing on top: the three-way decision about what an empty read
//! *means*, stated once so the two emit sites cannot disagree.
//!
//! Cross-file `message.id` dedupe is #9315 and deliberately out of scope: the
//! fold this calls into is unchanged.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::script_helpers::sweep_experiment::ModelUsageTotals;
use crate::telemetry::TokensStatus;

/// [`TokensStatus::Unattributable`] because no bounded wall-clock window was
/// available to attribute a read within.
///
/// Every reader behind [`crate::usage_source::sweep_tokens_by_model`] is
/// window-filtered, because a per-issue log/transcript directory accumulates
/// *every* dispatch of that issue. Reading it unbounded would fold an earlier
/// attempt's tokens into this one — turning an undercount into a silent
/// double-count, which is strictly worse. So a sweep whose start instant is
/// unknown reports honestly rather than guessing.
pub const REASON_NO_WINDOW: &str = "no-sweep-window";

/// [`TokensStatus::Unattributable`] because the runtime's usage store does not
/// exist on this host at all — e.g. a Claude sweep with no `~/.claude/projects`
/// directory. Distinct from [`REASON_NO_TRANSCRIPT`] on purpose: "the store is
/// missing" is an install/environment fact, while "the store had nothing for
/// this sweep" is a per-sweep one, and an operator chasing a low measurement
/// rate needs to know which.
pub const REASON_NO_STORE: &str = "no-usage-store";

/// [`TokensStatus::Unattributable`] because the store was read and held nothing
/// attributable to this sweep in its window — the pruned/rotated transcript
/// case.
pub const REASON_NO_TRANSCRIPT: &str = "no-attributable-transcript";

/// One sweep's resolved token usage and the status that explains it.
///
/// Field-for-field what a `sweep.outcome` record publishes, so a construction
/// site copies rather than re-derives — the way the two sites drifted apart in
/// the first place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepUsage {
    /// `sweep.outcome`'s `tokens_in`: `Some(0)` only for
    /// [`TokensStatus::NotSpawned`].
    pub tokens_in: Option<u64>,
    /// `sweep.outcome`'s `tokens_out`, same contract as [`Self::tokens_in`].
    pub tokens_out: Option<u64>,
    /// `sweep.outcome`'s `tokens_by_model`. Absent — never `Some(vec![])` — on
    /// every non-`Measured` status, including `NotSpawned`: a sweep that never
    /// ran has no *model* to attribute a zero row to, so the honest zero is the
    /// flat pair above and nothing here.
    pub tokens_by_model: Option<Vec<ModelUsageTotals>>,
    /// Which of the three cases this is.
    pub status: TokensStatus,
    /// The `GROUP BY`-able reason for a non-`Measured` status; `None` for
    /// [`TokensStatus::Measured`].
    pub reason: Option<String>,
}

impl SweepUsage {
    /// A true, measured zero: no agent process ever ran. `reason` is the death
    /// class that proved it, so a consumer can tell a token-pool death from a
    /// stale-MCP one without joining `sweep-outcomes.jsonl`.
    #[must_use]
    pub fn not_spawned(reason: impl Into<String>) -> Self {
        Self {
            tokens_in: Some(0),
            tokens_out: Some(0),
            tokens_by_model: None,
            status: TokensStatus::NotSpawned,
            reason: Some(reason.into()),
        }
    }

    /// Something ran, or could not be proven not to, and its usage could not be
    /// attributed. Counters stay absent — never coerced to zero.
    #[must_use]
    pub fn unattributable(reason: impl Into<String>) -> Self {
        Self {
            tokens_in: None,
            tokens_out: None,
            tokens_by_model: None,
            status: TokensStatus::Unattributable,
            reason: Some(reason.into()),
        }
    }
}

/// The death class, when it proves no agent process ever ran (Issue #9440).
///
/// Two families qualify, and only these two:
///
/// - **`preflight-*`** — `crate::sweep_registry::classify_preflight_death`
///   only ever returns one of these when the sweep's own log never reached the
///   `# CLAUDE_CLI_START` marker (either an explicit wrapper-abort signature,
///   or the absence-based fallback). That *is* the definition of "the CLI was
///   never exec'd".
/// - **`no-usable-account`** — `spawn-claude.sh` resolved a pool with no usable
///   account and exited before selecting one, so no session could exist.
///
/// Deliberately **not** `account-exhausted:*`: those are matched from rate-limit
/// and credit signatures the CLI itself printed, which means the CLI ran and
/// very often consumed tokens first. Classifying them as a zero would
/// re-introduce the exact undercount this work removes, in the one shape where
/// the spend is real and interesting.
#[must_use]
pub fn never_spawned_reason(failure_class: Option<&str>) -> Option<&str> {
    failure_class.filter(|class| class.starts_with("preflight-") || *class == "no-usable-account")
}

/// Flatten a per-model breakdown onto the `(tokens_in, tokens_out)` axes the
/// record's flat pair uses: `tokens_in` is the three billing-input counters
/// summed (input + cache read + both cache-write horizons), `tokens_out` is
/// `output` alone — byte-identical to what
/// [`crate::transcript_tokens::sum_sweep_tokens_split`] computes from the same
/// transcripts, so the two sources cannot report different totals for one
/// sweep.
#[must_use]
pub fn flatten(rows: &[ModelUsageTotals]) -> (u64, u64) {
    let mut tokens_in: u64 = 0;
    let mut tokens_out: u64 = 0;
    for row in rows {
        for counter in [
            row.input,
            row.cache_read,
            row.cache_write_5m,
            row.cache_write_1h,
        ] {
            tokens_in = tokens_in.saturating_add(u64::try_from(counter).unwrap_or(0));
        }
        tokens_out = tokens_out.saturating_add(u64::try_from(row.output).unwrap_or(0));
    }
    (tokens_in, tokens_out)
}

/// Resolve one sweep's token usage and its [`TokensStatus`].
///
/// `usage_runtime` is whatever the caller resolved for the usage *source* (see
/// [`crate::usage_source::sweep_usage_runtime`]); `window` is the sweep's own
/// wall-clock span, and `failure_class` is the terminal transition's most
/// specific classification, when it has one.
///
/// Order of decision, and why:
///
/// 1. **Never spawned** wins outright. A pre-flight death has no session to
///    read, and its zero is a measurement.
/// 2. **No window** is unattributable, not zero — see [`REASON_NO_WINDOW`].
/// 3. Otherwise read, and report whatever is there. A *partial* read (a sweep
///    cancelled mid-Builder, a watchdog kill) is [`TokensStatus::Measured`]:
///    the tokens were really spent, and dropping them is precisely the
///    lifecycle undercount #9440 measured.
///
/// Blocking file I/O — call from a blocking context (the reaper's terminal
/// transition, or a `spawn_blocking` hop off the collector's async task).
#[must_use]
pub fn resolve(
    usage_runtime: Option<&str>,
    workspace_root: &Path,
    issue: u32,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    failure_class: Option<&str>,
) -> SweepUsage {
    if let Some(reason) = never_spawned_reason(failure_class) {
        return SweepUsage::not_spawned(reason);
    }
    if window.is_none() {
        return SweepUsage::unattributable(REASON_NO_WINDOW);
    }

    let tokens_by_model =
        crate::usage_source::sweep_tokens_by_model(usage_runtime, workspace_root, issue, window)
            .filter(|rows| !rows.is_empty());

    // The Claude flat split stays the primary source for `tokens_in`/
    // `tokens_out` so a Claude sweep's published pair is byte-identical to
    // pre-#9440. It naturally yields `None` for a non-Claude runtime (there are
    // no Claude transcripts for that sweep), and the per-model breakdown then
    // supplies the same two axes — which is how an OpenCode/Codex/Pi sweep
    // gains a flat pair it never had.
    let projects_dir = crate::transcript_tokens::claude_projects_dir().filter(|dir| dir.is_dir());
    let split = projects_dir.as_deref().and_then(|dir| {
        crate::transcript_tokens::sum_sweep_tokens_split(dir, workspace_root, issue, window)
    });
    let (tokens_in, tokens_out) = match split.or_else(|| tokens_by_model.as_deref().map(flatten)) {
        Some((tokens_in, tokens_out)) => (Some(tokens_in), Some(tokens_out)),
        None => (None, None),
    };

    if tokens_in.is_none() && tokens_by_model.is_none() {
        // Distinguish "this host has no store to read" from "the store had
        // nothing for this sweep". Only decidable on the Claude arm — every
        // other reader is directory-scoped and cannot tell the two apart — so
        // the check is keyed on the source the runtime actually selects (NOT on
        // `usage_runtime.is_none()`: an explicit `runtime: "claude"` marker
        // selects the same transcripts an absent one does, and must classify
        // the same way), and asks whether the projects directory the split read
        // above needs exists on disk at all.
        let claude_arm = matches!(
            crate::usage_source::UsageSource::for_runtime(usage_runtime),
            crate::usage_source::UsageSource::ClaudeTranscripts
        );
        let reason = if claude_arm && projects_dir.is_none() {
            REASON_NO_STORE
        } else {
            REASON_NO_TRANSCRIPT
        };
        return SweepUsage::unattributable(reason);
    }

    SweepUsage {
        tokens_in,
        tokens_out,
        tokens_by_model,
        status: TokensStatus::Measured,
        reason: None,
    }
}

/// The wall-clock window to attribute a terminal sweep's usage within, given
/// whatever the caller knows about when it started.
///
/// `started_at` is the authoritative value when the emitting path has one (the
/// registry entry, or the correlation map's dispatch state). When it does not,
/// a measured `duration_sec` reconstructs the same window from the other end:
/// the terminal transition is happening *now*, so the sweep began
/// `duration_sec` ago. That is a reconstruction of a measured quantity, not a
/// guess — and it is what lets a sweep whose registry entry was already GC'd
/// still report its spend.
///
/// `None` (⇒ [`REASON_NO_WINDOW`]) only when neither is available: a
/// `duration_sec` of `0` is the collector's own "I had no dispatch state"
/// sentinel, not a measurement, so it deliberately does not qualify.
#[must_use]
pub fn window(
    started_at: Option<DateTime<Utc>>,
    duration_sec: i64,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let now = Utc::now();
    let start = started_at
        .or_else(|| (duration_sec > 0).then(|| now - chrono::Duration::seconds(duration_sec)))?;
    Some((start, now))
}

#[cfg(test)]
mod tests;
