//! Durable `role_tick.outcome` telemetry for the role runner (Issue #8056).
//!
//! # The gap this closes
//!
//! Before this module, a role-runner tick left exactly one trace in the
//! daemon: a [`crate::types::RoleTickRecord`] — `{root, role, at, ok, detail,
//! pool_exhausted}` — appended to a process-global
//! [`ROLE_TICK_RING_CAPACITY`](crate::role_runner::ROLE_TICK_RING_CAPACITY)
//! ring that is **lost on daemon restart** and evicted past 2048 entries. It
//! carries no model, no effort, no duration, and no token counts, so the
//! ~60% of fleet token spend that role ticks represent was invisible to any
//! model- or prompt-experiment. `sweep.outcome` answers all of those
//! questions for a sweep; this module is the per-tick counterpart.
//!
//! # Design
//!
//! - **Written by the daemon, from what the daemon observed.** Nothing here
//!   reads a file an agent wrote about itself — the #4809 lesson is that
//!   skill-written stats files never materialize in headless children. The
//!   model/effort come from the value the runner actually resolved and
//!   launched with; the duration is the runner's own measurement; the token
//!   and action counts come from the Claude Code transcripts the child wrote
//!   as a side effect of running.
//! - **"Unknown != zero" throughout.** Every measured field is optional and
//!   is *omitted* when it was not observed, never coerced to `0`/`[]`. A
//!   pre-spawn skip is the sharpest case: it has no transcript because no
//!   session existed, so `tokens_by_model`/`actions` are absent — and
//!   [`RoleTickResult::spawned`] is what lets a consumer read that absence as
//!   "correctly nothing" rather than "unknown".
//! - **Its own journal.** `role-tick-telemetry.jsonl`, not the per-sweep
//!   `sweep-outcome-telemetry.jsonl` — see
//!   [`ROLE_TICK_MAX_JOURNAL_BYTES`](crate::sweep_outcomes::ROLE_TICK_MAX_JOURNAL_BYTES)
//!   for the rate derivation that forces the split.
//! - **No forge call on the tick path beyond the cached one.** The repo slug
//!   comes from a local `git remote get-url origin`; the visibility tag comes
//!   from [`crate::telemetry::visibility::derive_visibility`]'s 300s-TTL
//!   memo, which is the same one every `sweep.outcome` already pays.
//!
//! # Attribution is time-and-role scoped, and says so
//!
//! A role tick has no issue number to key on the way a sweep does, so a
//! transcript is attributed to a tick when all three hold: it lives under the
//! workspace root's Claude project directory, its first user message names
//! `/loom:<role>`, and its mtime falls inside the tick's own window widened by
//! [`ROLE_TICK_WINDOW_SLACK`]. The run guard makes two concurrent ticks of the
//! same `(root, role)` impossible, so the only realistic ambiguity is an
//! operator running the same slash command by hand in the same window on the
//! same checkout. That is documented rather than defended against: a rare
//! over-count is preferable to the alternative of reporting nothing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::role_runner::RoleTickOutcome;
use crate::script_helpers::sweep_experiment::ModelUsageTotals;
use crate::telemetry::{
    RepoVisibility, RoleTickActions, RoleTickOutcomeRecord, RoleTickResult, TelemetryEnvelope,
    TelemetryRecord,
};

/// How far outside a tick's `[started_at, ended_at]` window a transcript's
/// mtime may fall and still be attributed to it.
///
/// Much tighter than [`crate::transcript_tokens::WINDOW_SLACK`]'s two hours,
/// and deliberately so: that constant widens a *sweep* window, where the
/// issue number in the slash command already makes misattribution impossible
/// and the slack only guards against a clock/`createdAt` skew. A role tick has
/// no such key — role + root + time IS the key — so a two-hour skirt would
/// happily fold several consecutive ticks of the same role into each other.
/// Five minutes covers a transcript flushed slightly after the child exits
/// while staying well inside the shortest role interval (300s).
pub const ROLE_TICK_WINDOW_SLACK: Duration = Duration::from_secs(5 * 60);

