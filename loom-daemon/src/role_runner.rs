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
use crate::types::{RoleLastTick, RoleOnIdlePromotionStatus, RoleTickRecord};
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
/// [`RoleTickOutcome::PoolExhausted`] (issue #7607) — a distinct,
/// independently-attributable tally, deliberately never folded into the
/// generic [`RoleTickOutcome::Failure`] count a real invocation failure
/// increments, exactly like [`NO_TOKEN_POOL_SKIP_COUNT`]: a pool present but
/// fully exhausted (every account bad-marked or `.ranking`-hard-excluded) is
/// self-healing, not a code/config defect.
static POOL_EXHAUSTED_SKIP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Total number of role-runner ticks skipped so far because the resolved
/// token pool was present but had zero spawnable accounts (see
/// [`RoleTickOutcome::PoolExhausted`]). Exposed for tests and future status
/// surfacing; the daemon does not reset this across its lifetime.
#[must_use]
pub fn pool_exhausted_skip_count() -> u64 {
    POOL_EXHAUSTED_SKIP_COUNT.load(Ordering::Relaxed)
}

/// Sink for the fleet-wide empty-pool advisory feed (issue #7607): the role
/// runner calls this once per [`RoleTickOutcome::PoolExhausted`] tick so a
/// pool discovered exhausted by a *role* trips the same #6614 cross-source
/// brake (and its #5030 half-open dispatch gate) as one discovered by a sweep.
///
/// Injected as a trait object rather than taking the concrete
/// [`crate::workspace_pool::WorkspacePool`] so the multi-workspace loop stays
/// unit-testable without provisioning registries, reapers, and watchdogs — and
/// so a deployment that runs role loops with no sweep registry at all (the
/// `None` case at the call site) simply has no observer rather than a
/// half-wired one.
pub trait PoolExhaustedObserver: Send + Sync {
    /// Note that `role`'s tick for `root` skipped its spawn because `root`'s
    /// resolved token pool had zero spawnable accounts.
    ///
    /// Implementations MUST be cheap and non-blocking-ish: this runs inline on
    /// the role loop, on the tick outcome whose entire point is that it costs
    /// almost nothing.
    fn note_pool_exhausted(&self, root: &Path, role: &str);
}

impl PoolExhaustedObserver for crate::workspace_pool::WorkspacePool {
    fn note_pool_exhausted(&self, root: &Path, role: &str) {
        // Side-effect-free lookup on purpose (#7607): never provision a
        // registry for a workspace this daemon has not dispatched into —
        // see `WorkspacePool::provisioned_registry_for`.
        if let Some(registry) = self.provisioned_registry_for(root) {
            registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .record_role_tick_pool_exhausted(role);
        }
    }
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
// `RoleTickOutcome` itself lives in `role_runner/outcome.rs` and is re-exported
// at the bottom of this file — see that module's doc for why.

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
        // #7607: recorded as NOT ok, same as `NoTokenPool` — a role that
        // cannot run at all is exactly what a health check must surface —
        // but the `RoleTickRecord::pool_exhausted` flag set below routes a
        // SELF-HEALING hold into `crate::health::RoleTickSummary::
        // pool_exhausted` instead of `persistent`, so it is never counted as
        // (or mistaken for) an ordinary role failure. That hold's detail is
        // deliberately volatile (spawnable/total counts and the next-clear
        // estimate change every tick) — unlike `NoTokenPool`'s fixed
        // sentinel, this means it never accidentally builds an escalation
        // streak (`consecutive_identical_failures` below), which is correct:
        // pool exhaustion is expected to self-heal, not a config-shaped
        // defect that "can never succeed as configured". A PERMANENT hold
        // (#8444: nothing provisioned, or unreadable pool state) is the
        // config-shaped defect, and gets the opposite of both treatments —
        // see `RoleTickOutcome::pool_exhausted_detail`.
        RoleTickOutcome::PoolExhausted { .. } => (false, Some(outcome.pool_exhausted_detail())),
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
    // #7607: distinguishes a `PoolExhausted` skip from every other not-ok
    // outcome — see `RoleTickRecord::pool_exhausted`'s doc comment. #8444:
    // only the SELF-HEALING hold; the two permanent ones stay on the
    // persistent/escalatable path, where a `NoTokenPool` already sits.
    let pool_exhausted = outcome.self_healing_pool_hold();
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
        pool_exhausted,
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

