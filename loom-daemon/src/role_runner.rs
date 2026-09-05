//! Autonomous periodic support-role runner — dispatches the standalone
//! support roles (Champion, Curator, Judge, Auditor, Guide) host-side through
//! `spawn-claude.sh`, drawing from the same rotated, health-ranked token pool
//! sweeps already use, instead of GitHub Actions cron with a static
//! `CLAUDE_API_KEY` secret (issue #4015).
//!
//! # Why
//!
//! Before this module the periodic support roles ran ONLY as GitHub Actions
//! cron jobs (`.github/workflows/loom-*.yml`, Phase 2a of epic #3372/#3375),
//! authenticating with a single static `ANTHROPIC_API_KEY` secret with no
//! rotation and no health-awareness. Sweeps, by contrast, run host-side via
//! [`crate::sweep_registry`], which selects a token from the rotated pool
//! (`.loom/tokens/`, ranked via claude-monitor) and automatically skips
//! exhausted/blocked accounts. That split meant an operator had to provision
//! *two* separate token systems for the same underlying `claude -p "/role"`
//! invocation — and a deployment with no `CLAUDE_API_KEY` secret had its
//! entire backlog-grooming pipeline (Curator/Guide/Auditor/standalone
//! Champion) silently dead even though sweeps ran fine on the rotated pool
//! (the incident that filed #4015).
//!
//! Precise scope (per the issue's verified-history comment): the *per-sweep*
//! lifecycle roles (Judge/Doctor/Champion-merge dispatched **inside** a
//! `/loom:sweep`) already run host-side on the rotated pool via
//! [`crate::sweep_registry`] and are unaffected by this module. This module
//! targets the **standalone periodic** roles that only ever had the GitHub
//! Actions cron path: Champion, Curator, Judge, Auditor, Guide (mirroring the
//! table in `.github/workflows/loom-*.yml` / CLAUDE.md "Scheduled Support
//! Roles"). The GitHub Actions workflows remain a supported fallback for
//! deployments with no always-on daemon — this module does not remove them,
//! it gives an always-on daemon host a better primary path.
//!
//! **Doctor is the one exception to "standalone vs. per-sweep" above
//! (issue #5272).** Before #5272, a `loom:changes-requested` PR was owned
//! *only* by the Doctor a live `/loom:sweep`'s judge-rejection loop
//! dispatches — so a PR left in that state after its sweep ended (crash,
//! token exhaustion, retry budget, or a judge rejection landing after the
//! sweep's own retry budget was spent) had no role left to pick it up,
//! ever. Doctor is therefore also in [`DEFAULT_ROLES`], invoked with **no**
//! PR number (`/loom:doctor`'s own "Finding Work" section, not "PR Fix
//! Mode") so a tick scans the live `loom:changes-requested` queue itself —
//! reusing the claim (`loom:treating`) + staleness (`LOOM_STALE_TREATING_MINUTES`)
//! discipline `doctor.md` already implements for the per-sweep case, so this
//! adds no new claim mechanism. This makes Doctor dual-mode: still dispatched
//! per-sweep by `sweep_registry` for a PR *currently* in a live sweep, and
//! now also dispatched standalone by this module as the queue's periodic
//! owner once a sweep is gone. The two can never race on the same PR: this
//! module's own in-progress guard ([`InProgressGuard`]) serializes standalone
//! `(root, "doctor")` ticks, and `doctor.md`'s `loom:treating` claim check
//! serializes against a *concurrent* per-sweep Doctor the same way it already
//! serializes against a concurrent standalone one.
//!
//! **Hermit is a second exception, of a different shape (issue #5601).**
//! Unlike the five roles above, Hermit never had a `.github/workflows/loom-*.yml`
//! cron job to begin with, and it was simply missing from [`DEFAULT_ROLES`]
//! entirely — so naming it in `autonomous.roleRunner.roles`/`onIdle` was
//! silently discarded with a "not a known standalone role" warning. It is a
//! proposal-generating role like Auditor (files `loom:hermit` proposals, no
//! PR/issue-queue argument, no cooldown/threshold gating of its own), so it is
//! dispatched the same way: plain interval cadence, matching Auditor's 600s.
//!
//! **Architect is a third exception, and the only one that is NOT in the
//! interval-cadence default set (issue #5656).** Like Hermit it was missing
//! from [`DEFAULT_ROLES`] entirely, so naming it in
//! `autonomous.roleRunner.roles`/`onIdle` was silently discarded — leaving a
//! repo whose backlog empties with no mechanism to acquire more work, because
//! every other admitted role either processes existing work
//! (Champion/Curator/Judge/Doctor) or reacts to an existing artifact (Hermit
//! to code, Auditor to a build). Unlike Hermit, though, adding it to the table
//! outright would be wrong: it is a proposal *generator*, and on a per-interval
//! cadence across every repo it would flood backlogs with speculative work
//! Champion then has to triage. So [`RoleSpec`] carries an
//! [`interval_default`](RoleSpec::interval_default) flag, `false` for
//! Architect: [`resolve_on_idle_roles`] can name it (the work-finder idle edge
//! is precisely the empty-backlog condition where a proposal is wanted, and it
//! is self-throttling — a repo with work never fires it), and an explicit
//! `roles` allowlist can opt into a timer deliberately, but
//! [`resolve_roles`]'s "unset `roles` ⇒ all defaults" fallback never sweeps it
//! in. Its dispatches additionally carry a per-repo, per-invocation proposal
//! cap (`autonomous.roleRunner.architectMaxProposals`, see
//! [`resolve_architect_max_proposals`]) as the actuator-saturation limit.
//!
//! # Shape (mirrors [`crate::token_ranking_refresh`] / [`crate::work_finder`])
//!
//! Per enabled role, on its own configurable cadence, the daemon shells out to
//! `spawn-claude.sh -p "/<role>" --dangerously-skip-permissions` in the target
//! workspace — the same launcher [`crate::sweep_registry`] uses for sweep
//! children, so the role draws a token via the identical 3-tier selection
//! (ranking -> allowlist -> random) and appears in the same
//! `.loom/tokens/.bad_tokens` / `.ranking` accounting as sweeps.
//!
//! - **Opt-in** ([`ROLE_RUNNER_ENABLE_ENV`], default OFF) — like
//!   [`crate::work_finder`] and [`crate::main_health_gate`], this loop has
//!   dispatch-affecting side effects (spawning a full `claude` session that
//!   can mutate issues/PRs on the forge), so an absent daemon config leaves
//!   the daemon's behavior byte-for-byte unchanged.
//! - **Config** read from `.loom/config.json` -> `autonomous.roleRunner` with
//!   the same soft-fail pattern as every other `autonomous.*` surface
//!   (missing file / malformed JSON / missing block all resolve to
//!   "env-var / built-in default").
//! - **Precedence env > config > default** for `enabled`, the role subset,
//!   and the cadence.
//! - **One task per role**, each with its own ticker at that role's resolved
//!   interval (defaults mirror the commented-out `cron:` schedules in
//!   `.github/workflows/loom-*.yml`: champion 10m, curator 5m, judge 5m,
//!   auditor 10m, guide 15m) — so a fast-cadence role (curator) is not forced
//!   onto a slow role's tick.
//! - **Multi-workspace** ([`spawn_multi_role_task`]): re-reads the workspace
//!   registry each tick and, for every registered repo that has this role
//!   enabled, runs one invocation — exactly like
//!   [`crate::token_ranking_refresh::spawn_multi_token_ranking_refresh_task`].
//!   An empty registry reduces to the single `fallback_root`.
//! - The invocation runs on a blocking thread via `tokio::task::spawn_blocking`
//!   (it shells out to a whole `claude -p` session) so it never parks a
//!   runtime worker.
//!
//! # Never fatal, first tick skipped
//!
//! A failed invocation (script missing, non-zero exit, timeout) is logged and
//! skipped — it never panics the loop or the daemon; the next tick tries
//! again. Unlike the read-only token-ranking refresh, this loop mirrors
//! [`crate::work_finder`] / [`crate::main_health_gate`] in skipping the first
//! tick: a role invocation has real dispatch side effects (it can flip
//! labels, comment, merge), so firing every enabled role's session
//! immediately at daemon boot would needlessly burst several concurrent
//! `claude` sessions at once rather than settling into the steady-state
//! cadence.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::script_helpers::log_filter::strip_ansi;
use crate::sweep_registry::{self, SweepRegistryConfig};
use crate::types::{RoleLastTick, RoleTickRecord};
use crate::workspace_registry::{filter_missing_roots, WorkspaceRegistry};

// ============================================================================
// Constants
// ============================================================================

/// Environment variable enabling the role-runner loop.
///
/// Opt-in — unset or a false-y value keeps it OFF (byte-for-byte unchanged
/// daemon behavior), because the loop spawns full `claude` sessions that can
/// mutate issues/PRs on the forge. Set to `1`/`true`/`yes`/`on`
/// (case-insensitive) to enable.
pub const ROLE_RUNNER_ENABLE_ENV: &str = "LOOM_ROLE_RUNNER";

/// Environment variable overriding EVERY enabled role's tick interval
/// (seconds), uniformly. Per-role cadence diversity still comes from
/// [`RoleSpec::default_interval_secs`] / `autonomous.roleRunner.intervalSecs`
/// when this is unset.
pub const ROLE_RUNNER_INTERVAL_ENV: &str = "LOOM_ROLE_RUNNER_INTERVAL_SECS";

/// Environment variable overriding the **per-invocation architect proposal
/// cap** (#5656) — the actuator-saturation limit on how many proposal issues
/// one `/loom:architect` run may file. Highest-precedence tier of
/// env > `autonomous.roleRunner.architectMaxProposals` >
/// [`DEFAULT_ARCHITECT_MAX_PROPOSALS`].
pub const ARCHITECT_MAX_PROPOSALS_ENV: &str = "LOOM_ARCHITECT_MAX_PROPOSALS";

/// Environment variable overriding the **concurrent role-agent ceiling**
/// (#6102) — how many role invocations may be in flight at once across every
/// managed workspace. Highest-precedence tier of
/// env > `autonomous.roleRunner.maxConcurrent` >
/// [`default_max_concurrent`].
///
/// This is the role-runner counterpart of
/// `LOOM_WORK_FINDER_MAX_CONCURRENT`, and it exists because that knob bounds
/// **sweep dispatch only**: role-runner agents are spawned by this module's own
/// interval / idle loops, never routed through
/// [`crate::work_finder`]'s `min(disk, ram, maxConcurrent)` admission, so
/// before #6102 nothing bounded them at all. See
/// [`resolve_max_concurrent`].
pub const ROLE_RUNNER_MAX_CONCURRENT_ENV: &str = "LOOM_ROLE_RUNNER_MAX_CONCURRENT";

/// Built-in per-invocation architect proposal cap when neither
/// [`ARCHITECT_MAX_PROPOSALS_ENV`] nor
/// `autonomous.roleRunner.architectMaxProposals` is set (#5656).
///
/// Deliberately a *default*, not a constant policy: the step-response
/// measurement in #5656 found the natural cap varies with repo maturity
/// (~5 while a repo's work is still narrow, 7+ once it fans out into more
/// parallel stages), so a fixed value would be right early and wrong later.
/// Repos tune it per-repo; this is only the starting point.
pub const DEFAULT_ARCHITECT_MAX_PROPOSALS: u64 = 5;

/// How long to wait for one role invocation (a full `claude -p "/<role>"`
/// session) before killing it. Generous — a role tick can involve several
/// forge round-trips (list/enrich/label issues, review PRs) — but bounded so
/// a wedged session can't block that role's loop forever.
const DEFAULT_ROLE_TIMEOUT: Duration = Duration::from_secs(1800);

/// Load-per-core (issue #6637) at or above which a ceiling-hit tick is
/// classified as [`RoleTickOutcome::LoadSkipped`] instead of
/// [`RoleTickOutcome::Failure`]. `1.0` — "as many runnable/uninterruptible
/// threads as logical cores" — matches the threshold
/// [`crate::cli::status::scale_timeout_for_load`] already uses to decide
/// whether the *status* IPC budget needs stretching: below it the host isn't
/// meaningfully loaded, so a ceiling hit there is a genuine hang, not host
/// saturation.
///
/// Deliberately reused as a **detection** threshold here rather than as a
/// timeout-*scaling* factor: `spawn-worker.sh` sessions have no fixed
/// duration model the way a single IPC round-trip does (they may run a full
/// `cargo build` + `cargo nextest` suite), so stretching
/// [`DEFAULT_ROLE_TIMEOUT`] itself either does nothing useful (a modest
/// scale factor is dwarfed by 1800s) or risks leaving a genuinely wedged
/// session running far longer under sustained load. Detecting saturation
/// *at* the existing ceiling and reclassifying the outcome gets the
/// observability fix (issue #6637: a load-timeout must not read as a role
/// failure) without touching the kill deadline itself.
const ROLE_TIMEOUT_LOAD_SATURATION_THRESHOLD: f64 = 1.0;

/// Poll granularity while waiting for a role invocation to finish.
const INVOCATION_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Grace period after SIGTERM before escalating to SIGKILL on timeout.
const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// Max bytes of captured invocation output retained in a failure log line.
const MAX_OUTPUT_TAIL_BYTES: usize = 2048;

/// Max characters of failure detail retained after ANSI-stripping and
/// cleanup (issue #5024). Bounds `RoleTickOutcome::Failure`'s reason string —
/// and therefore `RoleTickRecord.detail` — so a single failing invocation's
/// raw log tail (which can still carry ANSI escapes, banners, and multi-line
/// stderr even after [`MAX_OUTPUT_TAIL_BYTES`] truncation) cannot blow up the
/// `roles.summary` health line downstream. See `health::assess_roles`, which
/// folds every persistent failure's detail into one line.
const MAX_FAILURE_DETAIL_CHARS: usize = 500;

/// ANSI-strip and length-cap `text` for use as a `RoleTickOutcome::Failure`
/// reason. Reuses [`strip_ansi`] rather than reimplementing ANSI stripping
/// (issue #5024).
///
/// The cap is word-boundary-aware (issue #6757 AC3): rather than slicing at a
/// raw char count (which can land mid-token, e.g. cutting `"mtime: 2026-…"`
/// down to `"mti"`), it backs up to the last whitespace boundary at or before
/// the cap so the retained text always ends on a whole token. Falls back to
/// the raw char cut only when the capped window contains no whitespace at
/// all (a single token longer than the whole cap) — the pre-#6757 behavior,
/// preserved rather than producing an empty string.
fn clean_and_cap_detail(text: &str) -> String {
    let cleaned = strip_ansi(text).trim().to_string();
    if cleaned.chars().count() <= MAX_FAILURE_DETAIL_CHARS {
        return cleaned;
    }
    let mut capped: String = cleaned.chars().take(MAX_FAILURE_DETAIL_CHARS).collect();
    if let Some(last_space) = capped.rfind(char::is_whitespace) {
        capped.truncate(last_space);
    }
    let capped = capped.trim_end();
    format!("{capped}… [truncated]")
}

/// The exact stderr sentinel lines `defaults/scripts/claude-wrapper.sh`
/// writes immediately before aborting a child WITHOUT ever exec'ing the CLI —
/// `AUTH_PREFLIGHT_FAILED` at `claude-wrapper.sh:2603`, `MCP_PREFLIGHT_FAILED`
/// at `:2614` (issue #6757). Matched literally (not a regex — these are
/// fixed, purpose-built markers, not free-form prose).
const PREFLIGHT_SENTINELS: &[&str] = &["# AUTH_PREFLIGHT_FAILED", "# MCP_PREFLIGHT_FAILED"];

/// Search `full_log` — the ENTIRE contents of a role's own `role-<role>.log`,
/// not just the retained [`MAX_OUTPUT_TAIL_BYTES`] tail — for a pre-flight
/// rejection sentinel (issue #6757). Returns the matched sentinel text.
///
/// Full-file search is deliberate and free: every caller already has the
/// whole file in memory (`tail_of_file`/`read_role_log` read it all via
/// `std::fs::read_to_string` before truncating to a tail), and the sentinel
/// can land earlier in the file than the retained tail window if the child
/// wrote enough unrelated output afterward — the exact scenario the issue
/// reports (an `INFO` line from `lib/locate-daemon-bin.sh`'s ordinary
/// resolution logging pushing the sentinel out of the tail window).
#[must_use]
fn find_preflight_sentinel(full_log: &str) -> Option<&'static str> {
    PREFLIGHT_SENTINELS
        .iter()
        .copied()
        .find(|sentinel| full_log.contains(sentinel))
}

/// Build the `RoleTickOutcome::Failure` detail for a role invocation that
/// exited non-zero (issue #6757). `full_log` is the role's own log file's
/// complete contents; `log_path` is that file's path.
///
/// When `full_log` carries a [`find_preflight_sentinel`] match, the detail
/// names the sentinel and points directly at `log_path` — where the full
/// pre-flight block lives — instead of an arbitrary tail-window fragment of
/// stderr that varies run to run and frequently has nothing to do with the
/// real cause. Otherwise falls back to the pre-existing cleaned/capped byte
/// tail.
#[must_use]
fn describe_role_failure(full_log: &str, log_path: &Path) -> String {
    match find_preflight_sentinel(full_log) {
        Some(sentinel) => format!(
            "pre-flight rejected the session ({sentinel}) — see the full pre-flight block in {}",
            log_path.display()
        ),
        None => clean_and_cap_detail(&truncate_tail(full_log)),
    }
}

/// A `Success` outcome faster than this is implausible for a real
/// `claude -p "/<role>"` session — starting the process, authenticating, and
/// making at least one forge round-trip (list/enrich/label an issue, review a
/// PR) takes longer than this in practice. The incident that filed #4034 was
/// a silent no-op (the prompt matched no real slash command) that still
/// exited 0 in ~1.4s and was logged as a healthy `Success`. A tick this fast
/// is logged at `WARN` instead of `INFO` so that failure mode is visible in
/// the log without inspecting forge state.
const IMPLAUSIBLY_FAST_TICK: Duration = Duration::from_secs(10);

/// Minimum time between idle-edge-triggered runs of the **same** `(root, role)`
/// (#4364). The idle edge itself only fires on a non-idle → idle transition, so
/// a queue that stays empty never re-fires; this debounce is the second-line
/// guard against rapid idle/busy *flapping* (a queue that empties, refills, and
/// empties again within seconds) hot-looping a role. A constant, deliberately
/// not a config knob — the interval cadence is the tunable backstop.
const IDLE_TRIGGER_DEBOUNCE: Duration = Duration::from_secs(60);

/// Process-wide count of ticks skipped with [`RoleTickOutcome::NoTokenPool`]
/// (#4642) — a distinct, independently-attributable tally, deliberately never
/// folded into the generic [`RoleTickOutcome::Failure`] count a real
/// invocation failure increments (mirrors the named per-reason skip counters
/// in `sweep_registry.rs`, e.g. `OpenPrDispatchError`/`DispatchBackoffError`).
static NO_TOKEN_POOL_SKIP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Total number of role-runner ticks skipped so far for having no available
/// token pool (see [`RoleTickOutcome::NoTokenPool`]). Exposed for tests and
/// future status surfacing; the daemon does not reset this across its
/// lifetime.
#[must_use]
pub fn no_token_pool_skip_count() -> u64 {
    NO_TOKEN_POOL_SKIP_COUNT.load(Ordering::Relaxed)
}

/// Process-wide count of ticks skipped with
/// [`RoleTickOutcome::ModelRuntimeMismatch`] (#5028, follow-up to #5001 AC2/
/// AC3) — a distinct, independently-attributable tally, deliberately never
/// folded into the generic [`RoleTickOutcome::Failure`] count a real
/// invocation failure increments, exactly like [`NO_TOKEN_POOL_SKIP_COUNT`]:
/// this is a permanent config conflict, not a transient failure worth
/// retrying identically forever.
static MODEL_RUNTIME_MISMATCH_SKIP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Total number of role-runner ticks skipped so far for a provable
/// model/runtime mismatch (see [`RoleTickOutcome::ModelRuntimeMismatch`]).
/// Exposed for tests and future status surfacing; the daemon does not reset
/// this across its lifetime.
#[must_use]
pub fn model_runtime_mismatch_skip_count() -> u64 {
    MODEL_RUNTIME_MISMATCH_SKIP_COUNT.load(Ordering::Relaxed)
}

/// Process-wide count of ticks skipped with [`RoleTickOutcome::LoadSkipped`]
/// (issue #6637) — a distinct, independently-attributable tally, deliberately
/// never folded into the generic [`RoleTickOutcome::Failure`] count a real
/// invocation failure increments, exactly like [`NO_TOKEN_POOL_SKIP_COUNT`]:
/// the tick ceiling fired while the host was measurably saturated, which is
/// evidence against (not for) the invocation itself being broken.
static LOAD_SKIPPED_COUNT: AtomicU64 = AtomicU64::new(0);

/// Total number of role-runner ticks skipped so far because the tick ceiling
/// was reached under measured host saturation (see
/// [`RoleTickOutcome::LoadSkipped`]). Exposed for tests and future status
/// surfacing; the daemon does not reset this across its lifetime.
#[must_use]
pub fn load_skipped_count() -> u64 {
    LOAD_SKIPPED_COUNT.load(Ordering::Relaxed)
}

/// One standalone support role this module knows how to dispatch: its name
/// (used for config/env lookups and the per-role log file), the `/role`
/// slash-command prompt passed to `claude -p`, its default tick interval, and
/// whether it belongs to the **interval-cadence default set**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleSpec {
    /// Short name (e.g. `"champion"`), matched against
    /// `autonomous.roleRunner.roles` entries.
    pub name: &'static str,
    /// The `/role` prompt passed to `claude -p`.
    pub prompt: &'static str,
    /// Default tick interval in seconds when no config/env override applies.
    pub default_interval_secs: u64,
    /// Whether this role is part of the **interval-cadence default set** —
    /// i.e. whether an absent `autonomous.roleRunner.roles` key dispatches it
    /// on a timer (issue #5656).
    ///
    /// `true` for every role whose cadence is safe to run unattended on every
    /// repo. `false` marks an **idle-addressable-only** role: still a
    /// first-class member of [`DEFAULT_ROLES`] (so
    /// `autonomous.roleRunner.onIdle` can name it, and so an explicit
    /// `autonomous.roleRunner.roles` allowlist naming it still opts in
    /// deliberately), but never included in [`resolve_roles`]'s
    /// "unset `roles` ⇒ all defaults" fallback.
    ///
    /// This split is the structural half of #5656: `architect` must be
    /// reachable from `onIdle` (the empty-backlog edge is exactly when a new
    /// proposal is wanted) **without** becoming a per-interval proposal
    /// generator on every repo that never configured `roles` — which would
    /// flood backlogs with speculative work for Champion to triage.
    pub interval_default: bool,
}

impl RoleSpec {
    /// True when this role participates in the interval-cadence default set
    /// (see [`RoleSpec::interval_default`]).
    #[must_use]
    pub fn is_interval_default(&self) -> bool {
        self.interval_default
    }
}

/// Role name of the idle-addressable-only proposal generator (#5656). Named
/// once so the prompt resolver, the config surface, and the tests agree.
pub const ARCHITECT_ROLE: &str = "architect";

/// The standalone periodic support roles this module dispatches, with
/// defaults mirroring the commented-out `cron:` schedules in
/// `.github/workflows/loom-*.yml` (CLAUDE.md "Scheduled Support Roles"
/// table). Deliberately excludes Builder (never run standalone — always
/// dispatched with an issue number, either inside a sweep or by the work
/// finder) and does not touch the per-sweep Judge/Champion invocations
/// `sweep_registry` already handles.
///
/// `doctor` is the one role here that is *also* dispatched per-sweep (see the
/// module-level "Doctor is the one exception" doc above, issue #5272) — its
/// standalone tick here runs `/loom:doctor` with no PR number, so it exercises
/// the role's own "Finding Work" queue scan rather than "PR Fix Mode".
///
/// Each `prompt` is the **namespaced** slash command (`/loom:<role>`), not
/// the bare `/<role>` form — the installed commands live under
/// `.claude/commands/loom/<role>.md` and are only resolved under that
/// namespace (there are no top-level, unnamespaced command files). A bare
/// `/curator` etc. matches no real command, so `claude -p` falls back to
/// treating it as an ordinary prompt: it answers briefly and exits 0, which
/// the runner faithfully — and wrongly — logs as `Success` (issue #4034).
/// This mirrors the existing hardcoded-literal precedent in
/// `sweep_registry.rs` (`format!("/loom:sweep {issue}")`) rather than
/// deriving/configuring the namespace: it is a settled, deliberate install
/// layout, not a per-install variable.
pub const DEFAULT_ROLES: &[RoleSpec] = &[
    RoleSpec {
        name: "champion",
        prompt: "/loom:champion",
        default_interval_secs: 600,
        interval_default: true,
    },
    RoleSpec {
        name: "curator",
        prompt: "/loom:curator",
        default_interval_secs: 300,
        interval_default: true,
    },
    RoleSpec {
        name: "judge",
        prompt: "/loom:judge",
        default_interval_secs: 300,
        interval_default: true,
    },
    RoleSpec {
        // Standalone owner of the `loom:changes-requested` queue once a PR's
        // sweep is gone (#5272) — see the module-level doc comment. Same
        // 300s cadence as `judge`, its paired stage in the PR lifecycle.
        name: "doctor",
        prompt: "/loom:doctor",
        default_interval_secs: 300,
        interval_default: true,
    },
    RoleSpec {
        name: "auditor",
        prompt: "/loom:auditor",
        default_interval_secs: 600,
        interval_default: true,
    },
    RoleSpec {
        // Proposal-generating role like `auditor` (files `loom:hermit`
        // proposals, no PR/issue-queue argument) — was entirely missing from
        // this table before #5601, so `autonomous.roleRunner.roles`/`onIdle`
        // entries naming "hermit" were silently discarded. Same 600s cadence
        // as `auditor`, its closest analog in shape.
        name: "hermit",
        prompt: "/loom:hermit",
        default_interval_secs: 600,
        interval_default: true,
    },
    RoleSpec {
        name: "guide",
        prompt: "/loom:guide",
        default_interval_secs: 900,
        interval_default: true,
    },
    RoleSpec {
        // Idle-addressable ONLY (`interval_default: false`, #5656) — the one
        // entry in this table that an absent `autonomous.roleRunner.roles`
        // key does NOT dispatch on a timer.
        //
        // Architect was entirely missing from this table before #5656, so a
        // repo whose backlog emptied had no mechanism to acquire more work:
        // `onIdle` matches names against this same table, so naming
        // "architect" there was silently discarded with a "not a known
        // standalone role" warning. Every other admitted role only *processes*
        // existing work (judge/champion/curator/sweep) or reacts to existing
        // artifacts (hermit finds complexity in code that exists, auditor
        // finds breakage in a build that runs) — none proposes new design work
        // for a repo that has none.
        //
        // It is NOT interval-eligible by default because it is a proposal
        // *generator*: on a per-interval cadence across every repo it would
        // flood backlogs with speculative work Champion then has to triage.
        // The idle edge is self-throttling by construction (a repo with work
        // never fires it), which is exactly the condition where a fresh
        // proposal is wanted. A repo that genuinely wants a timer-driven
        // architect can still opt in by naming it in an explicit
        // `autonomous.roleRunner.roles` allowlist — at this deliberately slow
        // 3600s cadence, the slowest in the table.
        name: ARCHITECT_ROLE,
        prompt: "/loom:architect",
        default_interval_secs: 3600,
        interval_default: false,
    },
];

/// A stable, content-derived identifier for the running binary's
/// [`DEFAULT_ROLES`] snapshot — the ordered role names joined by commas,
/// prefixed with the count (e.g. `"7:champion,curator,judge,doctor,auditor,\
/// hermit,guide"`). Embedded in [`missing_defaults`]'s warning and the
/// resolved-role-list diagnostic line (issue #5654) so a log that spans a
/// daemon rebuild which changed `DEFAULT_ROLES` (e.g. doctor landing in
/// #5272, hermit in #5601) is never ambiguous about which roster a given
/// warning/log line was evaluated against — the exact confound issue #5654's
/// "DEFAULT_ROLES drift observed mid-capture" section flagged. Deliberately a
/// direct content signature rather than `CARGO_PKG_VERSION`/a build hash: it
/// changes exactly when — and only when — the set that actually matters here
/// changes, with no extra build-metadata plumbing required.
#[must_use]
pub fn default_roles_snapshot_id() -> String {
    format!(
        "{}:{}",
        DEFAULT_ROLES.len(),
        DEFAULT_ROLES
            .iter()
            .map(|s| s.name)
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// The subset of [`DEFAULT_ROLES`] that an **absent**
/// `autonomous.roleRunner.roles` key dispatches on the interval cadence
/// (#5656) — every entry with [`RoleSpec::interval_default`] set.
///
/// Pulled out as a pure function so the "which defaults tick on a timer"
/// question has one answer shared by [`resolve_roles`] and
/// [`missing_defaults`], and so the idle-only carve-out is unit-testable
/// without a running loop.
#[must_use]
pub fn interval_default_roles() -> Vec<RoleSpec> {
    DEFAULT_ROLES
        .iter()
        .filter(|s| s.is_interval_default())
        .copied()
        .collect()
}

// ============================================================================
// Outcome + runner (testable via a trait, mirrors token_ranking_refresh)
// ============================================================================

/// The result of one role invocation.
// Deliberately `PartialEq` only (not `Eq`): `LoadSkipped` carries an `f64`
// load-per-core reading, and `f64` has no total ordering (`NaN`), so it
// cannot implement `Eq`. Nothing in this module keys off `RoleTickOutcome`
// as a hash/ordered-set element — every comparison is `==`/`matches!`.
#[derive(Debug, Clone, PartialEq)]
pub enum RoleTickOutcome {
    /// The invocation ran to completion with a zero exit code.
    Success,
    /// The invocation could not be run, or ran and reported failure. Never
    /// fatal to the daemon — logged and skipped.
    Failure(String),
    /// Fail-closed scheduling rejection with machine-readable provenance.
    RuntimeRejected(crate::runtime_admission::RuntimeRejection),
    /// No available token pool for this workspace (issue #4642): neither a
    /// per-repo `.loom/tokens/` pool nor a provisioned shared pool
    /// (`LOOM_SHARED_TOKENS_DIR` / `~/.loom/tokens`) exists, so
    /// `spawn-claude.sh`'s own token-selection preflight is guaranteed to
    /// exit `78` (`EX_CONFIG`). A distinct variant — never folded into the
    /// generic [`RoleTickOutcome::Failure`] tally a real invocation failure
    /// increments — because this is a permanent config state until an
    /// operator provisions a pool, not a transient failure worth retrying
    /// identically forever.
    NoTokenPool,
    /// A provable model/runtime mismatch (#5028, follow-up to #5001 AC2/AC3):
    /// the admitted runtime and the resolved model are confidently-known,
    /// differing provider families (e.g. a Claude-shaped model resolved for a
    /// role admitted onto the Codex runtime) — see
    /// [`crate::sweep_registry::model_runtime_mismatch`]. A distinct variant,
    /// deliberately never folded into the generic [`Self::Failure`] tally: it
    /// is detected BEFORE any spawn, is a permanent config conflict rather
    /// than a transient invocation failure, and self-heals the moment the
    /// conflicting config is corrected (no restart, no one-shot disable).
    ModelRuntimeMismatch(ModelRuntimeMismatch),
    /// The invocation was still running when [`DEFAULT_ROLE_TIMEOUT`] (or a
    /// test override) was reached, AND the host was measured as saturated
    /// (`load_per_core >= `[`ROLE_TIMEOUT_LOAD_SATURATION_THRESHOLD`]`) at
    /// that moment (issue #6637). A distinct variant, deliberately never
    /// folded into the generic [`Self::Failure`] tally a real invocation
    /// failure increments: a fixed 1800s wall-clock ceiling reads as a
    /// role/machinery failure to a log consumer (e.g. `fleet-check`) even
    /// when it only fired because concurrent sweeps (or other host load)
    /// starved this tick of wall-clock progress — not because the invocation
    /// itself was broken. `detail` carries the tail of the role's own log
    /// file at the moment of termination (mirrors the exit-status failure
    /// path's `tail_of_file` use) so an operator can still see which phase
    /// the invocation was in, even though this isn't counted as a failure.
    LoadSkipped {
        /// The measured load-per-core ratio at the moment the ceiling fired.
        load_per_core: f64,
        /// Tail of the role's log file at termination — the same
        /// `clean_and_cap_detail`-cleaned text a genuine timeout `Failure`
        /// would carry, retained here purely for diagnostic value.
        detail: String,
    },
}

impl RoleTickOutcome {
    /// True for a completed, successful invocation.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }
}

/// The detail carried by [`RoleTickOutcome::ModelRuntimeMismatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRuntimeMismatch {
    /// The role that was ticked (e.g. `"judge"`).
    pub role: String,
    /// The admitted runtime (e.g. `"codex"`).
    pub runtime: String,
    /// The model resolved by [`resolve_role_runner_model`] (or the test-only
    /// `with_model` override) for this role.
    pub model: String,
    /// The config/env tier label [`resolve_role_runner_model`] attributes the
    /// model to (e.g. `"default"`, `"autonomous.roleRunner.model"`), unchanged
    /// from what a successful spawn's log header would have recorded.
    pub model_source: String,
    /// The [`crate::sweep_registry::model_runtime_mismatch`] reason string
    /// naming the two conflicting families.
    pub reason: String,
}

impl ModelRuntimeMismatch {
    /// One-line, operator-facing detail. `record_role_tick` stores this
    /// verbatim on the ring record, and `assess_roles` in `health.rs` already
    /// renders a persistent failure's `detail` as-is — so `loom-daemon
    /// health` names the broken config key without an operator reading a
    /// spawn transcript (#5028 AC2).
    #[must_use]
    pub fn detail(&self) -> String {
        format!(
            "model/runtime mismatch: {} (model source={}); set \
             autonomous.roleRunner.roleModels.{} to a model the {} runtime accepts, or point \
             this role back at a Claude runtime",
            self.reason, self.model_source, self.role, self.runtime
        )
    }
}

// ============================================================================
// Role-tick health ring (Issue #4761)
// ============================================================================

/// How many `(root, role)` tick outcomes the process-global ring retains.
///
/// The ring is carried verbatim over IPC in
/// [`crate::types::DaemonStatusReport::role_tick_records`], so the bound is
/// really a *payload* bound: at ~150 bytes a record, `2048` entries is still
/// well under 320 KB even when full, trivial for a local-socket 5s-interval
/// dashboard poll.
///
/// **Sizing derivation (#6239, correcting the previous "5 roles × 5-minute
/// cadence" estimate, which understated real fleet load by more than an
/// order of magnitude).** [`DEFAULT_ROLES`] is actually 8 roles, not 5, at
/// their own [`RoleSpec::default_interval_secs`] (300s-3600s); summing
/// `3600 / interval` per role gives ~59 ticks/hour **per registered root**.
/// The ring is process-global across every managed root, not just one — a
/// modest fleet of 20 registered roots (the incident host that filed this
/// issue) already produces ~1,180 ticks/hour, wrapping the old 128-entry ring
/// in well under ten minutes. `loom-daemon health`'s default window is 30
/// minutes, so a busy host's `assess_roles` (whose escalation call-out is
/// sourced from this ring, unlike [`crate::health::assess_role_liveness`]'s
/// never-evicted [`LAST_ROLE_TICK`]) could report a config-shaped, five-tick
/// escalation as a clean bill of health purely because the ring had already
/// wrapped past it.
///
/// `2048` covers a full hour (double `health`'s default window, for margin)
/// at up to 32 registered roots (60% headroom above the 20-root incident
/// host) — comfortably ahead of `roles × registered roots` cardinality
/// without needing this compile-time constant to become a runtime value
/// derived from the live workspace registry (the ring is created once, via
/// [`OnceLock`], before any root need be registered).
pub const ROLE_TICK_RING_CAPACITY: usize = 2048;

/// Process-global newest-last ring of role-runner tick outcomes.
///
/// Same "loop publishes, status reads" discipline as
/// [`crate::work_finder::last_tick_summary`]: the role-runner loop appends one
/// record per completed `(root, role)` invocation, and `build_daemon_status`
/// hands the window to clients, which apply their own
/// transient-vs-persistent classifier ([`crate::health::summarize_role_ticks`]).
/// The daemon deliberately stores *raw outcomes*, not a verdict — the window an
/// operator cares about is a client-side choice.
static ROLE_TICKS: OnceLock<Mutex<VecDeque<RoleTickRecord>>> = OnceLock::new();

fn role_tick_ring() -> &'static Mutex<VecDeque<RoleTickRecord>> {
    ROLE_TICKS.get_or_init(|| Mutex::new(VecDeque::with_capacity(ROLE_TICK_RING_CAPACITY)))
}

/// Process-global **last-observed-tick timestamp** per `(root, role)` pair
/// (#6201) — deliberately independent of [`ROLE_TICKS`]'s bounded ring.
///
/// The incident that filed #6201: `curator` stopped ticking on one workspace
/// for nine days while five other roles kept ticking normally on the same
/// workspace at a combined rate that wraps the shared
/// [`ROLE_TICK_RING_CAPACITY`]-entry ring within a couple of hours (see its
/// own doc comment: "~60 entries an hour" at 5 roles × 5-minute cadence). Once
/// wrapped, every trace that `curator` ever ran is evicted, so
/// [`crate::health::assess_roles`]'s windowed view sees **zero** records for
/// it and reports a clean bill of health ("no role ticks in window") instead
/// of a silent, indefinite gap — the exact false-green [`crate::health`]
/// section this issue's incident report flags.
///
/// This map is keyed by `(root, role)` — bounded by that cardinality (a
/// handful of roles across a handful of registered workspaces), never by tick
/// volume — so it can answer "when did this role last tick AT ALL" no matter
/// how many other roles' ticks have long since scrolled the ring. Consumed by
/// [`crate::health::assess_role_liveness`] via [`last_role_tick_snapshot`].
///
/// The stored value carries more than a timestamp (#6239): the tick's
/// outcome (`ok`/`detail`) and the trailing run of consecutive identical
/// failures ending at it, computed incrementally in [`record_role_tick_at`].
/// A role stuck repeating an identical pre-spawn skip
/// (`ModelRuntimeMismatch`/`NoTokenPool`/`RuntimeRejected`) ticks on
/// schedule — so the timestamp alone reads it as perfectly alive — and
/// [`crate::health::assess_roles`]'s equivalent escalation streak is sourced
/// from the capacity-bounded ring, exactly the state this whole map exists to
/// route around. Reusing this never-evicted, `(root, role)`-cardinality-bound
/// structure (rather than a second parallel map) is what makes the streak
/// survive however busy the rest of the fleet's ring traffic gets.
type LastRoleTickMap = HashMap<(PathBuf, String), LastRoleTickState>;

/// One `(root, role)` pair's value in [`LastRoleTickMap`] (#6239) — see that
/// type's doc comment.
#[derive(Debug, Clone)]
struct LastRoleTickState {
    at: chrono::DateTime<chrono::Utc>,
    ok: bool,
    detail: Option<String>,
    consecutive_identical_failures: usize,
    /// Sticky, never-evicted-within-this-process record of whether THIS
    /// `(root, role)` pair has EVER completed a successful tick (issue #6757
    /// AC4) — distinct from `ok` (this tick alone). Once `true` it never
    /// reverts to `false`: a failing tick after a prior success is a
    /// regression, not evidence the workspace "never worked". Lets
    /// [`had_ever_succeeded`] tell a workspace that regressed after working
    /// apart from one that has failed every tick since it was first
    /// registered — without this, both look identical in the log (the
    /// #6757 incident: a preflight-rejected workspace ticks "failing" on
    /// every pass forever, indistinguishable from a workspace that broke
    /// after months of healthy ticks).
    ever_succeeded: bool,
}

static LAST_ROLE_TICK: OnceLock<Mutex<LastRoleTickMap>> = OnceLock::new();

fn last_role_tick_map() -> &'static Mutex<LastRoleTickMap> {
    LAST_ROLE_TICK.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Snapshot of the last-observed-tick state for every `(root, role)` pair
/// this process has ever recorded a tick for (#6201, extended #6239) — see
/// [`LAST_ROLE_TICK`]'s doc comment for why this is tracked independently of
/// the bounded [`role_tick_records`] ring. Oldest-tick-order is not
/// meaningful here (one entry per pair); callers that want deterministic
/// ordering should sort.
#[must_use]
pub fn last_role_tick_snapshot() -> Vec<RoleLastTick> {
    last_role_tick_map()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|((root, role), state)| RoleLastTick {
            root: root.clone(),
            role: role.clone(),
            at: state.at,
            ok: state.ok,
            detail: state.detail.clone(),
            consecutive_identical_failures: state.consecutive_identical_failures,
        })
        .collect()
}

/// Test-only reset of the process-global last-tick map (mirrors
/// [`reset_role_tick_ring`]).
#[cfg(test)]
fn reset_last_role_tick_map() {
    last_role_tick_map()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

/// Append one `(root, role)` tick outcome to the process-global ring, stamped
/// at `at` (Issue #4761). Oldest entries are evicted past
/// [`ROLE_TICK_RING_CAPACITY`].
pub fn record_role_tick_at(
    role: &str,
    root: &Path,
    outcome: &RoleTickOutcome,
    at: chrono::DateTime<chrono::Utc>,
) {
    let (ok, detail) = match outcome {
        RoleTickOutcome::Success => (true, None),
        RoleTickOutcome::Failure(reason) => (false, Some(reason.clone())),
        RoleTickOutcome::RuntimeRejected(rejection) => {
            (false, Some(format!("runtime-rejected: {}", rejection.reason)))
        }
        // #4642's permanent no-pool state is recorded as NOT ok on purpose: a
        // role that cannot run at all is exactly what a health check must
        // surface, and the persistent-vs-transient classifier will (correctly)
        // never clear it until a pool is provisioned.
        RoleTickOutcome::NoTokenPool => (false, Some("no-token-pool".to_string())),
        // #5028: same reasoning as `NoTokenPool` — a permanent config
        // conflict is exactly what a health check must surface, and the
        // operator-facing `detail()` names the broken config key directly
        // (AC2), so `assess_roles`'s verbatim rendering needs no special case.
        RoleTickOutcome::ModelRuntimeMismatch(mismatch) => (false, Some(mismatch.detail())),
        // #6637: recorded as ok=true, the opposite polarity from
        // `NoTokenPool`/`ModelRuntimeMismatch` above — this is a *transient*,
        // self-clearing condition (the ceiling fired while the host was
        // measurably busy with other work), not a persistent role/config
        // defect a health check must surface as failing. Excluding it from
        // the failure tally is the whole point of this variant existing.
        RoleTickOutcome::LoadSkipped {
            load_per_core,
            detail,
        } => (true, Some(format!("load-skipped (load/core {load_per_core:.2}): {detail}"))),
    };
    let mut ring = role_tick_ring()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if ring.len() >= ROLE_TICK_RING_CAPACITY {
        ring.pop_front();
    }
    ring.push_back(RoleTickRecord {
        root: root.to_path_buf(),
        role: role.to_string(),
        at,
        ok,
        detail: detail.clone(),
    });
    drop(ring);
    // #6201: independent, never-evicted last-tick state — see
    // `LAST_ROLE_TICK`'s doc comment for why this cannot simply be derived
    // from the ring above. Extended #6239 with the outcome + a
    // consecutive-identical-failure streak, computed incrementally against
    // the PREVIOUS entry for this exact `(root, role)` pair — mirroring
    // `RoleFailure::consecutive_identical`'s windowed math
    // (`crate::health::summarize_role_ticks`) but as a running count rather
    // than a scan over the bounded ring, so it is immune to that ring's
    // eviction the same way the #6201 timestamp already is.
    let mut last_tick = last_role_tick_map()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let key = (root.to_path_buf(), role.to_string());
    let prev = last_tick.get(&key).cloned();
    let consecutive_identical_failures = if ok {
        0
    } else {
        let prev_streak = prev
            .as_ref()
            .filter(|p| !p.ok && p.detail == detail)
            .map_or(0, |p| p.consecutive_identical_failures);
        prev_streak + 1
    };
    // Issue #6757 AC4: sticky once true, so a failing tick never clears it —
    // see `LastRoleTickState::ever_succeeded`'s doc comment.
    let ever_succeeded = ok || prev.as_ref().is_some_and(|p| p.ever_succeeded);
    last_tick.insert(
        key,
        LastRoleTickState {
            at,
            ok,
            detail,
            consecutive_identical_failures,
            ever_succeeded,
        },
    );
}

/// Whether `(root, role)` has EVER recorded a successful tick, per the
/// durable (process-lifetime, never-evicted) [`LAST_ROLE_TICK`] state —
/// issue #6757 AC4. `false` for a pair this process has never seen tick at
/// all, exactly the "never completed a tick" case AC4 asks to be
/// distinguishable from a later regression.
///
/// Safe to call after [`record_role_tick`] has already folded in the CURRENT
/// (possibly failing) tick's outcome: a failing tick (`ok == false`) never
/// flips `ever_succeeded` from `true` back to `false` (see
/// `record_role_tick_at`), so the value read here for a just-failed tick is
/// exactly the pre-tick history.
#[must_use]
fn had_ever_succeeded(role: &str, root: &Path) -> bool {
    last_role_tick_map()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&(root.to_path_buf(), role.to_string()))
        .is_some_and(|s| s.ever_succeeded)
}

/// The "never completed a tick" vs "regressed after prior success" clause
/// folded into a `FailureEdge` log line (issue #6757 AC4). Pulled out as a
/// pure function — mirrors why [`tick_is_implausibly_fast`] and
/// [`classify_root_tick_log`] are pure — so it is unit-testable without
/// capturing `log` crate output.
#[must_use]
fn failure_history_note(had_ever_succeeded: bool) -> &'static str {
    if had_ever_succeeded {
        "regressed after previously completing at least one successful tick"
    } else {
        "has never completed a successful tick"
    }
}

/// [`record_role_tick_at`] stamped with the current wall clock.
///
/// The tick loop ([`spawn_multi_role_task`]) records *every* raw outcome here —
/// including the identical repeat failures its own log-dedup (#4349) downgrades
/// to `DEBUG`. That completeness is what lets the client-side classifier detect
/// a config-shaped failure that can never self-recover: N consecutive failures
/// for the same `(root, role)` pair with a byte-identical `detail` escalate from
/// ordinary "persistent" to a loud, distinct verdict via
/// [`crate::health::summarize_role_ticks`] /
/// [`crate::health::ROLE_TICK_ESCALATION_THRESHOLD`] (#5023) — rather than
/// retrying identically forever, silently burning a token slot each tick.
pub fn record_role_tick(role: &str, root: &Path, outcome: &RoleTickOutcome) {
    record_role_tick_at(role, root, outcome, chrono::Utc::now());
}

/// Snapshot the role-tick ring, oldest first (Issue #4761).
#[must_use]
pub fn role_tick_records() -> Vec<RoleTickRecord> {
    role_tick_ring()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

/// Test-only reset of the process-global role-tick ring AND the #6201
/// last-tick map (kept together since every production writer
/// ([`record_role_tick_at`]) updates both atomically-in-sequence; a test that
/// resets only one would leak state into the other across test runs).
#[cfg(test)]
fn reset_role_tick_ring() {
    role_tick_ring()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    reset_last_role_tick_map();
}

/// Crate-visible alias of [`reset_role_tick_ring`] for cross-module test use
/// (#6239) — `crate::health`'s ring-saturation regression coverage needs to
/// reset this module's process-global state from outside `role_runner`
/// itself, which a private `fn` cannot do even under `#[cfg(test)]`. Guarded
/// by the same `#[serial(role_tick_ring)]` discipline as every other writer
/// of this shared state; callers MUST hold that same serial key.
#[cfg(test)]
pub(crate) fn reset_role_tick_ring_for_tests() {
    reset_role_tick_ring();
}

/// Runs one role invocation. Abstracted behind a trait so the loop is
/// testable with a scripted fake, exactly as
/// [`crate::token_ranking_refresh::RankingRefreshRunner`] makes its loop
/// testable.
pub trait RoleInvocationRunner {
    /// Invoke `role` (whose `/role` prompt is `prompt`) once and return the
    /// outcome. Never panics — a spawn failure, timeout, or non-zero exit is
    /// a [`RoleTickOutcome::Failure`], never a propagated error.
    fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome;
}

/// The concrete [`RoleInvocationRunner`]: shells out to
/// `spawn-claude.sh -p "<prompt>" --dangerously-skip-permissions` in
/// `workspace_root` — the same launcher [`crate::sweep_registry`] uses for
/// sweep children, so role invocations draw from the identical rotated token
/// pool and appear in the same accounting.
pub struct ScriptRoleInvocationRunner {
    workspace_root: PathBuf,
    /// Explicit script override (tests point this at a fake executable).
    /// Production leaves this `None` and resolves via
    /// [`SweepRegistryConfig::resolve_spawn_bin`] — the same resolution
    /// sweeps use.
    spawn_bin: Option<PathBuf>,
    timeout: Duration,
    /// Explicit model override (tests only). Production leaves this `None` and
    /// resolves per invocation via [`resolve_role_runner_model`] — the same
    /// precedence chain sweep dispatch uses (issue #4501).
    model: Option<String>,
    /// Explicit load-per-core override for the ceiling-hit saturation check
    /// (issue #6637, tests only). Production leaves this `None` and measures
    /// the live host via [`crate::cpu_headroom::load_per_core`] at the
    /// moment the timeout fires — see [`run_role_with_timeout`].
    load_per_core_override: Option<f64>,
}

impl ScriptRoleInvocationRunner {
    /// Construct a runner for `workspace_root` with the production timeout.
    #[must_use]
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            spawn_bin: None,
            timeout: DEFAULT_ROLE_TIMEOUT,
            model: None,
            load_per_core_override: None,
        }
    }

    /// Override the spawn binary (tests only).
    #[must_use]
    pub fn with_spawn_bin(mut self, bin: PathBuf) -> Self {
        self.spawn_bin = Some(bin);
        self
    }

    /// Override the resolved model (tests only) — bypasses
    /// [`resolve_role_runner_model`].
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Override the invocation timeout (tests only).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the load-per-core reading used at ceiling-hit time (tests
    /// only, issue #6637) — bypasses the live [`crate::cpu_headroom`] read so
    /// a fake saturated/unsaturated host can be asserted deterministically.
    #[must_use]
    pub fn with_load_per_core_override(mut self, load_per_core: f64) -> Self {
        self.load_per_core_override = Some(load_per_core);
        self
    }

    fn resolve_spawn_bin(&self) -> Result<PathBuf, String> {
        if let Some(p) = &self.spawn_bin {
            return Ok(p.clone());
        }
        let mut cfg = SweepRegistryConfig::new(self.workspace_root.clone());
        cfg.spawn_bin = None;
        cfg.resolve_spawn_bin().map_err(|e| e.to_string())
    }

    /// Directory holding per-role log files: `<workspace_root>/.loom/logs`.
    fn logs_dir(&self) -> PathBuf {
        self.workspace_root.join(".loom").join("logs")
    }
}

impl RoleInvocationRunner for ScriptRoleInvocationRunner {
    fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome {
        let script = match self.resolve_spawn_bin() {
            Ok(p) => p,
            Err(e) => {
                note_pre_spawn_skip(&self.logs_dir(), role, &format!("spawn-bin unresolved: {e}"));
                return RoleTickOutcome::Failure(e);
            }
        };
        // Pre-spawn token-pool preflight (issue #4642): a workspace with
        // neither a per-repo `.loom/tokens/` pool nor a provisioned shared
        // pool is guaranteed to fail `spawn-claude.sh`'s own token-selection
        // preflight (`EX_CONFIG`, exit 78) — checking here, before spawning
        // anything, means the role runner skips the doomed spawn instead of
        // burning a tick on a guaranteed exit-78 failure every single time.
        // Gated the same way as the admission check just below: only the
        // real production path (`spawn_bin` unset) checks this — tests that
        // point `spawn_bin` at a fake script opt out, exactly like
        // `resolve_and_admit` below.
        if self.spawn_bin.is_none() && crate::tokens::token_pool_size(&self.workspace_root) == 0 {
            NO_TOKEN_POOL_SKIP_COUNT.fetch_add(1, Ordering::Relaxed);
            note_pre_spawn_skip(
                &self.logs_dir(),
                role,
                "no token pool available (neither a per-repo .loom/tokens/ pool nor a provisioned \
                 shared pool); run `loom-daemon tokens bootstrap` — #4642",
            );
            return RoleTickOutcome::NoTokenPool;
        }
        // Issue #5028 (follow-up to #5001 AC2/AC3): runtime admission now
        // resolves BEFORE the model, because the runtime is a per-role INPUT
        // to the model/runtime mismatch check just below — a Claude-shaped
        // model can only be judged wrong once the admitted runtime is known.
        let admission = if self.spawn_bin.is_none() {
            match crate::runtime_admission::resolve_and_admit(&self.workspace_root, role, None) {
                Ok(value) => Some(value),
                Err(e) => {
                    note_pre_spawn_skip(
                        &self.logs_dir(),
                        role,
                        &format!("runtime admission rejected: {e}"),
                    );
                    return RoleTickOutcome::RuntimeRejected(e);
                }
            }
        } else {
            None
        };
        // Issue #4501: pin the child's model instead of inheriting the account's
        // interactive CLI default (`fable` on the host that filed the issue,
        // where every role child burned the most constrained quota tier and then
        // died on "You've reached your Fable 5 limit").
        let (model, model_source) = match &self.model {
            Some(m) => (m.clone(), "override".to_string()),
            None => resolve_role_runner_model(&self.workspace_root, role),
        };
        // Issue #5028: refuse a launch whose resolved model is a provable
        // conflict with the just-admitted runtime — e.g.
        // `runtimes.roles.judge = "codex"` with no matching
        // `autonomous.roleRunner.roleModels.judge` override still resolves the
        // Claude-shaped default (`sonnet`), which the Codex adapter rejects
        // with an HTTP 400. Detected here, before any spawn, so the role
        // runner skips the doomed launch instead of burning a tick (and a
        // token draw) on a guaranteed failure every time (#5001 AC2/AC3).
        // Gated on `admission` being `Some` — tests that opt out of admission
        // via `spawn_bin` have no resolved runtime to check against, and are
        // unaffected (mirrors the token-pool preflight's `spawn_bin.is_none()`
        // gate above).
        if let Some(admitted) = &admission {
            if let Some(reason) =
                crate::sweep_registry::model_runtime_mismatch(&admitted.runtime, &model)
            {
                MODEL_RUNTIME_MISMATCH_SKIP_COUNT.fetch_add(1, Ordering::Relaxed);
                let mismatch = ModelRuntimeMismatch {
                    role: role.to_string(),
                    runtime: admitted.runtime.clone(),
                    model,
                    model_source,
                    reason,
                };
                note_pre_spawn_skip(&self.logs_dir(), role, &mismatch.detail());
                return RoleTickOutcome::ModelRuntimeMismatch(mismatch);
            }
        }
        run_role_with_timeout(
            &script,
            &self.workspace_root,
            role,
            prompt,
            self.logs_dir(),
            self.timeout,
            &model,
            &model_source,
            admission.as_ref(),
            self.load_per_core_override,
        )
    }
}

/// Issue #4501 / #5001: resolve the model a role-runner child must run with,
/// joining the SAME precedence chain sweep dispatch uses
/// ([`sweep_registry::resolve_dispatch_model`]) with a per-role override and the
/// role-runner-specific global `autonomous.roleRunner.model` occupying the
/// "explicit request" tier:
///
/// **`autonomous.roleRunner.roleModels.<role>` >
/// `autonomous.roleRunner.model` > `autonomous.model` > shipped
/// [`sweep_registry::DEFAULT_DISPATCH_MODEL`] (`sonnet`)**
///
/// Empty/whitespace values are treated as unset at every tier, so the resolved
/// model is never the empty string and never the CLI-inherited interactive
/// default. Returns the model plus a label naming the tier that supplied it (for
/// the per-role log header).
///
/// # Why the per-role tier (#5001)
///
/// `LOOM_RUNTIME_<ROLE>` gives each role its own **runtime** axis (Claude vs
/// Codex etc.), but before #5001 the model was a single global value shared by
/// every role. The moment one role (e.g. Judge) was pointed at a different
/// provider via `LOOM_RUNTIME_JUDGE=codex`, the globally-pinned Claude alias
/// (`sonnet`) was forwarded verbatim to the Codex adapter, which rejected it with
/// an HTTP 400 — so every Judge tick failed silently, fleet-wide. The per-role
/// override closes that gap: a repo can run Judge on Codex with a Codex-valid
/// model while Curator/Champion keep a Claude alias, all from config.
///
/// Before #4501, `run_role_with_timeout` emitted **no** `--model` argument at
/// all, so every scheduled curator/champion/judge/auditor/guide child inherited
/// whatever the selected account's interactive `claude` default happened to be —
/// the live defect this resolution exists to prevent.
#[must_use]
pub fn resolve_role_runner_model(repo_root: &Path, role: &str) -> (String, String) {
    let config = read_role_runner_config(repo_root);
    let role_key = role.trim().to_ascii_lowercase();
    // Per-role override (#5001) wins over the single global
    // `autonomous.roleRunner.model`; both occupy `resolve_dispatch_model`'s
    // "explicit request" (`Param`) tier, so a `per_role` flag disambiguates the
    // log label. A blank per-role value never reaches here — blanks are dropped
    // at parse time in `read_role_runner_config`, so it falls through to the
    // global tier just like an absent key.
    let (configured, per_role) = match config.role_models.get(&role_key) {
        Some(m) => (Some(m.clone()), true),
        None => (config.model.clone(), false),
    };
    let (model, source) = sweep_registry::resolve_dispatch_model(repo_root, configured.as_deref());
    let label = match source {
        sweep_registry::ModelSource::Param if per_role => {
            format!("autonomous.roleRunner.roleModels.{role_key}")
        }
        // `Param` without `per_role` can only arise from the global
        // `autonomous.roleRunner.model` — this function is its only caller.
        sweep_registry::ModelSource::Param => "autonomous.roleRunner.model".to_string(),
        sweep_registry::ModelSource::Config => "autonomous.model".to_string(),
        sweep_registry::ModelSource::Default => "default".to_string(),
    };
    (model, label)
}

/// The per-role log file every invocation — real or skipped — writes to:
/// `<logs_dir>/role-<role>.log`.
#[must_use]
fn role_log_path(logs_dir: &Path, role: &str) -> PathBuf {
    logs_dir.join(format!("role-{role}.log"))
}

/// Append a one-line **pre-spawn skip marker** to the role's own log file
/// (issue #6201, confirmed root cause).
///
/// `run_role_with_timeout` writes a `==== loom-daemon role_runner: … ====`
/// header to `role-<role>.log` at the start of every invocation, and that file
/// is the ONE artifact an operator inspects to answer "is this role still
/// running on this workspace?". But all four of
/// [`ScriptRoleInvocationRunner::invoke`]'s pre-spawn preflight bail-outs —
/// unresolvable spawn bin, [`RoleTickOutcome::NoTokenPool`] (#4642),
/// [`RoleTickOutcome::RuntimeRejected`], and
/// [`RoleTickOutcome::ModelRuntimeMismatch`] (#5028) — return **before**
/// `run_role_with_timeout` is ever called, so before this function existed a
/// role stuck in any of those states left that log completely untouched, for
/// as long as the condition persisted.
///
/// That is the mechanism behind #6201's incident, verified against the
/// affected host's own artifacts: `runtimes.roles.curator = "codex"` (a
/// leftover runtime experiment in `.loom-local/local.json`) admitted `curator`
/// onto the Codex runtime while the paired
/// `autonomous.roleRunner.roleModels.curator` pin was later removed, so the
/// model fell back to the Claude-shaped built-in default (`sonnet`) and
/// #5028's mismatch preflight skipped every tick before any spawn. The tick
/// loop kept retrying on its normal cadence the entire time — it was never
/// benched — but `role-curator.log` had not been written since the last real
/// spawn nine days earlier, and the daemon log's own WARN is deduped to the
/// state *edge* ([`RootTickLogAction::ModelMismatchRepeat`]), so the only
/// two surfaces an operator looks at were both silent while the role did
/// nothing.
///
/// Best-effort by construction: a role tick must never fail because a
/// diagnostic line could not be written, so every I/O error here is dropped.
fn note_pre_spawn_skip(logs_dir: &Path, role: &str, reason: &str) {
    use std::io::Write;
    if std::fs::create_dir_all(logs_dir).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(role_log_path(logs_dir, role))
    {
        let _ = writeln!(
            f,
            "\n==== loom-daemon role_runner: {} role={role} SKIPPED BEFORE SPAWN (#6201): {reason} \
             ====",
            chrono::Utc::now().to_rfc3339()
        );
    }
}

/// Run `spawn-claude.sh -p "<prompt>" --model <model>
/// --dangerously-skip-permissions` in `workspace_root`, appending combined
/// output to `<logs_dir>/role-<role>.log` (never a pipe — avoids the pipe-buffer
/// deadlock pattern documented in [`crate::main_health_gate`] /
/// [`crate::token_ranking_refresh`]) and killing it after `timeout`.
#[allow(clippy::too_many_arguments)]
fn run_role_with_timeout(
    script: &Path,
    workspace_root: &Path,
    role: &str,
    prompt: &str,
    logs_dir: PathBuf,
    timeout: Duration,
    model: &str,
    model_source: &str,
    admission: Option<&crate::runtime_admission::ResolvedRuntime>,
    load_per_core_override: Option<f64>,
) -> RoleTickOutcome {
    if let Err(e) = std::fs::create_dir_all(&logs_dir) {
        return RoleTickOutcome::Failure(format!(
            "could not create logs dir {}: {e}",
            logs_dir.display()
        ));
    }
    let log_path = role_log_path(&logs_dir, role);

    {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            // The resolved model + the tier that supplied it are recorded in the
            // per-role log header (#4501) so an operator can confirm from
            // `role-<role>.log` alone which model a scheduled child ran with —
            // the manual verification this fix needs on a live host.
            let _ = writeln!(
                f,
                "\n==== loom-daemon role_runner: {} role={role} model={model} \
                 (source={model_source}) ====",
                chrono::Utc::now().to_rfc3339()
            );
        }
    }

    let out_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            return RoleTickOutcome::Failure(format!(
                "could not open log {}: {e}",
                log_path.display()
            ))
        }
    };
    let stderr_file = match out_file.try_clone() {
        Ok(f) => f,
        Err(e) => return RoleTickOutcome::Failure(format!("could not clone log handle: {e}")),
    };

    let mut cmd = Command::new(script);
    cmd.arg("-p").arg(prompt);
    // Model pin (issue #4501): appended immediately after the prompt, exactly as
    // `sweep_registry::spawn_child` does, so a role child never inherits the
    // account's interactive CLI default (`fable` on the affected host — the most
    // constrained quota tier, and the escalation ceiling rather than the floor).
    // An empty value is treated as unset — `--model ""` must never be emitted —
    // mirroring the same guard on the sweep-dispatch path; `resolve_role_runner_model`
    // already filters blanks at every tier, so this is belt-and-braces.
    if !model.is_empty() {
        cmd.arg("--model").arg(model);
    }
    cmd.arg("--dangerously-skip-permissions");
    // Transient-error recovery (issue #4255): scheduled role spawns are the
    // same unattended class as daemon-dispatched sweeps, so route them through
    // `claude-wrapper.sh` (retry/backoff/classification, bounded by
    // `LOOM_MAX_RETRIES`) instead of running bare `claude` that dies on the
    // first transient API failure. `spawn-claude.sh` consumes `--use-wrapper`
    // (not forwarded to `claude`) and execs the wrapper. Operators can force
    // the legacy single-shot path with `LOOM_USE_WRAPPER=0`.
    if sweep_registry::wrapper_dispatch_enabled() {
        cmd.arg("--use-wrapper");
    }
    cmd.current_dir(workspace_root)
        .env(sweep_registry::WORKSPACE_ENV, workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(stderr_file));
    // Per-owner credential routing (#5401/#5431, gap closed by #5508): a
    // role-runner child is spawned with `current_dir(workspace_root)` above,
    // so it must carry the SAME per-owner `GH_CONFIG_DIR` every other
    // per-repo `gh`/`git` child-spawn call site already does — otherwise a
    // workspace registered under a non-default owner (e.g. `2AMLogic/*`)
    // gets the daemon's own process-global `GH_CONFIG_DIR` (an installation
    // token scoped only to the root owner's repos) and every forge call the
    // spawned Champion/Judge/etc. session makes 404s. A total no-op for a
    // single-owner fleet or the root owner's own repos — see
    // `apply_gh_config_for_root`'s doc comment.
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, workspace_root);
    if let Some(admission) = admission {
        // Pin the already-admitted choice so spawn-worker cannot re-resolve a
        // different runtime after the pre-spawn decision.
        cmd.env("LOOM_RUNTIME", &admission.runtime);
        // Issue #4768: pin the admitted role too, mirroring
        // `sweep_registry::spawn_child`. Without it, a Codex-runtime role
        // child (e.g. `LOOM_ROLE` unset for a champion/curator/judge/auditor/
        // guide tick) reaches `spawn-codex.sh` with no role signal at all,
        // which is indistinguishable from an unrecognized role there.
        cmd.env("LOOM_ROLE", &admission.role);
        log::info!(
            "role_runner: admitted role={} runtime={} source={}",
            admission.role,
            admission.runtime,
            admission.source
        );
        // #6201: loud, at-selection diagnostic when the admitted runtime
        // diverges from the role's own declared `suggestedWorkerType` — the
        // signal the filed incident (curator declared `claude`, silently
        // ran on Codex for 9 days) had nowhere to surface.
        if let Some(msg) =
            crate::runtime_admission::suggested_worker_type_mismatch_warning(admission)
        {
            log::warn!("{msg}");
        }
    }

    // Run the child as its own process-group leader so a timeout can tear
    // down the whole subtree (the `claude` session's tool-call
    // subprocesses), not just the top-level `spawn-claude.sh` PID — mirrors
    // `sweep_registry::spawn_child`'s `process_group(0)` treatment.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RoleTickOutcome::Failure(format!("could not spawn `{}`: {e}", script.display()))
        }
    };
    let pid = child.id();

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return RoleTickOutcome::Success,
            Ok(Some(status)) => {
                // Issue #6757: prefer a purpose-built pre-flight sentinel
                // (naming the real cause and the role's own log path) over an
                // arbitrary tail-window fragment of stderr, when one is
                // present — see `describe_role_failure`.
                let full_log = read_role_log(&log_path);
                let detail = describe_role_failure(&full_log, &log_path);
                return RoleTickOutcome::Failure(format!(
                    "`{}` exited with {status}: {detail}",
                    script.display()
                ));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    // Issue #6637: sample load-per-core AT the moment the
                    // ceiling fires (not after termination — killing the
                    // child would itself relieve load, understating what the
                    // invocation was actually contending with). A test
                    // override takes precedence over the live host read, so
                    // a fake saturated/unsaturated host can be asserted
                    // deterministically.
                    let load_per_core =
                        load_per_core_override.or_else(crate::cpu_headroom::load_per_core);
                    return terminate_timed_out(&mut child, pid, script, &log_path, load_per_core);
                }
                std::thread::sleep(INVOCATION_POLL_INTERVAL);
            }
            Err(e) => {
                return RoleTickOutcome::Failure(format!(
                    "could not poll `{}`: {e}",
                    script.display()
                ))
            }
        }
    }
}

/// SIGTERM the timed-out child's process group, give it [`TERMINATE_GRACE`]
/// to exit, then SIGKILL the group and reap. Never panics.
///
/// `load_per_core` is the host's measured load-per-core ratio taken at the
/// moment the ceiling fired (or a test override — see
/// [`ScriptRoleInvocationRunner::with_load_per_core_override`]), issue
/// #6637. At or above [`ROLE_TIMEOUT_LOAD_SATURATION_THRESHOLD`] this
/// returns [`RoleTickOutcome::LoadSkipped`] instead of
/// [`RoleTickOutcome::Failure`], so a load-induced ceiling hit is never
/// misread by a log consumer (e.g. `fleet-check`) as a role/machinery
/// failure. `None` (no load reading available on this platform, or a
/// transient read failure) fails safe to the ordinary `Failure` path,
/// mirroring [`crate::cpu_headroom`]'s own fail-open convention: absent
/// evidence is never treated as "the host is loaded". Either outcome carries
/// the same log-tail detail (AC2: distinguishing which phase — e.g. mid-test
/// vs. still starting up — the invocation was in when the ceiling hit).
fn terminate_timed_out(
    child: &mut Child,
    pid: u32,
    script: &Path,
    log_path: &Path,
    load_per_core: Option<f64>,
) -> RoleTickOutcome {
    send_group_signal(pid, 15);
    let grace_start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if grace_start.elapsed() >= TERMINATE_GRACE {
                    send_group_signal(pid, 9);
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(INVOCATION_POLL_INTERVAL);
            }
            Err(_) => break,
        }
    }
    let tail = clean_and_cap_detail(&tail_of_file(log_path));
    match load_per_core {
        Some(lpc) if lpc.is_finite() && lpc >= ROLE_TIMEOUT_LOAD_SATURATION_THRESHOLD => {
            LOAD_SKIPPED_COUNT.fetch_add(1, Ordering::Relaxed);
            RoleTickOutcome::LoadSkipped {
                load_per_core: lpc,
                detail: tail,
            }
        }
        _ => RoleTickOutcome::Failure(format!(
            "`{}` timed out (pid {pid} terminated): {tail}",
            script.display()
        )),
    }
}

/// Send `sig` to the process GROUP led by `pgid` (mirrors
/// `sweep_registry::send_group_signal` — duplicated here in miniature rather
/// than exposed cross-module, since this module's only need is "best-effort
/// tear down a timed-out invocation", not the full cancel-lifecycle
/// bookkeeping `sweep_registry` owns). `pgid == 0` is rejected: `kill(0,
/// sig)` would target the *daemon's own* group.
#[cfg(unix)]
fn send_group_signal(pgid: u32, sig: i32) -> bool {
    if pgid == 0 {
        return false;
    }
    let Ok(pgid_t): Result<i32, _> = pgid.try_into() else {
        return false;
    };
    // SAFETY: kill(2) with a negative pid targets the process group; this is
    // a documented POSIX signal-delivery call with no memory-safety concerns.
    unsafe { extern_kill(-pgid_t, sig) == 0 }
}

#[cfg(not(unix))]
fn send_group_signal(_pgid: u32, _sig: i32) -> bool {
    false
}

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn extern_kill(pid: i32, sig: i32) -> i32;
}

/// Read the full contents of `path` (a role's own log file), for failure-
/// detail construction that needs more than the retained tail — e.g.
/// [`find_preflight_sentinel`]'s full-file search (issue #6757). Empty string
/// if unreadable, never panics — mirrors [`tail_of_file`]'s existing
/// fail-safe read (the same underlying read this function factors out of).
#[must_use]
fn read_role_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Read the last [`MAX_OUTPUT_TAIL_BYTES`] of `path` for a failure log line.
fn tail_of_file(path: &Path) -> String {
    truncate_tail(&read_role_log(path))
}

/// Truncate captured output to the last [`MAX_OUTPUT_TAIL_BYTES`] bytes (the
/// failure detail is usually last), trimmed of surrounding whitespace.
///
/// The cut is word-boundary-aware (issue #6757 AC3): after finding a
/// char-boundary-safe start, it advances further to the next whitespace so
/// the retained tail never begins mid-token (e.g. a raw byte cut landing
/// inside `"mtime:"` must not retain `"time:"` as if it were a whole word).
/// Falls back to the char-boundary-only start when no whitespace appears
/// anywhere in the retained window (a single token longer than the whole
/// window) — the pre-#6757 behavior, preserved rather than producing an
/// empty string.
fn truncate_tail(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_TAIL_BYTES {
        return s.trim().to_string();
    }
    let byte_start = s.len() - MAX_OUTPUT_TAIL_BYTES;
    let byte_start = (byte_start..s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    let word_start = s[byte_start..]
        .find(char::is_whitespace)
        .map_or(byte_start, |offset| byte_start + offset);
    s[word_start..].trim().to_string()
}

// ============================================================================
// Config (.loom/config.json -> autonomous.roleRunner)
// ============================================================================

/// The subset of `.loom/config.json -> autonomous.roleRunner` this module
/// consumes. Each field is `Option` so an absent key falls through to the
/// env-var / built-in-default resolution — precedence is **env > config >
/// default** for every knob, matching every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleRunnerConfig {
    /// `autonomous.roleRunner.enabled` — whether to run the loop at all.
    pub enabled: Option<bool>,
    /// `autonomous.roleRunner.roles` — the subset of [`DEFAULT_ROLES`] (by
    /// name) to dispatch. `None` (key absent) runs every default role;
    /// `Some(vec![])` (explicit empty array) runs none.
    pub roles: Option<Vec<String>>,
    /// `autonomous.roleRunner.intervalSecs` — a single override applied
    /// uniformly to every enabled role's cadence (a zero/invalid value is
    /// dropped to `None`, falling through to that role's own default).
    pub interval_secs: Option<u64>,
    /// `autonomous.roleRunner.onIdle` — the subset of [`DEFAULT_ROLES`] (by
    /// name) to fire on the work-finder **idle edge** (#4364), in addition to
    /// (never replacing) the interval cadence. Unlike [`roles`](Self::roles),
    /// `None` (key absent) means **no** idle triggering — the opposite default,
    /// because idle firing is a distinct opt-in surface. Resolved by
    /// [`resolve_on_idle_roles`].
    pub on_idle: Option<Vec<String>>,
    /// `autonomous.roleRunner.model` — the model every role child is pinned to
    /// (issue #4501). `None` (key absent, blank, or non-string) falls through to
    /// `autonomous.model` and then the shipped
    /// [`sweep_registry::DEFAULT_DISPATCH_MODEL`]; it never falls through to the
    /// account's interactive CLI default. Resolved by
    /// [`resolve_role_runner_model`].
    pub model: Option<String>,
    /// `autonomous.roleRunner.roleModels` — per-role model overrides keyed by
    /// role name (issue #5001), each occupying a tier **above** the global
    /// [`model`](Self::model). This is the config axis that lets a repo run one
    /// role on a different runtime (e.g. `LOOM_RUNTIME_JUDGE=codex`) while giving
    /// that role a model its provider accepts, without forcing the other roles
    /// (still on Claude) onto the same alias. Keys are lower-cased and trimmed;
    /// blank keys and blank/non-string values are dropped, so an entry never
    /// emits `--model ""`. Absent / malformed / non-object soft-fails to an empty
    /// map (every role falls through to the global chain). Resolved by
    /// [`resolve_role_runner_model`].
    pub role_models: BTreeMap<String, String>,
    /// `autonomous.roleRunner.architectMaxProposals` — the **per-invocation**
    /// cap on how many proposal issues one architect dispatch may file
    /// (#5656). `None` (key absent, zero, or non-integer) falls through to
    /// [`ARCHITECT_MAX_PROPOSALS_ENV`]'s tier and then
    /// [`DEFAULT_ARCHITECT_MAX_PROPOSALS`]; resolved by
    /// [`resolve_architect_max_proposals`] and carried into the dispatch by
    /// [`resolve_role_prompt`].
    ///
    /// Per-repo by construction (it is read from each root's own
    /// `.loom/config.json`, like every other key here) because the workable
    /// cap is a property of the repo's maturity, not of the daemon.
    pub architect_max_proposals: Option<u64>,
    /// `autonomous.roleRunner.maxConcurrent` — the ceiling on how many role
    /// invocations may run **at once across every managed workspace** (#6102).
    /// `None` (key absent, zero, or non-integer) falls through to
    /// [`ROLE_RUNNER_MAX_CONCURRENT_ENV`]'s tier and then
    /// [`default_max_concurrent`]; resolved by [`resolve_max_concurrent`] and
    /// enforced at admission time by [`RoleRunGuard::admit`].
    ///
    /// **Read per-root (like every other key here) but compared against a
    /// process-wide count.** That asymmetry is deliberate: the resource being
    /// protected is the *host*, which is shared by every workspace this daemon
    /// manages, so the count must be global; the value comes from whichever
    /// root's tick is asking, exactly as `architectMaxProposals` does. On a
    /// fleet host where the roots disagree, the effective ceiling for a given
    /// tick is that root's own — the tighter root simply refuses sooner.
    pub max_concurrent: Option<usize>,
}

/// Read `.loom/config.json -> autonomous.roleRunner`, soft-failing every
/// field to `None` (env/default resolution) on any of: missing file,
/// malformed JSON, or a missing `autonomous` / `roleRunner` block. Mirrors
/// the soft-fail contract of
/// [`crate::token_ranking_refresh::read_token_ranking_refresh_config`].
#[must_use]
pub fn read_role_runner_config(repo_root: &Path) -> RoleRunnerConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.roleRunner") else {
        return RoleRunnerConfig::default();
    };

    let roles = block
        .get("roles")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        });

    // `onIdle` parses exactly like `roles` (array of strings; absent /
    // non-array soft-fails to `None`); non-string entries are dropped. Unknown
    // *names* are warned-and-ignored later, in `resolve_on_idle_roles`.
    let on_idle = block
        .get("onIdle")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        });

    // `model` (#4501): a blank / whitespace-only / non-string value soft-fails to
    // `None` so it falls through to `autonomous.model` -> the shipped default
    // rather than emitting `--model ""` or an inherited interactive default.
    let model = block
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(String::from);

    // `roleModels` (#5001): a `{ "<role>": "<model>" }` object of per-role
    // overrides. Keys are lower-cased + trimmed (matching how the resolver looks
    // them up); a blank key, or a blank / non-string value, is dropped — an
    // override must never emit `--model ""`. Absent / non-object soft-fails to an
    // empty map, so every role falls through to the global `model` chain
    // unchanged (zero behavior change when the key is not configured).
    let role_models = block
        .get("roleModels")
        .and_then(serde_json::Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| {
                    let key = k.trim().to_ascii_lowercase();
                    if key.is_empty() {
                        return None;
                    }
                    let val = v.as_str().map(str::trim).filter(|m| !m.is_empty())?;
                    Some((key, val.to_string()))
                })
                .collect::<BTreeMap<String, String>>()
        })
        .unwrap_or_default();

    RoleRunnerConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        roles,
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        on_idle,
        model,
        role_models,
        // `architectMaxProposals` (#5656): a zero / negative / non-integer
        // value soft-fails to `None` — a cap of 0 would mean "dispatch
        // architect, forbid it from filing anything", which is a pure waste of
        // a session (a repo that wants no proposals simply leaves architect
        // out of `onIdle`/`roles`). Falls through to env, then the built-in
        // default.
        architect_max_proposals: block
            .get("architectMaxProposals")
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0),
        // `maxConcurrent` (#6102): a zero / negative / non-integer value
        // soft-fails to `None` — a ceiling of 0 would mean "run the role
        // runner but never admit a tick", which is what
        // `autonomous.roleRunner.enabled=false` (or an empty `roles`) already
        // expresses far more legibly. Falls through to env, then the built-in
        // default.
        max_concurrent: block
            .get("maxConcurrent")
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0)
            .and_then(|n| usize::try_from(n).ok()),
    }
}

/// Which tier of the enabled precedence chain actually supplied the
/// resolved on/off value (#6469) — mirrors [`IntervalSource`]'s naming
/// pattern so the disabled-branch boot log can say *why* role loops are off,
/// not just *that* they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnabledSource {
    /// [`ROLE_RUNNER_ENABLE_ENV`] is set (to any value) in the **daemon's own
    /// process environment** — decides regardless of config, which on a
    /// launchd/systemd host is not the operator's interactive shell
    /// environment.
    Env,
    /// `autonomous.roleRunner.enabled` — from whichever config tier
    /// `config_resolver` resolved it out of.
    Config,
    /// Neither env nor config set anything — the built-in default (`false`).
    Default,
}

/// Resolve whether the loop is enabled with precedence **env > config >
/// default(false)**. When [`ROLE_RUNNER_ENABLE_ENV`] is *set* (to any value)
/// it decides (truthy enables, anything else disables); when unset the
/// config `enabled` flag decides; absent config leaves it off (opt-in, zero
/// behavior change).
#[must_use]
pub fn resolve_enabled(config: &RoleRunnerConfig) -> bool {
    resolve_enabled_with_source(config).0
}

/// [`resolve_enabled`] plus the tier that produced the value (#6469), so a
/// caller can log *why* the role runner is on/off instead of only *whether*
/// it is.
#[must_use]
pub fn resolve_enabled_with_source(config: &RoleRunnerConfig) -> (bool, EnabledSource) {
    if let Ok(v) = std::env::var(ROLE_RUNNER_ENABLE_ENV) {
        let enabled = matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        return (enabled, EnabledSource::Env);
    }
    match config.enabled {
        Some(v) => (v, EnabledSource::Config),
        None => (false, EnabledSource::Default),
    }
}

/// Whether [`ROLE_RUNNER_ENABLE_ENV`] is set in the daemon's own process
/// environment, independent of any single root's config (#6470) —
/// `Some(v)` when set (`v` is the resolved truthy/falsy value, mirroring
/// [`resolve_enabled_with_source`]'s `EnabledSource::Env` branch), `None`
/// when unset (every root's own `autonomous.roleRunner.enabled` decides
/// independently). This is the host-wide "master switch" reading a
/// `status`/diagnostic surface needs *before* looking at any particular
/// root: when `Some(false)`, no root's own config can turn the role runner
/// back on, so blaming a specific root's `.loom/config.json` (the #4377
/// message) is actively misleading — the whole reason this function exists
/// is to let the two call sites (`status`'s per-root line and the idle-edge
/// WARN in this module) name the true cause instead.
#[must_use]
pub fn host_env_override() -> Option<bool> {
    let v = std::env::var(ROLE_RUNNER_ENABLE_ENV).ok()?;
    Some(matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Compute which [`DEFAULT_ROLES`] entries are absent from an explicit
/// `autonomous.roleRunner.roles` allowlist (#5339) — pulled out as a pure
/// function, mirroring [`should_warn_disabled_root`], so the "a new default
/// role shipped but this repo's pinned `roles` list wasn't updated" warning
/// is unit-testable without captured log output.
///
/// An **empty** `names` is a deliberate, documented opt-out ("run none") —
/// not staleness — so it always returns empty rather than every default.
///
/// Only [`interval_default_roles`] are considered (#5656): an
/// idle-addressable-only role like `architect` is *deliberately* absent from
/// the interval-cadence default set, so warning that a pinned allowlist omits
/// it would push every repo to add it — reintroducing exactly the
/// per-interval proposal flood the carve-out exists to prevent.
#[must_use]
fn missing_defaults(names: &[String]) -> Vec<&'static str> {
    if names.is_empty() {
        return Vec::new();
    }
    interval_default_roles()
        .into_iter()
        .filter(|spec| !names.iter().any(|n| n == spec.name))
        .map(|spec| spec.name)
        .collect()
}

/// [`missing_defaults`] entries that are ALSO not named in
/// `autonomous.roleRunner.onIdle` (issue #6163 AC2) — the subset that
/// genuinely dispatches on **neither** path.
///
/// A role absent from `roles` but present in `on_idle` is not "missing" in
/// any actionable sense: it dispatches on the work-finder idle edge instead
/// of the interval cadence, by design (see [`resolve_on_idle_roles`]). The
/// pre-#6163 warning reported such a role as "will not be dispatched"
/// regardless — misleading for exactly this deliberate, documented
/// configuration (the `auditor`-under-`onIdle` case from #6163's own report).
#[must_use]
fn missing_defaults_uncovered_by_on_idle(
    names: &[String],
    on_idle: &[String],
) -> Vec<&'static str> {
    missing_defaults(names)
        .into_iter()
        .filter(|missing| !on_idle.iter().any(|n| n == missing))
        .collect()
}

/// Build the aggregated "stale pinned `roles` allowlist" diagnostic line for
/// `repo_root` (issue #6163 AC1/AC4) — `None` when `missing` is empty.
///
/// One line names every currently-missing role at once (AC4's suggested
/// shape) rather than the pre-#6163 one-`log::warn!`-call-per-role loop, and
/// **always names the workspace** (AC1) — the exact information gap that let
/// a genuinely-unrelated warning about a *different* registered repo be
/// misread as contradicting `loom`'s own config during a live incident
/// investigation (#6163's motivating report).
#[must_use]
fn missing_defaults_warning_line(repo_root: &Path, missing: &[&'static str]) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "role_runner: {}: {} of {} interval-default DEFAULT_ROLES not configured in \
         autonomous.roleRunner.roles and not covered by onIdle either (snapshot {}) — will not \
         be dispatched on either path: {}",
        repo_root.display(),
        missing.len(),
        interval_default_roles().len(),
        default_roles_snapshot_id(),
        missing.join(", ")
    ))
}

/// Whether `spec`'s multi-workspace loop is the one designated to emit the
/// per-workspace [`missing_defaults_warning_line`] diagnostic (#6163 AC3).
///
/// The missing-defaults set is a property of a **workspace's config**, not of
/// any one role, so every `DEFAULT_ROLES` loop would otherwise compute — and
/// warn — the byte-identical line for the same root. That is one duplicate
/// per spawned loop per workspace (8 × 25 workspaces = 200 identical lines on
/// the fleet host that filed #6163), which reproduces that issue's
/// dense-burst-at-boot complaint in miniature even with the per-root
/// change-dedup applied. Designating a single reporter loop collapses it to
/// exactly one line per workspace per resolved-config change.
///
/// `DEFAULT_ROLES[0]` is the designated reporter for one reason: `daemon_service`
/// spawns a loop for **every** `DEFAULT_ROLES` entry unconditionally, so the
/// first entry's loop is always running whenever the role runner is enabled at
/// all. Any single fixed choice works; the invariant that exactly one entry
/// satisfies this predicate is pinned by
/// `test_exactly_one_default_role_is_the_missing_defaults_reporter`.
#[must_use]
fn is_missing_defaults_reporter(spec: &RoleSpec) -> bool {
    DEFAULT_ROLES
        .first()
        .is_some_and(|first| first.name == spec.name)
}

/// Resolve the set of roles to dispatch on the **interval cadence**:
/// `config.roles` (by name, matched against [`DEFAULT_ROLES`], preserving
/// [`DEFAULT_ROLES`] order and ignoring unknown names with a warning) when
/// present, else [`interval_default_roles`].
///
/// **The absent-key fallback is the interval-default subset, NOT the whole
/// table (#5656).** A role marked `interval_default: false` — today only
/// `architect` — is addressable by name (an explicit `roles` allowlist naming
/// it, or `autonomous.roleRunner.onIdle` via [`resolve_on_idle_roles`]) but is
/// never swept in by "unset `roles` ⇒ all defaults". Without this split,
/// putting `architect` in [`DEFAULT_ROLES`] at all (the prerequisite for
/// `onIdle` to resolve it) would silently make every repo that never pins
/// `roles` run a proposal generator on a timer.
///
/// `autonomous.roleRunner.roles` is an **allowlist, not an addition**: a repo
/// that pins it must update it whenever a new role is added to
/// [`DEFAULT_ROLES`], or that role silently never dispatches (#5339).
///
/// **This function itself no longer warns about that staleness (#6163).**
/// [`missing_defaults`]/[`missing_defaults_uncovered_by_on_idle`] remain the
/// pure staleness computation, but the `log::warn!` side effect moved to
/// [`spawn_multi_role_task`]'s tick loop, which has three things this
/// function structurally cannot: the **workspace** the config was read from
/// (#6163 AC1 — the pre-move warning named the missing role but never the
/// repo, making a 25-workspace fleet's identical-looking warnings
/// undiagnosable), per-root dedup state so it fires once per resolved-config
/// change instead of every tick (#6163 AC3 — this function is called by every
/// standalone role's own multi-workspace loop, once per registered root,
/// every tick), and `onIdle` awareness (#6163 AC2, via
/// [`missing_defaults_uncovered_by_on_idle`]) so a role covered by the idle
/// edge is not misreported as undispatched on every path. Every other caller
/// of this function (status queries in `ipc.rs`/`daemon_service.rs`, tests)
/// only ever wanted the resolved role *list*, never this diagnostic — so
/// dropping it from here is a pure noise reduction for them too.
#[must_use]
pub fn resolve_roles(config: &RoleRunnerConfig) -> Vec<RoleSpec> {
    let Some(names) = &config.roles else {
        return interval_default_roles();
    };
    let mut out = Vec::new();
    for spec in DEFAULT_ROLES {
        if names.iter().any(|n| n == spec.name) {
            out.push(*spec);
        }
    }
    for name in names {
        if !DEFAULT_ROLES.iter().any(|s| s.name == name) {
            log::warn!(
                "role_runner: autonomous.roleRunner.roles entry {name:?} is not a known standalone \
                 role (expected one of {:?}) — ignored",
                DEFAULT_ROLES.iter().map(|s| s.name).collect::<Vec<_>>()
            );
        }
    }
    out
}

/// Human-readable "config layer" label for whichever tier file actually
/// supplied `autonomous.roleRunner.roles` for `repo_root` — the "source
/// path/layer" half of the per-repo, per-tick diagnostic in
/// [`resolved_roles_log_line`] (issue #5654 AC1). Built on
/// [`crate::config_resolver::source_of`], which already walks the tier chain
/// highest-precedence-first; this only adds the human label.
///
/// `None` from `source_of` means no tier sets the key at all (not even to an
/// explicit `null`) — [`resolve_roles`] then falls all the way through to the
/// built-in [`DEFAULT_ROLES`] default rather than any file on disk, labeled
/// `"default (no tier sets roles)"` here so that terminal case is explicit in
/// the log rather than silently absent.
#[must_use]
pub fn roles_source_label(repo_root: &Path) -> String {
    const DOTTED: &str = "autonomous.roleRunner.roles";
    config_tier_label(repo_root, DOTTED, "default (no tier sets roles)")
}

/// Human-readable "config layer" label for whichever tier file actually
/// supplied `dotted` for `repo_root`, or `absent_label` when no tier sets it
/// at all.
///
/// Extracted from [`roles_source_label`] (#6204) so the interval diagnostic
/// ([`interval_config_tier_label`]) reports the tier chain in exactly the same
/// vocabulary — a `private/shared defaults (...)` path is the single most
/// common reason a knob resolves to a value that appears nowhere in the repo's
/// own committed `.loom/config.json`.
fn config_tier_label(repo_root: &Path, dotted: &str, absent_label: &str) -> String {
    match crate::config_resolver::source_of(repo_root, dotted) {
        None => absent_label.to_string(),
        Some(path) if path == repo_root.join(crate::config_resolver::LOCAL_CONFIG_REL) => {
            format!("local ({})", path.display())
        }
        Some(path) if path == repo_root.join(crate::config_resolver::PROJECT_CONFIG_REL) => {
            format!("project ({})", path.display())
        }
        Some(path) if path == repo_root.join(crate::config_resolver::LEGACY_CONFIG_REL) => {
            format!("legacy ({})", path.display())
        }
        Some(path) => format!("private/shared defaults ({})", path.display()),
    }
}

/// Human-readable "config layer" label for whichever tier file supplied
/// `autonomous.roleRunner.intervalSecs` for `repo_root` (#6204).
#[must_use]
pub fn interval_config_tier_label(repo_root: &Path) -> String {
    config_tier_label(repo_root, "autonomous.roleRunner.intervalSecs", "no tier sets intervalSecs")
}

/// Human-readable "config layer" label for whichever tier file supplied
/// `autonomous.roleRunner.enabled` for `repo_root` (#6469).
#[must_use]
pub fn enabled_config_tier_label(repo_root: &Path) -> String {
    config_tier_label(repo_root, "autonomous.roleRunner.enabled", "no tier sets enabled")
}

/// Build the per-repo, per-tick "resolved role list" diagnostic line (issue
/// #5654 AC1): the fully resolved role names (post [`resolve_roles`]), the
/// config layer/path that produced the underlying `autonomous.roleRunner.roles`
/// key (or the built-in default when no tier sets it, via
/// [`roles_source_label`]), and the [`DEFAULT_ROLES`] snapshot identifier the
/// resolution ran against (via [`default_roles_snapshot_id`]).
///
/// This is the diagnostic the issue's own "Suggested investigation" asked
/// for: the current per-role [`missing_defaults`] warning says which roles
/// are *missing from a pinned list*, but never which config actually produced
/// the resolved set that gates dispatch — making a host-specific exclusion
/// like the reported "doctor never admitted" untraceable from the log alone.
/// A pure string-building function (not a `log::` call site) so its content
/// is directly unit-testable, mirroring [`missing_defaults`] /
/// [`tick_is_implausibly_fast`].
#[must_use]
pub fn resolved_roles_log_line(repo_root: &Path, resolved: &[RoleSpec]) -> String {
    let names: Vec<&str> = resolved.iter().map(|r| r.name).collect();
    format!(
        "role_runner: {} resolved roles={:?} source={} default_roles={}",
        repo_root.display(),
        names,
        roles_source_label(repo_root),
        default_roles_snapshot_id()
    )
}

/// Resolve the set of roles to fire on the work-finder **idle edge** (#4364):
/// `config.on_idle` (by name, matched against [`DEFAULT_ROLES`], preserving
/// [`DEFAULT_ROLES`] order and ignoring unknown names with a warning) when
/// present, else **empty**.
///
/// This mirrors [`resolve_roles`] except for the absent-key default: `None`
/// resolves to no roles (not every default), because idle triggering is a
/// distinct opt-in — a repo that never sets `onIdle` gets the interval-only
/// behavior byte-for-byte.
///
/// This matches against the **whole** [`DEFAULT_ROLES`] table, including
/// entries excluded from the interval-cadence default set
/// ([`RoleSpec::interval_default`] `== false`). That asymmetry is the point of
/// #5656: `architect` is reachable here — the work-finder idle edge is exactly
/// the "this repo has run out of work" condition where a fresh proposal is
/// wanted, and it is self-throttling (a repo with work never fires it) — while
/// [`resolve_roles`]'s default fallback still leaves it off every timer.
#[must_use]
pub fn resolve_on_idle_roles(config: &RoleRunnerConfig) -> Vec<RoleSpec> {
    let Some(names) = &config.on_idle else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for spec in DEFAULT_ROLES {
        if names.iter().any(|n| n == spec.name) {
            out.push(*spec);
        }
    }
    for name in names {
        if !DEFAULT_ROLES.iter().any(|s| s.name == name) {
            log::warn!(
                "role_runner: autonomous.roleRunner.onIdle entry {name:?} is not a known \
                 standalone role (expected one of {:?}) — ignored",
                DEFAULT_ROLES.iter().map(|s| s.name).collect::<Vec<_>>()
            );
        }
    }
    out
}

/// Which tier of the interval precedence chain actually supplied a role's
/// resolved tick interval (#6204).
///
/// Only [`IntervalSource::BuiltIn`] is *per-role*: both override tiers are
/// uniform, so when either is set every role logs the same number and the
/// per-role cadence diversity in [`DEFAULT_ROLES`] is entirely inert. That is
/// exactly the state #6204 was filed against — a host whose daemon inherited
/// `LOOM_ROLE_RUNNER_INTERVAL_SECS` from its launchd plist logged a uniform
/// interval for all eight roles, which read as a bug in the built-ins (the
/// documented 5–15 min per-role defaults) because the log named no source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalSource {
    /// [`ROLE_RUNNER_INTERVAL_ENV`] — uniform across every role, and set in
    /// the **daemon's own process environment**, which on a launchd/systemd
    /// host is not the operator's interactive shell environment.
    Env,
    /// `autonomous.roleRunner.intervalSecs` — uniform across every role, from
    /// whichever config tier `config_resolver` resolved it out of (which may
    /// be a private/shared defaults file, not the repo's committed config).
    Config,
    /// That role's own [`RoleSpec::default_interval_secs`] — the only per-role
    /// tier, and the only one under which the shipped 5–15 min cadence
    /// diversity is observable.
    BuiltIn,
}

impl IntervalSource {
    /// Whether this source applies one value uniformly to every role (both
    /// override tiers) rather than per-role.
    #[must_use]
    pub fn is_uniform_override(&self) -> bool {
        matches!(self, Self::Env | Self::Config)
    }
}

/// Resolve a single role's tick interval with precedence **env
/// ([`ROLE_RUNNER_INTERVAL_ENV`], applied uniformly to every role) > config
/// (`autonomous.roleRunner.intervalSecs`, also uniform) > that role's own
/// [`RoleSpec::default_interval_secs`]**.
#[must_use]
pub fn resolve_interval_for_role(spec: &RoleSpec, config: &RoleRunnerConfig) -> Duration {
    resolve_interval_for_role_with_source(spec, config).0
}

/// [`resolve_interval_for_role`] plus the tier that produced the value
/// (#6204), so a caller can log *why* a role ticks at the cadence it does
/// instead of only *what* the cadence is.
#[must_use]
pub fn resolve_interval_for_role_with_source(
    spec: &RoleSpec,
    config: &RoleRunnerConfig,
) -> (Duration, IntervalSource) {
    if let Some(secs) = std::env::var(ROLE_RUNNER_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
    {
        return (Duration::from_secs(secs), IntervalSource::Env);
    }
    if let Some(secs) = config.interval_secs {
        return (Duration::from_secs(secs), IntervalSource::Config);
    }
    (Duration::from_secs(spec.default_interval_secs), IntervalSource::BuiltIn)
}

/// Build the boot-time "resolved interval" diagnostic line for one role
/// (#6204): the resolved cadence **and the tier that supplied it**, mirroring
/// [`resolved_roles_log_line`]'s `source=` half and the per-role log header's
/// `model=<m> (source=<tier>)`.
///
/// A pure string-building function (not a `log::` call site) so its content is
/// directly unit-testable, mirroring [`resolved_roles_log_line`] /
/// [`missing_defaults`].
///
/// Under a uniform override the line also names the per-role built-in that was
/// **not** used, so the "every role shows the same interval" symptom is
/// self-diagnosing from a single line: it says which knob is overriding, which
/// file (or which env var) carries it, and what the cadence would otherwise
/// have been.
#[must_use]
pub fn resolved_interval_log_line(
    repo_root: &Path,
    spec: &RoleSpec,
    config: &RoleRunnerConfig,
) -> String {
    let (interval, source) = resolve_interval_for_role_with_source(spec, config);
    let source_label = match source {
        IntervalSource::Env => format!(
            "env:{ROLE_RUNNER_INTERVAL_ENV} (uniform override; per-role built-in {}s not used)",
            spec.default_interval_secs
        ),
        IntervalSource::Config => format!(
            "config:autonomous.roleRunner.intervalSecs from {} (uniform override; per-role \
             built-in {}s not used)",
            interval_config_tier_label(repo_root),
            spec.default_interval_secs
        ),
        IntervalSource::BuiltIn => "built-in (RoleSpec::default_interval_secs)".to_string(),
    };
    format!(
        "role_runner: {} interval={}s source={}",
        spec.name,
        interval.as_secs(),
        source_label
    )
}

/// Build the boot-time "role runner disabled" diagnostic line (#6469): names
/// **which tier** resolved the off state — env (`LOOM_ROLE_RUNNER=<value>`,
/// the daemon's own process environment) vs config
/// (`autonomous.roleRunner.enabled` false/absent, naming the config tier that
/// supplied it via [`enabled_config_tier_label`]) — mirroring how the enabled
/// branch already names its own resolution
/// (`daemon_service.rs`'s `role_runner: enabled (...)` line). Also states the
/// scope explicitly ("no role loops will run on this host for any registered
/// root") so a reader does not go hunting through a specific root's
/// `.loom/config.json` for an explanation that lives at the daemon-process
/// level instead.
///
/// A pure string-building function (not a `log::` call site) so its content
/// is directly unit-testable, mirroring [`resolved_interval_log_line`] /
/// [`resolved_roles_log_line`]. Only meaningful to call when
/// [`resolve_enabled`] is `false` for this `config` — the caller (the
/// `daemon_service.rs` boot sequence) only reaches this branch in that case.
#[must_use]
pub fn disabled_role_runner_log_line(repo_root: &Path, config: &RoleRunnerConfig) -> String {
    let (_, source) = resolve_enabled_with_source(config);
    let source_label = match source {
        EnabledSource::Env => {
            let raw = std::env::var(ROLE_RUNNER_ENABLE_ENV).unwrap_or_default();
            format!("env:{ROLE_RUNNER_ENABLE_ENV}={raw:?}")
        }
        EnabledSource::Config => format!(
            "config:autonomous.roleRunner.enabled=false from {}",
            enabled_config_tier_label(repo_root)
        ),
        EnabledSource::Default => {
            "default (no tier sets autonomous.roleRunner.enabled)".to_string()
        }
    };
    format!(
        "role_runner: disabled source={source_label} (set LOOM_ROLE_RUNNER=1 or \
         autonomous.roleRunner.enabled=true to enable) — no role loops will run on this host \
         for any registered root"
    )
}

/// Log [`disabled_role_runner_log_line`] at `info!` (#6469) — deliberately
/// `info!`, not `debug!`: a fleet running at the normal INFO level must see
/// this line, since the disabled branch is the single most consequential
/// switch on a host and a silent `debug!`-level line is invisible at the
/// level the fleet actually runs at.
///
/// Wrapping the log call here (rather than leaving
/// `log::info!("{}", disabled_role_runner_log_line(...))` inline at the
/// `daemon_service.rs` call site) means this module's own tests — via
/// [`crate::test_log_capture::capture_logs`] — exercise the exact call the
/// boot sequence makes, so the tested level and the shipped level cannot
/// drift apart the way a hand-duplicated `log::info!` at the call site could.
pub fn log_role_runner_disabled(repo_root: &Path, config: &RoleRunnerConfig) {
    log::info!("{}", disabled_role_runner_log_line(repo_root, config));
}

/// Resolve the **per-invocation architect proposal cap** (#5656) with
/// precedence **env ([`ARCHITECT_MAX_PROPOSALS_ENV`]) > config
/// (`autonomous.roleRunner.architectMaxProposals`, read from each root's own
/// `.loom/config.json`) > [`DEFAULT_ARCHITECT_MAX_PROPOSALS`]**.
///
/// A zero or unparseable value at either tier is dropped to the next one
/// rather than honored: `--max-proposals 0` would spend a whole session
/// forbidden from producing anything.
#[must_use]
pub fn resolve_architect_max_proposals(config: &RoleRunnerConfig) -> u64 {
    std::env::var(ARCHITECT_MAX_PROPOSALS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .or(config.architect_max_proposals)
        .unwrap_or(DEFAULT_ARCHITECT_MAX_PROPOSALS)
}

/// The built-in concurrent role-agent ceiling (#6102) when neither
/// [`ROLE_RUNNER_MAX_CONCURRENT_ENV`] nor
/// `autonomous.roleRunner.maxConcurrent` is set.
///
/// **Derived, not a magic number**: it is the count of interval-cadence
/// default roles ([`interval_default_roles`]) — so the shipped ceiling bounds
/// role-agent load at roughly *one wave of distinct roles*, instead of letting
/// it scale with the number of registered workspaces. That distinction is the
/// whole point of #6102: the incident host had 25 registered workspaces × 7
/// interval roles = 175 potentially-concurrent role agents with nothing
/// bounding them, while the operator's `maxConcurrent=8` bounded only sweeps.
///
/// Because it is derived, adding a role to [`DEFAULT_ROLES`] raises the
/// default ceiling by exactly one rather than silently squeezing every other
/// role — the same self-maintaining property `interval_default_roles` gives
/// the allowlist fallback. `.max(1)` keeps it a usable ceiling even if the
/// table were ever emptied (a `0` ceiling would deadlock the loop).
#[must_use]
pub fn default_max_concurrent() -> usize {
    interval_default_roles().len().max(1)
}

/// Resolve the **concurrent role-agent ceiling** (#6102) with precedence
/// **env ([`ROLE_RUNNER_MAX_CONCURRENT_ENV`]) > config
/// (`autonomous.roleRunner.maxConcurrent`, read from each root's own
/// `.loom/config.json`) > [`default_max_concurrent`]**.
///
/// A zero or unparseable value at either tier is dropped to the next one
/// rather than honored: a ceiling of 0 admits nothing, which is
/// `enabled=false` spelled confusingly.
///
/// # Why this exists separately from `maxConcurrent`
///
/// `autonomous.workFinder.maxConcurrent` bounds **sweep dispatch only**. Role
/// agents are spawned by this module's interval loops and the work-finder's
/// idle-edge path *without* passing through
/// [`crate::work_finder`]'s admission checks, so they were admitted entirely
/// outside `min(disk, ram, maxConcurrent)` — an operator lowering that knob
/// after a load-induced crash got less protection than the knob's own
/// documentation implied (#6102). This is the distinct ceiling for the other
/// half of the host's agent load; the two together, not `maxConcurrent`
/// alone, bound how many agents this daemon can have running.
#[must_use]
pub fn resolve_max_concurrent(config: &RoleRunnerConfig) -> usize {
    std::env::var(ROLE_RUNNER_MAX_CONCURRENT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .or(config.max_concurrent)
        .unwrap_or_else(default_max_concurrent)
}

/// Resolve the ceiling for `repo_root` by reading its own
/// `.loom/config.json` — the convenience form of
/// [`read_role_runner_config`] + [`resolve_max_concurrent`] for callers
/// (`loom-daemon status`, `calibrate`) that hold only a path.
#[must_use]
pub fn resolve_max_concurrent_for(repo_root: &Path) -> usize {
    resolve_max_concurrent(&read_role_runner_config(repo_root))
}

/// The prompt string actually passed to `claude -p` for one dispatch of
/// `spec`.
///
/// Every role but `architect` resolves to its static [`RoleSpec::prompt`]
/// verbatim (byte-for-byte the pre-#5656 behavior). `architect` additionally
/// carries the resolved per-invocation proposal cap as a slash-command
/// argument — `/loom:architect --max-proposals <n>` — which
/// `architect.md`'s own "Argument Handling" section reads from `$ARGUMENTS`
/// and enforces as a hard per-run ceiling.
///
/// Carrying the cap in the prompt (rather than, say, an environment variable
/// the session would have to be told to consult) is what makes it an actuator
/// limit rather than a doc note: the number is present in the instruction the
/// role is executing.
#[must_use]
pub fn resolve_role_prompt(spec: &RoleSpec, config: &RoleRunnerConfig) -> String {
    if spec.name == ARCHITECT_ROLE {
        return format!(
            "{} --max-proposals {}",
            spec.prompt,
            resolve_architect_max_proposals(config)
        );
    }
    spec.prompt.to_string()
}

// ============================================================================
// Idle-edge triggering (#4364) — shared in-progress guard + edge/debounce state
// ============================================================================

/// Shared "a role invocation is currently running" set, keyed by
/// `(workspace_root, role_name)`.
///
/// Shared (one instance, cloned) between the interval role loops
/// ([`spawn_multi_role_task`]) and the idle-edge-triggered path
/// ([`plan_idle_runs`]) so the two never overlap for the same `(root, role)`:
/// an interval tick holds the entry for the duration of its `invoke`, and the
/// idle path refuses to fire while the entry is present (and vice versa). This
/// is **in-process shared state only** — deliberately not an event-bus topic
/// (the taxonomy is frozen, #4364).
pub type InProgressGuard = Arc<Mutex<HashSet<(PathBuf, &'static str)>>>;

static ROLE_RUN_START_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Construct an empty [`InProgressGuard`]. One instance is created in `main.rs`
/// and cloned into every interval role loop and the work-finder's idle path so
/// they share a single view.
#[must_use]
pub fn new_in_progress_guard() -> InProgressGuard {
    Arc::new(Mutex::new(HashSet::new()))
}

/// Number of role invocations active across all managed workspaces.
#[must_use]
pub fn active_run_count(set: &InProgressGuard) -> usize {
    set.lock().unwrap_or_else(PoisonError::into_inner).len()
}

/// The daemon's single [`InProgressGuard`], registered once at startup so
/// out-of-band readers (`loom-daemon status` via [`crate::ipc`]) can sample the
/// live role-agent count without threading the `Arc` through the IPC server —
/// the same process-global read-back shape
/// [`crate::admission_brake::register_global`] uses for the brake.
static GLOBAL_IN_PROGRESS: OnceLock<InProgressGuard> = OnceLock::new();

/// Register the process-global [`InProgressGuard`] handle. Idempotent: only the
/// first registration wins (there is exactly one guard per daemon process).
pub fn register_global_in_progress(set: InProgressGuard) {
    let _ = GLOBAL_IN_PROGRESS.set(set);
}

/// Live count of role invocations in flight across every managed workspace,
/// read from the process-global guard (#6102).
///
/// `0` when no guard has been registered (role runner never spawned, or a
/// non-daemon process such as the `calibrate` CLI) — honestly "no role agents
/// observed here", the same zero-behavior-change contract
/// [`crate::admission_brake::global_is_holding`] has.
///
/// This is the count that [`crate::types::DaemonStatusReport::active_role_agents`]
/// reports, so `loom-daemon status` shows total agent load — sweeps *and* role
/// agents — from one place rather than making an operator run `pgrep`.
#[must_use]
pub fn global_active_run_count() -> usize {
    GLOBAL_IN_PROGRESS.get().map_or(0, active_run_count)
}

/// Monotonic process-wide count of successfully started role invocations.
///
/// Unlike an active-count sample, a generation change cannot miss a short role
/// that starts and finishes between idle-exit polling ticks.
#[must_use]
pub fn role_run_start_generation() -> u64 {
    ROLE_RUN_START_GENERATION.load(Ordering::Relaxed)
}

/// RAII guard: [`try_acquire`](Self::try_acquire) inserts `(root, role)` into
/// the shared [`InProgressGuard`]; [`Drop`] removes it.
///
/// Because removal runs in `Drop`, the entry is cleared on **every** exit path
/// of the invocation it guards — success, failure, timeout, or a panic
/// unwinding the task — so a wedged run can never leave a stale entry that
/// permanently blocks that role from ever running again.
#[derive(Debug)]
pub struct RoleRunGuard {
    set: InProgressGuard,
    key: (PathBuf, &'static str),
}

/// The outcome of one role-agent admission attempt ([`RoleRunGuard::admit`]).
///
/// Three states, not two, because the caller must log the *reason* it skipped:
/// "a run for this (root, role) is already going" and "the host's role-agent
/// ceiling is full" are operationally different conditions, and conflating them
/// is exactly what made role-runner load invisible before #6102.
#[derive(Debug)]
pub enum RoleAdmission {
    /// Admitted. Hold the guard for the whole invocation.
    Admitted(RoleRunGuard),
    /// Refused: an interval or idle run already holds this `(root, role)`
    /// (#4364). Unchanged pre-#6102 behavior.
    InProgress,
    /// Refused: admitting would exceed the concurrent role-agent ceiling
    /// ([`resolve_max_concurrent`], #6102). Carries the sampled numbers so the
    /// caller's log line names them.
    CeilingReached {
        /// Role invocations already in flight across every managed workspace.
        active: usize,
        /// The ceiling this tick was resolved against.
        ceiling: usize,
    },
}

impl RoleAdmission {
    /// Unwrap to the guard, discarding *why* it was refused — for callers
    /// (and tests) that only care whether a run started.
    #[must_use]
    pub fn into_guard(self) -> Option<RoleRunGuard> {
        match self {
            Self::Admitted(g) => Some(g),
            Self::InProgress | Self::CeilingReached { .. } => None,
        }
    }
}

impl RoleRunGuard {
    /// Try to mark `(root, role)` in progress. Returns `None` when it is
    /// already marked (another interval or idle run holds it) — the caller then
    /// skips rather than overlapping.
    ///
    /// **Unbounded**: this is [`Self::admit`] with a ceiling of
    /// [`usize::MAX`], retained for callers that genuinely have no ceiling to
    /// apply (and for tests of the #4364 overlap contract in isolation).
    /// Production loops call [`Self::admit`] with the resolved ceiling.
    #[must_use]
    pub fn try_acquire(set: InProgressGuard, root: PathBuf, role: &'static str) -> Option<Self> {
        Self::admit(set, root, role, usize::MAX).into_guard()
    }

    /// Try to mark `(root, role)` in progress, subject to the process-wide
    /// concurrent role-agent `ceiling` (#6102).
    ///
    /// Both checks happen under **one** lock acquisition, so the count a
    /// decision is made against is the same count the insert lands in: two role
    /// loops ticking on different runtime threads cannot both read `ceiling - 1`
    /// active and both admit. (A check-then-acquire pair would be exactly that
    /// race, and it is the race that matters here — a ceiling that leaks under
    /// concurrency is no ceiling.)
    ///
    /// The ceiling is compared against the count across **every** managed
    /// workspace, because the resource it protects (host CPU/RAM) is shared by
    /// all of them — see [`RoleRunnerConfig::max_concurrent`] on why the value
    /// is nonetheless read per-root.
    #[must_use]
    pub fn admit(
        set: InProgressGuard,
        root: PathBuf,
        role: &'static str,
        ceiling: usize,
    ) -> RoleAdmission {
        let key = (root, role);
        {
            let mut guard = set.lock().unwrap_or_else(PoisonError::into_inner);
            if guard.contains(&key) {
                return RoleAdmission::InProgress;
            }
            let active = guard.len();
            if active >= ceiling {
                return RoleAdmission::CeilingReached { active, ceiling };
            }
            guard.insert(key.clone());
        }
        ROLE_RUN_START_GENERATION.fetch_add(1, Ordering::Relaxed);
        RoleAdmission::Admitted(Self { set, key })
    }
}

impl Drop for RoleRunGuard {
    fn drop(&mut self) {
        let mut guard = self.set.lock().unwrap_or_else(PoisonError::into_inner);
        guard.remove(&self.key);
    }
}

/// Per-workspace idle-edge + debounce state for the idle-triggered role runs
/// (#4364). Owned by the work-finder task (one per daemon) and fed one idle
/// observation per root per tick.
///
/// * **Edge, not level.** [`observe_edge`](Self::observe_edge) returns `true`
///   only on the per-root transition from non-idle to idle, so a queue that
///   stays empty across many ticks triggers at most once (on the entering
///   edge).
/// * **Boot counts as already-idle.** A root with no prior observation is
///   treated as already idle, so a daemon that boots on an empty queue does not
///   fire at startup — the same first-tick-skip discipline the interval loops
///   use.
/// * **Debounce.** [`debounce_ok`](Self::debounce_ok) enforces a minimum
///   [`IDLE_TRIGGER_DEBOUNCE`] between idle-triggered runs per `(root, role)`.
#[derive(Debug, Default)]
pub struct IdleTrigger {
    prev_idle: HashMap<PathBuf, bool>,
    last_fired: HashMap<(PathBuf, &'static str), Instant>,
    /// Roots for which a "disabled but onIdle configured" warning has
    /// already been emitted (#4377) — the idle-path equivalent of the
    /// interval loop's `missing_roots_warned` (#4326) dedup. Cleared for a
    /// root the moment its role runner resolves enabled again, so a later
    /// re-disable warns once more rather than staying silent forever.
    disabled_warned: HashSet<PathBuf>,
    /// Whether the **host-level** "disabled by `LOOM_ROLE_RUNNER` env
    /// override" warning has already been emitted this daemon process
    /// (#6470) — deliberately a single bool, not a per-root set like
    /// [`Self::disabled_warned`]: when the env override is what disabled the
    /// role runner, every registered root shares the identical, non-root
    /// cause, so warning once per root (as `disabled_warned` does for the
    /// per-root config cause) would just be the same line N times. Cleared
    /// the moment [`host_env_override`] resolves to "not overriding" again
    /// (env unset or flips truthy), so a later re-disable warns once more.
    host_env_warned: bool,
}

impl IdleTrigger {
    /// Construct an empty tracker (every root starts treated as already-idle).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this tick's idle observation for `root` and return whether the
    /// idle EDGE (non-idle → idle) just fired. The first observation for a root
    /// treats the prior state as idle, so booting idle never fires.
    pub fn observe_edge(&mut self, root: &Path, idle_now: bool) -> bool {
        let prev = self.prev_idle.get(root).copied().unwrap_or(true);
        self.prev_idle.insert(root.to_path_buf(), idle_now);
        !prev && idle_now
    }

    /// Whether `(root, role)` is outside its debounce window — never fired, or
    /// the last idle-triggered run was at least [`IDLE_TRIGGER_DEBOUNCE`] ago.
    #[must_use]
    pub fn debounce_ok(&self, root: &Path, role: &'static str, now: Instant) -> bool {
        match self.last_fired.get(&(root.to_path_buf(), role)) {
            Some(&last) => now.duration_since(last) >= IDLE_TRIGGER_DEBOUNCE,
            None => true,
        }
    }

    /// Record that an idle-triggered run for `(root, role)` fired at `now`,
    /// starting its debounce window.
    pub fn record_fired(&mut self, root: &Path, role: &'static str, now: Instant) {
        self.last_fired.insert((root.to_path_buf(), role), now);
    }

    /// Whether a "disabled but onIdle configured" warning has already been
    /// recorded for `root` (#4377) — test-observable dedup state; also the
    /// hook a status/diagnostic surface could use without re-deriving it.
    #[must_use]
    pub fn disabled_warned(&self, root: &Path) -> bool {
        self.disabled_warned.contains(root)
    }

    /// Whether the single host-level env-override warning
    /// ([`Self::host_env_warned`]) has already been recorded this process
    /// (#6470) — test-observable dedup state, mirroring
    /// [`Self::disabled_warned`]'s per-root accessor.
    #[must_use]
    pub fn host_env_warned(&self) -> bool {
        self.host_env_warned
    }
}

/// Decide which on-idle roles should fire for `root` right now, given this
/// tick's idle observation. Pure of any claude spawning (the caller does the
/// fire-and-forget invocation), so the edge / debounce / guard logic is
/// unit-testable without a real `claude` session.
///
/// Steps, in order:
/// 1. Record the idle edge (always — so the level state stays accurate even on
///    a tick that ends up not firing).
/// 2. Bail on no edge, or on an active scheduled drain (#4090).
/// 3. Bail when the role runner is disabled for this root
///    ([`resolve_enabled`], precedence env > config > default) — this is the
///    **per-root** gate (#4377): it is resolved from `root`'s own
///    `.loom/config.json`, independent of the daemon workspace's own master
///    switch, which only decides whether the loops start at all. When
///    `onIdle` roles are configured for `root` but the gate is off, this is
///    the silent-no-op the issue exists to fix — see
///    [`warn_if_idle_configured_but_disabled`].
/// 4. Per configured on-idle role ([`resolve_on_idle_roles`]): skip if inside
///    the debounce window, or if an interval / idle run already holds the
///    in-progress guard; else record the fire and acquire the guard.
///
/// The returned [`RoleRunGuard`]s must be held by the caller for the duration
/// of each fire-and-forget invocation (they clear the in-progress entry on
/// drop).
#[must_use]
pub fn plan_idle_runs(
    trigger: &mut IdleTrigger,
    in_progress: &InProgressGuard,
    root: &Path,
    config: &RoleRunnerConfig,
    idle_now: bool,
    draining: bool,
    now: Instant,
) -> Vec<(RoleSpec, RoleRunGuard)> {
    let edge = trigger.observe_edge(root, idle_now);
    if !edge {
        return Vec::new();
    }
    if draining {
        log::debug!(
            "role_runner: idle edge for {} suppressed — drain in progress (#4090)",
            root.display()
        );
        return Vec::new();
    }
    if !resolve_enabled(config) {
        warn_if_idle_configured_but_disabled(trigger, root, config);
        return Vec::new();
    }
    // The root is enabled again — clear any stale disabled-warning so a
    // later disable re-warns instead of staying silent forever (#4377).
    trigger.disabled_warned.remove(root);
    // The host env override, if any, is not disabling right now either
    // (else this root could not have resolved enabled) — clear the
    // host-level dedup too so a later env-off re-warns once more (#6470).
    trigger.host_env_warned = false;
    // Host sharding (#6374), same gate and same ordering as the interval path
    // in `decide_root_tick`. The idle edge fires on every host that observes
    // it, so without this an idle-triggered role would duplicate across the
    // fleet exactly as the interval cadence did — the sharding invariant has
    // to cover BOTH dispatch surfaces or it does not hold.
    let shard = crate::role_shard::decide(root);
    crate::role_shard::log_decision_once(root, &shard);
    if !shard.owned {
        log::debug!(
            "role_runner: idle edge for {} suppressed — this workspace's role slice belongs to \
             another host (#6374)",
            root.display()
        );
        return Vec::new();
    }
    // Concurrent role-agent ceiling (#6102), resolved from this root's own
    // config. Resolved ONCE for the whole edge rather than per-spec so a single
    // idle edge cannot admit a burst that each individually passed a
    // re-resolved ceiling; the count itself is still re-sampled per admission
    // (inside `admit`), so guards taken earlier in this loop do count against
    // the ones taken later.
    let ceiling = resolve_max_concurrent(config);
    let mut out = Vec::new();
    for spec in resolve_on_idle_roles(config) {
        if !trigger.debounce_ok(root, spec.name, now) {
            log::debug!(
                "role_runner: idle edge for {} — {} within {}s debounce, skipping",
                root.display(),
                spec.name,
                IDLE_TRIGGER_DEBOUNCE.as_secs()
            );
            continue;
        }
        let guard = match RoleRunGuard::admit(
            in_progress.clone(),
            root.to_path_buf(),
            spec.name,
            ceiling,
        ) {
            RoleAdmission::Admitted(g) => g,
            RoleAdmission::InProgress => {
                log::debug!(
                    "role_runner: idle edge for {} — {} run already in progress, skipping",
                    root.display(),
                    spec.name
                );
                continue;
            }
            RoleAdmission::CeilingReached { active, ceiling } => {
                // #6102: logged at `warn!`, not `debug!` — a ceiling refusal is
                // the host telling the operator it is at its agent budget, which
                // is precisely the signal that was invisible before this cap
                // existed. The per-(root, role) skip above stays `debug!`
                // because it is routine cadence overlap, not a resource limit.
                log::warn!(
                    "role_runner: idle edge for {} — {} not admitted: {active} role agent(s) \
                     already in flight at the ceiling of {ceiling} \
                     (autonomous.roleRunner.maxConcurrent / \
                     {ROLE_RUNNER_MAX_CONCURRENT_ENV}, #6102)",
                    root.display(),
                    spec.name
                );
                continue;
            }
        };
        trigger.record_fired(root, spec.name, now);
        out.push((spec, guard));
    }
    out
}

/// Emit a warn-once-per-root line (#4377) when an idle edge fires for `root`
/// while `onIdle` roles are configured there but the role runner is disabled
/// for that root (`resolve_enabled` false). Before this the idle path bailed
/// with **no log at any level** — every neighboring bail (drain, debounce,
/// in-progress guard) already logs at `debug!`, so this was the fully-silent
/// gap: a registered workspace with `onIdle` set but no
/// `autonomous.roleRunner.enabled: true` in its own `.loom/config.json` got
/// zero ticks and zero diagnostics.
///
/// A root with **no** `onIdle` roles configured stays silent here — disabled
/// is that root's normal, unconfigured state, not a misconfiguration worth
/// flagging on every idle edge. Dedup state lives on [`IdleTrigger`] (see
/// [`IdleTrigger::disabled_warned`]) and is cleared the moment the root
/// resolves enabled again ([`plan_idle_runs`]), so a later re-disable warns
/// once more rather than staying silent forever.
///
/// **Names the true cause (#6470).** [`resolve_enabled_with_source`] can
/// return `false` for two structurally different reasons that used to be
/// collapsed into one message: this root's own `.loom/config.json` (the
/// #4377 case below), or the host-wide [`ROLE_RUNNER_ENABLE_ENV`] override,
/// which disables **every** registered root regardless of its own config.
/// The env case is handled first and separately: since it is the identical,
/// non-root cause for every root on this host, it collapses to a single
/// [`IdleTrigger::host_env_warned`] warning per daemon process instead of
/// one per root — the #4377 per-root dedup below would otherwise repeat the
/// same host-level fact once per registered root.
fn warn_if_idle_configured_but_disabled(
    trigger: &mut IdleTrigger,
    root: &Path,
    config: &RoleRunnerConfig,
) {
    let on_idle = resolve_on_idle_roles(config);
    if on_idle.is_empty() {
        return;
    }
    if resolve_enabled_with_source(config).1 == EnabledSource::Env {
        if trigger.host_env_warned {
            return; // already warned once this process; stay quiet until it re-enables
        }
        trigger.host_env_warned = true;
        let raw = std::env::var(ROLE_RUNNER_ENABLE_ENV).unwrap_or_default();
        log::warn!(
            "role_runner: idle edge fired for {} with onIdle roles {:?} configured, but the \
             role runner is disabled by the host-wide env override \
             {ROLE_RUNNER_ENABLE_ENV}={raw:?} — this overrides EVERY registered root's own \
             .loom/config.json (including this root's own, which may already say \
             autonomous.roleRunner.enabled=true), so editing this root's config will not help; \
             unset {ROLE_RUNNER_ENABLE_ENV} or set it to a truthy value to re-enable. This is a \
             one-time, HOST-LEVEL warning (not repeated per root) — see `loom-daemon status` \
             for the current per-root state (#6470).",
            root.display(),
            on_idle.iter().map(|r| r.name).collect::<Vec<_>>(),
        );
        return;
    }
    if !trigger.disabled_warned.insert(root.to_path_buf()) {
        return; // already warned for this root; stay quiet until it re-enables
    }
    log::warn!(
        "role_runner: idle edge fired for {} with onIdle roles {:?} configured, but the role \
         runner is disabled for this root (autonomous.roleRunner.enabled is false or absent in \
         {}'s own .loom/config.json) — these roles will never fire here until \
         autonomous.roleRunner.enabled=true is set in that root's own config; enablement is \
         resolved per registered root, not inherited from the daemon workspace's master switch \
         (#4377). This is a one-time warning for this root — see `loom-daemon status` for the \
         current per-root state.",
        root.display(),
        on_idle.iter().map(|r| r.name).collect::<Vec<_>>(),
        root.display(),
    );
}

/// Observe `root`'s post-tick idle state and, on the idle edge, fire-and-forget
/// each configured on-idle role (#4364) — the entry point the work-finder loop
/// calls once per root per tick.
///
/// Reads `root`'s own `.loom/config.json` (hot-apply, like the interval loops)
/// each tick and delegates the edge / debounce / guard decision to
/// [`plan_idle_runs`]. Each fired role runs as a detached `tokio::spawn` +
/// `spawn_blocking`, so this returns immediately — the work-finder tick NEVER
/// awaits a multi-minute role session. The in-progress guard for each run is
/// held for the whole invocation and cleared on every exit path.
pub fn observe_and_fire_idle(
    trigger: &mut IdleTrigger,
    in_progress: &InProgressGuard,
    root: &Path,
    idle_now: bool,
    draining: bool,
) {
    let config = read_role_runner_config(root);
    let plans =
        plan_idle_runs(trigger, in_progress, root, &config, idle_now, draining, Instant::now());
    for (spec, guard) in plans {
        let root_owned = root.to_path_buf();
        let name = spec.name;
        // #5656: identical to `spec.prompt` for every role but `architect`,
        // which carries this root's resolved per-invocation proposal cap.
        let prompt = resolve_role_prompt(&spec, &config);
        // The idle path has no ticker of its own, so the collision probe's
        // lookback window (#4623) defaults to this role's *interval* cadence —
        // the same span a peer's interval-driven pass would write within.
        let interval = resolve_interval_for_role(&spec, &config);
        log::info!(
            "role_runner: idle edge for {} — firing idle-triggered {} run (#4364)",
            root.display(),
            name
        );
        tokio::spawn(async move {
            // Held for the whole invocation; the in-progress entry clears when
            // this guard drops (every exit path — success/failure/panic).
            let _guard = guard;
            let run_root = root_owned.clone();
            let tick_start = Instant::now();
            let joined = tokio::task::spawn_blocking(move || {
                let mut runner = ScriptRoleInvocationRunner::new(run_root.clone());
                // Cross-host collision detection (#4623) — detection only.
                invoke_with_collision_probe(&mut runner, &run_root, name, &prompt, interval)
            })
            .await;
            let elapsed = tick_start.elapsed();
            match joined {
                Ok(outcome) => log_outcome_for_root(name, &root_owned, &outcome, elapsed),
                Err(e) => log::error!(
                    "role_runner: idle-triggered {name} run for {} panicked ({e})",
                    root_owned.display()
                ),
            }
        });
    }
}

/// Whether the interval loop ([`spawn_multi_role_task`]) should log a `WARN`
/// (vs. a quieter, already-warned `DEBUG`) for `root` being disabled on this
/// tick (#4377): `true` the first time `root` is newly inserted into
/// `warned`, `false` on every subsequent tick until the caller removes it
/// (which it does once `root` resolves enabled again). Pulled out as a pure
/// function — mirroring [`classify_root_tick_log`] — so the warn-once dedup
/// is unit-testable without a running loop or captured log output.
#[must_use]
fn should_warn_disabled_root(warned: &mut HashSet<PathBuf>, root: &Path) -> bool {
    warned.insert(root.to_path_buf())
}

// ============================================================================
// Runtime wiring
// ============================================================================

/// Run one role invocation wrapped in cross-host collision **detection**
/// (#4623): a pre-tick probe of the role's own forge queue, then self-run
/// window bookkeeping so the *next* probe can tell this process's own writes
/// apart from a peer daemon's.
///
/// Ordering matters and is load-bearing:
/// 1. **probe first** — [`crate::role_collision::probe_before_tick`] reads the
///    baseline left by our *previous* completed run; starting a new run first
///    would clear it.
/// 2. `record_run_started` opens this run's window (suppressing attribution
///    while it is in flight — under-count, never over-count).
/// 3. `record_run_finished` closes it, becoming the next probe's baseline.
///
/// The probe is a **no-op with no forge call** when detection is disabled for
/// `root` (default), so the disabled path costs one config read; the tick's
/// behavior is identical either way — detection never suppresses, delays, or
/// reorders an invocation.
///
/// **Must run on a blocking thread** (every call site is already inside
/// `spawn_blocking`): the probe shells out to `gh`.
fn invoke_with_collision_probe<R: RoleInvocationRunner + ?Sized>(
    runner: &mut R,
    root: &Path,
    role: &'static str,
    prompt: &str,
    interval: Duration,
) -> RoleTickOutcome {
    crate::role_collision::probe_before_tick(root, role, interval);
    crate::role_collision::record_run_started(root, role, chrono::Utc::now());
    let outcome = runner.invoke(role, prompt);
    crate::role_collision::record_run_finished(root, role, chrono::Utc::now());
    outcome
}

/// Render a caught panic payload (`Box<dyn Any + Send>`, as produced by
/// [`std::panic::catch_unwind`]) as a short, loggable string (#6201).
/// `panic!("...")` and `.unwrap()`/`.expect("...")` payloads are almost
/// always `&'static str` or `String`; anything else degrades to a generic
/// label rather than failing to log at all.
fn describe_panic(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// The synchronous per-root decision phase of one [`spawn_multi_role_task`]
/// interval tick: resolve this root's own config, the disabled/membership
/// checks, prompt resolution, and run-guard admission. `Some((prompt,
/// guard))` means "proceed to invoke, holding `guard` for the duration";
/// `None` means "nothing to do this tick for this root" — every branch that
/// returns it already logged its own reason, exactly mirroring the bare
/// `continue` statements this was factored out of.
///
/// Pulled out of the loop body specifically so the **caller** can wrap the
/// call in [`std::panic::catch_unwind`] (#6201 AC2): every downstream step
/// here (`spawn_blocking`) is already panic-isolated by tokio, but this
/// synchronous prefix runs directly on the loop's own task — an unguarded
/// panic anywhere in it would silently end the *entire* per-role
/// multi-workspace loop (every registered root, not just this one) with no
/// automatic recovery short of a daemon restart, no matter how many later,
/// healthy ticks would otherwise have retried. That is precisely the
/// "RECOVERABLE failure never retried… permanent silent benching" failure
/// mode the filed incident describes.
#[allow(clippy::too_many_arguments)]
fn decide_root_tick(
    root: &Path,
    spec: &RoleSpec,
    in_progress: &InProgressGuard,
    disabled_roots_warned: &mut HashSet<PathBuf>,
    resolved_roles_logged: &mut HashMap<PathBuf, String>,
    missing_defaults_logged: &mut HashMap<PathBuf, Vec<&'static str>>,
) -> Option<(String, RoleRunGuard)> {
    let config = read_role_runner_config(root);
    if !resolve_enabled(&config) {
        // Per-root gate (#4377): `enabled` is resolved from this root's own
        // `.loom/config.json`, independent of the daemon workspace's master
        // switch (which only decided whether this loop started at all).
        // First sighting warns at `info`-visible `warn!`; repeats downgrade
        // to `debug!` so a persistently-disabled root does not spam the log
        // every tick forever.
        if should_warn_disabled_root(disabled_roots_warned, root) {
            log::warn!(
                "role_runner: {} disabled for {} — autonomous.roleRunner.enabled is false or \
                 absent in that root's own .loom/config.json (enablement is resolved per \
                 registered root, not inherited from the daemon workspace's master switch, \
                 #4377); this root will receive zero {} ticks until \
                 autonomous.roleRunner.enabled=true is set there (see `loom-daemon status` for \
                 the current per-root state; further identical skips for this root are logged \
                 at DEBUG until it re-enables)",
                spec.name,
                root.display(),
                spec.name
            );
        } else {
            log::debug!(
                "role_runner: {} disabled for {} (autonomous.roleRunner.enabled=false or \
                 LOOM_ROLE_RUNNER unset-falsy) — skipping (already warned above)",
                spec.name,
                root.display()
            );
        }
        return None;
    }
    // The root resolved enabled again — clear any stale disabled-warning so a
    // later disable re-warns (#4377).
    disabled_roots_warned.remove(root);
    // Host sharding (#6374): on a fleet, each workspace's role rotation must
    // run on exactly ONE host per interval — otherwise N dispatchers each
    // spawn the same role session over the same forge queue, which is both
    // how the token pool got drawn down to 2/17 and how the #6332 / #6352
    // cross-host duplication bugs happened.
    //
    // Placed AFTER the `resolve_enabled` gate above on purpose: the host-wide
    // `LOOM_ROLE_RUNNER=0` override (AC3) must keep short-circuiting
    // everything before sharding is even consulted, so an operator's blunt
    // kill switch is never weakened (or second-guessed) by shard state.
    //
    // Unsharded hosts — the default, and every malformed/incomplete config —
    // own every workspace, so this is a no-op on a single-host install.
    let shard = crate::role_shard::decide(root);
    crate::role_shard::log_decision_once(root, &shard);
    if !shard.owned {
        log::debug!(
            "role_runner: {} tick for {} skipped — this workspace's role slice belongs to \
             another host (#6374)",
            spec.name,
            root.display()
        );
        return None;
    }
    // Resolved-role-list diagnostic (#5654 AC1): computed once per root per
    // tick and reused below for the membership check, rather than calling
    // `resolve_roles` twice.
    let resolved_roles = resolve_roles(&config);
    let roles_line = resolved_roles_log_line(root, &resolved_roles);
    match resolved_roles_logged.get(root) {
        Some(prev) if *prev == roles_line => log::debug!("{roles_line}"),
        _ => {
            log::info!("{roles_line}");
            resolved_roles_logged.insert(root.to_path_buf(), roles_line);
        }
    }
    // Stale pinned `roles` allowlist diagnostic (#6163): computed from this
    // same `config`/`root` already in scope, warned at most once per
    // resolved-config change, and only from the one designated reporter loop
    // (`is_missing_defaults_reporter`) so the other DEFAULT_ROLES loops do not
    // each re-emit the same workspace's identical line. AC1 names the
    // workspace, AC2 excludes anything covered by `onIdle`, AC3 stops the
    // pre-#6163 every-tick-forever repeat, AC4 aggregates every missing role
    // into one line.
    if let (true, Some(names)) = (is_missing_defaults_reporter(spec), &config.roles) {
        let on_idle = config.on_idle.as_deref().unwrap_or(&[]);
        let missing = missing_defaults_uncovered_by_on_idle(names, on_idle);
        match (missing.is_empty(), missing_defaults_logged.get(root)) {
            (true, _) => {
                missing_defaults_logged.remove(root);
            }
            (false, Some(prev)) if *prev == missing => {}
            (false, _) => {
                if let Some(line) = missing_defaults_warning_line(root, &missing) {
                    log::warn!("{line}");
                }
                missing_defaults_logged.insert(root.to_path_buf(), missing);
            }
        }
    }
    if !resolved_roles.iter().any(|r| r.name == spec.name) {
        log::debug!(
            "role_runner: {} not in autonomous.roleRunner.roles for {} — skipping",
            spec.name,
            root.display()
        );
        return None;
    }
    let name = spec.name;
    // #5656: identical to `spec.prompt` for every role but `architect`, which
    // carries this root's own resolved per-invocation proposal cap
    // (per-root, like every other knob resolved from `config` above).
    let prompt = resolve_role_prompt(spec, &config);
    // Shared in-progress guard (#4364): skip this root's interval tick when
    // an idle-triggered (or overlapping) run for the same (root, role) is
    // already active. Held across the invocation by the caller; cleared on
    // drop (every exit path).
    //
    // #6102: the same call now also enforces the concurrent role-agent
    // ceiling, resolved from this root's own config (already read above as
    // `config`) — the bound that `autonomous.workFinder.maxConcurrent` never
    // provided, since role agents never pass through work-finder admission.
    match RoleRunGuard::admit(
        in_progress.clone(),
        root.to_path_buf(),
        name,
        resolve_max_concurrent(&config),
    ) {
        RoleAdmission::Admitted(g) => Some((prompt, g)),
        RoleAdmission::InProgress => {
            log::debug!(
                "role_runner: {} tick for {} skipped — a run is already in progress (#4364)",
                name,
                root.display()
            );
            None
        }
        RoleAdmission::CeilingReached { active, ceiling } => {
            log::warn!(
                "role_runner: {} tick for {} not admitted — {active} role agent(s) already in \
                 flight at the ceiling of {ceiling} (autonomous.roleRunner.maxConcurrent / \
                 {ROLE_RUNNER_MAX_CONCURRENT_ENV}, #6102); retrying next tick",
                name,
                root.display()
            );
            None
        }
    }
}

/// Spawn the role-runner loop for a single role on a single workspace on the
/// shared daemon runtime. Intended for tests; production uses
/// [`spawn_multi_role_task`] (the multi-workspace entry point wired into
/// `main.rs`).
///
/// Mirrors [`crate::work_finder::spawn_work_finder_task`] /
/// [`crate::main_health_gate`]: the **first tick is skipped** so several
/// role loops starting at daemon boot don't burst several `claude` sessions
/// at once — see the module docs.
pub fn spawn_role_task<R>(
    mut runner: R,
    spec: RoleSpec,
    interval: Duration,
    drain: std::sync::Arc<std::sync::atomic::AtomicBool>,
    root: PathBuf,
    in_progress: InProgressGuard,
) -> tokio::task::JoinHandle<()>
where
    R: RoleInvocationRunner + Send + 'static,
{
    log::info!("role_runner: starting {} loop (interval={}s)", spec.name, interval.as_secs());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // skip immediate first tick (see module docs)
        loop {
            ticker.tick().await;
            // Scheduled drain (#4090): role ticks have no sweep-registry entry to
            // await, so a drain cannot wait for an in-flight tick — but it MUST
            // stop new ticks from *starting* (e.g. a Champion mid-merge). Skip
            // the whole tick while draining.
            if drain.load(std::sync::atomic::Ordering::Relaxed) {
                log::debug!(
                    "role_runner: {} tick skipped — drain in progress (no new role dispatch)",
                    spec.name
                );
                continue;
            }
            // Shared GitHub rate limit exhausted (#4429): a role session
            // spawned now would burn a token slot just to fail its own gh
            // calls against the same wall — skip until the window resets.
            if crate::rate_limit_breaker::global_is_suppressed() {
                log::debug!(
                    "role_runner: {} tick skipped — rate-limit cooldown (#4429)",
                    spec.name
                );
                continue;
            }
            let name = spec.name;
            // #5656: identical to `spec.prompt` for every role but `architect`
            // (whose per-invocation proposal cap is re-read each tick, so a
            // config edit hot-applies like every other role-runner knob).
            let tick_config = read_role_runner_config(&root);
            let prompt = resolve_role_prompt(&spec, &tick_config);
            // Shared in-progress guard (#4364): skip this interval tick if an
            // idle-triggered (or overlapping) run for the same (root, role) is
            // already active. Held for the whole invocation; cleared on drop.
            //
            // #6102: the same call now also enforces the concurrent role-agent
            // ceiling, re-resolved each tick so a config edit hot-applies like
            // every other role-runner knob.
            let _run_guard = match RoleRunGuard::admit(
                in_progress.clone(),
                root.clone(),
                name,
                resolve_max_concurrent(&tick_config),
            ) {
                RoleAdmission::Admitted(g) => g,
                RoleAdmission::InProgress => {
                    log::debug!(
                        "role_runner: {} tick for {} skipped — a run is already in progress \
                         (#4364)",
                        name,
                        root.display()
                    );
                    continue;
                }
                RoleAdmission::CeilingReached { active, ceiling } => {
                    log::warn!(
                        "role_runner: {} tick for {} not admitted — {active} role agent(s) \
                         already in flight at the ceiling of {ceiling} \
                         (autonomous.roleRunner.maxConcurrent / \
                         {ROLE_RUNNER_MAX_CONCURRENT_ENV}, #6102); retrying next tick",
                        name,
                        root.display()
                    );
                    continue;
                }
            };
            let tick_start = Instant::now();
            let probe_root = root.clone();
            let joined = tokio::task::spawn_blocking(move || {
                // Cross-host collision detection (#4623) — detection only; the
                // invocation itself is unchanged.
                let outcome =
                    invoke_with_collision_probe(&mut runner, &probe_root, name, &prompt, interval);
                (outcome, runner)
            })
            .await;
            let elapsed = tick_start.elapsed();
            match joined {
                Ok((outcome, r)) => {
                    runner = r;
                    log_outcome(spec.name, &outcome, elapsed);
                }
                Err(e) => {
                    log::error!(
                        "role_runner: {} invocation task panicked ({e}); stopping this role's loop",
                        spec.name
                    );
                    return;
                }
            }
        }
    })
}

/// Spawn the **multi-workspace** role-runner loop for one role (mirrors
/// [`crate::token_ranking_refresh::spawn_multi_token_ranking_refresh_task`])
/// on the shared daemon runtime.
///
/// Every `interval` it re-reads [`WorkspaceRegistry::effective_roots`]
/// against `fallback_root` (an **empty** registry yields the single
/// `fallback_root`), drops any root whose directory no longer exists on disk
/// via the shared [`filter_missing_roots`] hygiene (#4326/#4349 — warn once
/// per missing period, never auto-remove), and, for each surviving root
/// whose own `.loom/config.json` has this role enabled (`resolve_enabled`
/// AND the role name present in `resolve_roles` — precedence env > config >
/// default), runs one invocation. Invocations run **sequentially** per tick
/// (no shared mutable state to leak across repos, and it avoids bursting
/// concurrent `claude` sessions across every registered repo at once).
///
/// A repeatedly-failing root (e.g. a broken MCP preflight, #4349) logs once
/// on the fail edge and once on recovery — not once per tick — via a
/// per-root failing-state map tracked across ticks (mirrors the
/// `was_halted`/`was_pressured` state-change-dedup discipline in
/// [`crate::work_finder`]).
pub fn spawn_multi_role_task(
    spec: RoleSpec,
    fallback_root: PathBuf,
    interval: Duration,
    drain: std::sync::Arc<std::sync::atomic::AtomicBool>,
    in_progress: InProgressGuard,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "role_runner: starting {} multi-workspace loop (interval={}s)",
        spec.name,
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // skip immediate first tick (see module docs)
                             // Missing-root warn-once-per-period state (#4326), shared discipline
                             // with `work_finder` via `filter_missing_roots`.
        let mut missing_roots_warned: HashSet<PathBuf> = HashSet::new();
        // Per-root failing state (#4349), so a persistently failing tick logs
        // only on the fail edge and on recovery, not every tick.
        let mut failing_roots: HashMap<PathBuf, bool> = HashMap::new();
        // Per-root no-token-pool state (#4642), tracked completely
        // independently of `failing_roots` so a permanent missing-pool skip
        // is never conflated with (or silences the WARN for) a genuine
        // invocation failure — see `RootTickLogAction::is_no_token_pool`.
        let mut no_token_pool_roots: HashMap<PathBuf, bool> = HashMap::new();
        // Per-root model/runtime-mismatch state (#5028), tracked completely
        // independently of both `failing_roots` and `no_token_pool_roots` so a
        // permanent config-conflict skip is never conflated with (or silences
        // the WARN for) either — see `RootTickLogAction::is_model_mismatch`.
        let mut model_mismatch_roots: HashMap<PathBuf, bool> = HashMap::new();
        // Disabled-root warn-once state (#4377): the per-tick disabled-skip
        // below is otherwise only a `debug!` — invisible at the default `info`
        // level, so a registered root left disabled gets zero diagnostics.
        // Same warn-once-then-dedup shape as `missing_roots_warned`, but
        // without `filter_missing_roots`'s reset-every-tick semantics: an
        // entry here is cleared only when its root resolves enabled again
        // (see below), so re-disabling re-warns instead of staying silent.
        let mut disabled_roots_warned: HashSet<PathBuf> = HashSet::new();
        // Per-root last-logged resolved-role-list line (issue #5654 AC1):
        // every tick logs the current [`resolved_roles_log_line`] at DEBUG
        // (satisfying "per-repo, per-tick"), but escalates to INFO whenever
        // the line's content differs from the last one recorded for this
        // root — first sighting, a config edit, or a daemon rebuild that
        // changed [`DEFAULT_ROLES`] (surfaced here as a changed
        // `default_roles=` snapshot id) all trip this edge. Same
        // warn/info-once-then-dedup shape as `disabled_roots_warned` /
        // `missing_roots_warned` above.
        let mut resolved_roles_logged: HashMap<PathBuf, String> = HashMap::new();
        // Per-root last-warned "stale pinned roles allowlist" set (#6163
        // AC3): the [`missing_defaults_warning_line`] `log::warn!` fires only
        // when this root's currently-missing set differs from the last one
        // recorded here — first sighting (including this loop's own startup)
        // or a config edit that changes which roles are missing. Only the
        // designated reporter loop ([`is_missing_defaults_reporter`]) ever
        // populates this map; the other DEFAULT_ROLES loops leave it empty
        // rather than duplicating the same workspace's line N times. Cleared
        // (not just left stale) once the root stops being missing anything,
        // so a later regression re-warns instead of staying silent forever.
        // Own map, deliberately not folded into `resolved_roles_logged`
        // above: that one already has its own change-detection semantics
        // (any content difference, including a `default_roles=` snapshot
        // bump) and this repo's own tests pin its exact string output —
        // keeping the two independent avoids coupling either's format to the
        // other's dedup trigger.
        let mut missing_defaults_logged: HashMap<PathBuf, Vec<&'static str>> = HashMap::new();
        loop {
            ticker.tick().await;

            // Scheduled drain (#4090): stop starting new role ticks across every
            // workspace while a drain is in progress (Finding 2 — role ticks are
            // not in the sweep registry, so the drain cannot await them, but it
            // must not let a fresh Champion/Curator tick start mid-roll).
            if drain.load(std::sync::atomic::Ordering::Relaxed) {
                log::debug!(
                    "role_runner: {} multi-workspace tick skipped — drain in progress",
                    spec.name
                );
                continue;
            }
            // Shared GitHub rate limit exhausted (#4429): a role session
            // spawned now would burn a token slot just to fail its own gh
            // calls against the same wall — skip until the window resets.
            if crate::rate_limit_breaker::global_is_suppressed() {
                log::debug!(
                    "role_runner: {} multi-workspace tick skipped — rate-limit cooldown (#4429)",
                    spec.name
                );
                continue;
            }

            let roots = WorkspaceRegistry::load_default()
                .unwrap_or_else(|e| {
                    log::warn!(
                        "role_runner: could not load workspace registry ({e}); using fallback"
                    );
                    WorkspaceRegistry::default()
                })
                .effective_roots(&fallback_root);
            // Skip registered roots whose directory no longer exists on disk
            // (#4326) so a dangling entry cannot burn every tick forever —
            // warn-and-skip, never auto-remove (`loom-daemon status` flags it,
            // `workspace remove` clears it).
            let roots = filter_missing_roots(roots, &mut missing_roots_warned);

            for root in roots {
                // #6201 AC2: the synchronous decision phase — config reads,
                // the disabled/membership checks, prompt resolution, and
                // run-guard admission — is defended with `catch_unwind`.
                // Every step past this point (`spawn_blocking` below) is
                // already panic-isolated by tokio; this closes the one gap
                // that would otherwise let a panic HERE silently end this
                // role's ENTIRE multi-workspace loop (every registered root,
                // not just this one) with no automatic recovery short of a
                // daemon restart. `AssertUnwindSafe` is sound here: the two
                // captured `&mut` maps are dedup/bookkeeping state only — a
                // panic mid-update leaves them, at worst, one tick stale
                // (re-warning or re-logging once more than strictly
                // necessary), never a correctness or safety issue.
                let decision = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    decide_root_tick(
                        &root,
                        &spec,
                        &in_progress,
                        &mut disabled_roots_warned,
                        &mut resolved_roles_logged,
                        &mut missing_defaults_logged,
                    )
                })) {
                    Ok(decision) => decision,
                    Err(panic) => {
                        log::error!(
                            "role_runner: {} tick decision for {} panicked ({}) — skipping only \
                             this root's this tick; the loop continues on the next interval \
                             (#6201)",
                            spec.name,
                            root.display(),
                            describe_panic(&*panic)
                        );
                        None
                    }
                };
                let Some((prompt, _run_guard)) = decision else {
                    continue;
                };
                let name = spec.name;
                let root_for_task = root.clone();
                let tick_start = Instant::now();
                let joined = tokio::task::spawn_blocking(move || {
                    let mut runner = ScriptRoleInvocationRunner::new(root_for_task.clone());
                    // Cross-host collision detection (#4623) — detection only;
                    // the invocation itself is unchanged.
                    invoke_with_collision_probe(
                        &mut runner,
                        &root_for_task,
                        name,
                        &prompt,
                        interval,
                    )
                })
                .await;
                let elapsed = tick_start.elapsed();
                match joined {
                    Ok(outcome) => log_outcome_for_root_deduped(
                        spec.name,
                        &root,
                        &outcome,
                        elapsed,
                        &mut failing_roots,
                        &mut no_token_pool_roots,
                        &mut model_mismatch_roots,
                    ),
                    Err(e) => log::error!(
                        "role_runner: {} invocation task for {} panicked ({e}); continuing to the \
                         next repo",
                        spec.name,
                        root.display()
                    ),
                }
            }
        }
    })
}

/// True when `outcome` is a [`RoleTickOutcome::Success`] that completed
/// faster than [`IMPLAUSIBLY_FAST_TICK`] — the signal that distinguishes a
/// genuine no-op-that-reports-success (issue #4034: a slash-command prompt
/// that did not resolve, so `claude -p` answered a one-off prompt and exited
/// 0 in ~1.4s) from a healthy tick. A real `claude -p "/<role>"` session
/// cannot start, authenticate, and do real forge work that quickly. Pulled
/// out of the two `log_outcome*` functions so the threshold logic is
/// unit-testable without capturing `log` crate output.
#[must_use]
fn tick_is_implausibly_fast(outcome: &RoleTickOutcome, elapsed: Duration) -> bool {
    matches!(outcome, RoleTickOutcome::Success) && elapsed < IMPLAUSIBLY_FAST_TICK
}

/// Log a single-workspace invocation outcome, including elapsed tick
/// duration. Never escalates to `error!` — a role-invocation failure is never
/// fatal to the daemon. See [`tick_is_implausibly_fast`] for the `WARN`
/// escalation on a suspiciously-fast `Success`.
fn log_outcome(role: &str, outcome: &RoleTickOutcome, elapsed: Duration) {
    match outcome {
        RoleTickOutcome::Success if tick_is_implausibly_fast(outcome, elapsed) => {
            log::warn!(
                "role_runner: {role} tick completed in {elapsed:.1?} — implausibly fast for a \
                 real session (threshold {IMPLAUSIBLY_FAST_TICK:.0?}); this may be a no-op that \
                 exited 0 without doing real work (e.g. a slash-command prompt that did not \
                 resolve)"
            );
        }
        RoleTickOutcome::Success => {
            log::info!("role_runner: {role} tick completed in {elapsed:.1?}");
        }
        RoleTickOutcome::Failure(reason) => {
            log::warn!(
                "role_runner: {role} tick failed after {elapsed:.1?} (logged and skipped, never \
                 fatal): {reason}"
            );
        }
        RoleTickOutcome::RuntimeRejected(rejection) => {
            log::warn!("role_runner: {role} runtime admission rejected: {rejection}");
        }
        RoleTickOutcome::NoTokenPool => {
            log::warn!(
                "role_runner: {role} tick skipped after {elapsed:.1?} — no token pool available \
                 (neither a per-repo .loom/tokens/ pool nor a provisioned shared pool at \
                 ~/.loom/tokens; run `loom-daemon tokens bootstrap` for a per-repo pool, or \
                 `loom-daemon tokens bootstrap --shared` for the machine-level pool — see \
                 .loom/docs/token-pool.md, #4642)"
            );
        }
        RoleTickOutcome::ModelRuntimeMismatch(mismatch) => {
            log::warn!(
                "role_runner: {role} tick skipped after {elapsed:.1?} — {} (#5028)",
                mismatch.detail()
            );
        }
        RoleTickOutcome::LoadSkipped {
            load_per_core,
            detail,
        } => {
            log::warn!(
                "role_runner: {role} tick skipped after {elapsed:.1?}: skipped: host saturated \
                 (load/core {load_per_core:.2}) at the tick ceiling — not counted as a failure \
                 (#6637): {detail}"
            );
        }
    }
}

/// Root-aware variant of [`log_outcome`] for the **fire-and-forget idle path**
/// ([`observe_and_fire_idle`], #4364). Unlike the repeating multi-workspace
/// interval loop — which uses [`log_outcome_for_root_deduped`] to suppress a
/// persistently-failing root's per-tick WARN noise (#4349) — an idle-triggered
/// run fires exactly once on a busy→idle *edge* and is dispatched as a detached
/// `tokio::spawn`. There is no repeating tick and no natural place to thread
/// the per-root `failing` dedup state through the detached task, so a single
/// plain (un-deduped) log line with root context is the correct, minimal fit
/// here. See #4376 for the design rationale.
fn log_outcome_for_root(role: &str, root: &Path, outcome: &RoleTickOutcome, elapsed: Duration) {
    match outcome {
        RoleTickOutcome::Success if tick_is_implausibly_fast(outcome, elapsed) => {
            log::warn!(
                "role_runner: {role} tick completed for {} in {elapsed:.1?} — implausibly fast \
                 for a real session (threshold {IMPLAUSIBLY_FAST_TICK:.0?}); this may be a no-op \
                 that exited 0 without doing real work (e.g. a slash-command prompt that did not \
                 resolve)",
                root.display()
            );
        }
        RoleTickOutcome::Success => {
            log::info!(
                "role_runner: {role} tick completed for {} in {elapsed:.1?}",
                root.display()
            );
        }
        RoleTickOutcome::Failure(reason) => log::warn!(
            "role_runner: {role} tick failed for {} after {elapsed:.1?} (logged and skipped, \
             never fatal): {reason}",
            root.display()
        ),
        RoleTickOutcome::RuntimeRejected(rejection) => log::warn!(
            "role_runner: {role} runtime admission rejected for {} after {elapsed:.1?}: {rejection}",
            root.display()
        ),
        RoleTickOutcome::NoTokenPool => log::warn!(
            "role_runner: {role} tick for {} skipped after {elapsed:.1?} — no token pool \
             available (neither a per-repo .loom/tokens/ pool nor a provisioned shared pool at \
             ~/.loom/tokens; run `loom-daemon tokens bootstrap` for a per-repo pool, or \
             `loom-daemon tokens bootstrap --shared` for the machine-level pool — see \
             .loom/docs/token-pool.md, #4642)",
            root.display()
        ),
        RoleTickOutcome::ModelRuntimeMismatch(mismatch) => log::warn!(
            "role_runner: {role} tick for {} skipped after {elapsed:.1?} — {} (#5028)",
            root.display(),
            mismatch.detail()
        ),
        RoleTickOutcome::LoadSkipped {
            load_per_core,
            detail,
        } => log::warn!(
            "role_runner: {role} tick for {} skipped after {elapsed:.1?}: skipped: host saturated \
             (load/core {load_per_core:.2}) at the tick ceiling — not counted as a failure \
             (#6637): {detail}",
            root.display()
        ),
    }
}

/// The classified log action for one root's tick outcome, given whether that
/// root was already failing on the *previous* tick. Pulled out of
/// [`log_outcome_for_root_deduped`] as a pure function so the state-change
/// dedup logic (#4349) is unit-testable without capturing `log` crate output
/// — mirrors why [`tick_is_implausibly_fast`] was extracted the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootTickLogAction {
    /// Steady-state success: log at `INFO`, same as always.
    Success,
    /// Success, but implausibly fast: log at `WARN`, same as always.
    SuccessImplausiblyFast,
    /// Success immediately after a failing period: log once at `INFO` with
    /// an explicit "recovered" message (the edge back to healthy).
    Recovered,
    /// Success immediately after a failing period, but implausibly fast:
    /// log once at `WARN` combining both signals.
    RecoveredImplausiblyFast,
    /// First failure (edge into a failing period): log at `WARN`, same as
    /// always.
    FailureEdge,
    /// Repeat failure (already failing on the previous tick): downgrade to
    /// `DEBUG` — the identical failure no longer re-logs at `WARN` every
    /// tick forever (the #4349 symptom: a broken worktree's MCP preflight
    /// failing every 5-minute champion/curator tick with ERROR-level noise).
    FailureRepeat,
    /// First tick with no available token pool (edge into this state, #4642):
    /// log at `WARN`. Distinct from [`Self::FailureEdge`] — a missing token
    /// pool is a permanent config state, not an invocation failure, and must
    /// never be tallied as one.
    NoTokenPoolEdge,
    /// Repeat tick with no available token pool (already warned, #4642):
    /// downgrade to `DEBUG`, mirroring [`Self::FailureRepeat`]'s dedup shape
    /// but tracked completely independently of the Failure/RuntimeRejected
    /// state.
    NoTokenPoolRepeat,
    /// First tick with a model/runtime mismatch (edge into this state, #5028):
    /// log at `WARN`. Distinct from [`Self::FailureEdge`] and
    /// [`Self::NoTokenPoolEdge`] — a provable model/runtime conflict is a
    /// permanent config state detected before any spawn, never tallied as an
    /// invocation failure.
    ModelMismatchEdge,
    /// Repeat tick with a model/runtime mismatch (already warned, #5028):
    /// downgrade to `DEBUG`, mirroring [`Self::FailureRepeat`] /
    /// [`Self::NoTokenPoolRepeat`]'s dedup shape but tracked completely
    /// independently of both.
    ModelMismatchRepeat,
    /// The tick ceiling fired under measured host saturation (issue #6637):
    /// log at `WARN`, distinct from [`Self::FailureEdge`] — this is host
    /// load, not an invocation defect, and must never be tallied as one.
    /// Deliberately **not** edge/repeat-deduped like the three states above:
    /// unlike `NoTokenPool`/`ModelRuntimeMismatch` (checked every tick before
    /// any spawn) this only fires after riding out the full
    /// [`DEFAULT_ROLE_TIMEOUT`] ceiling, so repeat-tick log spam is not a
    /// realistic concern.
    LoadSkipped,
}

impl RootTickLogAction {
    /// Whether this action should mark the root as failing for the *next*
    /// tick's edge/repeat decision.
    #[must_use]
    fn is_failing(self) -> bool {
        matches!(self, Self::FailureEdge | Self::FailureRepeat)
    }

    /// Whether this action should mark the root as no-token-pool for the
    /// *next* tick's edge/repeat decision (#4642) — tracked independently of
    /// [`Self::is_failing`] so the two conditions never bleed into each
    /// other's dedup state.
    #[must_use]
    fn is_no_token_pool(self) -> bool {
        matches!(self, Self::NoTokenPoolEdge | Self::NoTokenPoolRepeat)
    }

    /// Whether this action should mark the root as model-mismatched for the
    /// *next* tick's edge/repeat decision (#5028) — tracked independently of
    /// both [`Self::is_failing`] and [`Self::is_no_token_pool`] so none of the
    /// three axes bleed into each other's dedup state.
    #[must_use]
    fn is_model_mismatch(self) -> bool {
        matches!(self, Self::ModelMismatchEdge | Self::ModelMismatchRepeat)
    }
}

#[must_use]
fn classify_root_tick_log(
    outcome: &RoleTickOutcome,
    elapsed: Duration,
    was_failing: bool,
    was_no_token_pool: bool,
    was_model_mismatch: bool,
) -> RootTickLogAction {
    match outcome {
        RoleTickOutcome::NoTokenPool if was_no_token_pool => RootTickLogAction::NoTokenPoolRepeat,
        RoleTickOutcome::NoTokenPool => RootTickLogAction::NoTokenPoolEdge,
        RoleTickOutcome::ModelRuntimeMismatch(_) if was_model_mismatch => {
            RootTickLogAction::ModelMismatchRepeat
        }
        RoleTickOutcome::ModelRuntimeMismatch(_) => RootTickLogAction::ModelMismatchEdge,
        RoleTickOutcome::LoadSkipped { .. } => RootTickLogAction::LoadSkipped,
        RoleTickOutcome::Failure(_) | RoleTickOutcome::RuntimeRejected(_) if was_failing => {
            RootTickLogAction::FailureRepeat
        }
        RoleTickOutcome::Failure(_) | RoleTickOutcome::RuntimeRejected(_) => {
            RootTickLogAction::FailureEdge
        }
        RoleTickOutcome::Success if tick_is_implausibly_fast(outcome, elapsed) && was_failing => {
            RootTickLogAction::RecoveredImplausiblyFast
        }
        RoleTickOutcome::Success if tick_is_implausibly_fast(outcome, elapsed) => {
            RootTickLogAction::SuccessImplausiblyFast
        }
        RoleTickOutcome::Success if was_failing => RootTickLogAction::Recovered,
        RoleTickOutcome::Success => RootTickLogAction::Success,
    }
}

/// Root-aware, **state-change-deduped** variant of [`log_outcome`] for the
/// multi-workspace loop (#4349). `failing` tracks, per root, whether the
/// *previous* tick for that root ended in [`RoleTickOutcome::Failure`] (or
/// [`RoleTickOutcome::RuntimeRejected`]); `no_token_pool` tracks, per root and
/// completely independently, whether the previous tick ended in
/// [`RoleTickOutcome::NoTokenPool`] (#4642); `model_mismatch` tracks, per root
/// and completely independently of both, whether the previous tick ended in
/// [`RoleTickOutcome::ModelRuntimeMismatch`] (#5028) — see [`RootTickLogAction`]
/// for the per-transition logging rules.
#[allow(clippy::too_many_arguments)]
fn log_outcome_for_root_deduped(
    role: &str,
    root: &Path,
    outcome: &RoleTickOutcome,
    elapsed: Duration,
    failing: &mut HashMap<PathBuf, bool>,
    no_token_pool: &mut HashMap<PathBuf, bool>,
    model_mismatch: &mut HashMap<PathBuf, bool>,
) {
    // Record the raw outcome BEFORE the log-dedup decision (#4761): the
    // edge/repeat dedup exists to keep the *log* quiet, but a health check needs
    // every tick — a persistently-failing root logs at DEBUG after its first
    // WARN, which is exactly the case that must still surface as degraded.
    record_role_tick(role, root, outcome);
    let was_failing = failing.get(root).copied().unwrap_or(false);
    let was_no_token_pool = no_token_pool.get(root).copied().unwrap_or(false);
    let was_model_mismatch = model_mismatch.get(root).copied().unwrap_or(false);
    let action = classify_root_tick_log(
        outcome,
        elapsed,
        was_failing,
        was_no_token_pool,
        was_model_mismatch,
    );
    let reason = match outcome {
        RoleTickOutcome::Failure(reason) => reason.as_str(),
        RoleTickOutcome::RuntimeRejected(rejection) => rejection.reason.as_str(),
        RoleTickOutcome::Success | RoleTickOutcome::NoTokenPool => "",
        RoleTickOutcome::ModelRuntimeMismatch(_) => "",
        RoleTickOutcome::LoadSkipped { .. } => "",
    };
    match action {
        RootTickLogAction::Success => {
            log::info!(
                "role_runner: {role} tick completed for {} in {elapsed:.1?}",
                root.display()
            );
        }
        RootTickLogAction::SuccessImplausiblyFast => {
            log::warn!(
                "role_runner: {role} tick completed for {} in {elapsed:.1?} — implausibly fast \
                 for a real session (threshold {IMPLAUSIBLY_FAST_TICK:.0?}); this may be a no-op \
                 that exited 0 without doing real work (e.g. a slash-command prompt that did not \
                 resolve)",
                root.display()
            );
        }
        RootTickLogAction::Recovered => {
            log::info!(
                "role_runner: {role} recovered for {} — tick completed in {elapsed:.1?} after a \
                 prior failing period",
                root.display()
            );
        }
        RootTickLogAction::RecoveredImplausiblyFast => {
            log::warn!(
                "role_runner: {role} tick for {} recovered from a failing period but completed \
                 in {elapsed:.1?} — implausibly fast for a real session (threshold \
                 {IMPLAUSIBLY_FAST_TICK:.0?}); this may be a no-op that exited 0 without doing \
                 real work",
                root.display()
            );
        }
        RootTickLogAction::FailureEdge => {
            // Issue #6757 AC4: read AFTER `record_role_tick` above has
            // already folded in this (failing) tick — see
            // `had_ever_succeeded`'s doc comment for why that ordering is
            // safe.
            let history_note = failure_history_note(had_ever_succeeded(role, root));
            log::warn!(
                "role_runner: {role} tick failed for {} after {elapsed:.1?} ({history_note}; \
                 logged and skipped, never fatal; further identical failures for this root are \
                 logged at DEBUG until it recovers): {reason}",
                root.display()
            );
        }
        RootTickLogAction::FailureRepeat => {
            log::debug!(
                "role_runner: {role} tick failed for {} again after {elapsed:.1?} (repeat of an \
                 already-logged failure; not re-warned every tick — see the fail-edge WARN \
                 above, or the eventual recovery INFO): {reason}",
                root.display()
            );
        }
        RootTickLogAction::NoTokenPoolEdge => {
            log::warn!(
                "role_runner: {role} tick for {} skipped after {elapsed:.1?} — no token pool \
                 available (neither a per-repo .loom/tokens/ pool nor a provisioned shared pool \
                 at ~/.loom/tokens; run `loom-daemon tokens bootstrap` for a per-repo pool, or \
                 `loom-daemon tokens bootstrap --shared` for the machine-level pool — see \
                 .loom/docs/token-pool.md; further identical skips for this root are logged at \
                 DEBUG until a pool becomes available, #4642)",
                root.display()
            );
        }
        RootTickLogAction::NoTokenPoolRepeat => {
            log::debug!(
                "role_runner: {role} tick for {} skipped again after {elapsed:.1?} — no token \
                 pool available (repeat of an already-logged skip; not re-warned every tick — \
                 see the skip-edge WARN above, #4642)",
                root.display()
            );
        }
        RootTickLogAction::ModelMismatchEdge => {
            if let RoleTickOutcome::ModelRuntimeMismatch(mismatch) = outcome {
                log::warn!(
                    "role_runner: {role} tick for {} skipped after {elapsed:.1?} — {} (further \
                     identical skips for this root are logged at DEBUG until the config is \
                     corrected, #5028)",
                    root.display(),
                    mismatch.detail()
                );
            }
        }
        RootTickLogAction::ModelMismatchRepeat => {
            if let RoleTickOutcome::ModelRuntimeMismatch(mismatch) = outcome {
                log::debug!(
                    "role_runner: {role} tick for {} skipped again after {elapsed:.1?} — repeat \
                     of an already-logged model/runtime mismatch (see the mismatch-edge WARN \
                     above, #5028): {}",
                    root.display(),
                    mismatch.detail()
                );
            }
        }
        RootTickLogAction::LoadSkipped => {
            if let RoleTickOutcome::LoadSkipped {
                load_per_core,
                detail,
            } = outcome
            {
                log::warn!(
                    "role_runner: {role} tick for {} skipped after {elapsed:.1?}: skipped: host \
                     saturated (load/core {load_per_core:.2}) at the tick ceiling — not counted \
                     as a failure (#6637): {detail}",
                    root.display()
                );
            }
        }
    }
    failing.insert(root.to_path_buf(), action.is_failing());
    no_token_pool.insert(root.to_path_buf(), action.is_no_token_pool());
    model_mismatch.insert(root.to_path_buf(), action.is_model_mismatch());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;

    fn write_config(root: &Path, contents: &str) {
        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::write(root.join(".loom").join("config.json"), contents).unwrap();
    }

    // -- clean_and_cap_detail (#5024) ---------------------------------------

    #[test]
    fn clean_and_cap_detail_strips_ansi_and_trims() {
        let raw = "\x1b[31merror:\x1b[0m something failed\n";
        assert_eq!(clean_and_cap_detail(raw), "error: something failed");
    }

    #[test]
    fn clean_and_cap_detail_round_trips_short_clean_text_unchanged() {
        let raw = "exit code 1: connection refused";
        assert_eq!(clean_and_cap_detail(raw), raw);
    }

    #[test]
    fn clean_and_cap_detail_caps_oversized_text() {
        let raw = "x".repeat(MAX_FAILURE_DETAIL_CHARS * 4);
        let cleaned = clean_and_cap_detail(&raw);
        // Capped body + a short "… [truncated]" marker — bound generously
        // above MAX_FAILURE_DETAIL_CHARS so the assertion doesn't hardcode
        // the marker's exact byte/char width.
        assert!(
            cleaned.chars().count() <= MAX_FAILURE_DETAIL_CHARS + 32,
            "cleaned detail was not capped: {} chars",
            cleaned.chars().count()
        );
        assert!(cleaned.ends_with("[truncated]"));
    }

    #[test]
    fn clean_and_cap_detail_never_cuts_mid_token() {
        // Issue #6757 AC3: a byte-count cap must not slice through a word.
        // Build text whose exact-char-count cut point (MAX_FAILURE_DETAIL_CHARS)
        // lands in the middle of a distinctive token, and assert that token
        // never appears fragmented in the output.
        let filler = "word ".repeat(MAX_FAILURE_DETAIL_CHARS); // plenty over the cap
        let raw = format!("{filler}UNMISTAKABLE_TOKEN_BOUNDARY more text after");
        let cleaned = clean_and_cap_detail(&raw);
        assert!(
            !cleaned.contains("UNMISTAKABLE_TOKEN"),
            "token should have been cut before it started: {cleaned:?}"
        );
        assert!(cleaned.ends_with("… [truncated]"));
        // The retained body (before the truncation marker) must end on a
        // whole "word", never a fragment like "wor" or "wo".
        let body = cleaned
            .strip_suffix("… [truncated]")
            .expect("checked ends_with above");
        assert!(
            body.is_empty() || body.ends_with("word"),
            "cap did not land on a word boundary: {body:?}"
        );
    }

    // -- truncate_tail (#6757 AC3) -------------------------------------

    #[test]
    fn truncate_tail_round_trips_short_text_unchanged() {
        let raw = "short output, well under the cap";
        assert_eq!(truncate_tail(raw), raw);
    }

    #[test]
    fn truncate_tail_never_cuts_mid_token() {
        // Construct text where the raw byte-window start (len - MAX_OUTPUT_TAIL_BYTES)
        // lands inside a distinctive token, and assert the retained tail
        // never contains a fragment of it — only the whole token or nothing.
        let padding = "x".repeat(MAX_OUTPUT_TAIL_BYTES - 5);
        let raw = format!("{padding}resolved /some/very/long/path/to/loom-daemon via $PATH (mtime: 2026-01-01T00:00:00Z)");
        let tail = truncate_tail(&raw);
        assert!(
            !tail.contains("solved") && !tail.contains("esolved"),
            "tail must not contain a fragment of \"resolved\": {tail:?}"
        );
        // Either the whole word survived, or the cut landed past it entirely.
        if tail.contains("resolved") {
            assert!(
                tail.starts_with("resolved") || tail.split_whitespace().next() == Some("resolved")
            );
        }
    }

    #[test]
    fn truncate_tail_falls_back_to_byte_cut_for_a_single_giant_token() {
        // No whitespace anywhere in the oversized text — no word boundary
        // exists, so the pre-#6757 byte-cut behavior must still apply
        // rather than the result becoming empty.
        let raw = "x".repeat(MAX_OUTPUT_TAIL_BYTES * 3);
        let tail = truncate_tail(&raw);
        assert!(!tail.is_empty());
        assert!(tail.chars().all(|c| c == 'x'));
    }

    // -- find_preflight_sentinel / describe_role_failure (#6757 AC1/AC2) ---

    #[test]
    fn find_preflight_sentinel_detects_auth_failure() {
        let log = "some INFO noise\n# AUTH_PREFLIGHT_FAILED\nmore noise after\n";
        assert_eq!(find_preflight_sentinel(log), Some("# AUTH_PREFLIGHT_FAILED"));
    }

    #[test]
    fn find_preflight_sentinel_detects_mcp_failure() {
        let log = "some INFO noise\n# MCP_PREFLIGHT_FAILED\nmore noise after\n";
        assert_eq!(find_preflight_sentinel(log), Some("# MCP_PREFLIGHT_FAILED"));
    }

    #[test]
    fn find_preflight_sentinel_absent_returns_none() {
        let log = "just ordinary output, no sentinel here\n";
        assert_eq!(find_preflight_sentinel(log), None);
    }

    #[test]
    fn find_preflight_sentinel_found_even_outside_retained_tail_window() {
        // Reproduces the issue's exact scenario: the sentinel occurs early
        // in the log, followed by enough unrelated INFO noise to push it
        // outside the MAX_OUTPUT_TAIL_BYTES tail window that `tail_of_file`
        // alone would retain.
        let noise =
            "resolved /path/to/loom-daemon via $PATH (mtime: 2026-01-01T00:00:00Z)\n".repeat(100);
        let log = format!("# MCP_PREFLIGHT_FAILED\n{noise}");
        assert!(log.len() > MAX_OUTPUT_TAIL_BYTES);
        // The raw tail window alone no longer contains the sentinel...
        assert!(!truncate_tail(&log).contains("MCP_PREFLIGHT_FAILED"));
        // ...but full-file detection still finds it.
        assert_eq!(find_preflight_sentinel(&log), Some("# MCP_PREFLIGHT_FAILED"));
    }

    #[test]
    fn describe_role_failure_names_sentinel_and_log_path_when_present() {
        let log =
            "INFO: starting up\n# AUTH_PREFLIGHT_FAILED\nINFO: resolved something unrelated\n";
        let log_path = Path::new("/tmp/some-workspace/.loom/logs/role-champion.log");
        let detail = describe_role_failure(log, log_path);
        assert!(detail.contains("AUTH_PREFLIGHT_FAILED"), "{detail:?}");
        assert!(
            detail.contains("/tmp/some-workspace/.loom/logs/role-champion.log"),
            "{detail:?}"
        );
        // Must NOT be the raw trailing noise line.
        assert!(!detail.contains("resolved something unrelated"), "{detail:?}");
    }

    #[test]
    fn describe_role_failure_falls_back_to_tail_when_no_sentinel() {
        let log = "ordinary error: connection refused\n";
        let log_path = Path::new("/tmp/some-workspace/.loom/logs/role-judge.log");
        let detail = describe_role_failure(log, log_path);
        assert_eq!(detail, "ordinary error: connection refused");
    }

    // -- had_ever_succeeded / failure_history_note (#6757 AC4) --------------

    #[test]
    fn failure_history_note_distinguishes_never_succeeded_from_regressed() {
        assert_eq!(failure_history_note(false), "has never completed a successful tick");
        assert_eq!(
            failure_history_note(true),
            "regressed after previously completing at least one successful tick"
        );
    }

    #[test]
    #[serial(role_tick_ring)]
    fn had_ever_succeeded_false_for_a_pair_that_has_only_ever_failed() {
        let root = PathBuf::from("/tmp/loom-6757-never-succeeded");
        record_role_tick("champion", &root, &RoleTickOutcome::Failure("boom".into()));
        assert!(!had_ever_succeeded("champion", &root));
        record_role_tick("champion", &root, &RoleTickOutcome::Failure("boom again".into()));
        assert!(!had_ever_succeeded("champion", &root));
    }

    #[test]
    #[serial(role_tick_ring)]
    fn had_ever_succeeded_stays_true_after_a_regression() {
        let root = PathBuf::from("/tmp/loom-6757-regressed-after-success");
        record_role_tick("curator", &root, &RoleTickOutcome::Success);
        assert!(had_ever_succeeded("curator", &root));
        // A later failure must not clear the sticky flag.
        record_role_tick("curator", &root, &RoleTickOutcome::Failure("boom".into()));
        assert!(had_ever_succeeded("curator", &root));
    }

    #[test]
    #[serial(role_tick_ring)]
    fn had_ever_succeeded_is_independent_per_role_and_root() {
        let root_a = PathBuf::from("/tmp/loom-6757-independent-a");
        let root_b = PathBuf::from("/tmp/loom-6757-independent-b");
        record_role_tick("judge", &root_a, &RoleTickOutcome::Success);
        record_role_tick("doctor", &root_a, &RoleTickOutcome::Failure("boom".into()));
        record_role_tick("judge", &root_b, &RoleTickOutcome::Failure("boom".into()));

        assert!(had_ever_succeeded("judge", &root_a));
        assert!(!had_ever_succeeded("doctor", &root_a));
        assert!(!had_ever_succeeded("judge", &root_b));
    }

    /// RAII guard that clears the ambient `LOOM_RUNTIME` env var for the
    /// scope of a test and restores whatever value (if any) it previously
    /// had — including across a mid-test assertion panic, since Rust
    /// unwinds through `Drop`. Some host/dev-container shells export
    /// `LOOM_RUNTIME` (as the `spawn-worker.sh` runtime selector), and
    /// without this guard that ambient value silently outranks the
    /// `runtimes.roles` config precedence this test exercises (#4739).
    struct ClearedLoomRuntimeEnv(Option<String>);

    impl ClearedLoomRuntimeEnv {
        fn new() -> Self {
            let prior = std::env::var("LOOM_RUNTIME").ok();
            std::env::remove_var("LOOM_RUNTIME");
            Self(prior)
        }
    }

    impl Drop for ClearedLoomRuntimeEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("LOOM_RUNTIME", v),
                None => std::env::remove_var("LOOM_RUNTIME"),
            }
        }
    }

    /// As [`ClearedLoomRuntimeEnv`] but for `GH_CONFIG_DIR` (#5508): the test
    /// process may itself be running under a `GH_CONFIG_DIR` (a developer
    /// shell, or the daemon's own #4458 process-global default), which would
    /// otherwise leak into a spawned child's environment and make the
    /// "unregistered root leaves GH_CONFIG_DIR untouched" test observe an
    /// ambient value instead of a genuine absence.
    struct ClearedGhConfigDirEnv(Option<String>);

    impl ClearedGhConfigDirEnv {
        fn new() -> Self {
            let prior = std::env::var("GH_CONFIG_DIR").ok();
            std::env::remove_var("GH_CONFIG_DIR");
            Self(prior)
        }
    }

    impl Drop for ClearedGhConfigDirEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("GH_CONFIG_DIR", v),
                None => std::env::remove_var("GH_CONFIG_DIR"),
            }
        }
    }

    #[test]
    #[serial]
    fn mixed_runtime_role_launch_is_admitted_and_pinned_before_spawn() {
        use std::os::unix::fs::PermissionsExt;

        let _env_guard = ClearedLoomRuntimeEnv::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for sub in [
            ".loom/roles",
            ".loom/runtimes",
            ".loom/scripts",
            ".loom/tokens",
        ] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        // #4642: a per-repo token pool so the new pre-spawn token-pool check
        // does not short-circuit this test's runtime-admission scenario.
        fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
        // #5028: without a matching `roleModels.curator` override, curator
        // admitted onto `codex` would resolve the Claude-shaped default model
        // (`sonnet`) and now get refused as a `ModelRuntimeMismatch` BEFORE
        // this test's runtime-admission/pinning scenario ever reaches the
        // adapter — supplying the override keeps this test's scope on
        // admission/pinning, not the (separately tested) mismatch refusal.
        write_config(
            root,
            r#"{"runtimes":{"roles":{"curator":"codex"}},"autonomous":{"roleRunner":{"roleModels":{"curator":"gpt-5-codex"}}}}"#,
        );
        fs::write(root.join(".loom/roles/curator.json"), r#"{"runtimeRequirements":["mcp"]}"#)
            .unwrap();
        fs::write(
            root.join(".loom/runtimes/codex.json"),
            r#"{"runtime":"codex","capabilities":{"mcp":"yes","worktreeIsolation":"partial"}}"#,
        )
        .unwrap();
        let adapter = root.join(".loom/scripts/spawn-codex.sh");
        fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
        let observed = root.join("observed-runtime");
        let worker = root.join(".loom/scripts/spawn-worker.sh");
        fs::write(
            &worker,
            format!("#!/bin/sh\nprintf '%s' \"$LOOM_RUNTIME\" > '{}'\n", observed.display()),
        )
        .unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

        let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf())
            .with_timeout(Duration::from_secs(5));
        assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
        assert_eq!(fs::read_to_string(observed).unwrap(), "codex");
    }

    /// Issue #6507: regression test pinning the `LOOM_ROLE` env-var contract
    /// (documented in `defaults/docs/daemon-reference.md` § "The `LOOM_ROLE`
    /// contract") for `role_runner.rs`'s admission-success spawn path
    /// (`run_role_with_timeout`'s `cmd.env("LOOM_ROLE", ...)` call, mirroring
    /// `sweep_registry::spawn_child`'s #4768 fix).
    ///
    /// Every other fixture in this module reaches `invoke()` via
    /// `.with_spawn_bin(fake_script)`, which — per `invoke()`'s own
    /// `self.spawn_bin.is_none()` gate — ALSO disables runtime admission, so
    /// `admission` is always `None` there and the `LOOM_ROLE`-setting branch
    /// is never exercised. This test instead leaves `spawn_bin` unset (like
    /// `mixed_runtime_role_launch_is_admitted_and_pinned_before_spawn` above)
    /// so `resolve_spawn_bin()` falls through to the on-disk
    /// `.loom/scripts/spawn-worker.sh` fixture and admission runs for real,
    /// on the built-in `claude` runtime (no codex adapter/model-override
    /// fixture needed).
    ///
    /// Negative control (see this issue's Test Plan): commenting out the
    /// `cmd.env("LOOM_ROLE", &admission.role);` line in `run_role_with_timeout`
    /// makes this test fail — confirmed manually while authoring it (the
    /// child then inherits whatever `LOOM_ROLE` happens to be ambient in the
    /// *test process's own* environment, e.g. `sweep-lifecycle` when the test
    /// itself runs under a dispatched Loom sweep, rather than reading
    /// `"curator"` — either way, not the admitted role, so the assertion
    /// fails either way).
    #[test]
    #[serial]
    fn invoke_sets_loom_role_env_on_admitted_spawn() {
        use std::os::unix::fs::PermissionsExt;

        let _env_guard = ClearedLoomRuntimeEnv::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for sub in [
            ".loom/roles",
            ".loom/runtimes",
            ".loom/scripts",
            ".loom/tokens",
        ] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        // #4642: a per-repo token pool so the pre-spawn token-pool check does
        // not short-circuit before admission ever runs.
        fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
        fs::write(root.join(".loom/roles/curator.json"), r#"{"runtimeRequirements":[]}"#).unwrap();
        fs::write(
            root.join(".loom/runtimes/claude.json"),
            r#"{"runtime":"claude","capabilities":{}}"#,
        )
        .unwrap();
        // `resolve_and_admit` requires the chosen runtime's adapter script to
        // exist on disk even for the built-in `claude` runtime — it is never
        // invoked here (the recording `spawn-worker.sh` fixture below is what
        // actually runs), just checked for presence.
        let claude_adapter = root.join(".loom/scripts/spawn-claude.sh");
        fs::write(&claude_adapter, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&claude_adapter, fs::Permissions::from_mode(0o755)).unwrap();
        let observed = root.join("observed-role");
        let worker = root.join(".loom/scripts/spawn-worker.sh");
        fs::write(
            &worker,
            format!(
                "#!/bin/sh\nprintf '%s' \"${{LOOM_ROLE:-unset}}\" > '{}'\n",
                observed.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

        let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf())
            .with_timeout(Duration::from_secs(5));
        assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
        assert_eq!(
            fs::read_to_string(&observed).unwrap(),
            "curator",
            "role_runner's admission-success spawn must carry LOOM_ROLE (issue #6507)"
        );
    }

    /// A fake script that just exits with a fixed code, optionally writing to
    /// stdout/stderr first. Written with a shebang so it's directly
    /// executable — mirrors `token_ranking_refresh`'s test helper.
    fn write_fake_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    // ===================================================================
    // ScriptRoleInvocationRunner — resolution + execution
    // ===================================================================

    #[test]
    fn test_resolve_spawn_bin_missing_is_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf());
        let outcome = runner.invoke("curator", "/curator");
        assert!(!outcome.is_success());
        let RoleTickOutcome::Failure(reason) = outcome else {
            panic!("expected Failure");
        };
        assert!(reason.contains("spawn-worker.sh not found"), "{reason}");
    }

    /// #4642: a workspace with a resolvable `spawn-worker.sh` but NO token
    /// pool (neither per-repo nor shared) must short-circuit to
    /// `NoTokenPool` — proving the pre-spawn check fires *before*
    /// `run_role_with_timeout` ever runs the script — by asserting a marker
    /// file the script would write is never created.
    #[test]
    #[serial(loom_shared_tokens_dir_env)]
    fn test_invoke_short_circuits_with_no_token_pool_before_running_the_script() {
        use std::os::unix::fs::PermissionsExt;

        // Force a deterministic "no shared pool" resolution regardless of a
        // real `~/.loom/tokens` on the machine running this test.
        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".loom/scripts")).unwrap();
        // A real, resolvable spawn-worker.sh that proves whether it ran by
        // writing a marker file.
        let marker = root.join("script-ran");
        let worker = root.join(".loom/scripts/spawn-worker.sh");
        fs::write(&worker, format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display())).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

        let before = no_token_pool_skip_count();
        let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf());
        let outcome = runner.invoke("curator", "/loom:curator");

        assert_eq!(outcome, RoleTickOutcome::NoTokenPool);
        assert!(!outcome.is_success());
        assert!(!marker.exists(), "the doomed script must never actually run");
        assert_eq!(no_token_pool_skip_count(), before + 1);

        match prev_shared {
            Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
            None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
        }
    }

    /// #4642: the SAME workspace with a per-repo `.loom/tokens/` pool
    /// populated proceeds past the check and actually runs the script —
    /// proving the gate re-checks live state rather than caching a verdict.
    #[test]
    #[serial(loom_shared_tokens_dir_env)]
    fn test_invoke_proceeds_once_a_per_repo_token_pool_exists() {
        use std::os::unix::fs::PermissionsExt;

        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".loom/scripts")).unwrap();
        fs::create_dir_all(root.join(".loom/tokens")).unwrap();
        fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
        let worker = root.join(".loom/scripts/spawn-worker.sh");
        fs::write(&worker, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

        let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf());
        let outcome = runner.invoke("curator", "/loom:curator");
        // Not asserting `Success` specifically: with no
        // `.loom/roles`/`.loom/runtimes` manifests in this minimal fixture,
        // the runtime-admission step below the token check is expected to
        // reject the (unconfigured) default runtime — the point of this test
        // is only that the token-pool gate itself let the tick past, i.e.
        // the outcome is never `NoTokenPool` once a pool exists.
        assert_ne!(outcome, RoleTickOutcome::NoTokenPool);

        match prev_shared {
            Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
            None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
        }
    }

    /// Shared fixture for the #5028 end-to-end mismatch tests: a workspace
    /// admitted onto the `codex` runtime for `judge`, with a real per-repo
    /// token pool (so the #4642 preflight does not short-circuit first) and a
    /// fake `spawn-worker.sh` (the actual script `resolve_spawn_bin` resolves
    /// and `invoke` runs — mirrors `mixed_runtime_role_launch_is_admitted_and_pinned_before_spawn`)
    /// that writes a marker file if it is ever actually invoked — proving a
    /// refused launch never reaches the spawn.
    fn setup_codex_judge_fixture(root: &Path, config_extra: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        for sub in [
            ".loom/roles",
            ".loom/runtimes",
            ".loom/scripts",
            ".loom/tokens",
        ] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
        write_config(
            root,
            &format!(r#"{{"runtimes":{{"roles":{{"judge":"codex"}}}}{}}}"#, config_extra),
        );
        fs::write(root.join(".loom/roles/judge.json"), r#"{"runtimeRequirements":[]}"#).unwrap();
        fs::write(
            root.join(".loom/runtimes/codex.json"),
            r#"{"runtime":"codex","capabilities":{}}"#,
        )
        .unwrap();
        // Admission (`resolve_and_admit`) validates that the `codex` adapter
        // file exists on disk before admitting the runtime at all — it is
        // never actually exec'd in this fixture (that's `spawn-worker.sh`
        // below), but its mere absence would itself refuse the launch with a
        // `RuntimeRejected`, which is not what these tests are exercising.
        let adapter = root.join(".loom/scripts/spawn-codex.sh");
        fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();

        let marker = root.join("spawn-ran");
        let worker = root.join(".loom/scripts/spawn-worker.sh");
        fs::write(&worker, format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display())).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();
        marker
    }

    /// Issue #5028 (#5001 AC2/AC3): `runtimes.roles.judge = "codex"` with NO
    /// `autonomous.roleRunner.roleModels.judge` override resolves the
    /// Claude-shaped default model (`sonnet`) for a role admitted onto Codex —
    /// a provable, doomed launch. `invoke` must refuse it as
    /// `ModelRuntimeMismatch` BEFORE the spawn, never create the adapter's
    /// marker file, and increment the dedicated skip counter — never a bare
    /// `Failure`/`RuntimeRejected`.
    #[test]
    #[serial]
    fn test_invoke_refuses_a_provable_model_runtime_mismatch_before_spawning() {
        let _env_guard = ClearedLoomRuntimeEnv::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let marker = setup_codex_judge_fixture(root, "");

        let before = model_runtime_mismatch_skip_count();
        let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf())
            .with_timeout(Duration::from_secs(5));
        let outcome = runner.invoke("judge", "/loom:judge");

        let RoleTickOutcome::ModelRuntimeMismatch(mismatch) = outcome else {
            panic!("expected ModelRuntimeMismatch, got {outcome:?}");
        };
        assert_eq!(mismatch.role, "judge");
        assert_eq!(mismatch.runtime, "codex");
        assert_eq!(mismatch.model, "sonnet", "the unfixed Claude-shaped default");
        assert!(!marker.exists(), "a doomed launch must never actually spawn the adapter");
        assert_eq!(model_runtime_mismatch_skip_count(), before + 1);
    }

    /// Issue #5028: the SAME fixture with `roleModels.judge` pointed at a
    /// Codex-valid model spawns successfully — proving the check is a
    /// targeted refusal, not a blanket block on Judge-on-Codex, and that it
    /// self-heals the moment the config is corrected (no restart needed).
    #[test]
    #[serial]
    fn test_invoke_succeeds_once_role_models_supplies_a_matching_model() {
        let _env_guard = ClearedLoomRuntimeEnv::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let marker = setup_codex_judge_fixture(
            root,
            r#","autonomous":{"roleRunner":{"roleModels":{"judge":"gpt-5-codex"}}}"#,
        );

        let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf())
            .with_timeout(Duration::from_secs(5));
        let outcome = runner.invoke("judge", "/loom:judge");

        assert_eq!(outcome, RoleTickOutcome::Success);
        assert!(marker.exists(), "a matching model must let the launch actually spawn");
    }

    #[test]
    fn test_invoke_success_on_zero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo ok; exit 0");
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
    }

    #[test]
    fn test_invoke_failure_on_nonzero_exit_includes_output_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo boom detail; exit 1");
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        let outcome = runner.invoke("curator", "/curator");
        let RoleTickOutcome::Failure(reason) = outcome else {
            panic!("expected Failure");
        };
        assert!(reason.contains("boom detail"), "{reason}");
    }

    /// Issue #6757 (end-to-end): a real invocation whose stderr carries a
    /// pre-flight sentinel followed by unrelated trailing noise must surface
    /// the sentinel — and the role's own log path — in the `Failure` reason,
    /// not the arbitrary trailing noise line.
    #[test]
    fn test_invoke_failure_names_preflight_sentinel_not_trailing_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "echo '# MCP_PREFLIGHT_FAILED' >&2; echo 'resolved /some/path via \\$PATH (mtime: \
             2026-01-01)' >&2; exit 1",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        let outcome = runner.invoke("curator", "/curator");
        let RoleTickOutcome::Failure(reason) = outcome else {
            panic!("expected Failure");
        };
        assert!(reason.contains("MCP_PREFLIGHT_FAILED"), "{reason}");
        assert!(reason.contains("role-curator.log"), "{reason}");
        assert!(!reason.contains("resolved /some/path"), "{reason}");
    }

    #[test]
    fn test_invoke_receives_prompt_and_skip_permissions_flag() {
        let tmp = tempfile::tempdir().unwrap();
        // Fail unless invoked with
        //   -p "/curator" --model <m> --dangerously-skip-permissions
        // (the `--model` pin was inserted after the prompt by #4501, mirroring
        // `sweep_registry::spawn_child`'s argv order).
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "[ \"$1\" = \"-p\" ] && [ \"$2\" = \"/curator\" ] && [ \"$3\" = \"--model\" ] && [ -n \"$4\" ] && [ \"$5\" = \"--dangerously-skip-permissions\" ] && exit 0 || exit 1",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
    }

    // -- #5508: per-owner GH_CONFIG_DIR forwarded to role-runner children --

    /// A role-runner child spawned for a workspace registered under a
    /// non-default owner (mirrors a `2AMLogic/*` managed repo, #5401/#5431)
    /// must carry that owner's `GH_CONFIG_DIR` — otherwise it inherits the
    /// daemon's own installation token (scoped to the root owner only) and
    /// every forge call the spawned Champion/Judge/etc. session makes 404s,
    /// exactly the live incident #5508 reported.
    #[test]
    #[serial]
    fn run_role_with_timeout_forwards_owner_gh_config_dir_for_a_registered_root() {
        crate::credential_preflight::clear_owner_root_registry();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let owner_dir = root.join(".loom/gh-config-by-owner/2AMLogic");
        crate::credential_preflight::register_root_gh_config_dir(&root, &owner_dir);

        let observed = root.join("observed-gh-config-dir");
        let script = write_fake_script(
            &root,
            "fake-spawn.sh",
            &format!("printf '%s' \"$GH_CONFIG_DIR\" > '{}'", observed.display()),
        );
        let mut runner = ScriptRoleInvocationRunner::new(root.clone()).with_spawn_bin(script);
        assert_eq!(runner.invoke("champion", "/loom:champion"), RoleTickOutcome::Success);
        assert_eq!(
            fs::read_to_string(&observed).unwrap(),
            owner_dir.to_string_lossy(),
            "a registered root's role child must carry the owner's GH_CONFIG_DIR"
        );

        crate::credential_preflight::clear_owner_root_registry();
    }

    /// The flip side: a workspace that is NOT registered under a non-default
    /// owner (the common single-owner fleet, or the root owner's own repos)
    /// must be a byte-identical no-op — the child's `GH_CONFIG_DIR` is left
    /// untouched so it inherits the daemon's own process-global default.
    #[test]
    #[serial]
    fn run_role_with_timeout_leaves_gh_config_dir_untouched_for_an_unregistered_root() {
        let _env_guard = ClearedGhConfigDirEnv::new();
        crate::credential_preflight::clear_owner_root_registry();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        let observed = root.join("observed-gh-config-dir");
        let script = write_fake_script(
            &root,
            "fake-spawn.sh",
            &format!("printf '%s' \"${{GH_CONFIG_DIR:-__unset__}}\" > '{}'", observed.display()),
        );
        let mut runner = ScriptRoleInvocationRunner::new(root.clone()).with_spawn_bin(script);
        assert_eq!(runner.invoke("champion", "/loom:champion"), RoleTickOutcome::Success);
        assert_eq!(fs::read_to_string(&observed).unwrap(), "__unset__");
    }

    /// Issue #4501: a role spawn pins the model explicitly — a role child must
    /// never inherit the account's interactive CLI default (`fable` on the host
    /// that filed the issue, where every child instantly died on "You've reached
    /// your Fable 5 limit"). With no config the pin is the shipped
    /// `DEFAULT_DISPATCH_MODEL` (`sonnet`).
    #[test]
    fn test_invoke_appends_resolved_model_defaulting_to_sonnet() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "printf '%s\\n' \"$@\" > argv.txt; exit 0",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
        let argv = fs::read_to_string(tmp.path().join("argv.txt")).unwrap();
        let args: Vec<&str> = argv.lines().collect();
        let idx = args
            .iter()
            .position(|a| *a == "--model")
            .expect("role spawn argv must contain --model");
        assert_eq!(
            args[idx + 1],
            sweep_registry::DEFAULT_DISPATCH_MODEL,
            "default role-runner model must be the shipped dispatch default; argv: {args:?}"
        );
        assert_ne!(args[idx + 1], "fable", "role children must never run fable by default");
    }

    /// Issue #4501: `autonomous.roleRunner.model` wins over the shipped default
    /// (and over `autonomous.model`) — the explicit-request tier of the shared
    /// `resolve_dispatch_model` chain.
    #[test]
    fn test_invoke_config_model_override_wins() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"model": "opus", "roleRunner": {"enabled": true, "model": "claude-sonnet-4-6"}}}"#,
        );
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "printf '%s\\n' \"$@\" > argv.txt; exit 0",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
        let argv = fs::read_to_string(tmp.path().join("argv.txt")).unwrap();
        assert!(
            argv.contains("--model\nclaude-sonnet-4-6\n"),
            "autonomous.roleRunner.model must win; argv: {argv}"
        );
    }

    /// Issue #5001: end-to-end, a `roleModels.<role>` override reaches the actual
    /// `--model` argv for that role while a peer role (no override) still gets the
    /// global `autonomous.roleRunner.model`. This is the argv-level proof of the
    /// mixed-runtime fix: the Codex-bound Judge pins a Codex-valid model while the
    /// Claude-bound Curator keeps the Claude alias — from one config block.
    #[test]
    fn test_invoke_per_role_model_override_reaches_argv() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {
                "enabled": true,
                "model": "sonnet",
                "roleModels": {"judge": "gpt-5-codex"}
            }}}"#,
        );
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "printf '%s\\n' \"$@\" > argv-last.txt; exit 0",
        );

        // Judge gets its per-role Codex model.
        let mut judge = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
            .with_spawn_bin(script.clone());
        assert_eq!(judge.invoke("judge", "/loom:judge"), RoleTickOutcome::Success);
        let judge_argv = fs::read_to_string(tmp.path().join("argv-last.txt")).unwrap();
        assert!(
            judge_argv.contains("--model\ngpt-5-codex\n"),
            "judge must pin its per-role model; argv: {judge_argv}"
        );

        // Curator (no override) still gets the global roleRunner.model.
        let mut curator =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(curator.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
        let curator_argv = fs::read_to_string(tmp.path().join("argv-last.txt")).unwrap();
        assert!(
            curator_argv.contains("--model\nsonnet\n"),
            "curator must keep the global roleRunner.model; argv: {curator_argv}"
        );
    }

    /// Issue #4501: with only `autonomous.model` set, the role runner joins the
    /// SAME chain sweep dispatch uses rather than keeping a private default.
    //
    // NOTE: see the comment above `test_config_missing_file_is_default` —
    // `resolve_role_runner_model` reads `read_role_runner_config` internally
    // (and this test also calls it directly for the `blank` case), so it needs
    // the same private-defaults-tier guard + `#[serial(loom_config_env)]`
    // (#4593, discovered during review of #4590 / #4538).
    #[test]
    #[serial(loom_config_env)]
    fn test_resolve_role_runner_model_precedence_chain() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

        // No config at all -> shipped default, labelled `default`.
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_role_runner_model(bare.path(), "curator"),
            (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default".to_string())
        );

        // `autonomous.model` only -> that value, labelled `autonomous.model`.
        // Routing through `resolve_dispatch_model` also means the role runner
        // inherits the #3982 logical-tier alias resolution for free
        // (`opus` -> `claude-opus-5`), exactly as sweep dispatch does.
        let shared = tempfile::tempdir().unwrap();
        write_config(shared.path(), r#"{"autonomous": {"model": "opus"}}"#);
        assert_eq!(
            resolve_role_runner_model(shared.path(), "curator"),
            ("claude-opus-5".to_string(), "autonomous.model".to_string())
        );

        // Both -> the role-runner-specific value, labelled as such.
        let both = tempfile::tempdir().unwrap();
        write_config(
            both.path(),
            r#"{"autonomous": {"model": "opus", "roleRunner": {"model": "haiku"}}}"#,
        );
        assert_eq!(
            resolve_role_runner_model(both.path(), "curator"),
            ("haiku".to_string(), "autonomous.roleRunner.model".to_string())
        );

        // A blank override is treated as unset at every tier (never `--model ""`).
        let blank = tempfile::tempdir().unwrap();
        write_config(blank.path(), r#"{"autonomous": {"roleRunner": {"model": "   "}}}"#);
        assert_eq!(read_role_runner_config(blank.path()).model, None);
        assert_eq!(
            resolve_role_runner_model(blank.path(), "curator"),
            (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default".to_string())
        );

        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    }

    /// Issue #5001: `autonomous.roleRunner.roleModels.<role>` is a tier ABOVE the
    /// global `autonomous.roleRunner.model` — a repo can point one role (Judge,
    /// on Codex) at a provider-valid model while the other roles
    /// (Curator/Champion, on Claude) keep a Claude alias, all from config. This
    /// is the config-only fix for the `LOOM_RUNTIME_JUDGE=codex` -> `sonnet` 400
    /// incident: the per-role and global model axes can finally disagree.
    #[test]
    #[serial(loom_config_env)]
    fn test_resolve_role_runner_model_per_role_override() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

        // Judge gets a Codex-valid model; curator/champion keep the global
        // Claude alias — the exact mixed-runtime shape the incident needed.
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"{"autonomous": {"roleRunner": {
                "model": "sonnet",
                "roleModels": {"judge": "gpt-5-codex"}
            }}}"#,
        );
        assert_eq!(
            resolve_role_runner_model(dir.path(), "judge"),
            ("gpt-5-codex".to_string(), "autonomous.roleRunner.roleModels.judge".to_string())
        );
        // A role with no per-role entry falls through to the global tier.
        assert_eq!(
            resolve_role_runner_model(dir.path(), "curator"),
            ("sonnet".to_string(), "autonomous.roleRunner.model".to_string())
        );
        assert_eq!(
            resolve_role_runner_model(dir.path(), "champion"),
            ("sonnet".to_string(), "autonomous.roleRunner.model".to_string())
        );

        // Per-role override with NO global model set: the overridden role uses
        // its override; every other role falls all the way through to the
        // shipped default (not the override).
        let no_global = tempfile::tempdir().unwrap();
        write_config(
            no_global.path(),
            r#"{"autonomous": {"roleRunner": {"roleModels": {"judge": "gpt-5-codex"}}}}"#,
        );
        assert_eq!(
            resolve_role_runner_model(no_global.path(), "judge"),
            ("gpt-5-codex".to_string(), "autonomous.roleRunner.roleModels.judge".to_string())
        );
        assert_eq!(
            resolve_role_runner_model(no_global.path(), "guide"),
            (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default".to_string())
        );

        // The lookup is case-insensitive: a `Judge` config key matches the
        // lower-cased `judge` role name the runner dispatches under.
        let cased = tempfile::tempdir().unwrap();
        write_config(
            cased.path(),
            r#"{"autonomous": {"roleRunner": {"roleModels": {"Judge": "gpt-5-codex"}}}}"#,
        );
        assert_eq!(resolve_role_runner_model(cased.path(), "judge").0, "gpt-5-codex".to_string());

        // A per-role override that is a logical Claude alias still resolves
        // through the #3982 tier map (`opus` -> `claude-opus-5`), exactly like
        // the other tiers.
        let alias = tempfile::tempdir().unwrap();
        write_config(
            alias.path(),
            r#"{"autonomous": {"roleRunner": {"roleModels": {"judge": "opus"}}}}"#,
        );
        assert_eq!(resolve_role_runner_model(alias.path(), "judge").0, "claude-opus-5");

        // A blank per-role value is dropped at parse time and falls through to
        // the global tier — never `--model ""`.
        let blank = tempfile::tempdir().unwrap();
        write_config(
            blank.path(),
            r#"{"autonomous": {"roleRunner": {"model": "sonnet", "roleModels": {"judge": "   "}}}}"#,
        );
        assert!(read_role_runner_config(blank.path()).role_models.is_empty());
        assert_eq!(
            resolve_role_runner_model(blank.path(), "judge"),
            ("sonnet".to_string(), "autonomous.roleRunner.model".to_string())
        );

        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    }

    /// Issue #5001: `read_role_runner_config` soft-fails a malformed / absent /
    /// non-object `roleModels` to an empty map (every role falls through to the
    /// global chain), and drops blank keys — mirroring the soft-fail contract of
    /// every other `autonomous.roleRunner.*` field.
    #[test]
    #[serial(loom_config_env)]
    fn test_read_role_models_soft_fails_and_normalizes() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

        // Absent key -> empty map.
        let absent = tempfile::tempdir().unwrap();
        write_config(absent.path(), r#"{"autonomous": {"roleRunner": {"enabled": true}}}"#);
        assert!(read_role_runner_config(absent.path())
            .role_models
            .is_empty());

        // Non-object value -> empty map (no panic).
        let non_object = tempfile::tempdir().unwrap();
        write_config(
            non_object.path(),
            r#"{"autonomous": {"roleRunner": {"roleModels": "sonnet"}}}"#,
        );
        assert!(read_role_runner_config(non_object.path())
            .role_models
            .is_empty());

        // Blank keys and blank/non-string values are dropped; good entries are
        // kept, lower-cased, and trimmed.
        let mixed = tempfile::tempdir().unwrap();
        write_config(
            mixed.path(),
            r#"{"autonomous": {"roleRunner": {"roleModels": {
                "  Judge  ": "  gpt-5-codex  ",
                "curator": "",
                "   ": "sonnet",
                "guide": 42
            }}}}"#,
        );
        let models = read_role_runner_config(mixed.path()).role_models;
        assert_eq!(models.get("judge").map(String::as_str), Some("gpt-5-codex"));
        assert_eq!(models.len(), 1, "blank/non-string entries must be dropped: {models:?}");

        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    }

    /// Issue #4501: the per-role log header records the pinned model and the tier
    /// that supplied it, so an operator can verify the pin from
    /// `role-<role>.log` alone on a live host.
    #[test]
    fn test_invoke_log_header_records_pinned_model() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "exit 0");
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("guide", "/loom:guide"), RoleTickOutcome::Success);
        let log = fs::read_to_string(tmp.path().join(".loom").join("logs").join("role-guide.log"))
            .unwrap();
        assert!(
            log.contains(&format!(
                "model={} (source=default)",
                sweep_registry::DEFAULT_DISPATCH_MODEL
            )),
            "{log}"
        );
    }

    /// Issue #4255: a scheduled role spawn routes through `claude-wrapper.sh` by
    /// appending `--use-wrapper` after `--dangerously-skip-permissions`, so a
    /// transient API death is retried instead of killing the unattended role run
    /// on the first failure. Serialized on a named lock shared with the opt-out
    /// test so the `LOOM_USE_WRAPPER` env mutation cannot race it.
    #[test]
    #[serial(loom_use_wrapper_env)]
    fn test_invoke_appends_use_wrapper_flag() {
        std::env::remove_var("LOOM_USE_WRAPPER");
        let tmp = tempfile::tempdir().unwrap();
        // Succeeds only when --use-wrapper directly follows
        // --dangerously-skip-permissions (argv is now
        // `-p <prompt> --model <m> --dangerously-skip-permissions --use-wrapper`
        // since the #4501 model pin).
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "[ \"$5\" = \"--dangerously-skip-permissions\" ] && [ \"$6\" = \"--use-wrapper\" ] && exit 0 || exit 1",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
    }

    /// Issue #4255: the `LOOM_USE_WRAPPER=0` debug opt-out restores the legacy
    /// single-shot argv — argv ends at `--dangerously-skip-permissions` with no
    /// `--use-wrapper` token.
    #[test]
    #[serial(loom_use_wrapper_env)]
    fn test_invoke_opt_out_omits_use_wrapper_flag() {
        std::env::set_var("LOOM_USE_WRAPPER", "0");
        let tmp = tempfile::tempdir().unwrap();
        // Succeeds only when nothing follows --dangerously-skip-permissions
        // (argv ends there; the #4501 model pin shifted it to $5).
        let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "[ \"$5\" = \"--dangerously-skip-permissions\" ] && [ -z \"$6\" ] && exit 0 || exit 1",
        );
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        let outcome = runner.invoke("curator", "/curator");
        std::env::remove_var("LOOM_USE_WRAPPER");
        assert_eq!(outcome, RoleTickOutcome::Success);
    }

    #[test]
    fn test_invoke_writes_per_role_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo hello-from-role; exit 0");
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
        assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
        let log_path = tmp
            .path()
            .join(".loom")
            .join("logs")
            .join("role-curator.log");
        let contents = fs::read_to_string(log_path).unwrap();
        assert!(contents.contains("hello-from-role"), "{contents}");
    }

    #[test]
    fn test_invoke_times_out_on_hung_script() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30");
        let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
            .with_spawn_bin(script)
            .with_timeout(Duration::from_millis(300))
            // Issue #7242: pin load-per-core low so this test deterministically
            // exercises the plain-timeout `Failure` path, independent of the
            // real host's load at test time (which would otherwise route
            // through `LoadSkipped` per issue #6637's saturation check).
            .with_load_per_core_override(0.0);
        let outcome = runner.invoke("curator", "/curator");
        let RoleTickOutcome::Failure(reason) = outcome else {
            panic!("expected Failure");
        };
        assert!(reason.contains("timed out"), "{reason}");
    }

    /// Issue #6637 AC4: a fake `spawn-worker.sh` that sleeps past the tick
    /// ceiling, under an injected high load-per-core, must produce a
    /// [`RoleTickOutcome::LoadSkipped`] — never the bare unscaled `Failure` a
    /// timeout normally records. This is the exact scenario from the
    /// incident that filed the issue: the auditor's 1800s ceiling firing on
    /// a host that was simultaneously running sweeps, not a broken role.
    #[test]
    #[serial(load_skipped_count)]
    fn test_invoke_times_out_under_high_load_is_load_skipped_not_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30; echo done");
        let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
            .with_spawn_bin(script)
            .with_timeout(Duration::from_millis(300))
            .with_load_per_core_override(3.5);

        let outcome = runner.invoke("auditor", "/loom:auditor");

        let RoleTickOutcome::LoadSkipped {
            load_per_core,
            detail: _,
        } = outcome
        else {
            panic!("expected LoadSkipped, got {outcome:?}");
        };
        assert!((load_per_core - 3.5).abs() < f64::EPSILON, "{load_per_core}");
    }

    /// Counterpart to the above: the SAME hung-script/ceiling scenario, but
    /// with load-per-core measured BELOW the saturation threshold, must
    /// still classify as an ordinary `Failure` — the load-skip path must
    /// never fire on an unloaded host (issue #6637's fail-safe requirement).
    #[test]
    fn test_invoke_times_out_under_low_load_is_still_a_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30");
        let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
            .with_spawn_bin(script)
            .with_timeout(Duration::from_millis(300))
            .with_load_per_core_override(0.2);

        let outcome = runner.invoke("auditor", "/loom:auditor");

        let RoleTickOutcome::Failure(reason) = outcome else {
            panic!("expected Failure, got {outcome:?}");
        };
        assert!(reason.contains("timed out"), "{reason}");
    }

    /// Issue #6637: the `LoadSkipped` outcome must increment its own
    /// counter, and must NOT be tallied under any of the pre-existing
    /// skip/failure counters — mirrors the equivalent `NoTokenPool`/
    /// `ModelRuntimeMismatch` counter-isolation tests below.
    #[test]
    #[serial(load_skipped_count)]
    fn test_load_skipped_count_increments_on_load_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30");
        let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
            .with_spawn_bin(script)
            .with_timeout(Duration::from_millis(300))
            .with_load_per_core_override(2.0);

        let before = load_skipped_count();
        let outcome = runner.invoke("auditor", "/loom:auditor");
        assert!(matches!(outcome, RoleTickOutcome::LoadSkipped { .. }), "{outcome:?}");
        assert_eq!(load_skipped_count(), before + 1);
    }

    #[test]
    fn test_invoke_spawn_failure_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("does-not-exist.sh");
        let mut runner =
            ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(bogus);
        let outcome = runner.invoke("curator", "/curator");
        assert!(!outcome.is_success());
    }

    // ===================================================================
    // Config surface — autonomous.roleRunner
    // ===================================================================

    // NOTE: these tests read `read_role_runner_config`, which merges the
    // private-defaults tier (`config_resolver::private_defaults_path()`) ahead
    // of the tempdir-scoped config under test. That tier resolves off
    // `$LOOM_CONFIG_DEFAULTS_FILE` / `$HOME` — independent of `tmp.path()` — so
    // a host's real `~/.local/share/loom/config/defaults.json` can leak into
    // the result. Neutralize it for the duration of each test (#4538), and use
    // the same named serial group (`loom_config_env`) as the other tests below
    // that mutate this exact env var — a bare `#[serial]` would not serialize
    // against it, since `serial_test` locks are per-key.
    #[test]
    #[serial(loom_config_env)]
    fn test_config_missing_file_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, RoleRunnerConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_malformed_json_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, RoleRunnerConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_missing_block_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, RoleRunnerConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_reads_enabled_roles_and_interval() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {"enabled": true, "roles": ["curator", "guide"], "intervalSecs": 120}}}"#,
        );
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(
            cfg,
            RoleRunnerConfig {
                enabled: Some(true),
                roles: Some(vec!["curator".to_string(), "guide".to_string()]),
                interval_secs: Some(120),
                on_idle: None,
                model: None,
                role_models: BTreeMap::new(),
                architect_max_proposals: None,
                max_concurrent: None,
            }
        );
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_zero_interval_is_dropped_to_none() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"intervalSecs": 0}}}"#);
        let interval_secs = read_role_runner_config(tmp.path()).interval_secs;
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(interval_secs, None);
    }

    // ===================================================================
    // config_resolver migration (#4058) — tier precedence
    // ===================================================================

    fn write_project_config(root: &Path, contents: &str) {
        let full = root.join(crate::config_resolver::PROJECT_CONFIG_REL);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, contents).unwrap();
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_project_tier_only_is_honored_like_legacy() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_project_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {"enabled": true, "roles": ["curator"], "intervalSecs": 60}}}"#,
        );
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(
            cfg,
            RoleRunnerConfig {
                enabled: Some(true),
                roles: Some(vec!["curator".to_string()]),
                interval_secs: Some(60),
                on_idle: None,
                model: None,
                role_models: BTreeMap::new(),
                architect_max_proposals: None,
                max_concurrent: None,
            }
        );
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_project_tier_overrides_legacy_overlap_and_supplies_non_overlap() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {"enabled": true, "intervalSecs": 120}}}"#,
        );
        write_project_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"intervalSecs": 30}}}"#);
        let cfg = read_role_runner_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        // Overlapping `intervalSecs` -> project tier wins.
        assert_eq!(cfg.interval_secs, Some(30));
        // Non-overlapping `enabled` still supplied by legacy tier.
        assert_eq!(cfg.enabled, Some(true));
    }

    // ===================================================================
    // resolve_roles
    // ===================================================================

    #[test]
    fn test_resolve_roles_absent_is_the_interval_default_subset() {
        // Pre-#5656 this asserted `DEFAULT_ROLES.to_vec()`. It is now the
        // *interval-default subset* — every entry except the
        // idle-addressable-only ones (`architect`) — because putting
        // `architect` in DEFAULT_ROLES (the prerequisite for `onIdle` to
        // resolve it at all) must not make every repo that never pins
        // `roles` run a proposal generator on a timer.
        assert_eq!(resolve_roles(&RoleRunnerConfig::default()), interval_default_roles());
        // Every interval-default role is still present, unchanged.
        assert_eq!(
            resolve_roles(&RoleRunnerConfig::default())
                .iter()
                .map(|r| r.name)
                .collect::<Vec<_>>(),
            vec!["champion", "curator", "judge", "doctor", "auditor", "hermit", "guide"]
        );
    }

    #[test]
    fn test_resolve_roles_empty_array_is_none() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: Some(vec![]),
            interval_secs: None,
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        };
        assert_eq!(resolve_roles(&config), Vec::new());
    }

    #[test]
    fn test_resolve_roles_filters_and_preserves_default_order() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: Some(vec!["guide".to_string(), "champion".to_string()]),
            interval_secs: None,
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        };
        let roles = resolve_roles(&config);
        assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["champion", "guide"]);
    }

    #[test]
    fn test_resolve_roles_ignores_unknown_names() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: Some(vec!["curator".to_string(), "not-a-role".to_string()]),
            interval_secs: None,
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        };
        let roles = resolve_roles(&config);
        assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["curator"]);
    }

    // ===================================================================
    // missing_defaults (#5339) — the pinned-allowlist-goes-stale warning
    // ===================================================================

    #[test]
    fn test_missing_defaults_warns_for_default_absent_from_pinned_list() {
        // A pinned `roles: ["curator"]` predates `doctor` joining
        // DEFAULT_ROLES (#5272/#5291) — every other default is silently
        // missing from the resolved set too, but this asserts the specific
        // regression from the issue.
        let names = vec!["curator".to_string()];
        let missing = missing_defaults(&names);
        assert!(missing.contains(&"doctor"), "expected \"doctor\" in {missing:?}");
        // And resolve_roles's actual output omits it, per the allowlist semantics.
        let config = RoleRunnerConfig {
            enabled: None,
            roles: Some(names),
            interval_secs: None,
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        };
        let resolved = resolve_roles(&config);
        assert!(!resolved.iter().any(|r| r.name == "doctor"));
    }

    #[test]
    fn test_missing_defaults_empty_list_is_deliberate_opt_out_no_warning() {
        // An explicit `"roles": []` means "run none" — not staleness — so it
        // must not be reported as missing anything.
        assert_eq!(missing_defaults(&[]), Vec::<&str>::new());
    }

    #[test]
    fn test_missing_defaults_empty_when_list_covers_every_default() {
        let names: Vec<String> = DEFAULT_ROLES.iter().map(|s| s.name.to_string()).collect();
        assert_eq!(missing_defaults(&names), Vec::<&str>::new());
    }

    #[test]
    fn test_resolve_roles_unknown_name_and_missing_default_fire_independently() {
        // A list with both an unknown name (already-handled case) and a
        // missing DEFAULT_ROLES entry (#5339) must trigger both warning
        // paths in the same call without either suppressing the other —
        // asserted here via each function's independent, testable output.
        let config = RoleRunnerConfig {
            enabled: None,
            roles: Some(vec!["curator".to_string(), "not-a-role".to_string()]),
            interval_secs: None,
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        };
        let roles = resolve_roles(&config);
        assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["curator"]);
        let names = vec!["curator".to_string(), "not-a-role".to_string()];
        assert!(missing_defaults(&names).contains(&"doctor"));
    }

    // ===================================================================
    // default_roles_snapshot_id / roles_source_label / resolved_roles_log_line
    // (#5654 — per-repo, per-tick diagnosability for the resolved role list)
    // ===================================================================

    #[test]
    fn test_default_roles_snapshot_id_is_stable_and_content_derived() {
        let id = default_roles_snapshot_id();
        // Count prefix matches the live DEFAULT_ROLES length.
        assert!(id.starts_with(&format!("{}:", DEFAULT_ROLES.len())), "{id}");
        // Every current default role name appears in the identifier, in
        // DEFAULT_ROLES order — so a reader can tell exactly which roster a
        // log line was evaluated against without cross-referencing source.
        for spec in DEFAULT_ROLES {
            assert!(id.contains(spec.name), "expected {:?} in snapshot id {id:?}", spec.name);
        }
        assert!(id.contains("doctor"), "doctor must be part of the snapshot id: {id}");
        // Deterministic across calls (pure function of the static list).
        assert_eq!(id, default_roles_snapshot_id());
    }

    #[test]
    fn test_missing_defaults_warning_embeds_snapshot_id_via_resolve_roles() {
        // missing_defaults_warning_line's warning text is not directly
        // capturable here (it goes through the `log` crate, and the
        // `log::warn!` call site itself lives in `spawn_multi_role_task`'s
        // tick loop, not in `resolve_roles`, since #6163), but the snapshot
        // id it embeds is the same pure `default_roles_snapshot_id()` this
        // test can assert independently — see
        // `test_missing_defaults_warning_line_names_workspace_and_snapshot`
        // below for a direct assertion against the built line's content.
        let id = default_roles_snapshot_id();
        assert!(id.contains("doctor"), "snapshot id must name doctor: {id}");
    }

    // ===================================================================
    // missing_defaults_uncovered_by_on_idle / missing_defaults_warning_line
    // (#6163) — workspace-naming, onIdle-awareness, and the aggregated line
    // the multi-workspace tick loop now dedups on a per-resolved-config-change
    // basis instead of warning on every tick.
    // ===================================================================

    #[test]
    fn test_missing_defaults_uncovered_by_on_idle_excludes_on_idle_covered_roles() {
        // Mirrors this repo's own live config (#6163's motivating example):
        // roles pins curator/champion/judge/doctor/guide, onIdle covers
        // auditor. Only hermit is genuinely uncovered by either path.
        let names: Vec<String> = ["curator", "champion", "judge", "doctor", "guide"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let on_idle: Vec<String> = vec!["auditor".to_string()];
        let missing = missing_defaults_uncovered_by_on_idle(&names, &on_idle);
        assert_eq!(
            missing,
            vec!["hermit"],
            "auditor is onIdle-covered, must not appear: {missing:?}"
        );
    }

    #[test]
    fn test_missing_defaults_uncovered_by_on_idle_no_on_idle_reports_everything_missing() {
        let names = vec!["curator".to_string()];
        let missing = missing_defaults_uncovered_by_on_idle(&names, &[]);
        assert_eq!(missing, missing_defaults(&names), "empty onIdle must filter nothing");
    }

    #[test]
    fn test_missing_defaults_uncovered_by_on_idle_all_missing_covered_by_on_idle_is_empty() {
        let names = vec!["curator".to_string()];
        let on_idle: Vec<String> = missing_defaults(&names)
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            missing_defaults_uncovered_by_on_idle(&names, &on_idle),
            Vec::<&str>::new(),
            "every missing default is onIdle-covered — nothing left to warn about"
        );
    }

    #[test]
    fn test_exactly_one_default_role_is_the_missing_defaults_reporter() {
        // AC3/AC4: the diagnostic is a property of the workspace, not of a
        // role, so exactly one of the spawned DEFAULT_ROLES loops may emit it
        // — otherwise every workspace's line is repeated once per loop.
        let reporters: Vec<&str> = DEFAULT_ROLES
            .iter()
            .filter(|spec| is_missing_defaults_reporter(spec))
            .map(|spec| spec.name)
            .collect();
        assert_eq!(
            reporters.len(),
            1,
            "exactly one DEFAULT_ROLES loop may report missing defaults: {reporters:?}"
        );
    }

    #[test]
    fn test_missing_defaults_warning_line_is_none_when_nothing_missing() {
        assert_eq!(missing_defaults_warning_line(Path::new("/repo"), &[]), None);
    }

    #[test]
    fn test_missing_defaults_warning_line_names_workspace_and_snapshot() {
        // AC1: names the workspace. AC4: one aggregated line for multiple
        // missing roles (not one `log::warn!` call per role).
        let root = Path::new("/Users/example/repo");
        let line = missing_defaults_warning_line(root, &["auditor", "hermit"]).unwrap();
        assert!(line.contains("/Users/example/repo"), "expected workspace path in line: {line}");
        assert!(line.contains("auditor"), "{line}");
        assert!(line.contains("hermit"), "{line}");
        assert!(line.contains(&default_roles_snapshot_id()), "expected snapshot id: {line}");
        assert!(
            line.contains("2 of"),
            "expected an aggregated count, not one line per role: {line}"
        );
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_roles_source_label_absent_key_is_default() {
        // Neutralize the private/shared defaults tier (#4538 pattern above):
        // a real host can have `~/.local/share/loom/config/defaults.json`
        // set `autonomous.roleRunner.roles`, which would otherwise leak into
        // this "no tier sets it" assertion.
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        // No .loom/config.json at all — no tier sets `roles`.
        let label = roles_source_label(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(label, "default (no tier sets roles)");
    }

    #[test]
    fn test_roles_source_label_names_the_legacy_tier() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {"roles": ["curator", "champion", "judge", "doctor", "guide"]}}}"#,
        );
        let label = roles_source_label(tmp.path());
        assert!(label.starts_with("legacy ("), "{label}");
        assert!(label.contains(".loom/config.json"), "{label}");
    }

    #[test]
    fn test_roles_source_label_names_the_project_tier_over_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"roles": ["curator"]}}}"#);
        write_project_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {"roles": ["curator", "judge"]}}}"#,
        );
        let label = roles_source_label(tmp.path());
        assert!(label.starts_with("project ("), "expected project tier to win: {label}");
    }

    #[test]
    fn test_missing_defaults_for_loom_repos_own_pinned_list_names_only_auditor_and_hermit() {
        // The exact pinned list from this repo's own `.loom/config.json`
        // (`["curator","champion","judge","doctor","guide"]`) — missing only
        // auditor and hermit, not doctor, per the issue's own "Verified
        // corrections" trace.
        let names: Vec<String> = ["curator", "champion", "judge", "doctor", "guide"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let missing = missing_defaults(&names);
        assert_eq!(missing, vec!["auditor", "hermit"]);
        assert!(
            !missing.contains(&"doctor"),
            "doctor is present in this pinned list: {missing:?}"
        );
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_resolved_roles_log_line_reports_full_default_roles_with_default_source() {
        // Mirrors the Test Plan's first manual-verification case: a repo
        // config with `roleRunner: {}` (roles: None) resolves to the full
        // DEFAULT_ROLES list, including doctor, sourced from the default.
        // Neutralize the private/shared defaults tier — see the
        // `#4538`-pattern comment on `test_config_missing_file_is_default`
        // above — since a real host's own defaults file could otherwise
        // supply a `roles` value under this empty `roleRunner: {}` overlay.
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {}}}"#);
        let config = read_role_runner_config(tmp.path());
        let resolved = resolve_roles(&config);
        let line = resolved_roles_log_line(tmp.path(), &resolved);
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert!(line.contains("doctor"), "expected doctor in resolved roles line: {line}");
        assert!(
            line.contains("source=default (no tier sets roles)"),
            "expected default source label: {line}"
        );
        assert!(
            line.contains(&default_roles_snapshot_id()),
            "expected the DEFAULT_ROLES snapshot id embedded: {line}"
        );
        for spec in DEFAULT_ROLES {
            assert!(line.contains(spec.name), "expected {:?} in line: {line}", spec.name);
        }
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_resolved_roles_log_line_reports_pinned_list_with_its_source() {
        // Mirrors the Test Plan's second manual-verification case: a repo
        // pinning a non-empty roles list (matching the `loom` repo's own
        // config) reports exactly that list with the legacy-tier source.
        // `roles` is explicitly set here, so this does not depend on the
        // private/shared defaults tier — env neutralization is only for
        // parallel-test isolation against the other tests in this
        // `loom_config_env` serial group.
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {"roles": ["curator", "champion", "judge", "doctor", "guide"]}}}"#,
        );
        let config = read_role_runner_config(tmp.path());
        let resolved = resolve_roles(&config);
        let line = resolved_roles_log_line(tmp.path(), &resolved);
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        let names: Vec<&str> = resolved.iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["champion", "curator", "judge", "doctor", "guide"]);
        // The resolved-list portion of the line (before `source=`) must omit
        // auditor/hermit — they're correctly absent from *this* pinned
        // list's resolution. (The trailing `default_roles=` segment of the
        // line intentionally names every DEFAULT_ROLES entry regardless —
        // that's the whole-roster snapshot identifier, not the resolved
        // subset, so it is not asserted against here.)
        let resolved_segment = line.split("source=").next().unwrap();
        assert!(
            !resolved_segment.contains("auditor"),
            "auditor must be absent from the resolved-roles segment: {resolved_segment}"
        );
        assert!(
            !resolved_segment.contains("hermit"),
            "hermit must be absent from the resolved-roles segment: {resolved_segment}"
        );
        assert!(line.contains("doctor"), "{line}");
        assert!(line.starts_with(&format!("role_runner: {} resolved roles=", tmp.path().display())));
        assert!(line.contains("source=legacy ("), "expected legacy-tier source: {line}");
    }

    // ===================================================================
    // Precedence — env > config > default
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_enabled_default_is_false() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        assert!(!resolve_enabled(&RoleRunnerConfig::default()));
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_config_can_enable() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        assert!(resolve_enabled(&RoleRunnerConfig {
            enabled: Some(true),
            roles: None,
            interval_secs: None,
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        }));
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_env_overrides_config() {
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&RoleRunnerConfig {
            enabled: Some(true),
            roles: None,
            interval_secs: None,
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        }));
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "1");
        assert!(resolve_enabled(&RoleRunnerConfig {
            enabled: Some(false),
            roles: None,
            interval_secs: None,
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        }));
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    }

    // ===================================================================
    // #6469 — the disabled branch must log at INFO and name its source
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_enabled_with_source_names_default_when_nothing_set() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let (enabled, source) = resolve_enabled_with_source(&RoleRunnerConfig::default());
        assert!(!enabled);
        assert_eq!(source, EnabledSource::Default);
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_with_source_names_config_when_env_unset() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let cfg = RoleRunnerConfig {
            enabled: Some(false),
            ..RoleRunnerConfig::default()
        };
        let (enabled, source) = resolve_enabled_with_source(&cfg);
        assert!(!enabled);
        assert_eq!(source, EnabledSource::Config);
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_with_source_names_env_even_over_config() {
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
        let cfg = RoleRunnerConfig {
            enabled: Some(true),
            ..RoleRunnerConfig::default()
        };
        let (enabled, source) = resolve_enabled_with_source(&cfg);
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        assert!(!enabled);
        assert_eq!(source, EnabledSource::Env);
    }

    // ===================================================================
    // #6470 — `host_env_override()` resolution table (config-independent)
    // ===================================================================

    #[test]
    #[serial]
    fn test_host_env_override_none_when_unset() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        assert_eq!(host_env_override(), None);
    }

    #[test]
    #[serial]
    fn test_host_env_override_some_true_for_every_truthy_spelling() {
        for v in ["1", "true", "TRUE", "yes", "on", " on "] {
            std::env::set_var(ROLE_RUNNER_ENABLE_ENV, v);
            assert_eq!(host_env_override(), Some(true), "{v:?} must resolve truthy");
        }
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_host_env_override_some_false_for_every_falsy_spelling() {
        // Any *set* value that isn't one of the truthy spellings above is
        // falsy — including a value that isn't "0"/"false" at all, mirroring
        // `resolve_enabled_with_source`'s own precedence rule (env decides
        // regardless of the exact non-truthy spelling).
        for v in ["0", "false", "no", "off", "garbage", ""] {
            std::env::set_var(ROLE_RUNNER_ENABLE_ENV, v);
            assert_eq!(host_env_override(), Some(false), "{v:?} must resolve falsy");
        }
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_host_env_override_agrees_with_resolve_enabled_with_source_env_branch() {
        // `host_env_override()` must never disagree with the config-aware
        // resolver's own `EnabledSource::Env` branch — it is a
        // config-independent read of the same precedence rule, not a
        // second, potentially-drifting implementation of it.
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
        let cfg_true = RoleRunnerConfig {
            enabled: Some(true),
            ..RoleRunnerConfig::default()
        };
        let (enabled, source) = resolve_enabled_with_source(&cfg_true);
        assert_eq!(source, EnabledSource::Env);
        assert_eq!(host_env_override(), Some(enabled));
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    }

    /// AC1/AC2 (env case): the disabled-branch line names `env:LOOM_ROLE_RUNNER=<value>`
    /// as the source and states the "no role loops … any registered root" scope.
    #[test]
    #[serial]
    fn test_disabled_role_runner_log_line_names_env_source() {
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
        let tmp = tempfile::tempdir().unwrap();
        let line = disabled_role_runner_log_line(tmp.path(), &RoleRunnerConfig::default());
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        assert!(line.contains("source=env:LOOM_ROLE_RUNNER=\"0\""), "unexpected line: {line}");
        assert!(
            line.contains("no role loops will run on this host for any registered root"),
            "unexpected line: {line}"
        );
    }

    /// AC1/AC2 (config case): the disabled-branch line names the config tier
    /// that resolved `autonomous.roleRunner.enabled` to `false`, not just "config".
    #[test]
    #[serial]
    fn test_disabled_role_runner_log_line_names_config_source() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let tmp = tempfile::tempdir().unwrap();
        write_project_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"enabled": false}}}"#);
        let cfg = RoleRunnerConfig {
            enabled: Some(false),
            ..RoleRunnerConfig::default()
        };
        let line = disabled_role_runner_log_line(tmp.path(), &cfg);
        assert!(
            line.starts_with(
                "role_runner: disabled source=config:autonomous.roleRunner.enabled=false from "
            ),
            "unexpected line: {line}"
        );
        assert!(
            line.contains("no role loops will run on this host for any registered root"),
            "unexpected line: {line}"
        );
    }

    /// AC1 (default case, no source set at all): still names its tier
    /// explicitly rather than silently reading as "config" when nothing set it.
    #[test]
    #[serial]
    fn test_disabled_role_runner_log_line_names_default_source() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let line = disabled_role_runner_log_line(tmp.path(), &RoleRunnerConfig::default());
        assert!(
            line.contains("source=default (no tier sets autonomous.roleRunner.enabled)"),
            "unexpected line: {line}"
        );
    }

    /// AC3: the disabled branch's actual log emission (not just the string
    /// content) must land at `Level::Info`, not `Level::Debug` — this is the
    /// regression #6469 was filed against (a fleet running at INFO saw zero
    /// trace that role loops were off). Exercises `log_role_runner_disabled`,
    /// the exact function `daemon_service.rs`'s boot sequence calls.
    #[test]
    #[serial]
    fn test_log_role_runner_disabled_emits_at_info_for_env_source() {
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "false");
        let tmp = tempfile::tempdir().unwrap();
        let records = crate::test_log_capture::capture_logs(|| {
            log_role_runner_disabled(tmp.path(), &RoleRunnerConfig::default());
        });
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);

        let disabled_lines: Vec<_> = records
            .iter()
            .filter(|(_, msg)| msg.starts_with("role_runner: disabled"))
            .collect();
        assert_eq!(disabled_lines.len(), 1, "expected exactly one line, got {records:?}");
        let (level, msg) = disabled_lines[0];
        assert_eq!(*level, log::Level::Info, "disabled branch must log at INFO, not debug: {msg}");
        assert!(msg.contains("source=env:LOOM_ROLE_RUNNER=\"false\""), "unexpected line: {msg}");
    }

    /// AC3 (config case): same INFO-level assertion, but with the source
    /// resolved from config rather than env.
    #[test]
    #[serial]
    fn test_log_role_runner_disabled_emits_at_info_for_config_source() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let cfg = RoleRunnerConfig {
            enabled: Some(false),
            ..RoleRunnerConfig::default()
        };
        let records = crate::test_log_capture::capture_logs(|| {
            log_role_runner_disabled(tmp.path(), &cfg);
        });

        let disabled_lines: Vec<_> = records
            .iter()
            .filter(|(_, msg)| msg.starts_with("role_runner: disabled"))
            .collect();
        assert_eq!(disabled_lines.len(), 1, "expected exactly one line, got {records:?}");
        let (level, msg) = disabled_lines[0];
        assert_eq!(*level, log::Level::Info, "disabled branch must log at INFO, not debug: {msg}");
        assert!(
            msg.contains("source=config:autonomous.roleRunner.enabled=false"),
            "unexpected line: {msg}"
        );
    }

    #[test]
    #[serial]
    fn test_resolve_interval_for_role_precedence() {
        std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
        let spec = DEFAULT_ROLES[0];

        // Absent config + unset env => the role's own built-in default.
        assert_eq!(
            resolve_interval_for_role(&spec, &RoleRunnerConfig::default()),
            Duration::from_secs(spec.default_interval_secs)
        );

        // Config sets a uniform override.
        assert_eq!(
            resolve_interval_for_role(
                &spec,
                &RoleRunnerConfig {
                    enabled: None,
                    roles: None,
                    interval_secs: Some(42),
                    on_idle: None,
                    model: None,
                    role_models: BTreeMap::new(),
                    architect_max_proposals: None,
                    max_concurrent: None,
                }
            ),
            Duration::from_secs(42)
        );

        // Env overrides config.
        std::env::set_var(ROLE_RUNNER_INTERVAL_ENV, "7");
        assert_eq!(
            resolve_interval_for_role(
                &spec,
                &RoleRunnerConfig {
                    enabled: None,
                    roles: None,
                    interval_secs: Some(42),
                    on_idle: None,
                    model: None,
                    role_models: BTreeMap::new(),
                    architect_max_proposals: None,
                    max_concurrent: None,
                }
            ),
            Duration::from_secs(7)
        );
        std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
    }

    // -- per-role built-in intervals + source attribution (#6204) -----------

    /// The shipped built-ins are **per-role**, not a uniform value — the claim
    /// `daemon-reference.md`'s config table makes ("per-role built-in
    /// (5–15 min)"). #6204 was filed after every role logged an identical
    /// interval; this pins the documented shape so a future uniform-collapse
    /// (or a table that drifts from the code) fails here instead of in a
    /// fleet's throughput.
    #[test]
    #[serial]
    fn test_builtin_intervals_are_per_role_not_uniform() {
        std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
        let cfg = RoleRunnerConfig::default();
        let resolved: Vec<(&str, u64)> = DEFAULT_ROLES
            .iter()
            .map(|spec| (spec.name, resolve_interval_for_role(spec, &cfg).as_secs()))
            .collect();

        assert_eq!(
            resolved,
            vec![
                ("champion", 600),
                ("curator", 300),
                ("judge", 300),
                ("doctor", 300),
                ("auditor", 600),
                ("hermit", 600),
                ("guide", 900),
                ("architect", 3600),
            ],
            "built-in per-role intervals drifted — update defaults/docs/daemon-reference.md's \
             role-runner table in the same change (#6204)"
        );

        // Every interval-cadence default role sits inside the documented
        // 5–15 minute band (architect is idle-addressable-only, #5656, and is
        // deliberately the slow outlier).
        for spec in DEFAULT_ROLES.iter().filter(|s| s.is_interval_default()) {
            let secs = resolve_interval_for_role(spec, &cfg).as_secs();
            assert!(
                (300..=900).contains(&secs),
                "{} built-in interval {secs}s is outside the documented 5–15 min band",
                spec.name
            );
        }

        // …and they are genuinely diverse: the reported #6204 symptom was one
        // value for all eight roles.
        let distinct: std::collections::BTreeSet<u64> = resolved.iter().map(|(_, s)| *s).collect();
        assert!(
            distinct.len() > 1,
            "built-in intervals collapsed to a uniform value: {distinct:?}"
        );
    }

    #[test]
    #[serial]
    fn test_resolved_interval_log_line_names_builtin_source() {
        std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let champion = DEFAULT_ROLES.iter().find(|s| s.name == "champion").unwrap();
        let line = resolved_interval_log_line(tmp.path(), champion, &RoleRunnerConfig::default());
        assert_eq!(
            line,
            "role_runner: champion interval=600s source=built-in (RoleSpec::default_interval_secs)"
        );
    }

    #[test]
    #[serial]
    fn test_resolved_interval_log_line_names_config_source() {
        std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let champion = DEFAULT_ROLES.iter().find(|s| s.name == "champion").unwrap();
        let cfg = RoleRunnerConfig {
            interval_secs: Some(1800),
            ..RoleRunnerConfig::default()
        };
        let line = resolved_interval_log_line(tmp.path(), champion, &cfg);
        assert!(
            line.starts_with(
                "role_runner: champion interval=1800s \
                 source=config:autonomous.roleRunner.intervalSecs from "
            ),
            "unexpected line: {line}"
        );
        // The overridden per-role built-in is named, so "every role shows the
        // same interval" is self-diagnosing from one line.
        assert!(
            line.contains("(uniform override; per-role built-in 600s not used)"),
            "unexpected line: {line}"
        );
    }

    #[test]
    #[serial]
    fn test_resolved_interval_log_line_names_env_source() {
        std::env::set_var(ROLE_RUNNER_INTERVAL_ENV, "1800");
        let tmp = tempfile::tempdir().unwrap();
        let champion = DEFAULT_ROLES.iter().find(|s| s.name == "champion").unwrap();
        // Env wins even over a config value, and says so.
        let cfg = RoleRunnerConfig {
            interval_secs: Some(42),
            ..RoleRunnerConfig::default()
        };
        let line = resolved_interval_log_line(tmp.path(), champion, &cfg);
        std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
        assert_eq!(
            line,
            "role_runner: champion interval=1800s \
             source=env:LOOM_ROLE_RUNNER_INTERVAL_SECS (uniform override; per-role built-in 600s \
             not used)"
        );
    }

    #[test]
    #[serial]
    fn test_resolve_interval_for_role_with_source_tiers() {
        std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
        let spec = DEFAULT_ROLES[0];

        let (d, source) =
            resolve_interval_for_role_with_source(&spec, &RoleRunnerConfig::default());
        assert_eq!(d, Duration::from_secs(spec.default_interval_secs));
        assert_eq!(source, IntervalSource::BuiltIn);
        assert!(!source.is_uniform_override());

        let cfg = RoleRunnerConfig {
            interval_secs: Some(42),
            ..RoleRunnerConfig::default()
        };
        let (d, source) = resolve_interval_for_role_with_source(&spec, &cfg);
        assert_eq!(d, Duration::from_secs(42));
        assert_eq!(source, IntervalSource::Config);
        assert!(source.is_uniform_override());

        std::env::set_var(ROLE_RUNNER_INTERVAL_ENV, "7");
        let (d, source) = resolve_interval_for_role_with_source(&spec, &cfg);
        std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
        assert_eq!(d, Duration::from_secs(7));
        assert_eq!(source, IntervalSource::Env);
        assert!(source.is_uniform_override());
    }

    // ===================================================================
    // Loop wiring — a scripted fake runner proves ticks + panics behave
    // ===================================================================

    struct FakeRunner {
        outcomes: Vec<RoleTickOutcome>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl RoleInvocationRunner for FakeRunner {
        fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.outcomes.get(n).cloned().unwrap_or_else(|| {
                self.outcomes
                    .last()
                    .cloned()
                    .unwrap_or(RoleTickOutcome::Success)
            })
        }
    }

    async fn wait_for_calls(
        calls: &std::sync::atomic::AtomicUsize,
        target: usize,
        timeout: Duration,
    ) {
        let deadline = Instant::now() + timeout;
        loop {
            if calls.load(std::sync::atomic::Ordering::SeqCst) >= target {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for call count to reach {target} (saw {})",
                calls.load(std::sync::atomic::Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn test_loop_ticks_repeatedly_skipping_first_tick() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runner = FakeRunner {
            outcomes: vec![RoleTickOutcome::Success; 3],
            calls: calls.clone(),
        };
        let spec = RoleSpec {
            name: "curator",
            prompt: "/loom:curator",
            default_interval_secs: 1,
            interval_default: true,
        };
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = spawn_role_task(
            runner,
            spec,
            Duration::from_millis(20),
            drain,
            PathBuf::from("/tmp/loom-test-root"),
            new_in_progress_guard(),
        );

        wait_for_calls(&calls, 1, Duration::from_secs(2)).await;
        wait_for_calls(&calls, 3, Duration::from_secs(2)).await;

        handle.abort();
    }

    /// **#6201 AC2, stated affirmatively**: a tick that ends in
    /// [`RoleTickOutcome::Failure`] is followed by another invocation on the
    /// role's very next interval — no backoff, no benching, no persistent
    /// "this role failed once" state gating any future tick.
    ///
    /// The incident report for #6201 read the nine-day curator silence as
    /// "permanent silent benching after a RECOVERABLE failure". Investigating
    /// it (see [`note_pre_spawn_skip`]) showed the loop never benched anything
    /// — but nothing in this module actually *proved* that, so the claim could
    /// not be checked against the code either way. This test is that proof,
    /// and it is what would fail if someone later added a
    /// failure-count-gated skip.
    #[tokio::test]
    async fn failure_outcome_is_retried_on_the_very_next_tick_never_benched() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runner = FakeRunner {
            // Every tick fails, forever (the `unwrap_or_else(last)` fallback in
            // `FakeRunner::invoke` repeats the final entry).
            outcomes: vec![RoleTickOutcome::Failure("codex 400 (RECOVERABLE)".into())],
            calls: calls.clone(),
        };
        let spec = RoleSpec {
            name: "curator",
            prompt: "/loom:curator",
            default_interval_secs: 1,
            interval_default: true,
        };
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = spawn_role_task(
            runner,
            spec,
            Duration::from_millis(20),
            drain,
            PathBuf::from("/tmp/loom-test-root-6201-ac2"),
            new_in_progress_guard(),
        );

        // Four consecutive failing ticks still produce four invocations: the
        // first failure does not suppress the second, third, or fourth.
        wait_for_calls(&calls, 4, Duration::from_secs(5)).await;

        handle.abort();
    }

    /// **#6201 AC4**: a role failing on a broken runtime recovers
    /// automatically once the runtime works again — no daemon restart, no
    /// operator un-benching step, no manual re-enable. Drives the real tick
    /// loop through the incident's own shape (several consecutive failures,
    /// then the underlying condition is fixed) and asserts the very next tick
    /// succeeds.
    #[tokio::test]
    async fn role_recovers_automatically_once_the_broken_runtime_works_again() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let outcomes = vec![
            RoleTickOutcome::Failure("codex: model not supported (RECOVERABLE)".into()),
            RoleTickOutcome::Failure("codex: model not supported (RECOVERABLE)".into()),
            RoleTickOutcome::Failure("codex: model not supported (RECOVERABLE)".into()),
            // The operator corrects the runtime/model config here; the loop
            // has taken no action of its own to make this reachable.
            RoleTickOutcome::Success,
        ];
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        struct RecordingFake {
            outcomes: Vec<RoleTickOutcome>,
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            observed: std::sync::Arc<std::sync::Mutex<Vec<RoleTickOutcome>>>,
        }
        impl RoleInvocationRunner for RecordingFake {
            fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
                let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let outcome = self
                    .outcomes
                    .get(n)
                    .cloned()
                    .unwrap_or(RoleTickOutcome::Success);
                self.observed.lock().unwrap().push(outcome.clone());
                outcome
            }
        }

        let spec = RoleSpec {
            name: "curator",
            prompt: "/loom:curator",
            default_interval_secs: 1,
            interval_default: true,
        };
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = spawn_role_task(
            RecordingFake {
                outcomes,
                calls: calls.clone(),
                observed: observed.clone(),
            },
            spec,
            Duration::from_millis(20),
            drain,
            PathBuf::from("/tmp/loom-test-root-6201-ac4"),
            new_in_progress_guard(),
        );

        wait_for_calls(&calls, 4, Duration::from_secs(5)).await;
        handle.abort();

        let seen = observed.lock().unwrap().clone();
        assert!(
            seen.len() >= 4,
            "expected at least 4 ticks through the failing period and out the other side, saw {}",
            seen.len()
        );
        assert!(
            seen[..3].iter().all(|o| !o.is_success()),
            "fixture precondition: the first three ticks must be the failing period"
        );
        assert!(
            seen[3].is_success(),
            "the tick immediately after the underlying condition was fixed must succeed — the \
             loop must not have benched the role during the failing period (#6201 AC4)"
        );
    }

    /// **#6201, the confirmed mechanism**: every pre-spawn preflight bail-out
    /// leaves a dated line in the role's OWN log
    /// (`.loom/logs/role-<role>.log`) — the file an operator greps to answer
    /// "is this role still running here?", and the file that stayed frozen
    /// for nine days on the affected host precisely because these bail-outs
    /// return before `run_role_with_timeout` (its only other writer) is
    /// reached. See [`note_pre_spawn_skip`].
    #[test]
    fn pre_spawn_skip_is_recorded_in_the_roles_own_log() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join(".loom").join("logs");

        note_pre_spawn_skip(
            &logs_dir,
            "curator",
            "model/runtime mismatch: runtime \"codex\" only accepts Codex models, but the \
             resolved model \"sonnet\" is a Claude model",
        );

        // The marker must land in the SAME file a real invocation writes its
        // header to — a skip logged to a different path would still leave the
        // operator-facing artifact silent.
        let log = std::fs::read_to_string(role_log_path(&logs_dir, "curator")).unwrap();
        assert!(
            log.contains("SKIPPED BEFORE SPAWN (#6201)"),
            "skip marker missing from role log: {log}"
        );
        assert!(
            log.contains("runtime \"codex\" only accepts Codex models"),
            "skip marker must name the actual reason so the log alone is diagnostic: {log}"
        );
        assert!(log.contains("role=curator"), "skip marker must name the role: {log}");

        // Repeated skips append rather than overwrite: a role stuck in this
        // state shows a growing, timestamped trail instead of one stale line.
        note_pre_spawn_skip(&logs_dir, "curator", "no token pool available");
        let log = std::fs::read_to_string(role_log_path(&logs_dir, "curator")).unwrap();
        assert_eq!(
            log.matches("SKIPPED BEFORE SPAWN (#6201)").count(),
            2,
            "each skipped tick must leave its own line: {log}"
        );
    }

    /// A drain in progress (#4090) stops role ticks from *starting*: with the
    /// drain flag set before the loop runs, `spawn_role_task` performs ZERO
    /// `invoke` calls even after several tick intervals elapse. This is the
    /// highest-value new role-runner coverage (Finding 2 — role ticks had no
    /// halt gate at all before this).
    #[tokio::test]
    async fn test_drain_stops_role_ticks_from_starting() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runner = FakeRunner {
            outcomes: vec![RoleTickOutcome::Success; 3],
            calls: calls.clone(),
        };
        let spec = RoleSpec {
            name: "champion",
            prompt: "/loom:champion",
            default_interval_secs: 1,
            interval_default: true,
        };
        // Drain already engaged before the loop starts.
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let handle = spawn_role_task(
            runner,
            spec,
            Duration::from_millis(20),
            drain.clone(),
            PathBuf::from("/tmp/loom-test-root"),
            new_in_progress_guard(),
        );

        // Let several tick intervals elapse; not a single invoke may fire.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no role tick may start while draining"
        );

        // Clearing the drain resumes dispatch — proving the gate, not a dead loop.
        drain.store(false, std::sync::atomic::Ordering::SeqCst);
        wait_for_calls(&calls, 1, Duration::from_secs(2)).await;

        handle.abort();
    }

    #[tokio::test]
    async fn test_loop_stops_cleanly_when_runner_panics() {
        struct PanicOnceRunner;
        impl RoleInvocationRunner for PanicOnceRunner {
            fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
                panic!("boom");
            }
        }
        let spec = RoleSpec {
            name: "curator",
            prompt: "/loom:curator",
            default_interval_secs: 1,
            interval_default: true,
        };
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = spawn_role_task(
            PanicOnceRunner,
            spec,
            Duration::from_millis(20),
            drain,
            PathBuf::from("/tmp/loom-test-root"),
            new_in_progress_guard(),
        );
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(result.is_ok(), "loop task should finish (not hang) after the runner panics");
    }

    // ===================================================================
    // DEFAULT_ROLES prompts — regression guard for #4034 (bare `/curator`
    // matches no real command; the installed commands are namespaced).
    // ===================================================================

    #[test]
    fn test_default_roles_prompts_are_namespaced() {
        for spec in DEFAULT_ROLES {
            let expected = format!("/loom:{}", spec.name);
            assert_eq!(
                spec.prompt, expected,
                "RoleSpec {:?} prompt must be the namespaced `/loom:<role>` command, not a bare \
                 `/<role>` (see #4034 — a bare prompt matches no installed slash command and \
                 silently no-ops)",
                spec.name
            );
        }
    }

    // ===================================================================
    // Doctor in DEFAULT_ROLES — regression guard for #5272 (before this,
    // a `loom:changes-requested` PR whose sweep ended had no role left to
    // pick it up standalone, ever).
    // ===================================================================

    #[test]
    fn test_default_roles_includes_doctor_with_no_pr_number() {
        let doctor = DEFAULT_ROLES
            .iter()
            .find(|s| s.name == "doctor")
            .expect("#5272: DEFAULT_ROLES must include doctor as a standalone role");
        assert_eq!(
            doctor.prompt, "/loom:doctor",
            "must invoke Doctor's own Finding Work queue scan, not PR Fix Mode \
             (no PR number appended to the prompt)"
        );
        // Same cadence as `judge` — its paired stage in the PR lifecycle: a
        // fresh Judge rejection should not sit unaddressed materially longer
        // than a fresh Judge review sits unclaimed.
        let judge = DEFAULT_ROLES
            .iter()
            .find(|s| s.name == "judge")
            .expect("judge is default");
        assert_eq!(doctor.default_interval_secs, judge.default_interval_secs);
    }

    #[test]
    fn test_resolve_roles_can_select_doctor_alone() {
        let config = RoleRunnerConfig {
            roles: Some(vec!["doctor".to_string()]),
            ..Default::default()
        };
        let resolved = resolve_roles(&config);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "doctor");
    }

    // ===================================================================
    // Hermit in DEFAULT_ROLES — regression guard for #5601 (before this,
    // `hermit` was entirely absent from DEFAULT_ROLES, so naming it in
    // `autonomous.roleRunner.roles`/`onIdle` was silently discarded with a
    // "not a known standalone role" warning).
    // ===================================================================

    #[test]
    fn test_default_roles_includes_hermit() {
        let hermit = DEFAULT_ROLES
            .iter()
            .find(|s| s.name == "hermit")
            .expect("#5601: DEFAULT_ROLES must include hermit as a standalone role");
        assert_eq!(hermit.prompt, "/loom:hermit");
        // Same cadence as `auditor` — both are proposal-generating roles with
        // no PR/issue-queue argument.
        let auditor = DEFAULT_ROLES
            .iter()
            .find(|s| s.name == "auditor")
            .expect("auditor is default");
        assert_eq!(hermit.default_interval_secs, auditor.default_interval_secs);
    }

    #[test]
    fn test_resolve_roles_can_select_hermit_alone() {
        let config = RoleRunnerConfig {
            roles: Some(vec!["hermit".to_string()]),
            ..Default::default()
        };
        let resolved = resolve_roles(&config);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "hermit");
    }

    #[test]
    fn test_resolve_on_idle_roles_can_select_hermit_alone() {
        let config = RoleRunnerConfig {
            on_idle: Some(vec!["hermit".to_string()]),
            ..Default::default()
        };
        let resolved = resolve_on_idle_roles(&config);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "hermit");
    }

    // ===================================================================
    // Architect in DEFAULT_ROLES, idle-addressable ONLY — regression guards
    // for #5656 (before this, `architect` was entirely absent from
    // DEFAULT_ROLES, so naming it in `autonomous.roleRunner.onIdle` was
    // silently discarded and a repo whose backlog emptied had no mechanism
    // to acquire more work). Mirrors the `doctor` (#5272) / `hermit` (#5601)
    // guards above, plus the interval-exclusion half that is unique to it.
    // ===================================================================

    #[test]
    fn test_default_roles_includes_architect_as_idle_only() {
        let architect = DEFAULT_ROLES
            .iter()
            .find(|s| s.name == "architect")
            .expect("#5656: DEFAULT_ROLES must include architect so onIdle can resolve it");
        assert_eq!(architect.prompt, "/loom:architect");
        // The load-bearing half: it must NOT be an interval-cadence default.
        assert!(
            !architect.is_interval_default(),
            "#5656: architect must be idle-addressable ONLY — an interval-default architect \
             floods every unpinned repo's backlog with speculative proposals"
        );
        // Every other shipped role is an interval default; architect is the
        // sole carve-out today.
        assert_eq!(
            DEFAULT_ROLES
                .iter()
                .filter(|s| !s.is_interval_default())
                .count(),
            1,
            "a new idle-only role needs its own docs/table update (see daemon-reference.md)"
        );
    }

    #[test]
    fn test_resolve_roles_default_excludes_architect() {
        // The core silent-flood regression: an unset `autonomous.roleRunner.roles`
        // must never put architect on a timer.
        let resolved = resolve_roles(&RoleRunnerConfig::default());
        assert!(
            !resolved.iter().any(|r| r.name == "architect"),
            "#5656: unset `roles` must not dispatch architect on the interval cadence; got {:?}",
            resolved.iter().map(|r| r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_resolve_on_idle_roles_can_select_architect_alone() {
        // ...while `onIdle` DOES resolve it — the other half of the pair.
        let config = RoleRunnerConfig {
            on_idle: Some(vec!["architect".to_string()]),
            ..Default::default()
        };
        let resolved = resolve_on_idle_roles(&config);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "architect");
        // And naming it in `onIdle` alone still leaves the interval set free
        // of it (`roles` is unset here).
        assert!(!resolve_roles(&config).iter().any(|r| r.name == "architect"));
    }

    #[test]
    fn test_resolve_roles_explicit_allowlist_can_opt_architect_into_the_interval() {
        // Idle-only is the *default*, not a prohibition: a repo that
        // deliberately names architect in `roles` still gets a timer.
        let config = RoleRunnerConfig {
            roles: Some(vec!["architect".to_string()]),
            ..Default::default()
        };
        let resolved = resolve_roles(&config);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "architect");
    }

    #[test]
    fn test_missing_defaults_never_reports_architect() {
        // A pinned allowlist omitting architect is correct, not stale — so it
        // must not be nagged into adding it (which would reintroduce the
        // flood this carve-out prevents).
        let names = vec!["curator".to_string()];
        let missing = missing_defaults(&names);
        assert!(!missing.contains(&"architect"), "expected no \"architect\" in {missing:?}");
        // The #5339 staleness warning still fires for real interval defaults.
        assert!(missing.contains(&"doctor"));
    }

    #[test]
    fn test_missing_defaults_empty_when_list_covers_every_interval_default() {
        let names: Vec<String> = interval_default_roles()
            .iter()
            .map(|s| s.name.to_string())
            .collect();
        assert_eq!(missing_defaults(&names), Vec::<&str>::new());
    }

    // ===================================================================
    // Per-invocation architect proposal cap (#5656) — the actuator
    // saturation limit, per-repo configurable rather than a constant.
    // ===================================================================

    #[test]
    #[serial]
    fn test_architect_cap_default_when_unset() {
        std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
        assert_eq!(
            resolve_architect_max_proposals(&RoleRunnerConfig::default()),
            DEFAULT_ARCHITECT_MAX_PROPOSALS
        );
    }

    #[test]
    #[serial]
    fn test_architect_cap_config_overrides_default() {
        std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
        let config = RoleRunnerConfig {
            architect_max_proposals: Some(9),
            ..Default::default()
        };
        assert_eq!(resolve_architect_max_proposals(&config), 9);
    }

    #[test]
    #[serial]
    fn test_architect_cap_env_wins_over_config() {
        let config = RoleRunnerConfig {
            architect_max_proposals: Some(9),
            ..Default::default()
        };
        std::env::set_var(ARCHITECT_MAX_PROPOSALS_ENV, "3");
        let resolved = resolve_architect_max_proposals(&config);
        std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
        assert_eq!(resolved, 3);
    }

    #[test]
    #[serial]
    fn test_architect_cap_invalid_env_falls_through_to_config() {
        let config = RoleRunnerConfig {
            architect_max_proposals: Some(7),
            ..Default::default()
        };
        for bad in ["0", "-1", "many", ""] {
            std::env::set_var(ARCHITECT_MAX_PROPOSALS_ENV, bad);
            let resolved = resolve_architect_max_proposals(&config);
            assert_eq!(resolved, 7, "env {bad:?} should have been dropped to the config tier");
        }
        std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_read_architect_max_proposals_parses_and_soft_fails() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let ok = tempfile::tempdir().unwrap();
        write_config(ok.path(), r#"{"autonomous": {"roleRunner": {"architectMaxProposals": 12}}}"#);
        assert_eq!(read_role_runner_config(ok.path()).architect_max_proposals, Some(12));

        // Zero / negative / non-integer all soft-fail to None (→ env → default).
        for bad in ["0", "-4", "\"seven\"", "null", "{}"] {
            let tmp = tempfile::tempdir().unwrap();
            write_config(
                tmp.path(),
                &format!(
                    r#"{{"autonomous": {{"roleRunner": {{"architectMaxProposals": {bad}}}}}}}"#
                ),
            );
            assert_eq!(
                read_role_runner_config(tmp.path()).architect_max_proposals,
                None,
                "architectMaxProposals={bad} should soft-fail to None"
            );
        }

        // Absent key → None, and the rest of the block still parses.
        let absent = tempfile::tempdir().unwrap();
        write_config(absent.path(), r#"{"autonomous": {"roleRunner": {"enabled": true}}}"#);
        let cfg = read_role_runner_config(absent.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.architect_max_proposals, None);
        assert_eq!(cfg.enabled, Some(true));
    }

    #[test]
    #[serial]
    fn test_resolve_role_prompt_carries_the_cap_for_architect_only() {
        std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
        let architect = DEFAULT_ROLES
            .iter()
            .find(|s| s.name == "architect")
            .expect("architect is shipped");
        // Default cap.
        assert_eq!(
            resolve_role_prompt(architect, &RoleRunnerConfig::default()),
            format!("/loom:architect --max-proposals {DEFAULT_ARCHITECT_MAX_PROPOSALS}")
        );
        // Per-repo override reaches the prompt actually dispatched.
        let config = RoleRunnerConfig {
            architect_max_proposals: Some(7),
            ..Default::default()
        };
        assert_eq!(
            resolve_role_prompt(architect, &config),
            "/loom:architect --max-proposals 7".to_string()
        );
        // Every other role's prompt is byte-for-byte its static spec prompt.
        for spec in DEFAULT_ROLES.iter().filter(|s| s.name != "architect") {
            assert_eq!(resolve_role_prompt(spec, &config), spec.prompt.to_string());
        }
    }

    // ===================================================================
    // tick_is_implausibly_fast — #4034 AC #4 (a no-op success must be
    // distinguishable in the log from a real, slower tick).
    // ===================================================================

    #[test]
    fn test_implausibly_fast_success_is_flagged() {
        assert!(tick_is_implausibly_fast(
            &RoleTickOutcome::Success,
            Duration::from_millis(1400) // the observed #4034 incident duration
        ));
    }

    #[test]
    fn test_success_at_or_above_threshold_is_not_flagged() {
        assert!(!tick_is_implausibly_fast(&RoleTickOutcome::Success, IMPLAUSIBLY_FAST_TICK));
        assert!(!tick_is_implausibly_fast(
            &RoleTickOutcome::Success,
            IMPLAUSIBLY_FAST_TICK + Duration::from_secs(60)
        ));
    }

    #[test]
    fn test_failure_is_never_flagged_regardless_of_duration() {
        assert!(!tick_is_implausibly_fast(
            &RoleTickOutcome::Failure("boom".to_string()),
            Duration::from_millis(1)
        ));
    }

    // ===================================================================
    // onIdle config parsing (#4364)
    // ===================================================================

    // NOTE: see the comment above `test_config_missing_file_is_default` — these
    // tests read `read_role_runner_config` too, so they need the same
    // private-defaults-tier guard + `#[serial(loom_config_env)]` (#4538).
    #[test]
    #[serial(loom_config_env)]
    fn test_config_on_idle_absent_is_none() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"enabled": true}}}"#);
        let on_idle = read_role_runner_config(tmp.path()).on_idle;
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(on_idle, None);
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_on_idle_parses_array() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": ["champion"]}}}"#);
        let on_idle = read_role_runner_config(tmp.path()).on_idle;
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(on_idle, Some(vec!["champion".to_string()]));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_on_idle_non_array_soft_fails_to_none() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        // A non-array (string) value must not panic — it soft-fails to `None`,
        // matching the `roles` contract.
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": "champion"}}}"#);
        let on_idle = read_role_runner_config(tmp.path()).on_idle;
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(on_idle, None);
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_on_idle_drops_non_string_entries() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        // Non-string entries are dropped; string entries survive (unknown
        // *names* are filtered later in `resolve_on_idle_roles`).
        write_config(
            tmp.path(),
            r#"{"autonomous": {"roleRunner": {"onIdle": ["champion", 7, true]}}}"#,
        );
        let on_idle = read_role_runner_config(tmp.path()).on_idle;
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(on_idle, Some(vec!["champion".to_string()]));
    }

    // ===================================================================
    // resolve_on_idle_roles (#4364)
    // ===================================================================

    #[test]
    fn test_resolve_on_idle_roles_absent_is_empty() {
        // Opposite default from `roles`: absent key means NO idle triggering.
        assert_eq!(resolve_on_idle_roles(&RoleRunnerConfig::default()), Vec::new());
    }

    #[test]
    fn test_resolve_on_idle_roles_parses_and_preserves_order() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: None,
            interval_secs: None,
            on_idle: Some(vec!["guide".to_string(), "champion".to_string()]),
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        };
        let roles = resolve_on_idle_roles(&config);
        assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["champion", "guide"]);
    }

    #[test]
    fn test_resolve_on_idle_roles_ignores_unknown_names() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: None,
            interval_secs: None,
            on_idle: Some(vec![
                "champion".to_string(),
                "builder".to_string(),
                "nope".to_string(),
            ]),
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        };
        let roles = resolve_on_idle_roles(&config);
        assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["champion"]);
    }

    #[test]
    fn test_resolve_on_idle_roles_empty_array_is_empty() {
        let config = RoleRunnerConfig {
            enabled: None,
            roles: None,
            interval_secs: None,
            on_idle: Some(vec![]),
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        };
        assert_eq!(resolve_on_idle_roles(&config), Vec::new());
    }

    // ===================================================================
    // IdleTrigger — edge detection + debounce (#4364)
    // ===================================================================

    #[test]
    fn test_idle_trigger_boot_idle_does_not_fire() {
        let mut t = IdleTrigger::new();
        let root = Path::new("/tmp/loom-root-a");
        // First-ever observation is idle: boot on an empty queue must NOT fire.
        assert!(!t.observe_edge(root, true));
    }

    #[test]
    fn test_idle_trigger_fires_on_non_idle_to_idle_edge() {
        let mut t = IdleTrigger::new();
        let root = Path::new("/tmp/loom-root-b");
        // Boot idle (no fire), then busy, then idle => the edge fires exactly on
        // the busy → idle transition.
        assert!(!t.observe_edge(root, true));
        assert!(!t.observe_edge(root, false));
        assert!(t.observe_edge(root, true));
    }

    #[test]
    fn test_idle_trigger_does_not_refire_on_sustained_idle() {
        let mut t = IdleTrigger::new();
        let root = Path::new("/tmp/loom-root-c");
        assert!(!t.observe_edge(root, false)); // busy
        assert!(t.observe_edge(root, true)); // edge
                                             // Staying idle across N further ticks must not re-fire.
        assert!(!t.observe_edge(root, true));
        assert!(!t.observe_edge(root, true));
    }

    #[test]
    fn test_idle_trigger_no_fire_while_in_flight_then_fires_when_drained() {
        let mut t = IdleTrigger::new();
        let root = Path::new("/tmp/loom-root-d");
        // A tick that dispatched nothing but still has in-flight sweeps is
        // non-idle (not empty) — no edge; the edge fires on the later tick where
        // in-flight reaches zero.
        assert!(!t.observe_edge(root, false));
        assert!(!t.observe_edge(root, false));
        assert!(t.observe_edge(root, true));
    }

    #[test]
    fn test_idle_trigger_edge_is_per_root() {
        let mut t = IdleTrigger::new();
        let a = Path::new("/tmp/loom-root-e1");
        let b = Path::new("/tmp/loom-root-e2");
        // Drive root a busy→idle (edge) while b stays idle from boot (no edge).
        assert!(!t.observe_edge(a, false));
        assert!(!t.observe_edge(b, true));
        assert!(t.observe_edge(a, true)); // a fires
        assert!(!t.observe_edge(b, true)); // b never fired
    }

    #[test]
    fn test_idle_trigger_debounce_window() {
        let mut t = IdleTrigger::new();
        let root = Path::new("/tmp/loom-root-f");
        let t0 = Instant::now();
        // Never fired => outside the window.
        assert!(t.debounce_ok(root, "champion", t0));
        t.record_fired(root, "champion", t0);
        // Within 60s => debounced.
        assert!(!t.debounce_ok(root, "champion", t0 + Duration::from_secs(30)));
        assert!(!t.debounce_ok(root, "champion", t0 + Duration::from_secs(59)));
        // At/after 60s => allowed again.
        assert!(t.debounce_ok(root, "champion", t0 + IDLE_TRIGGER_DEBOUNCE));
        assert!(t.debounce_ok(root, "champion", t0 + Duration::from_secs(61)));
        // Debounce is per-role: a different role is unaffected.
        assert!(t.debounce_ok(root, "curator", t0 + Duration::from_secs(1)));
    }

    // ===================================================================
    // RoleRunGuard — in-progress overlap protection (#4364)
    // ===================================================================

    #[test]
    fn test_role_run_guard_blocks_second_acquire_then_releases_on_drop() {
        let set = new_in_progress_guard();
        let root = PathBuf::from("/tmp/loom-root-g");
        let g1 = RoleRunGuard::try_acquire(set.clone(), root.clone(), "champion");
        assert!(g1.is_some(), "first acquire should succeed");
        // Second acquire of the same (root, role) is refused while held.
        assert!(
            RoleRunGuard::try_acquire(set.clone(), root.clone(), "champion").is_none(),
            "second acquire of the same key must be refused"
        );
        // A different role on the same root is independent.
        assert!(RoleRunGuard::try_acquire(set.clone(), root.clone(), "curator").is_some());
        // Dropping the first guard clears the entry — a later acquire succeeds.
        drop(g1);
        assert!(
            RoleRunGuard::try_acquire(set, root, "champion").is_some(),
            "guard must clear its entry on drop"
        );
    }

    // ===================================================================
    // Concurrent role-agent ceiling (#6102)
    // ===================================================================

    /// The ceiling refuses admission once the process-wide active count reaches
    /// it — the bound `autonomous.workFinder.maxConcurrent` never provided,
    /// because role agents never pass through work-finder admission.
    ///
    /// Crucially the refusal is counted **across roots**: the incident host had
    /// 25 registered workspaces, so a per-root ceiling would have bounded
    /// nothing.
    #[test]
    fn test_admit_refuses_once_ceiling_reached_across_roots() {
        let set = new_in_progress_guard();
        let a = PathBuf::from("/tmp/loom-ceiling-a");
        let b = PathBuf::from("/tmp/loom-ceiling-b");
        let c = PathBuf::from("/tmp/loom-ceiling-c");

        let g1 = RoleRunGuard::admit(set.clone(), a, "champion", 2)
            .into_guard()
            .expect("first admit under a ceiling of 2");
        // A DIFFERENT root and a DIFFERENT role — still counts against the same
        // host-wide budget.
        let g2 = RoleRunGuard::admit(set.clone(), b, "curator", 2)
            .into_guard()
            .expect("second admit under a ceiling of 2");

        match RoleRunGuard::admit(set.clone(), c.clone(), "judge", 2) {
            RoleAdmission::CeilingReached { active, ceiling } => {
                assert_eq!(active, 2, "refusal must report the sampled active count");
                assert_eq!(ceiling, 2, "refusal must report the ceiling it compared against");
            }
            other => panic!("expected CeilingReached, got {other:?}"),
        }

        // Releasing one guard frees exactly one slot.
        drop(g1);
        assert!(
            RoleRunGuard::admit(set.clone(), c, "judge", 2)
                .into_guard()
                .is_some(),
            "a dropped guard must free a slot in the ceiling"
        );
        drop(g2);
    }

    /// `InProgress` (cadence overlap, #4364) and `CeilingReached` (resource
    /// limit, #6102) are distinct outcomes. Conflating them is what made
    /// role-agent load invisible: an operator grepping for a skip reason could
    /// not tell "this role is already running" from "the host is full".
    #[test]
    fn test_admit_distinguishes_in_progress_from_ceiling_reached() {
        let set = new_in_progress_guard();
        let root = PathBuf::from("/tmp/loom-ceiling-distinct");
        let _held = RoleRunGuard::admit(set.clone(), root.clone(), "champion", 4)
            .into_guard()
            .expect("first admit");

        // Same (root, role) with headroom to spare ⇒ overlap, not a ceiling hit.
        assert!(
            matches!(
                RoleRunGuard::admit(set.clone(), root.clone(), "champion", 4),
                RoleAdmission::InProgress
            ),
            "same (root, role) while held must report InProgress"
        );
        // Different role, but the ceiling is already met ⇒ ceiling, not overlap.
        assert!(
            matches!(
                RoleRunGuard::admit(set, root, "curator", 1),
                RoleAdmission::CeilingReached {
                    active: 1,
                    ceiling: 1
                }
            ),
            "a full ceiling must report CeilingReached, not InProgress"
        );
    }

    /// `try_acquire` keeps its pre-#6102 unbounded behavior, so the #4364
    /// overlap contract is unchanged for every caller that has no ceiling.
    #[test]
    fn test_try_acquire_remains_unbounded() {
        let set = new_in_progress_guard();
        let mut held = Vec::new();
        for (i, role) in ["champion", "curator", "judge", "doctor", "guide"]
            .iter()
            .enumerate()
        {
            let g =
                RoleRunGuard::try_acquire(set.clone(), PathBuf::from(format!("/tmp/r{i}")), role);
            assert!(g.is_some(), "try_acquire must not apply any ceiling");
            held.push(g);
        }
        assert_eq!(active_run_count(&set), 5);
    }

    /// The shipped default is derived from the interval-default role table, not
    /// hard-coded — so adding a role raises the ceiling by one instead of
    /// silently squeezing every other role.
    #[test]
    fn test_default_max_concurrent_is_derived_from_interval_default_roles() {
        assert_eq!(default_max_concurrent(), interval_default_roles().len());
        assert!(default_max_concurrent() >= 1, "a 0 ceiling would admit nothing");
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_max_concurrent_parses_and_rejects_zero() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"maxConcurrent": 3}}}"#);
        let parsed = read_role_runner_config(tmp.path()).max_concurrent;

        let tmp0 = tempfile::tempdir().unwrap();
        // 0 soft-fails to `None` (falls through to env/default) rather than
        // being honored as "admit nothing" — that is `enabled: false`.
        write_config(tmp0.path(), r#"{"autonomous": {"roleRunner": {"maxConcurrent": 0}}}"#);
        let zero = read_role_runner_config(tmp0.path()).max_concurrent;

        let tmp_bad = tempfile::tempdir().unwrap();
        write_config(tmp_bad.path(), r#"{"autonomous": {"roleRunner": {"maxConcurrent": "8"}}}"#);
        let bad = read_role_runner_config(tmp_bad.path()).max_concurrent;
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);

        assert_eq!(parsed, Some(3));
        assert_eq!(zero, None, "0 must soft-fail to None");
        assert_eq!(bad, None, "a non-integer must soft-fail to None");
    }

    #[test]
    #[serial(loom_role_runner_max_concurrent_env)]
    fn test_resolve_max_concurrent_precedence_env_over_config_over_default() {
        std::env::remove_var(ROLE_RUNNER_MAX_CONCURRENT_ENV);
        let unset = RoleRunnerConfig::default();
        let configured = RoleRunnerConfig {
            max_concurrent: Some(2),
            ..RoleRunnerConfig::default()
        };
        assert_eq!(resolve_max_concurrent(&unset), default_max_concurrent());
        assert_eq!(resolve_max_concurrent(&configured), 2);

        std::env::set_var(ROLE_RUNNER_MAX_CONCURRENT_ENV, "9");
        assert_eq!(resolve_max_concurrent(&configured), 9, "env must outrank config");

        // A zero / unparseable env value drops to the next tier rather than
        // being honored — same contract as `architectMaxProposals`.
        std::env::set_var(ROLE_RUNNER_MAX_CONCURRENT_ENV, "0");
        assert_eq!(resolve_max_concurrent(&configured), 2);
        std::env::set_var(ROLE_RUNNER_MAX_CONCURRENT_ENV, "lots");
        assert_eq!(resolve_max_concurrent(&unset), default_max_concurrent());
        std::env::remove_var(ROLE_RUNNER_MAX_CONCURRENT_ENV);
    }

    /// The status surface's read path (#6102 AC3): after the daemon registers
    /// its guard, `global_active_run_count` tracks live role agents — this is
    /// the number `loom-daemon status` reports next to in-flight sweeps, and
    /// the number that previously required `pgrep` to obtain.
    ///
    /// `#[serial]` because `GLOBAL_IN_PROGRESS` is a process-wide `OnceLock`:
    /// this is the only test that registers it, and it must not race a
    /// concurrent reader.
    #[test]
    #[serial(loom_role_runner_global_guard)]
    fn test_global_active_run_count_tracks_registered_guard() {
        // Unregistered (or before this process registers) reads as 0 rather
        // than panicking — the contract `calibrate` and every non-daemon
        // process rely on.
        let set = new_in_progress_guard();
        register_global_in_progress(set.clone());
        assert_eq!(global_active_run_count(), 0, "an empty guard reads as 0");

        let g = RoleRunGuard::admit(
            set.clone(),
            PathBuf::from("/tmp/loom-global-count"),
            "champion",
            4,
        )
        .into_guard()
        .expect("admit under the ceiling");
        assert_eq!(global_active_run_count(), 1, "a live role agent must be visible to status");
        drop(g);
        assert_eq!(global_active_run_count(), 0, "the count must fall as agents finish");
    }

    // ===================================================================
    // invoke_with_collision_probe — cross-host collision detection (#4623)
    // ===================================================================

    /// A runner that records every `(role, prompt)` it was asked to invoke and
    /// returns a scripted outcome.
    struct RecordingRunner {
        calls: Vec<(String, String)>,
        outcome: RoleTickOutcome,
    }

    impl RoleInvocationRunner for RecordingRunner {
        fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome {
            self.calls.push((role.to_string(), prompt.to_string()));
            self.outcome.clone()
        }
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_collision_probe_wrapper_is_transparent_to_the_invocation() {
        // Detection is opt-in and default-off: with it disabled the wrapper
        // must pass the invocation through byte-for-byte (same role, same
        // prompt, same outcome) and make no forge call.
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        std::env::remove_var(crate::role_collision::ROLE_COLLISION_DETECT_ENV);
        std::env::remove_var(crate::sweep_registry::COLLISION_DETECT_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = RecordingRunner {
            calls: Vec::new(),
            outcome: RoleTickOutcome::Failure("boom".into()),
        };
        let outcome = invoke_with_collision_probe(
            &mut runner,
            tmp.path(),
            "champion",
            "/loom:champion",
            Duration::from_secs(600),
        );
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(outcome, RoleTickOutcome::Failure("boom".into()));
        assert_eq!(runner.calls, vec![("champion".to_string(), "/loom:champion".to_string())]);
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_collision_probe_wrapper_records_the_self_run_window() {
        // The baseline the NEXT tick attributes foreign forge activity
        // against: the wrapper must open and close a self-run window around
        // every invocation, even a failing one, and even with detection off
        // (so enabling it mid-run has a baseline immediately).
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        std::env::remove_var(crate::role_collision::ROLE_COLLISION_DETECT_ENV);
        std::env::remove_var(crate::sweep_registry::COLLISION_DETECT_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = RecordingRunner {
            calls: Vec::new(),
            outcome: RoleTickOutcome::Failure("boom".into()),
        };
        let before = chrono::Utc::now();
        let _ = invoke_with_collision_probe(
            &mut runner,
            tmp.path(),
            "guide",
            "/loom:guide",
            Duration::from_secs(900),
        );
        let after = chrono::Utc::now();
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        let window = crate::role_collision::last_self_run(tmp.path(), "guide")
            .expect("a self-run window must be recorded");
        assert!(window.started >= before && window.started <= after);
        let ended = window
            .ended
            .expect("the window must be closed after the invocation");
        assert!(ended >= window.started && ended <= after);
    }

    // ===================================================================
    // plan_idle_runs — the composed edge/drain/enabled/debounce/guard decision
    // ===================================================================

    fn on_idle_config(enabled: Option<bool>, roles: Vec<&str>) -> RoleRunnerConfig {
        RoleRunnerConfig {
            enabled,
            roles: None,
            interval_secs: None,
            on_idle: Some(roles.into_iter().map(str::to_string).collect()),
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        }
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_fires_on_edge_when_enabled() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-a");
        let cfg = on_idle_config(Some(true), vec!["champion"]);
        let now = Instant::now();
        // Boot idle: no edge, so no plan.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
        // Go busy: no edge.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        // Busy → idle edge: champion fires (and its guard is now held).
        let plan = plan_idle_runs(&mut t, &set, root, &cfg, true, false, now);
        assert_eq!(plan.iter().map(|(s, _)| s.name).collect::<Vec<_>>(), vec!["champion"]);
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_drain_suppresses() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-b");
        let cfg = on_idle_config(Some(true), vec!["champion"]);
        let now = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        // Edge present, but draining => suppressed.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, true, now).is_empty());
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_disabled_suppresses() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-c");
        let cfg = on_idle_config(Some(false), vec!["champion"]);
        let now = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        // Edge present, but role runner disabled => no fire.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
        // #4377: onIdle is configured for this root, so the disabled-suppression
        // must be observable, not silent.
        assert!(t.disabled_warned(root), "onIdle configured + disabled must record a warning");
    }

    // ===================================================================
    // #4377 — idle-path disabled-suppression is visible, not silent
    // ===================================================================

    #[test]
    #[serial]
    fn test_plan_idle_runs_disabled_without_on_idle_does_not_warn() {
        // A root with no `onIdle` roles configured is disabled in its normal,
        // unconfigured state — not a misconfiguration, so no warning.
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-no-onidle");
        let cfg = RoleRunnerConfig {
            enabled: Some(false),
            roles: None,
            interval_secs: None,
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
        };
        let now = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
        assert!(
            !t.disabled_warned(root),
            "no onIdle configured => disabled is normal, must not warn"
        );
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_disabled_warning_dedupes_across_repeated_edges() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-dedupe");
        let cfg = on_idle_config(Some(false), vec!["champion"]);
        let t0 = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0).is_empty());
        // First edge: disabled, onIdle configured => warns.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0).is_empty());
        assert!(t.disabled_warned(root));
        // Flap busy -> idle again: still disabled; the warning stays deduped
        // (no observable way to detect a re-warn other than the state not
        // regressing — the log line itself is the thing that must not repeat).
        assert!(plan_idle_runs(
            &mut t,
            &set,
            root,
            &cfg,
            false,
            false,
            t0 + Duration::from_secs(5)
        )
        .is_empty());
        assert!(plan_idle_runs(
            &mut t,
            &set,
            root,
            &cfg,
            true,
            false,
            t0 + Duration::from_secs(10)
        )
        .is_empty());
        assert!(t.disabled_warned(root), "still deduped on the second edge");
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_disabled_warning_clears_once_enabled() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-clears");
        let disabled_cfg = on_idle_config(Some(false), vec!["champion"]);
        let enabled_cfg = on_idle_config(Some(true), vec!["champion"]);
        let t0 = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &disabled_cfg, false, false, t0).is_empty());
        assert!(plan_idle_runs(&mut t, &set, root, &disabled_cfg, true, false, t0).is_empty());
        assert!(t.disabled_warned(root));

        // Root flips to enabled (hot-apply) well outside the debounce window.
        assert!(plan_idle_runs(
            &mut t,
            &set,
            root,
            &enabled_cfg,
            false,
            false,
            t0 + Duration::from_secs(70)
        )
        .is_empty());
        let fire = plan_idle_runs(
            &mut t,
            &set,
            root,
            &enabled_cfg,
            true,
            false,
            t0 + Duration::from_secs(80),
        );
        assert_eq!(fire.len(), 1, "enabled root must fire normally");
        assert!(
            !t.disabled_warned(root),
            "warned flag must clear once the root resolves enabled"
        );
    }

    // ===================================================================
    // #6470 — idle-edge WARN names the true cause (env vs config) and
    // collapses the per-root #4377 warning to one host-level line when the
    // host-wide LOOM_ROLE_RUNNER env override is what disabled it.
    // ===================================================================

    #[test]
    #[serial]
    fn test_plan_idle_runs_env_disabled_warns_host_level_not_per_root() {
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-env-disabled");
        // This root's OWN config says enabled — the env override still wins
        // and is the true cause, not this root's config.
        let cfg = on_idle_config(Some(true), vec!["champion"]);
        let now = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        // The env-override branch records the HOST-level dedup, not the
        // per-root #4377 one — the per-root cause was never the reason.
        assert!(t.host_env_warned(), "env-caused disable must record the host-level warning");
        assert!(
            !t.disabled_warned(root),
            "env-caused disable must NOT record the per-root #4377 warning"
        );
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_env_disabled_collapses_across_multiple_roots() {
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root_a = Path::new("/tmp/loom-plan-env-a");
        let root_b = Path::new("/tmp/loom-plan-env-b");
        let cfg = on_idle_config(Some(true), vec!["champion"]);
        let now = Instant::now();
        // Both roots boot idle (no edge yet).
        assert!(plan_idle_runs(&mut t, &set, root_a, &cfg, false, false, now).is_empty());
        assert!(plan_idle_runs(&mut t, &set, root_b, &cfg, false, false, now).is_empty());
        // Root A's idle edge fires the (one-time) host-level warning.
        assert!(plan_idle_runs(&mut t, &set, root_a, &cfg, true, false, now).is_empty());
        assert!(t.host_env_warned());
        // Root B's idle edge, same host-wide cause: must NOT warn again —
        // `warn_if_idle_configured_but_disabled` is a no-op the second time,
        // regardless of which root triggers it (this is the whole point of
        // collapsing to a single host-level line instead of N per-root ones).
        assert!(plan_idle_runs(&mut t, &set, root_b, &cfg, true, false, now).is_empty());
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        assert!(
            !t.disabled_warned(root_a) && !t.disabled_warned(root_b),
            "env-caused disable never records the per-root dedup for either root"
        );
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_env_disabled_warning_clears_once_reenabled() {
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-env-clears");
        let cfg = on_idle_config(Some(true), vec!["champion"]);
        let t0 = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0).is_empty());
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0).is_empty());
        assert!(t.host_env_warned());

        // The env override clears (host re-enabled) well outside the
        // debounce window — this root's own config (`enabled: true`) now
        // decides, and it fires normally.
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        assert!(plan_idle_runs(
            &mut t,
            &set,
            root,
            &cfg,
            false,
            false,
            t0 + Duration::from_secs(70)
        )
        .is_empty());
        let fire =
            plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0 + Duration::from_secs(80));
        assert_eq!(fire.len(), 1, "root must fire once the env override clears");
        assert!(
            !t.host_env_warned(),
            "host-level warned flag must clear once the override is no longer disabling"
        );
    }

    /// Cross-config case (#4377 curated AC): a target root has `onIdle` set
    /// but its own per-root `enabled` is absent (resolves `false`) —
    /// independent of whatever the daemon's own workspace's master switch is
    /// set to (the master switch only decides whether these loops start at
    /// all, never a target root's own gate). `observe_and_fire_idle` is the
    /// real entry point the work-finder loop calls, reading the root's own
    /// on-disk config each tick — exercised here end-to-end rather than via
    /// the already-parsed `RoleRunnerConfig` the other tests use.
    // NOTE: see the comment above `test_config_missing_file_is_default` — this
    // test's `observe_and_fire_idle` calls read the private-defaults tier via
    // `read_role_runner_config` too, so it needs the same guard +
    // `#[serial(loom_config_env)]` (#4593, discovered during review of #4590 /
    // #4538).
    #[test]
    #[serial(loom_config_env)]
    fn test_observe_and_fire_idle_cross_config_disabled_target_root_warns_and_suppresses() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": ["champion"]}}}"#);
        let mut trigger = IdleTrigger::new();
        let in_progress = new_in_progress_guard();

        observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), true, false); // boot idle: no edge
        observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), false, false); // go busy: no edge
        observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), true, false); // busy -> idle edge

        assert!(
            trigger.disabled_warned(tmp.path()),
            "idle edge on a disabled-but-onIdle-configured root must record the warning"
        );
        assert!(
            in_progress.lock().unwrap().is_empty(),
            "a disabled root must never acquire/fire a run"
        );

        // A second flap must stay deduped — no panic, no re-fire, warned state
        // holds (this is the "second edge does not re-warn" acceptance case).
        observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), false, false);
        observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), true, false);
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert!(trigger.disabled_warned(tmp.path()));
        assert!(in_progress.lock().unwrap().is_empty());
    }

    // ===================================================================
    // #4377 — interval-path disabled-root warn-once dedup
    // ===================================================================

    #[test]
    fn test_should_warn_disabled_root_warns_once_then_dedupes_until_reenable() {
        let mut warned: HashSet<PathBuf> = HashSet::new();
        let root = PathBuf::from("/tmp/loom-interval-disabled-root");
        assert!(
            should_warn_disabled_root(&mut warned, &root),
            "first sighting of a disabled root must warn"
        );
        assert!(
            !should_warn_disabled_root(&mut warned, &root),
            "repeat sighting must be deduped (downgraded to DEBUG by the caller)"
        );
        assert!(
            !should_warn_disabled_root(&mut warned, &root),
            "stays deduped across further ticks"
        );
        // Caller clears the entry once the root resolves enabled again.
        warned.remove(&root);
        assert!(
            should_warn_disabled_root(&mut warned, &root),
            "a re-disable after a re-enable must warn again"
        );
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_debounced_second_edge_then_fires_after_window() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-d");
        let cfg = on_idle_config(Some(true), vec!["champion"]);
        let t0 = Instant::now();
        // First edge fires and records the debounce timestamp.
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0).is_empty());
        let first = plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0);
        assert_eq!(first.len(), 1);
        drop(first); // release the guard so only debounce can block the next edge
                     // Flap busy→idle again within 60s: edge present but debounced.
        assert!(plan_idle_runs(
            &mut t,
            &set,
            root,
            &cfg,
            false,
            false,
            t0 + Duration::from_secs(10)
        )
        .is_empty());
        let debounced =
            plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0 + Duration::from_secs(20));
        assert!(debounced.is_empty(), "second edge within 60s must be debounced");
        // Flap again after the window: fires.
        assert!(plan_idle_runs(
            &mut t,
            &set,
            root,
            &cfg,
            false,
            false,
            t0 + Duration::from_secs(70)
        )
        .is_empty());
        let after =
            plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0 + Duration::from_secs(80));
        assert_eq!(after.len(), 1, "edge after the debounce window must fire");
    }

    #[test]
    #[serial]
    fn test_plan_idle_runs_skips_when_guard_already_held() {
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let root = Path::new("/tmp/loom-plan-e");
        let cfg = on_idle_config(Some(true), vec!["champion"]);
        let now = Instant::now();
        // Simulate an interval run already holding the guard for (root, champion).
        let _held = RoleRunGuard::try_acquire(set.clone(), root.to_path_buf(), "champion");
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        // Edge present, but the guard is held by the interval run => idle skips.
        assert!(
            plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty(),
            "idle trigger must skip while an interval run holds the guard"
        );
    }

    // ===================================================================
    // Interval loop honors the shared in-progress guard (#4364)
    // ===================================================================

    /// A pre-held guard for (root, role) makes the interval loop skip every
    /// tick (0 invokes); clearing it resumes dispatch — proving the interval
    /// path also respects the shared guard, so an idle-triggered run in
    /// progress cannot be overlapped by an interval tick.
    #[tokio::test]
    async fn test_interval_loop_skips_while_guard_held() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runner = FakeRunner {
            outcomes: vec![RoleTickOutcome::Success; 3],
            calls: calls.clone(),
        };
        let spec = RoleSpec {
            name: "champion",
            prompt: "/loom:champion",
            default_interval_secs: 1,
            interval_default: true,
        };
        let root = PathBuf::from("/tmp/loom-interval-guard");
        let in_progress = new_in_progress_guard();
        // Pre-hold the guard for (root, champion) so the loop cannot acquire it.
        in_progress
            .lock()
            .unwrap()
            .insert((root.clone(), "champion"));
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = spawn_role_task(
            runner,
            spec,
            Duration::from_millis(20),
            drain,
            root.clone(),
            in_progress.clone(),
        );

        // Several intervals elapse; not a single invoke may fire.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "interval tick must skip while the shared guard is held"
        );

        // Release the guard — dispatch resumes, proving the gate (not a dead loop).
        in_progress.lock().unwrap().remove(&(root, "champion"));
        wait_for_calls(&calls, 1, Duration::from_secs(2)).await;

        handle.abort();
    }

    // ===================================================================
    // classify_root_tick_log / log_outcome_for_root_deduped — #4349 state-
    // change log dedup: a repeatedly failing root logs once on the fail
    // edge and once on recovery, not once per tick.
    // ===================================================================

    const NORMAL_TICK: Duration = Duration::from_secs(90);

    #[test]
    fn test_classify_first_failure_is_edge() {
        assert_eq!(
            classify_root_tick_log(
                &RoleTickOutcome::Failure("boom".into()),
                NORMAL_TICK,
                false,
                false,
                false
            ),
            RootTickLogAction::FailureEdge
        );
    }

    #[test]
    fn test_classify_repeat_failure_is_downgraded() {
        assert_eq!(
            classify_root_tick_log(
                &RoleTickOutcome::Failure("boom".into()),
                NORMAL_TICK,
                true,
                false,
                false
            ),
            RootTickLogAction::FailureRepeat
        );
    }

    #[test]
    fn test_classify_success_after_failure_is_recovery() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::Success, NORMAL_TICK, true, false, false),
            RootTickLogAction::Recovered
        );
    }

    #[test]
    fn test_classify_steady_state_success_is_plain() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::Success, NORMAL_TICK, false, false, false),
            RootTickLogAction::Success
        );
    }

    #[test]
    fn test_classify_implausibly_fast_variants() {
        assert_eq!(
            classify_root_tick_log(
                &RoleTickOutcome::Success,
                Duration::from_millis(100),
                false,
                false,
                false
            ),
            RootTickLogAction::SuccessImplausiblyFast
        );
        assert_eq!(
            classify_root_tick_log(
                &RoleTickOutcome::Success,
                Duration::from_millis(100),
                true,
                false,
                false
            ),
            RootTickLogAction::RecoveredImplausiblyFast
        );
    }

    // ---- no-token-pool classification (#4642) -------------------------

    #[test]
    fn test_classify_first_no_token_pool_is_edge() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::NoTokenPool, NORMAL_TICK, false, false, false),
            RootTickLogAction::NoTokenPoolEdge
        );
    }

    #[test]
    fn test_classify_repeat_no_token_pool_is_downgraded() {
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::NoTokenPool, NORMAL_TICK, false, true, false),
            RootTickLogAction::NoTokenPoolRepeat
        );
    }

    #[test]
    fn test_classify_no_token_pool_is_independent_of_failing_state() {
        // A root that was previously `Failure`-failing must not have its
        // no-token-pool skip demoted to `Repeat` just because `was_failing`
        // is true — the two conditions are tracked on separate axes.
        assert_eq!(
            classify_root_tick_log(&RoleTickOutcome::NoTokenPool, NORMAL_TICK, true, false, false),
            RootTickLogAction::NoTokenPoolEdge
        );
    }

    #[test]
    fn test_root_tick_log_action_no_token_pool_is_not_failing() {
        // #4642: a no-token-pool skip must never contribute to the
        // Failure/RuntimeRejected tally.
        assert!(!RootTickLogAction::NoTokenPoolEdge.is_failing());
        assert!(!RootTickLogAction::NoTokenPoolRepeat.is_failing());
        assert!(RootTickLogAction::NoTokenPoolEdge.is_no_token_pool());
        assert!(RootTickLogAction::NoTokenPoolRepeat.is_no_token_pool());
        assert!(!RootTickLogAction::FailureEdge.is_no_token_pool());
        assert!(!RootTickLogAction::FailureRepeat.is_no_token_pool());
    }

    // ---- model/runtime mismatch classification (#5028) -----------------

    fn mismatch_outcome() -> RoleTickOutcome {
        RoleTickOutcome::ModelRuntimeMismatch(ModelRuntimeMismatch {
            role: "judge".to_string(),
            runtime: "codex".to_string(),
            model: "sonnet".to_string(),
            model_source: "default".to_string(),
            reason: "runtime \"codex\" only accepts an OpenAI/Codex model but got \"sonnet\""
                .to_string(),
        })
    }

    #[test]
    fn test_classify_first_model_mismatch_is_edge() {
        assert_eq!(
            classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, false, false, false),
            RootTickLogAction::ModelMismatchEdge
        );
    }

    #[test]
    fn test_classify_repeat_model_mismatch_is_downgraded() {
        assert_eq!(
            classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, false, false, true),
            RootTickLogAction::ModelMismatchRepeat
        );
    }

    #[test]
    fn test_classify_model_mismatch_is_independent_of_failing_and_no_token_pool_state() {
        // A root previously `Failure`-failing OR previously no-token-pool must
        // not have its model-mismatch skip demoted to `Repeat` just because
        // one of the OTHER two axes is `true` — all three are tracked
        // independently.
        assert_eq!(
            classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, true, false, false),
            RootTickLogAction::ModelMismatchEdge
        );
        assert_eq!(
            classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, false, true, false),
            RootTickLogAction::ModelMismatchEdge
        );
    }

    #[test]
    fn test_root_tick_log_action_model_mismatch_is_not_failing_or_no_token_pool() {
        // #5028: a model-mismatch skip must never contribute to the
        // Failure/RuntimeRejected tally, nor to the NoTokenPool tally.
        assert!(!RootTickLogAction::ModelMismatchEdge.is_failing());
        assert!(!RootTickLogAction::ModelMismatchRepeat.is_failing());
        assert!(!RootTickLogAction::ModelMismatchEdge.is_no_token_pool());
        assert!(!RootTickLogAction::ModelMismatchRepeat.is_no_token_pool());
        assert!(RootTickLogAction::ModelMismatchEdge.is_model_mismatch());
        assert!(RootTickLogAction::ModelMismatchRepeat.is_model_mismatch());
        assert!(!RootTickLogAction::FailureEdge.is_model_mismatch());
        assert!(!RootTickLogAction::NoTokenPoolEdge.is_model_mismatch());
    }

    #[test]
    #[serial(role_tick_ring)]
    fn test_log_outcome_for_root_deduped_tracks_failing_state_across_ticks() {
        let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test");
        let mut failing: HashMap<PathBuf, bool> = HashMap::new();
        let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
        let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

        // Tick 1: failure -> edge, marks failing.
        log_outcome_for_root_deduped(
            "champion",
            &root,
            &RoleTickOutcome::Failure("MCP_PREFLIGHT_FAILED".into()),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&true));

        // Ticks 2-4: identical repeat failures -> still marked failing (the
        // dedup happens in the log call, not observable here directly, but
        // the state must remain `true` without ever clearing).
        for _ in 0..3 {
            log_outcome_for_root_deduped(
                "champion",
                &root,
                &RoleTickOutcome::Failure("MCP_PREFLIGHT_FAILED".into()),
                NORMAL_TICK,
                &mut failing,
                &mut no_token_pool,
                &mut model_mismatch,
            );
            assert_eq!(failing.get(&root), Some(&true));
        }

        // Tick 5: recovers -> state flips back to healthy.
        log_outcome_for_root_deduped(
            "champion",
            &root,
            &RoleTickOutcome::Success,
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&false));

        // Tick 6: steady-state success keeps it healthy.
        log_outcome_for_root_deduped(
            "champion",
            &root,
            &RoleTickOutcome::Success,
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&false));
    }

    #[test]
    #[serial(role_tick_ring)]
    fn test_log_outcome_for_root_deduped_is_independent_per_root() {
        // A failure on one registered root must not affect another root's
        // failing state (each workspace's health is tracked independently).
        let root_a = PathBuf::from("/tmp/root-a");
        let root_b = PathBuf::from("/tmp/root-b");
        let mut failing: HashMap<PathBuf, bool> = HashMap::new();
        let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
        let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

        log_outcome_for_root_deduped(
            "curator",
            &root_a,
            &RoleTickOutcome::Failure("boom".into()),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        log_outcome_for_root_deduped(
            "curator",
            &root_b,
            &RoleTickOutcome::Success,
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );

        assert_eq!(failing.get(&root_a), Some(&true));
        assert_eq!(failing.get(&root_b), Some(&false));
    }

    #[test]
    #[serial(role_tick_ring)]
    fn test_log_outcome_for_root_deduped_no_token_pool_tracked_independently_of_failing() {
        // #4642: a NoTokenPool tick must never mark `failing` true, and a
        // real Failure tick must never mark `no_token_pool` true — the two
        // maps are independent axes even for the SAME root.
        let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test-2");
        let mut failing: HashMap<PathBuf, bool> = HashMap::new();
        let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
        let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

        log_outcome_for_root_deduped(
            "auditor",
            &root,
            &RoleTickOutcome::NoTokenPool,
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(no_token_pool.get(&root), Some(&true));
        assert_eq!(failing.get(&root), Some(&false));

        // A subsequent real failure must still log as a fresh `FailureEdge`
        // (not `FailureRepeat`) even though the root was just skipped for no
        // token pool — proving the two states never cross-contaminate.
        log_outcome_for_root_deduped(
            "auditor",
            &root,
            &RoleTickOutcome::Failure("boom".into()),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&true));
        assert_eq!(no_token_pool.get(&root), Some(&false));
    }

    #[test]
    #[serial(role_tick_ring)]
    fn test_log_outcome_for_root_deduped_model_mismatch_tracked_independently() {
        // #5028: a ModelRuntimeMismatch tick must never mark `failing` or
        // `no_token_pool` true, and must not itself be marked by either of
        // those two axes — all three maps are independent even for the SAME
        // root.
        let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test-3");
        let mut failing: HashMap<PathBuf, bool> = HashMap::new();
        let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
        let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

        log_outcome_for_root_deduped(
            "judge",
            &root,
            &mismatch_outcome(),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(model_mismatch.get(&root), Some(&true));
        assert_eq!(failing.get(&root), Some(&false));
        assert_eq!(no_token_pool.get(&root), Some(&false));

        // A subsequent real failure must still log as a fresh `FailureEdge`
        // even though the root was just skipped for a model/runtime mismatch.
        log_outcome_for_root_deduped(
            "judge",
            &root,
            &RoleTickOutcome::Failure("boom".into()),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&true));
        assert_eq!(model_mismatch.get(&root), Some(&false));
    }

    /// Issue #6637: a `LoadSkipped` tick must never mark `failing`,
    /// `no_token_pool`, or `model_mismatch` true — it is its own axis, and a
    /// load-induced skip must not be tallied against any of the three
    /// existing dedup states (mirroring the independence tests above).
    #[test]
    fn test_log_outcome_for_root_deduped_load_skipped_tracked_independently() {
        let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test-4");
        let mut failing: HashMap<PathBuf, bool> = HashMap::new();
        let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
        let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

        log_outcome_for_root_deduped(
            "auditor",
            &root,
            &RoleTickOutcome::LoadSkipped {
                load_per_core: 2.4,
                detail: "still resolving spawn-worker.sh".to_string(),
            },
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&false));
        assert_eq!(no_token_pool.get(&root), Some(&false));
        assert_eq!(model_mismatch.get(&root), Some(&false));

        // A subsequent real failure must still log as a fresh `FailureEdge`
        // even though the root was just load-skipped.
        log_outcome_for_root_deduped(
            "auditor",
            &root,
            &RoleTickOutcome::Failure("boom".into()),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&true));
    }

    // ===================================================================
    // spawn_multi_role_task missing-root hygiene (#4326/#4349) — a
    // registered root whose directory no longer exists is skipped, not
    // spawned against, mirroring work_finder's filter_missing_roots.
    // ===================================================================

    #[tokio::test]
    #[serial]
    async fn test_multi_role_task_skips_missing_registered_root() {
        let tmp = tempfile::tempdir().unwrap();
        let existing_root = tmp.path().join("existing");
        let missing_root = tmp.path().join("gone");
        std::fs::create_dir_all(&existing_root).unwrap();
        write_config(&existing_root, r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);
        // `add` validates the path exists at registration time, so create the
        // "missing" root first, register it, then delete it — reproducing a
        // registered-but-later-deleted worktree (#4349's #4188 scenario).
        std::fs::create_dir_all(&missing_root).unwrap();

        let registry_path = tmp.path().join("workspaces.json");
        std::env::set_var(
            crate::workspace_registry::REGISTRY_PATH_ENV,
            registry_path.to_str().unwrap(),
        );
        let mut registry = WorkspaceRegistry::default();
        registry.add(&existing_root, None).unwrap();
        registry.add(&missing_root, None).unwrap();
        registry.save_default().unwrap();
        std::fs::remove_dir_all(&missing_root).unwrap();

        let spec = RoleSpec {
            name: "curator",
            prompt: "/loom:curator",
            default_interval_secs: 1,
            interval_default: true,
        };
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let in_progress = new_in_progress_guard();
        let handle = spawn_multi_role_task(
            spec,
            tmp.path().to_path_buf(),
            Duration::from_millis(20),
            drain,
            in_progress,
        );

        // Let a couple of ticks fire. The missing root must never be spawned
        // against (there is no script at its `.loom/config.json`/spawn path
        // to invoke, so a spawn attempt would either fail loudly or panic
        // the resolve step; the assertion here is simply that the loop
        // survives several ticks without erroring the test process, which
        // it would if the missing root were not filtered before dispatch).
        tokio::time::sleep(Duration::from_millis(80)).await;
        handle.abort();

        std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
    }

    // ===================================================================
    // decide_root_tick + its catch_unwind isolation (#6201 AC2)
    // ===================================================================

    fn curator_spec() -> RoleSpec {
        RoleSpec {
            name: "curator",
            prompt: "/loom:curator",
            default_interval_secs: 300,
            interval_default: true,
        }
    }

    /// Put [`ROLE_RUNNER_ENABLE_ENV`] back exactly as it was found — set to
    /// its prior value, or unset if it was unset (#6644). Paired with a
    /// `std::env::var(ROLE_RUNNER_ENABLE_ENV).ok()` capture + `remove_var` at
    /// the top of a `#[serial]` test.
    fn restore_role_runner_env(prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(ROLE_RUNNER_ENABLE_ENV, v),
            None => std::env::remove_var(ROLE_RUNNER_ENABLE_ENV),
        }
    }

    #[test]
    fn decide_root_tick_skips_a_disabled_root_and_warns_once() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous":{"roleRunner":{"enabled":false}}}"#);
        let mut disabled_warned = HashSet::new();
        let mut resolved_logged = HashMap::new();
        let in_progress = new_in_progress_guard();

        let decision = decide_root_tick(
            tmp.path(),
            &curator_spec(),
            &in_progress,
            &mut disabled_warned,
            &mut resolved_logged,
            &mut HashMap::new(),
        );
        assert!(decision.is_none());
        assert!(disabled_warned.contains(tmp.path()));
    }

    #[test]
    fn decide_root_tick_skips_a_role_not_in_the_resolved_list() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["judge"]}}}"#,
        );
        let mut disabled_warned = HashSet::new();
        let mut resolved_logged = HashMap::new();
        let in_progress = new_in_progress_guard();

        let decision = decide_root_tick(
            tmp.path(),
            &curator_spec(),
            &in_progress,
            &mut disabled_warned,
            &mut resolved_logged,
            &mut HashMap::new(),
        );
        assert!(decision.is_none());
    }

    /// #6644: `decide_root_tick` resolves enablement through
    /// [`resolve_enabled_with_source`], which consults the ambient
    /// [`ROLE_RUNNER_ENABLE_ENV`] **before** the root's own config — so an
    /// inherited falsy `LOOM_ROLE_RUNNER` (e.g. on an agent dispatched by a
    /// daemon whose unit/plist baked the var into its environment) would
    /// override the tempdir config this test writes and make the admission
    /// assertion fail for reasons unrelated to the code under test. Scope the
    /// var explicitly, under `#[serial]` so this does not race the file's
    /// other `ROLE_RUNNER_ENABLE_ENV`-mutating tests.
    #[test]
    #[serial]
    fn decide_root_tick_admits_and_returns_a_guard_when_configured() {
        let prev_env = std::env::var(ROLE_RUNNER_ENABLE_ENV).ok();
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);

        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);
        let mut disabled_warned = HashSet::new();
        let mut resolved_logged = HashMap::new();
        let in_progress = new_in_progress_guard();

        let decision = decide_root_tick(
            tmp.path(),
            &curator_spec(),
            &in_progress,
            &mut disabled_warned,
            &mut resolved_logged,
            &mut HashMap::new(),
        );

        // Restore BEFORE asserting: a failing assertion must not leak this
        // test's scoped value into the rest of the suite.
        restore_role_runner_env(prev_env);

        let (prompt, _guard) = decision.expect("curator is enabled and in the default role set");
        assert_eq!(prompt, "/loom:curator");
        // The guard holds the (root, role) pair in-progress until dropped.
        assert_eq!(active_run_count(&in_progress), 1);
    }

    /// #6201 AC2: the exact `catch_unwind(AssertUnwindSafe(...))` shape
    /// `spawn_multi_role_task`'s loop wraps [`decide_root_tick`] in isolates
    /// a panic — it must never propagate out of the tick, and a subsequent
    /// call using the SAME shared dedup state must still succeed normally
    /// (proving the caught panic left no poisoned/inconsistent state behind
    /// that would itself wedge later ticks).
    ///
    /// `#[serial]` + explicit [`ROLE_RUNNER_ENABLE_ENV`] scoping for the same
    /// reason as the test above (#6644): the recovery half of this test calls
    /// [`decide_root_tick`] for real, so an ambient `LOOM_ROLE_RUNNER` would
    /// otherwise decide the outcome instead of the tempdir config.
    #[test]
    #[serial]
    fn root_tick_decision_panic_is_isolated_and_does_not_propagate() {
        let prev_env = std::env::var(ROLE_RUNNER_ENABLE_ENV).ok();
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);

        let mut disabled_warned: HashSet<PathBuf> = HashSet::new();
        let mut resolved_logged: HashMap<PathBuf, String> = HashMap::new();

        // Same call shape as the production site, but the closure panics
        // instead of calling `decide_root_tick` — reproducing "a panic
        // anywhere in the synchronous decision phase" without depending on
        // an actual panic trigger existing in today's (deliberately
        // soft-failing) config-parsing code.
        let result: Result<Option<(String, RoleRunGuard)>, _> =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = &mut disabled_warned;
                let _ = &mut resolved_logged;
                panic!("synthetic tick-decision panic (#6201 regression fixture)");
            }));
        assert!(result.is_err(), "the panic must be caught, not propagated");
        let msg = describe_panic(&*result.unwrap_err());
        assert!(msg.contains("synthetic tick-decision panic"), "{msg}");

        // The loop's own recovery: the SAME shared dedup maps are still
        // usable afterward, and a real decision call succeeds normally —
        // exactly "skip only this tick; the loop continues on the next
        // interval".
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);
        let in_progress = new_in_progress_guard();
        let decision = decide_root_tick(
            tmp.path(),
            &curator_spec(),
            &in_progress,
            &mut disabled_warned,
            &mut resolved_logged,
            &mut HashMap::new(),
        );

        // Restore BEFORE asserting (see the sibling test above).
        restore_role_runner_env(prev_env);

        assert!(
            decision.is_some(),
            "a later, healthy tick must still succeed after a caught panic"
        );
    }

    #[test]
    fn describe_panic_extracts_str_and_string_payloads() {
        let str_panic =
            std::panic::catch_unwind(|| -> () { panic!("literal message") }).unwrap_err();
        assert_eq!(describe_panic(&*str_panic), "literal message");

        let string_panic =
            std::panic::catch_unwind(|| -> () { panic!("{}", "formatted message".to_string()) })
                .unwrap_err();
        assert_eq!(describe_panic(&*string_panic), "formatted message");
    }

    // ===================================================================
    // Host sharding at the dispatch surface (#6374)
    //
    // `role_shard`'s own tests pin the *arithmetic* (exactly one owner per
    // key, an even spread, the fail-safe fallbacks). These pin the thing
    // that arithmetic alone cannot: that `decide_root_tick` and
    // `plan_idle_runs` — the two surfaces that actually spend a token —
    // honor it, in the right order relative to the `LOOM_ROLE_RUNNER`
    // kill switch.
    // ===================================================================

    /// Capture and clear every env var these tests manipulate, restoring the
    /// prior values on drop so a failing assertion cannot leak state into the
    /// rest of the (serial) suite.
    struct ShardEnvGuard {
        enable: Option<String>,
        index: Option<String>,
        count: Option<String>,
    }

    impl ShardEnvGuard {
        fn capture() -> Self {
            let g = Self {
                enable: std::env::var(ROLE_RUNNER_ENABLE_ENV).ok(),
                index: std::env::var(crate::role_shard::SHARD_INDEX_ENV).ok(),
                count: std::env::var(crate::role_shard::SHARD_COUNT_ENV).ok(),
            };
            std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
            std::env::remove_var(crate::role_shard::SHARD_INDEX_ENV);
            std::env::remove_var(crate::role_shard::SHARD_COUNT_ENV);
            g
        }

        /// Pretend to be host `index` of a `count`-host fleet.
        fn become_host(index: usize, count: usize) {
            std::env::set_var(crate::role_shard::SHARD_INDEX_ENV, index.to_string());
            std::env::set_var(crate::role_shard::SHARD_COUNT_ENV, count.to_string());
        }
    }

    impl Drop for ShardEnvGuard {
        fn drop(&mut self) {
            for (name, prev) in [
                (ROLE_RUNNER_ENABLE_ENV, &self.enable),
                (crate::role_shard::SHARD_INDEX_ENV, &self.index),
                (crate::role_shard::SHARD_COUNT_ENV, &self.count),
            ] {
                match prev {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    /// A role-runner-enabled tempdir workspace whose shard key is its own
    /// (random) basename — so a set of them stands in for a fleet of
    /// distinctly-keyed workspaces without needing real git remotes.
    fn enabled_workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);
        tmp
    }

    /// Whether one host (identified by the ambient shard env) would spend a
    /// curator tick on `root` this interval.
    fn tick_admitted(root: &Path) -> bool {
        let in_progress = new_in_progress_guard();
        let decision = decide_root_tick(
            root,
            &curator_spec(),
            &in_progress,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        decision.is_some()
    }

    /// AC1 (first half), at the dispatch surface rather than in the hash:
    /// across a two-host fleet, each workspace's curator tick is admitted by
    /// **exactly one** host per interval — never zero (the slice would go
    /// unrotated fleet-wide) and never two (the #6332 / #6352 duplication
    /// this issue exists to prevent).
    #[test]
    #[serial]
    fn two_host_fleet_admits_each_workspace_curator_tick_on_exactly_one_host() {
        let _env = ShardEnvGuard::capture();
        let fleet: Vec<tempfile::TempDir> = (0..12).map(|_| enabled_workspace()).collect();

        for workspace in &fleet {
            let root = workspace.path();
            let admitting: Vec<usize> = (0..2)
                .filter(|host| {
                    ShardEnvGuard::become_host(*host, 2);
                    tick_admitted(root)
                })
                .collect();
            assert_eq!(
                admitting.len(),
                1,
                "{} admitted by hosts {admitting:?}; exactly one host must run each workspace's \
                 role tick per interval (#6374)",
                root.display()
            );
        }
    }

    /// AC2, measured the way the incident measured it: the fleet-wide *count
    /// of role sessions spawned per interval*. Unsharded, a 4-host fleet
    /// spends 4 curator ticks per workspace; sharded, it spends 1 — the token
    /// draw scales with workspaces, not workspaces x hosts.
    #[test]
    #[serial]
    fn sharding_makes_the_fleet_wide_tick_draw_scale_with_workspaces_not_hosts() {
        let _env = ShardEnvGuard::capture();
        let fleet: Vec<tempfile::TempDir> = (0..12).map(|_| enabled_workspace()).collect();
        const HOSTS: usize = 4;

        // Unsharded (today's behavior, and the fail-safe fallback): every
        // host spends a tick on every workspace.
        let unsharded: usize = (0..HOSTS)
            .map(|_| fleet.iter().filter(|w| tick_admitted(w.path())).count())
            .sum();
        assert_eq!(unsharded, fleet.len() * HOSTS);

        // Sharded: the same fleet spends exactly one tick per workspace.
        let sharded: usize = (0..HOSTS)
            .map(|host| {
                ShardEnvGuard::become_host(host, HOSTS);
                fleet.iter().filter(|w| tick_admitted(w.path())).count()
            })
            .sum();
        assert_eq!(
            sharded,
            fleet.len(),
            "a {HOSTS}-host fleet drew {sharded} curator ticks for {} workspaces (#6374 AC2)",
            fleet.len()
        );
    }

    /// AC3: the blunt per-host kill switch keeps working, and keeps
    /// short-circuiting **before** sharding is consulted — so an operator who
    /// sets `LOOM_ROLE_RUNNER=0` gets zero ticks regardless of whether this
    /// host owns the slice. Asserted for the owning host specifically, since
    /// a non-owning host would skip for the wrong reason and prove nothing.
    #[test]
    #[serial]
    fn role_runner_env_zero_still_disables_the_host_that_owns_the_slice() {
        let _env = ShardEnvGuard::capture();
        let workspace = enabled_workspace();
        let root = workspace.path();

        let owner = (0..2)
            .find(|host| {
                ShardEnvGuard::become_host(*host, 2);
                tick_admitted(root)
            })
            .expect("exactly one of the two hosts owns this workspace");

        ShardEnvGuard::become_host(owner, 2);
        assert!(tick_admitted(root), "precondition: the owning host ticks");

        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
        assert!(
            !tick_admitted(root),
            "LOOM_ROLE_RUNNER=0 must still disable role ticks on the host that owns the slice \
             (#6374 AC3)"
        );
    }

    /// AC3, the other direction: sharding must not *weaken* the kill switch's
    /// counterpart either — an unsharded host (no shard env at all) behaves
    /// exactly as it did before #6374, owning every workspace.
    #[test]
    #[serial]
    fn an_unsharded_host_still_ticks_every_workspace() {
        let _env = ShardEnvGuard::capture();
        let fleet: Vec<tempfile::TempDir> = (0..6).map(|_| enabled_workspace()).collect();
        for workspace in &fleet {
            assert!(
                tick_admitted(workspace.path()),
                "an unsharded host must keep rotating every workspace (#6374 fail-safe)"
            );
        }
    }

    /// AC1 (second half), as far as this PR's **static** assignment goes:
    /// shrinking the ring reassigns the departed host's slice to the
    /// survivors, and no workspace is left unowned by the reassignment. This
    /// is the operator-driven reassignment path (lower `shardCount`, or point
    /// the survivor at the vacated index); automatic, roster-driven
    /// reassignment on host loss is deliberately deferred to #6704 — see
    /// `role_shard`'s module docs for why.
    #[test]
    #[serial]
    fn shrinking_the_ring_reassigns_the_departed_hosts_slice_to_the_survivor() {
        let _env = ShardEnvGuard::capture();
        let fleet: Vec<tempfile::TempDir> = (0..12).map(|_| enabled_workspace()).collect();

        // Host 1 dies. Its slice is exactly what host 0 was NOT ticking.
        ShardEnvGuard::become_host(0, 2);
        let orphaned: Vec<&Path> = fleet
            .iter()
            .map(tempfile::TempDir::path)
            .filter(|root| !tick_admitted(root))
            .collect();
        assert!(!orphaned.is_empty(), "precondition: host 1 must have owned something to orphan");

        // The operator shrinks the ring to the one survivor; every orphaned
        // workspace is picked up, and nothing is dropped in the process.
        ShardEnvGuard::become_host(0, 1);
        for root in &fleet {
            assert!(
                tick_admitted(root.path()),
                "{} must be rotated by the surviving host after the ring shrinks (#6374)",
                root.path().display()
            );
        }
    }

    /// The idle-edge dispatch surface (#4364) is sharded too. Without this,
    /// an idle-triggered role would duplicate across the fleet exactly as the
    /// interval cadence did, and the invariant would hold on only one of the
    /// two paths that spend tokens.
    #[test]
    #[serial]
    fn the_idle_edge_is_sharded_on_the_same_key_as_the_interval_tick() {
        let _env = ShardEnvGuard::capture();
        let workspace = enabled_workspace();
        let root = workspace.path();
        let cfg = on_idle_config(Some(true), vec!["champion"]);

        let owner = (0..2)
            .find(|host| {
                ShardEnvGuard::become_host(*host, 2);
                tick_admitted(root)
            })
            .expect("exactly one of the two hosts owns this workspace");

        // The owning host fires on the busy -> idle edge...
        ShardEnvGuard::become_host(owner, 2);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        let now = Instant::now();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        assert_eq!(
            plan_idle_runs(&mut t, &set, root, &cfg, true, false, now)
                .iter()
                .map(|(s, _)| s.name)
                .collect::<Vec<_>>(),
            vec!["champion"]
        );

        // ...and the peer, observing the same edge, does not.
        ShardEnvGuard::become_host((owner + 1) % 2, 2);
        let mut t = IdleTrigger::new();
        let set = new_in_progress_guard();
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
        assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
        assert!(
            plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty(),
            "a non-owning host must not fire an idle-triggered role (#6374)"
        );
    }

    // ===================================================================
    // Role-tick health ring (#4761)
    // ===================================================================

    #[test]
    #[serial(role_tick_ring)]
    fn recording_a_tick_makes_it_readable_cross_process() {
        reset_role_tick_ring();
        let at = chrono::Utc::now();
        record_role_tick_at("curator", Path::new("/r/loom"), &RoleTickOutcome::Success, at);

        let records = role_tick_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].role, "curator");
        assert_eq!(records[0].root, PathBuf::from("/r/loom"));
        assert_eq!(records[0].at, at);
        assert!(records[0].ok);
        assert_eq!(records[0].detail, None);
    }

    #[test]
    #[serial(role_tick_ring)]
    fn a_failure_records_its_reason() {
        reset_role_tick_ring();
        record_role_tick(
            "champion",
            Path::new("/r/loom"),
            &RoleTickOutcome::Failure("mcp preflight failed".to_string()),
        );
        let records = role_tick_records();
        assert!(!records[0].ok);
        assert_eq!(records[0].detail.as_deref(), Some("mcp preflight failed"));
    }

    /// #4642's permanent no-pool state must surface as NOT-ok — a role that
    /// cannot run at all is precisely what a health check exists to report.
    #[test]
    #[serial(role_tick_ring)]
    fn a_missing_token_pool_records_as_not_ok() {
        reset_role_tick_ring();
        record_role_tick("guide", Path::new("/r/loom"), &RoleTickOutcome::NoTokenPool);
        let records = role_tick_records();
        assert!(!records[0].ok);
        assert_eq!(records[0].detail.as_deref(), Some("no-token-pool"));
    }

    /// Issue #6637: unlike `NoTokenPool`/`ModelRuntimeMismatch` (permanent
    /// config defects), a load-saturated tick ceiling records as OK — it is
    /// a transient, self-clearing condition, not a role/machinery failure a
    /// health check must surface as degraded.
    #[test]
    #[serial(role_tick_ring)]
    fn a_load_skipped_tick_records_as_ok() {
        reset_role_tick_ring();
        record_role_tick(
            "auditor",
            Path::new("/r/loom"),
            &RoleTickOutcome::LoadSkipped {
                load_per_core: 1.8,
                detail: "cargo nextest run".to_string(),
            },
        );
        let records = role_tick_records();
        assert!(records[0].ok, "a load-skip must not be tallied as a failure");
        assert!(
            records[0]
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("1.80") && d.contains("cargo nextest run")),
            "{:?}",
            records[0].detail
        );
    }

    #[test]
    #[serial(role_tick_ring)]
    fn the_ring_is_bounded_and_keeps_the_newest_entries() {
        reset_role_tick_ring();
        for _ in 0..(ROLE_TICK_RING_CAPACITY + 10) {
            record_role_tick("curator", Path::new("/r/loom"), &RoleTickOutcome::Success);
        }
        record_role_tick(
            "curator",
            Path::new("/r/loom"),
            &RoleTickOutcome::Failure("newest".to_string()),
        );
        let records = role_tick_records();
        assert_eq!(records.len(), ROLE_TICK_RING_CAPACITY);
        assert_eq!(records.last().unwrap().detail.as_deref(), Some("newest"));
    }

    /// The log-dedup path (#4349) downgrades a *repeat* failure to DEBUG, but
    /// the health ring must still see every one of them — otherwise a
    /// persistently-broken root would look quiet to a health check.
    #[test]
    #[serial(role_tick_ring)]
    fn repeat_failures_are_all_recorded_even_though_the_log_dedups_them() {
        reset_role_tick_ring();
        let mut failing = HashMap::new();
        let mut no_pool = HashMap::new();
        let mut model_mismatch = HashMap::new();
        let root = PathBuf::from("/r/loom");
        for _ in 0..3 {
            log_outcome_for_root_deduped(
                "curator",
                &root,
                &RoleTickOutcome::Failure("boom".to_string()),
                Duration::from_secs(30),
                &mut failing,
                &mut no_pool,
                &mut model_mismatch,
            );
        }
        let records = role_tick_records();
        assert_eq!(records.len(), 3, "every tick is recorded, not just the fail edge");
        assert!(records.iter().all(|r| !r.ok));
    }

    /// #5028 AC2: a `ModelRuntimeMismatch` outcome records as NOT-ok with an
    /// operator-facing `detail()` string that names the broken config key —
    /// exactly what `assess_roles` in `health.rs` renders verbatim into
    /// `loom-daemon health`, so an operator learns the fix without reading a
    /// spawn transcript.
    #[test]
    #[serial(role_tick_ring)]
    fn a_model_runtime_mismatch_records_its_operator_facing_detail() {
        reset_role_tick_ring();
        record_role_tick("judge", Path::new("/r/loom"), &mismatch_outcome());
        let records = role_tick_records();
        assert!(!records[0].ok);
        let detail = records[0].detail.as_deref().unwrap();
        assert!(
            detail.contains("autonomous.roleRunner.roleModels.judge"),
            "detail must name the broken config key: {detail}"
        );
        assert!(detail.contains("model/runtime mismatch"), "detail: {detail}");
    }
}