/// Everything the role runner knows about a just-finished tick — the input to
/// [`emit`]. Deliberately a struct rather than a long argument list: the
/// runner assembles it at one site and the size ratchet on `role_runner.rs`
/// leaves no room for an eight-parameter call there.
#[derive(Debug, Clone)]
pub struct RoleTickTelemetry {
    /// Workspace root the tick ran for.
    pub root: PathBuf,
    /// Role name (`champion`, `judge`, …).
    pub role: String,
    /// When the invocation began.
    pub started_at: DateTime<Utc>,
    /// When the outcome was observed.
    pub ended_at: DateTime<Utc>,
    /// How it ended.
    pub result: RoleTickResult,
    /// The model the runner resolved, when it got that far.
    pub model: Option<String>,
    /// The effort the runner resolved, when one was configured.
    pub effort: Option<String>,
    /// Failure/skip detail; `None` for a success.
    pub detail: Option<String>,
    /// Which credential pool gated a pre-spawn pool skip (Issue #8408) — see
    /// [`RoleTickOutcome::gated_pool`]. `None` for every other result.
    pub gated_pool: Option<String>,
}

/// Project one [`crate::role_runner::RoleTickOutcome`] onto the
/// `(result, detail)` pair the record carries.
///
/// A **total** match, one arm per variant, deliberately not a catch-all: a
/// future outcome variant must be classified explicitly rather than silently
/// becoming `failure` (which is exactly the mis-read #7607 documents — a
/// fleet-wide exhausted pool's identical skips reading as broken roles).
///
/// The `detail` strings mirror what
/// [`crate::role_runner::record_role_tick_at`] puts in the in-memory ring, so
/// the durable record and the `loom-daemon status` view describe a tick the
/// same way.
#[must_use]
pub fn classify(outcome: &RoleTickOutcome) -> (RoleTickResult, Option<String>) {
    match outcome {
        RoleTickOutcome::Success => (RoleTickResult::Success, None),
        RoleTickOutcome::Failure(reason) => (RoleTickResult::Failure, Some(reason.clone())),
        RoleTickOutcome::RuntimeRejected(rejection) => (
            RoleTickResult::RuntimeRejected,
            Some(format!("runtime-rejected[{}]: {}", rejection.runtime, rejection.reason)),
        ),
        RoleTickOutcome::NoTokenPool => {
            (RoleTickResult::SkippedNoTokenPool, Some("no-token-pool".to_string()))
        }
        // #8408: the tag names the pool that was read (`pool-exhausted` is the
        // Claude pool's pre-#8408 literal); the machine-readable form is the
        // record's `gated_pool` key, projected by `RoleTickOutcome::gated_pool`.
        // #8444: a permanent hold (nothing provisioned, unreadable pool
        // state) carries the same stable, timestamp-free tail the in-memory
        // ring uses, so a consumer reading the durable records sees the same
        // "identical every tick" shape the stuck-role streak is built on.
        RoleTickOutcome::PoolExhausted {
            total,
            next_clear_at,
            pool,
            hold,
        } => (
            RoleTickResult::SkippedPoolExhausted,
            Some(if hold.is_self_healing() {
                format!(
                    "{}: 0/{total} spawnable; next check ~{}",
                    pool.detail_tag(),
                    next_clear_at.to_rfc3339()
                )
            } else {
                format!("{}: {}", pool.detail_tag(), hold.detail_suffix())
            }),
        ),
        RoleTickOutcome::ModelRuntimeMismatch(mismatch) => {
            (RoleTickResult::SkippedModelRuntimeMismatch, Some(mismatch.detail()))
        }
        RoleTickOutcome::LoadSkipped {
            load_per_core,
            detail,
        } => (
            RoleTickResult::SkippedLoad,
            Some(format!("load-skipped[{load_per_core:.2}]: {detail}")),
        ),
    }
}