    /// `(model, effort)` as resolved by the most recent [`invoke`](Self::invoke),
    /// or `None` when that invocation bailed out before resolving them (Issue
    /// #8056). Defaulted so a test double implementing only `invoke` is
    /// unaffected — a `None` here simply omits both keys from the emitted
    /// `role_tick.outcome` record, which is the honest reading.
    fn resolved_model_effort(&self) -> Option<(String, String)> {
        None
    }
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
    /// `(model, effort)` as actually resolved by the most recent [`invoke`]
    /// (Issue #8056) — recorded rather than re-read so the `role_tick.outcome`
    /// record can never disagree with what was launched (the #7894 unpinned
    /// reconciliation and the `with_model` test override both land here, and
    /// neither is visible to a fresh config read). `None` until an invocation
    /// gets past its pre-spawn preflights.
    ///
    /// [`invoke`]: RoleInvocationRunner::invoke
    resolved_model_effort: Option<(String, String)>,
    trace_context: Option<crate::telemetry::trace::TraceContext>,
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
            resolved_model_effort: None,
            trace_context: None,
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

mod invocation;

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
/// running on this workspace?". But all five of
/// [`ScriptRoleInvocationRunner::invoke`]'s pre-spawn preflight bail-outs —
/// unresolvable spawn bin, [`RoleTickOutcome::NoTokenPool`] (#4642),
/// [`RoleTickOutcome::PoolExhausted`] (#7607),
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

/// Run `spawn-claude.sh -p "<prompt>" --model <model> [--effort <level>]
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
    effort: &str,
    effort_source: &str,
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
    // Issue #8443: this tick's own unique anchor into its per-role log — the
    // timestamp opening this tick's header line below, reused after exit to
    // scope `provider_health_feedback`'s terminal-result scan to only this
    // dispatch, mirroring the sweep path's `sweep_id=` anchor (a fresh log
    // line above the anchor never leaks into an OLDER tick's scan, and this
    // tick's own scan never reads a STALE record left by a previous one).
    let tick_anchor = chrono::Utc::now().to_rfc3339();