/// Emit the `role_tick.outcome` record for one just-finished tick.
///
/// The role runner's whole entry point: it owns only the four facts it has in
/// scope (root, role, start instant, outcome) plus the runner's own resolved
/// `(model, effort)`; every projection, attribution, and write decision lives
/// here. Keeping the mapping on this side is what lets `role_runner.rs` —
/// which is at its `file-size-baseline.txt` ceiling — carry a single call.
///
/// **Blocking**; best-effort. See [`emit`].
pub fn emit_for_tick(
    root: &Path,
    role: &str,
    started_at: DateTime<Utc>,
    outcome: &RoleTickOutcome,
    resolved_model_effort: Option<(String, String)>,
) {
    let (result, detail) = classify(outcome);
    let (model, effort) = match resolved_model_effort {
        Some((model, effort)) => (Some(model), Some(effort)),
        None => (None, None),
    };
    emit(&RoleTickTelemetry {
        root: root.to_path_buf(),
        role: role.to_string(),
        started_at,
        ended_at: Utc::now(),
        result,
        model,
        effort,
        detail,
        gated_pool: outcome.gated_pool().map(str::to_string),
    });
}

/// Whether `head` — the leading bytes of a Claude Code session transcript —
/// shows that session being launched as `/loom:<role>`.
///
/// Matches the same `<command-name>` marker
/// [`crate::transcript_tokens::head_names_sweep_issue`] and
/// [`crate::activity::transcript_parse::attribute_role`] key on, so all three
/// agree about what a role session's transcript looks like. The comparison is
/// exact (after ASCII-lowercasing): `/loom:judge` must not match a
/// `/loom:judgement` command that does not exist today but might.
#[must_use]
pub fn head_names_role(head: &str, role: &str) -> bool {
    const NAME_OPEN: &str = "<command-name>/loom:";
    let Some(at) = head.find(NAME_OPEN) else {
        return false;
    };
    let rest = &head[at + NAME_OPEN.len()..];
    let name = rest.split('<').next().unwrap_or("").trim();
    name.eq_ignore_ascii_case(role)
}

/// Whether `path`'s mtime falls inside `[started_at - slack, ended_at + slack]`.
///
/// Unlike the sweep-side equivalent this is **fail-closed**: a file whose
/// mtime cannot be read is *rejected*. The sweep path can afford to fail open
/// because the issue number still gates the match; here the window is half
/// the key, so keeping an untimestampable file would let an arbitrarily old
/// session of the same role contribute to this tick's totals.
fn mtime_in_tick_window(path: &Path, started_at: DateTime<Utc>, ended_at: DateTime<Utc>) -> bool {
    let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return false;
    };
    let lo = std::time::SystemTime::from(started_at).checked_sub(ROLE_TICK_WINDOW_SLACK);
    let hi = std::time::SystemTime::from(ended_at).checked_add(ROLE_TICK_WINDOW_SLACK);
    lo.is_none_or(|lo| mtime >= lo) && hi.is_none_or(|hi| mtime <= hi)
}

/// Every transcript attributable to one tick: each `<uuid>.jsonl` session
/// under `projects_dir/<project slug>` whose head names `/loom:<role>` and
/// whose mtime is in the tick window, plus that session's subagent
/// transcripts (which carry no slash command of their own).
#[must_use]
fn attributed_transcripts(
    projects_dir: &Path,
    root: &Path,
    role: &str,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
) -> Vec<PathBuf> {
    let project = projects_dir.join(crate::transcript_tokens::project_slug(root));
    let Ok(entries) = std::fs::read_dir(&project) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        if !mtime_in_tick_window(&path, started_at, ended_at) {
            continue;
        }
        let Some(head) = crate::transcript_tokens::read_head(&path) else {
            continue;
        };
        if !head_names_role(&head, role) {
            continue;
        }
        out.extend(crate::transcript_tokens::session_transcripts(&path));
    }
    out.sort();
    out.dedup();
    out
}

/// What one pass over a tick's transcripts measured.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptScan {
    /// Per-`(model, speed, service_tier)` token totals, in the deterministic
    /// tuple order [`ModelUsageTotals`] callers already expect.
    pub tokens_by_model: Vec<ModelUsageTotals>,
    /// Forge-mutating commands observed.
    pub actions: RoleTickActions,
}