    {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            // Rendered by `model_resolution::role_log_header` (#4501/#8054) —
            // the module that resolved the pair owns how it is reported.
            let _ = writeln!(
                f,
                "\n{}",
                model_resolution::role_log_header(
                    &tick_anchor,
                    role,
                    model,
                    model_source,
                    effort,
                    effort_source,
                )
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
    // Reasoning-effort pin (issue #8054): appended immediately after `--model`,
    // exactly as `sweep_registry::dispatch`'s spawn does (#3716), so the two
    // dispatch surfaces share one positional argv contract
    // (`-p`, `--model`, `--effort`, `--dangerously-skip-permissions`). An empty
    // value is treated as unset — `--effort ""` must NEVER be emitted: it would
    // clobber the session-default effort with nothing. Unconfigured is the
    // normal case, and it must leave the argv byte-identical to the pre-#8054
    // argv, which is why there is no shipped default effort to fall back on.
    if !effort.is_empty() {
        cmd.arg("--effort").arg(effort);
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

    crate::observability::lifecycle::role_command(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RoleTickOutcome::Failure(format!("could not spawn `{}`: {e}", script.display()))
        }
    };
    let pid = child.id();
    crate::observability::lifecycle::role_child_spawned(pid);

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                crate::observability::lifecycle::role_child_exited("success");
                // Issue #8443: feed this tick's own terminal record back into
                // account health BEFORE reporting success — mirrors the
                // sweep reaper calling `apply_provider_health_feedback`
                // ahead of any re-dispatch decision.
                provider_health_feedback::apply_role_tick_provider_health_feedback(
                    workspace_root,
                    &log_path,
                    admission,
                    &tick_anchor,
                    status.code(),
                );
                // Issue #8448: exit 0 is not, by itself, evidence that a
                // GUARDED NATIVE launch did anything. When this tick's own
                // native event stream shows the `loom_*` binding was never
                // offered, report the failed launch it actually was instead
                // of a healthy `Success`. A no-opinion result (any non-native
                // runtime, an unreadable log, an unparseable stream) leaves
                // the pre-#8448 behaviour byte-identical — see
                // `toolless_launch`'s module doc for the four conditions.
                if let Some(detail) = toolless_launch::detect(&log_path, admission, &tick_anchor) {
                    log::warn!("role_runner: {detail}");
                    return RoleTickOutcome::Failure(detail);
                }
                return RoleTickOutcome::Success;
            }
            Ok(Some(status)) => {
                crate::observability::lifecycle::role_child_exited(if status.code().is_some() {
                    "failure"
                } else {
                    "signal"
                });
                // Issues #6757/#8123: prefer a purpose-built failure sentinel
                // (naming the real cause and the role's own log path) over an
                // arbitrary tail-window fragment of stderr, when one is
                // present — see `describe_role_failure`.
                let full_log = read_role_log(&log_path);
                let detail = describe_role_failure(&full_log, &log_path);
                // Issue #8443: same terminal-record feedback on a non-zero
                // exit — this is the path a `TOKEN_EXHAUSTED` death actually
                // takes.
                provider_health_feedback::apply_role_tick_provider_health_feedback(
                    workspace_root,
                    &log_path,
                    admission,
                    &tick_anchor,
                    status.code(),
                );
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
/// [`find_failure_sentinel`]'s full-file search (issues #6757, #8123). Empty string
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
    /// `autonomous.roleRunner.onIdleMaxWait` — a per-role `{"<role>":
    /// "<duration>"}` map (issue #7511) naming the longest an
    /// [`on_idle`](Self::on_idle) role may go without a completed tick
    /// (tracked via [`last_role_tick_snapshot`]) before it is **promoted**
    /// into the next interval-cadence pass, admitted through the exact same
    /// [`RoleRunGuard::admit`] call (and therefore the same
    /// `maxConcurrent`/token/disk/RAM ceilings) as an ordinary interval
    /// role — see the promotion check in [`decide_root_tick`].
    ///
    /// Duration strings use a single trailing unit suffix — `s`/`m`/`h`/`d`
    /// (seconds/minutes/hours/days), e.g. `"24h"`, `"90m"` — parsed by
    /// [`parse_duration_suffix`]; there is no duration-parsing crate already
    /// pulled into this workspace, so this is a small hand-rolled parser
    /// rather than a new dependency. A key is dropped (not the whole field)
    /// when its value fails to parse (non-string, empty, unknown suffix, a
    /// leading number that doesn't parse as `u64`, or exactly `0` — a
    /// zero-length max-wait is rejected as almost certainly a typo, mirroring
    /// how [`max_concurrent`](Self::max_concurrent) and
    /// [`architect_max_proposals`](Self::architect_max_proposals) both treat
    /// `0` as malformed rather than a meaningful "disable" value). Keys are
    /// trimmed and lower-cased, matching [`role_models`](Self::role_models).
    ///
    /// `None` (key absent, or the whole value is a non-object) ⇒ **zero
    /// behavior change**: no role is ever promoted, matching today's
    /// idle-edge-only firing exactly. A role named here but NOT present in
    /// [`on_idle`](Self::on_idle) is never promoted either — promotion only
    /// ever applies to a role that is already configured to fire on the idle
    /// edge; naming it here without `onIdle` is inert, not an error.
    pub on_idle_max_wait: Option<BTreeMap<String, Duration>>,
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
    /// `autonomous.roleRunner.effort` — the reasoning effort every role child is
    /// pinned to (issue #8054), the role-runner counterpart of the sweep
    /// dispatch path's `--effort` param (#3716). `None` (key absent, blank, or
    /// non-string) means **unset**: [`run_role_with_timeout`] then emits no
    /// `--effort` argument at all and the runtime CLI's own session-default
    /// effort survives end-to-end. Deliberately **no** shipped default — unlike
    /// [`model`](Self::model), which must never fall through to an inherited CLI
    /// default, an inherited effort is exactly the pre-#8054 behaviour every
    /// unconfigured workspace must keep. Resolved by
    /// [`resolve_role_runner_effort`].
    pub effort: Option<String>,
    /// `autonomous.roleRunner.roleEfforts` — per-role effort overrides keyed by
    /// role name (issue #8054), each occupying a tier **above** the global
    /// [`effort`](Self::effort), exactly as
    /// [`role_models`](Self::role_models) sits above
    /// [`model`](Self::model). This is the config axis that lets a repo run the
    /// cheap bookkeeping roles (curator, guide) at a low effort while leaving
    /// the merge-gate roles (judge, doctor) alone. Keys are lower-cased and
    /// trimmed; blank keys and blank/non-string values are dropped **per
    /// entry**, so an entry never emits `--effort ""`. Absent / malformed /
    /// non-object soft-fails to an empty map (every role falls through to the
    /// global tier). Resolved by [`resolve_role_runner_effort`].
    pub role_efforts: BTreeMap<String, String>,
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

/// Hand-rolled duration-string parser for
/// [`RoleRunnerConfig::on_idle_max_wait`] (issue #7511): a decimal integer
/// followed by exactly one trailing unit suffix — `s` (seconds), `m`
/// (minutes), `h` (hours), or `d` (days) — e.g. `"24h"`, `"90m"`, `"7d"`.
///
/// No combined/compound forms (`"1h30m"`) and no bare-number-means-seconds
/// fallback — both would add ambiguity for a knob whose only two existing
/// config precedents (`intervalSecs`, a plain `u64`; `roleModels`, a plain
/// string) don't need a unit at all. `None` on: empty/whitespace-only input,
/// an unrecognized trailing character, a non-numeric leading component, or a
/// numeric value of exactly `0` (rejected as almost certainly a
/// fat-fingered typo rather than a deliberate "promote every tick", mirroring
/// how [`RoleRunnerConfig::max_concurrent`] and
/// [`RoleRunnerConfig::architect_max_proposals`] both treat `0` as malformed
/// rather than a meaningful value — an operator who genuinely wants "promote
/// as soon as possible" gets that today, for free, by simply never letting
/// the role tick at all: [`on_idle_role_promotion_due`]'s missing-snapshot
/// branch already treats "never ticked" as infinitely overdue).
#[must_use]
fn parse_duration_suffix(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (digits, unit_secs) = match s.as_bytes().last()? {
        b's' => (&s[..s.len() - 1], 1u64),
        b'm' => (&s[..s.len() - 1], 60u64),
        b'h' => (&s[..s.len() - 1], 3600u64),
        b'd' => (&s[..s.len() - 1], 86_400u64),
        _ => return None,
    };
    let n: u64 = digits.parse().ok()?;
    if n == 0 {
        return None;
    }
    n.checked_mul(unit_secs).map(Duration::from_secs)
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

    // `onIdleMaxWait` (#7511): a `{ "<role>": "<duration>" }` object. Keys are
    // trimmed + lower-cased (matching `roleModels` below); a blank key, a
    // non-string value, or a value [`parse_duration_suffix`] rejects is
    // dropped **per-entry** (the other, well-formed entries in the same
    // object still parse) — mirroring `roleModels`'s per-entry soft-fail
    // rather than `roles`/`onIdle`'s whole-field soft-fail, since a typo in
    // one role's max-wait should not silently disable every other role's.
    // Absent / non-object soft-fails to `None` for the whole field, which is
    // this knob's "zero behavior change" default (see the field's own doc
    // comment).
    let on_idle_max_wait = block
        .get("onIdleMaxWait")
        .and_then(serde_json::Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| {
                    let key = k.trim().to_ascii_lowercase();
                    if key.is_empty() {
                        return None;
                    }
                    let dur = v.as_str().and_then(parse_duration_suffix)?;
                    Some((key, dur))
                })
                .collect::<BTreeMap<String, Duration>>()
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

    // `effort` / `roleEfforts` (#8054) — parsed by the module that resolves
    // them (`role_runner/model_resolution.rs`), with the identical
    // trim/blank-is-unset/per-entry-soft-fail contract `roleModels` uses above.
    let (effort, role_efforts) = model_resolution::parse_effort_config(block);

    RoleRunnerConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        roles,
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        on_idle,
        on_idle_max_wait,
        model,
        role_models,
        effort,
        role_efforts,
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

/// Age since `role`'s last completed tick at `root`, per
/// [`last_role_tick_snapshot`] — `None` means this process has never
/// recorded a tick for this `(root, role)` pair at all (issue #7511's
/// "first registration" case), as opposed to `Some(age)` for any completed
/// tick, however recent. Reused by both the interval-cadence promotion check
/// ([`on_idle_role_is_promotable`]) and the `loom-daemon status`
/// age/promoted surface ([`resolve_on_idle_max_wait_status`]).
#[must_use]
fn role_tick_age(
    role: &str,
    root: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::Duration> {
    last_role_tick_snapshot()
        .into_iter()
        .find(|t| t.role == role && t.root == root)
        .map(|t| now.signed_duration_since(t.at))
}

/// Whether `age` (a role's time since its last completed tick, `None` =
/// never ticked) has reached or passed `max_wait` (issue #7511).
///
/// **A missing tick (`age == None`) is always overdue** — "first
/// registration" (no prior tick recorded at all) must be immediately
/// eligible for promotion, never permanently exempt; the issue's own
/// semantics call this out explicitly ("from the last completed tick, or
/// first registration").
///
/// A negative `age` (the recorded tick is, by wall-clock, in the future —
/// only plausible from clock skew) is treated as **not** overdue rather than
/// panicking or promoting: [`chrono::Duration::to_std`] fails on a negative
/// value, and the conservative reading of "a tick this process just saw
/// happen" is "fresh", not "infinitely stale".
#[must_use]
fn is_overdue(age: Option<chrono::Duration>, max_wait: Duration) -> bool {
    match age {
        None => true,
        Some(age) => age.to_std().unwrap_or(Duration::ZERO) >= max_wait,
    }
}

/// Whether `spec` should be **promoted** into this tick's interval-cadence
/// admission path for `root` (issue #7511) — i.e. it is absent from
/// `autonomous.roleRunner.roles` (already established by the caller) but:
/// (1) present in `autonomous.roleRunner.onIdle`, (2) has a configured
/// `autonomous.roleRunner.onIdleMaxWait` entry, and (3) its last completed
/// tick (or "never", per [`role_tick_age`]) is at or past that deadline.
///
/// A role named in `onIdleMaxWait` but NOT in `onIdle` never promotes — by
/// design (see [`RoleRunnerConfig::on_idle_max_wait`]'s doc comment):
/// promotion is only meaningful for a role already configured to fire on the
/// idle edge, so naming it here alone is inert rather than an error.
#[must_use]
fn on_idle_role_is_promotable(
    spec: &RoleSpec,
    config: &RoleRunnerConfig,
    root: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let on_idle = config.on_idle.as_deref().unwrap_or(&[]);
    if !on_idle.iter().any(|n| n == spec.name) {
        return false;
    }
    let Some(max_wait) = config
        .on_idle_max_wait
        .as_ref()
        .and_then(|m| m.get(spec.name))
    else {
        return false;
    };
    is_overdue(role_tick_age(spec.name, root, now), *max_wait)
}

/// Build the `loom-daemon status` age/promoted surface for `root` (issue
/// #7511 AC4): one [`RoleOnIdlePromotionStatus`] entry per
/// [`DEFAULT_ROLES`] member that has a configured `onIdleMaxWait` entry AND
/// is present in `onIdle` — i.e. exactly the set [`on_idle_role_is_promotable`]
/// ever evaluates for this root. A role with a configured max-wait but no
/// `onIdle` entry is omitted here too (nothing to report — it can never
/// promote, so surfacing it would misleadingly suggest it might).
#[must_use]
pub fn resolve_on_idle_max_wait_status(
    config: &RoleRunnerConfig,
    root: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<RoleOnIdlePromotionStatus> {
    let on_idle = config.on_idle.as_deref().unwrap_or(&[]);
    let Some(max_wait_map) = &config.on_idle_max_wait else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for spec in DEFAULT_ROLES {
        if !on_idle.iter().any(|n| n == spec.name) {
            continue;
        }
        let Some(max_wait) = max_wait_map.get(spec.name) else {
            continue;
        };
        let age = role_tick_age(spec.name, root, now);
        out.push(RoleOnIdlePromotionStatus {
            role: spec.name.to_string(),
            max_wait_secs: max_wait.as_secs(),
            age_secs: age.and_then(|a| a.to_std().ok()).map(|a| a.as_secs()),
            promoted: is_overdue(age, *max_wait),
        });
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
    if !shard.admits_role_tick() {
        log::debug!(
            "role_runner: idle edge for {} suppressed — {} (#6374/#6704)",
            root.display(),
            describe_shard_refusal(&shard)
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

/// Why a shard decision refused a role tick, for the `debug!` line both
/// dispatch surfaces emit.
///
/// Distinguishes the two refusals that look identical from the outside but
/// mean opposite things operationally: **another host owns this slice** (the
/// #6374 steady state — someone is rotating this workspace) versus **this host
/// is yielding under the roster fence** (#6704 — possibly *nobody* is
/// rotating it right now, by design, for a bounded window).
fn describe_shard_refusal(decision: &crate::role_shard::ShardDecision) -> String {
    match &decision.roster {
        crate::role_shard::RosterMode::Yield(reason) => {
            format!("the roster fence is yielding role ticks on this host: {}", reason.describe())
        }
        _ => "this workspace's role slice belongs to another host".to_string(),
    }
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
    if !shard.admits_role_tick() {
        log::debug!(
            "role_runner: {} tick for {} skipped — {} (#6374/#6704)",
            spec.name,
            root.display(),
            describe_shard_refusal(&shard)
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
        // #7511: a role absent from the interval-cadence resolved list is not
        // necessarily dead to this tick — an `onIdle` role whose configured
        // `onIdleMaxWait` deadline has passed with no idle-edge trigger is
        // *promoted* here, falling through into the exact same admission path
        // (`RoleRunGuard::admit` below) an ordinary interval role uses, so the
        // promoted tick still respects `maxConcurrent` and every other
        // interval-tick ceiling — no bypass.
        if on_idle_role_is_promotable(spec, &config, root, chrono::Utc::now()) {
            log::info!(
                "role_runner: {} promoted into the interval cadence for {} — its \
                 onIdleMaxWait deadline passed with no idle-edge trigger (#7511); admitting \
                 via the same RoleRunGuard::admit ceilings as any interval role",
                spec.name,
                root.display()
            );
        } else {
            log::debug!(
                "role_runner: {} not in autonomous.roleRunner.roles for {} — skipping",
                spec.name,
                root.display()
            );
            return None;
        }
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
///
/// `pool_exhausted_observer` (issue #7607) is notified once per
/// [`RoleTickOutcome::PoolExhausted`] tick so a token pool discovered
/// exhausted by a *role* feeds the same #6614 cross-source empty-pool brake as
/// one discovered by a sweep's `finish_issue_dispatch`. `None` disables the
/// feed entirely (the role loop is otherwise unchanged) — see
/// [`PoolExhaustedObserver`].
pub fn spawn_multi_role_task(
    spec: RoleSpec,
    fallback_root: PathBuf,
    interval: Duration,
    drain: std::sync::Arc<std::sync::atomic::AtomicBool>,
    in_progress: InProgressGuard,
    pool_exhausted_observer: Option<std::sync::Arc<dyn PoolExhaustedObserver>>,
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
        // Per-root pool-exhausted state (#7607), tracked completely
        // independently of `failing_roots`/`no_token_pool_roots` so a
        // present-but-fully-exhausted pool skip is never conflated with (or
        // silences the WARN for) either — see
        // `RootTickLogAction::is_pool_exhausted`.
        let mut pool_exhausted_roots: HashMap<PathBuf, bool> = HashMap::new();
        // Per-root model/runtime-mismatch state (#5028), tracked completely
        // independently of `failing_roots`/`no_token_pool_roots`/
        // `pool_exhausted_roots` so a permanent config-conflict skip is
        // never conflated with (or silences the WARN for) any of them — see
        // `RootTickLogAction::is_model_mismatch`.
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
                    let started_at = chrono::Utc::now();
                    // Cross-host collision detection (#4623) — detection only;
                    // the invocation itself is unchanged.
                    let outcome = invoke_with_collision_probe(
                        &mut runner,
                        &root_for_task,
                        name,
                        &prompt,
                        interval,
                    );
                    // Durable `role_tick.outcome` record (#8056). Emitted from
                    // inside this blocking task — it does filesystem and (at
                    // most one memoized) subprocess work — and best-effort by
                    // contract: it can never change whether the role keeps
                    // ticking.
                    crate::role_tick_telemetry::emit_for_tick_correlated(
                        &root_for_task,
                        name,
                        started_at,
                        &outcome,
                        runner.resolved_model_effort(),
                        runner.trace_context.clone(),
                    );
                    outcome
                })
                .await;
                let elapsed = tick_start.elapsed();
                match joined {
                    Ok(outcome) => {
                        feed_pool_exhausted_observer(
                            pool_exhausted_observer.as_deref(),
                            &outcome,
                            &root,
                            spec.name,
                        );
                        log_outcome_for_root_deduped(
                            spec.name,
                            &root,
                            &outcome,
                            elapsed,
                            &mut failing_roots,
                            &mut no_token_pool_roots,
                            &mut pool_exhausted_roots,
                            &mut model_mismatch_roots,
                        );
                    }
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

/// Feed the fleet-wide #6614 empty-pool brake from one role-tick outcome
/// (issue #7607) — a no-op for every outcome except a
/// [`RoleTickOutcome::PoolExhausted`] of the **Claude** pool, and for a `None`
/// observer. A dry codex account pool (#8408) never feeds it: the brake it
/// trips holds *sweep* dispatch on a token-selection wall, and sweeps do not
/// draw from the pool a codex-pinned role just found empty.
///
/// Called BEFORE the log-dedup decision in
/// [`log_outcome_for_root_deduped`], and deliberately on **every**
/// exhausted-pool tick rather than only the dedup's WARN edge: the brake does
/// its own distinct-source dedup, and refreshing its entry each tick is
/// exactly what keeps the source inside the trailing window for as long as the
/// pool stays dry. Suppressing repeats here would let the advisory age out
/// mid-outage.
fn feed_pool_exhausted_observer(
    observer: Option<&dyn PoolExhaustedObserver>,
    outcome: &RoleTickOutcome,
    root: &Path,
    role: &str,
) {
    if !outcome.exhausted_claude_token_pool() {
        return;
    }
    if let Some(observer) = observer {
        observer.note_pool_exhausted(root, role);
    }
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
        RoleTickOutcome::PoolExhausted { next_clear_at, .. } => {
            log::warn!(
                "role_runner: {role} tick skipped after {elapsed:.1?} — {}; next check ~{} (#7607)",
                outcome.pool_hold_phrase(),
                next_clear_at.to_rfc3339()
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
        RoleTickOutcome::PoolExhausted { next_clear_at, .. } => log::warn!(
            "role_runner: {role} tick for {} skipped after {elapsed:.1?} — {}; next check ~{} \
             (#7607)",
            root.display(),
            outcome.pool_hold_phrase(),
            next_clear_at.to_rfc3339()
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
    /// First tick with a present-but-fully-exhausted token pool (edge into
    /// this state, #7607): log at `WARN`. Distinct from both
    /// [`Self::FailureEdge`] and [`Self::NoTokenPoolEdge`] — this is a
    /// self-healing, shared-resource condition (not a code/config defect,
    /// and not "no pool provisioned at all"), so it must never be tallied as
    /// either.
    PoolExhaustedEdge,
    /// Repeat tick with a present-but-fully-exhausted token pool (already
    /// warned, #7607): downgrade to `DEBUG`, mirroring
    /// [`Self::NoTokenPoolRepeat`]'s dedup shape but tracked completely
    /// independently of it.
    PoolExhaustedRepeat,
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

    /// Whether this action should mark the root as pool-exhausted for the
    /// *next* tick's edge/repeat decision (#7607) — tracked independently of
    /// [`Self::is_failing`] and [`Self::is_no_token_pool`] so none of the
    /// three axes bleed into each other's dedup state.
    #[must_use]
    fn is_pool_exhausted(self) -> bool {
        matches!(self, Self::PoolExhaustedEdge | Self::PoolExhaustedRepeat)
    }

    /// Whether this action should mark the root as model-mismatched for the
    /// *next* tick's edge/repeat decision (#5028) — tracked independently of
    /// [`Self::is_failing`], [`Self::is_no_token_pool`], and
    /// [`Self::is_pool_exhausted`] so none of the four axes bleed into each
    /// other's dedup state.
    #[must_use]
    fn is_model_mismatch(self) -> bool {
        matches!(self, Self::ModelMismatchEdge | Self::ModelMismatchRepeat)
    }
}

#[must_use]
#[allow(clippy::too_many_arguments)]
fn classify_root_tick_log(
    outcome: &RoleTickOutcome,
    elapsed: Duration,
    was_failing: bool,
    was_no_token_pool: bool,
    was_pool_exhausted: bool,
    was_model_mismatch: bool,
) -> RootTickLogAction {
    match outcome {
        RoleTickOutcome::NoTokenPool if was_no_token_pool => RootTickLogAction::NoTokenPoolRepeat,
        RoleTickOutcome::NoTokenPool => RootTickLogAction::NoTokenPoolEdge,
        RoleTickOutcome::PoolExhausted { .. } if was_pool_exhausted => {
            RootTickLogAction::PoolExhaustedRepeat
        }
        RoleTickOutcome::PoolExhausted { .. } => RootTickLogAction::PoolExhaustedEdge,
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
/// [`RoleTickOutcome::NoTokenPool`] (#4642); `pool_exhausted` tracks, per root
/// and completely independently of both, whether the previous tick ended in
/// [`RoleTickOutcome::PoolExhausted`] (#7607); `model_mismatch` tracks, per
/// root and completely independently of all three, whether the previous tick
/// ended in [`RoleTickOutcome::ModelRuntimeMismatch`] (#5028) — see
/// [`RootTickLogAction`] for the per-transition logging rules.
#[allow(clippy::too_many_arguments)]
fn log_outcome_for_root_deduped(
    role: &str,
    root: &Path,
    outcome: &RoleTickOutcome,
    elapsed: Duration,
    failing: &mut HashMap<PathBuf, bool>,
    no_token_pool: &mut HashMap<PathBuf, bool>,
    pool_exhausted: &mut HashMap<PathBuf, bool>,
    model_mismatch: &mut HashMap<PathBuf, bool>,
) {
    // Record the raw outcome BEFORE the log-dedup decision (#4761): the
    // edge/repeat dedup exists to keep the *log* quiet, but a health check needs
    // every tick — a persistently-failing root logs at DEBUG after its first
    // WARN, which is exactly the case that must still surface as degraded.
    record_role_tick(role, root, outcome);
    let was_failing = failing.get(root).copied().unwrap_or(false);
    let was_no_token_pool = no_token_pool.get(root).copied().unwrap_or(false);
    let was_pool_exhausted = pool_exhausted.get(root).copied().unwrap_or(false);
    let was_model_mismatch = model_mismatch.get(root).copied().unwrap_or(false);
    let action = classify_root_tick_log(
        outcome,
        elapsed,
        was_failing,
        was_no_token_pool,
        was_pool_exhausted,
        was_model_mismatch,
    );
    let reason = match outcome {
        RoleTickOutcome::Failure(reason) => reason.as_str(),
        RoleTickOutcome::RuntimeRejected(rejection) => rejection.reason.as_str(),
        RoleTickOutcome::Success | RoleTickOutcome::NoTokenPool => "",
        RoleTickOutcome::PoolExhausted { .. } => "",
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
        RootTickLogAction::PoolExhaustedEdge => {
            if let RoleTickOutcome::PoolExhausted { next_clear_at, .. } = outcome {
                log::warn!(
                    "role_runner: {role} tick for {} skipped after {elapsed:.1?} — {}; next \
                     check ~{} (further identical skips for this root are logged at DEBUG until \
                     the pool regains capacity, #7607)",
                    root.display(),
                    outcome.pool_hold_phrase(),
                    next_clear_at.to_rfc3339()
                );
            }
        }
        // #8444: the clause names the pool that was actually gated (a codex
        // skip no longer reports "token pool") and the hold it is still
        // under — see `CredentialPool::repeat_phrase`.
        RootTickLogAction::PoolExhaustedRepeat => {
            log::debug!(
                "role_runner: {role} tick for {} skipped again after {elapsed:.1?} — {} (repeat \
                 of an already-logged skip; not re-warned every tick — see the skip-edge WARN \
                 above, #7607)",
                root.display(),
                outcome.pool_repeat_phrase()
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
    pool_exhausted.insert(root.to_path_buf(), action.is_pool_exhausted());
    model_mismatch.insert(root.to_path_buf(), action.is_model_mismatch());
}

// The per-invocation result type (#8056) — see `role_runner/outcome.rs`.
mod outcome;
pub use outcome::{CredentialPool, PoolHold, PoolStateFile, RoleTickOutcome};

// Failure-sentinel classification for a role's `role-<role>.log` (issues
// #6757, #8123) — see `role_runner/failure_sentinel.rs`.
mod failure_sentinel;
use failure_sentinel::describe_role_failure;

// Roster heartbeat task (Issue #7690 Phase A + #7691 Phase B of #6704) —
// see `role_runner/roster.rs`.
mod roster;
pub use roster::spawn_roster_heartbeat_task;

// Model resolution + runtime reconciliation (#4501/#5001/#7894) —
// see `role_runner/model_resolution.rs`.
mod model_resolution;
use model_resolution::reconcile_unpinned_model_with_runtime;
pub use model_resolution::{
    is_cli_default_model_sentinel, resolve_role_runner_effort, resolve_role_runner_model,
    ModelRuntimeMismatch, CLI_DEFAULT_MODEL_SENTINEL, UNSET_EFFORT_SOURCE,
};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

// `pub(crate)` since #8436: `runtime_preference::availability` asks the same
// "which pool does this runtime consume" question without the skip side
// effects, and reuses this module's codex reads rather than forking them.
pub(crate) mod runtime_preflight;

// Feeds a codex-runtime role tick's own `LOOM_TERMINAL_RESULT` record into
// account health (issue #8443) — the role-tick analogue of
// `sweep_registry::apply_provider_health_feedback`.
mod provider_health_feedback;

// Demotes an exit-0 guarded-native tick that never used a `loom_*` tool from
// `Success` to `Failure` (issue #8448) — see `role_runner/toolless_launch.rs`.
mod toolless_launch;