/// Classify one shell command into the [`RoleTickActions`] buckets it
/// increments. A single command increments each bucket **at most once** even
/// if it chains several such invocations with `&&` — the counts are a lower
/// bound by construction (see [`RoleTickActions`]) and over-counting a
/// compound command would be a worse error than under-counting it.
fn tally_command(command: &str, actions: &mut RoleTickActions) {
    let has_label_flag = command.contains("--add-label") || command.contains("--remove-label");
    if has_label_flag && (command.contains("gh issue edit") || command.contains("gh pr edit")) {
        actions.issues_labeled = actions.issues_labeled.saturating_add(1);
    }
    if command.contains("merge-pr.sh")
        || command.contains("gh pr merge")
        || (command.contains("gh api") && command.contains("/merge"))
    {
        actions.prs_merged = actions.prs_merged.saturating_add(1);
    }
    if command.contains("gh issue comment")
        || command.contains("gh pr comment")
        || (command.contains("gh api") && command.contains("/comments"))
    {
        actions.comments_posted = actions.comments_posted.saturating_add(1);
    }
}

/// Fold one transcript record's `tool_use` blocks into `actions`.
fn tally_record_actions(obj: &Value, actions: &mut RoleTickActions) {
    let container = obj.get("message").filter(|m| m.is_object()).unwrap_or(obj);
    let Some(content) = container.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(command) = block
            .get("input")
            .and_then(|i| i.get("command"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        tally_command(command, actions);
    }
}

/// One pass over `transcripts`, folding both token usage and forge actions.
///
/// Deliberately a single pass: the role-tick emit path runs on every tick
/// (~59/hour per registered root), so reading each file twice — once for
/// tokens, once for actions — would double a per-tick I/O cost that is
/// already the most expensive thing this module does. Both folds reuse the
/// exact record decoders their single-purpose counterparts use
/// ([`crate::script_helpers::transcript_usage::usage_from_record`]), so the
/// token numbers here cannot drift from `sweep.outcome`'s.
#[must_use]
pub fn scan_transcripts(transcripts: &[PathBuf]) -> Option<TranscriptScan> {
    use std::collections::BTreeMap;

    let mut totals: BTreeMap<(String, String, String), ModelUsageTotals> = BTreeMap::new();
    let mut actions = RoleTickActions::default();
    let mut read_any = false;

    for path in transcripts {
        if std::fs::metadata(path)
            .is_ok_and(|m| m.len() > crate::transcript_tokens::MAX_TRANSCRIPT_BYTES)
        {
            log::warn!("role_tick_telemetry: skipping oversized transcript {}", path.display());
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        read_any = true;
        for raw in text.lines() {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let Ok(obj) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            tally_record_actions(&obj, &mut actions);
            let Some(rec) = crate::script_helpers::transcript_usage::usage_from_record(&obj) else {
                continue;
            };
            let key = (rec.model.clone(), rec.speed.clone(), rec.service_tier.clone());
            let entry = totals.entry(key).or_insert_with(|| ModelUsageTotals {
                model: rec.model,
                speed: rec.speed,
                service_tier: rec.service_tier,
                ..ModelUsageTotals::default()
            });
            entry.input += rec.input;
            entry.cache_read += rec.cache_read;
            entry.cache_write_5m += rec.cache_write_5m;
            entry.cache_write_1h += rec.cache_write_1h;
            entry.output += rec.output;
        }
    }

    // `None` — never a zeroed scan — when no transcript was readable at all.
    // That is the difference between "this tick did nothing observable" and
    // "nothing about this tick was observable", which the record's optional
    // fields exist to preserve.
    read_any.then_some(TranscriptScan {
        tokens_by_model: totals.into_values().collect(),
        actions,
    })
}

/// The distinct model ids in a token breakdown, sorted and deduped — the same
/// derivation `sweep.outcome`'s `models_used` uses
/// (`sweep_registry::outcome_journal::models_used_from`), kept independent
/// only because that one is `pub(crate)` to its own module tree.
#[must_use]
fn models_used_from(rows: &[ModelUsageTotals]) -> Option<Vec<String>> {
    let mut models: Vec<String> = rows.iter().map(|r| r.model.clone()).collect();
    models.sort_unstable();
    models.dedup();
    (!models.is_empty()).then_some(models)
}

/// Resolve `(repo, visibility)` for a workspace root.
///
/// The slug comes from the checkout's own `origin` remote — a local `git`
/// call, never a forge round trip — falling back to the root's path exactly
/// the way `sweep.outcome`'s own construction site falls back. The visibility
/// tag is the 300s-TTL memoized probe, so a busy host pays at most one `gh`
/// call per repo per five minutes for it regardless of tick rate, and any
/// probe failure resolves to [`RepoVisibility::Private`].
#[must_use]
fn resolve_repo(root: &Path) -> (String, RepoVisibility) {
    match crate::release_resolve::host::repo_slug(root) {
        Some(slug) => {
            let visibility = crate::telemetry::visibility::derive_visibility(&slug);
            (slug, visibility)
        }
        None => (root.display().to_string(), RepoVisibility::Private),
    }
}

/// Build the `role_tick.outcome` record for one tick, given an already-run
/// transcript `scan` (`None` when nothing was attributable).
///
/// Split out from [`emit`] so every field-population rule — above all the
/// omit-rather-than-zero ones — is testable without a filesystem, a git
/// checkout, or a forge probe.
#[must_use]
pub fn build_record(
    tick: &RoleTickTelemetry,
    repo: String,
    visibility: RepoVisibility,
    scan: Option<TranscriptScan>,
) -> RoleTickOutcomeRecord {
    // A pre-spawn skip never launched a session, so it has no transcript to
    // find. Refusing to even look keeps a *neighbouring* tick's transcript
    // (same role, same root, overlapping slack window) from being attributed
    // to a tick that provably consumed nothing.
    let scan = if tick.result.spawned() { scan } else { None };
    let (tokens_by_model, actions) = match scan {
        Some(scan) => {
            let tokens = (!scan.tokens_by_model.is_empty()).then_some(scan.tokens_by_model);
            (tokens, Some(scan.actions))
        }
        None => (None, None),
    };
    let models_used = tokens_by_model.as_deref().and_then(models_used_from);
    RoleTickOutcomeRecord {
        repo,
        visibility,
        role: tick.role.clone(),
        started_at: tick.started_at,
        duration_sec: (tick.ended_at - tick.started_at).num_seconds().max(0),
        result: tick.result,
        model: tick.model.clone().filter(|m| !m.is_empty()),
        effort: tick.effort.clone().filter(|e| !e.is_empty()),
        detail: tick.detail.clone().filter(|d| !d.is_empty()),
        gated_pool: tick.gated_pool.clone().filter(|p| !p.is_empty()),
        tokens_by_model,
        models_used,
        actions,
    }
}

/// Append one `role_tick.outcome` envelope for `tick` to the role-tick
/// journal under its workspace root.
///
/// **Blocking** (filesystem reads, one local `git`, possibly one memoized
/// `gh`) — call it from a blocking context, never from the async tick loop
/// directly. Best-effort by contract, exactly like
/// `append_outcome_telemetry_journal`: any failure is logged and swallowed,
/// because a telemetry write must never change whether a role keeps ticking.
pub fn emit(tick: &RoleTickTelemetry) {
    let (repo, visibility) = resolve_repo(&tick.root);
    let scan = tick.result.spawned().then(|| {
        let projects_dir = crate::transcript_tokens::claude_projects_dir()?;
        let transcripts = attributed_transcripts(
            &projects_dir,
            &tick.root,
            &tick.role,
            tick.started_at,
            tick.ended_at,
        );
        scan_transcripts(&transcripts)
    });
    let record = build_record(tick, repo, visibility, scan.flatten());
    let path = crate::sweep_outcomes::default_role_tick_telemetry_path(&tick.root);
    let envelope = TelemetryEnvelope::new(
        crate::sweep_registry::host_identity(),
        TelemetryRecord::RoleTickOutcome(record),
    );
    if let Err(e) = crate::sweep_outcomes::append_role_tick_telemetry(&path, &envelope) {
        log::warn!(
            "role_tick_telemetry: failed to append {} tick record for {} at {}: {e} — \
             best-effort, never fatal to the tick (#8056)",
            tick.role,
            tick.root.display(),
            path.display()
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
