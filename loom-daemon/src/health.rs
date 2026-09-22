//! `loom-daemon health` — one-shot consolidated fleet vitals (Issue #4761).
//!
//! # Why this module exists
//!
//! During the 2026-07-30→31 overnight fleet watch, 21 manual health ticks each
//! re-ran the same five-check battery by hand (4–6 shell commands per tick).
//! This module is that battery, collected once, structured, with an exit-code
//! contract a `watch` loop can branch on without parsing:
//!
//! | code | meaning |
//! |------|---------|
//! | `0`  | every section green |
//! | `1`  | degraded — at least one section is non-green for a reason other than the busy-timeout case below |
//! | `2`  | the daemon is **genuinely** dead |
//! | `3`  | busy, not confirmed unhealthy — see "Busy vs degraded" (#6191) below |
//!
//! # This module is a *collector*, not a set of new probes
//!
//! Every input comes from a source that already existed; nothing here invents a
//! second way to ask the same question:
//!
//! | section | source |
//! |---------|--------|
//! | liveness | [`crate::daemon_install_state`] (`probe()` + [`crate::daemon_install_state::pgrep_daemon_pids`]) plus the caller's IPC round-trip result |
//! | dispatch | [`crate::types::DaemonStatusReport`] + [`crate::work_finder::last_tick_summary`] |
//! | tokens | [`crate::types::CapacityReport`] + the resolved pool's `.ranking` mtime |
//! | roles | [`crate::role_runner::role_tick_records`] |
//! | queues | [`crate::pipeline_snapshot`] (`queued`) |
//! | throughput | [`crate::pipeline_snapshot`] (`merged_24h`, over the requested window) |
//! | operator_attention | [`crate::pipeline_snapshot`] (`operator_held`, `operator_held_conflicting`, `operator_held_oldest_days`, `operator_only_issues`) — **always [`Verdict::Green`]**, deliberately (Issue #8091, see [`assess_operator_attention`]) |
//! | peer_coordination | [`crate::types::DaemonStatusReport::safehouse`] (RPC socket reachability) + [`crate::types::DaemonStatusReport::peer_claims`]`.coordination` (published by [`crate::peer_claims::PeerClaimView::evaluate_coordination`], Issue #6157) |
//! | auto_update | [`crate::types::DaemonStatusReport::auto_update_*`] (the daemon-side rebuild loop's own state, Issue #4055) + [`crate::self_update::check`] (this CLI process's own source-vs-built-commit staleness magnitude, Issue #6261) — unconditional, mirroring `liveness`/`dispatch` (Issue #7584) |
//! | worktree_reaper | [`crate::types::DaemonStatusReport::stuck_worktree_reclaims`] (published by [`crate::worktree_reaper::stuck_worktree_removals`], Issue #7590) |
//! | pool_hold | [`crate::types::DaemonStatusReport::pool_exhaustion_holds`] (published by [`crate::work_finder::pool_preflight::active_hold_statuses`], Issues #7708/#7990) — its own bucket, deliberately NOT folded into `tokens` |
//! | observability *(only when non-green)* | [`crate::types::DaemonStatusReport::observability_host_id_mismatch`] (published by [`crate::observability::HostIdStatus`]) + [`crate::types::DaemonStatusReport::observability_export`] (published by [`crate::observability::ExportStatus`], #5083) |
//!
//! [`assess`] itself is **pure** — it takes an already-collected
//! [`HealthInputs`] and returns a [`HealthReport`] — so every verdict rule
//! (including the #4694 liveness precedence) is unit-testable with no daemon,
//! no forge, and no subprocess. The I/O side lives in `cli::health`, and the
//! dashboard's `GET /api/health` route calls this same [`assess`] rather than
//! re-deriving a verdict of its own.
//!
//! # Liveness precedence: pgrep + pid-file first, launchd NEVER alone (#4694)
//!
//! The single most important rule in this module. #4694's launchd domain probe
//! twice declared a live, *dispatching* daemon dead during the night watch; the
//! singleton guard was the only thing preventing a sweep-killing restart. So
//! [`assess_liveness`] declares [`Verdict::Dead`] only on **positive** evidence
//! of absence, in this order:
//!
//! 1. **IPC answered** ⇒ alive. Unconditionally. No local probe can overrule a
//!    daemon that just answered a round-trip.
//! 2. **`daemon_install_state` says the process is alive** (that classification
//!    is itself launchd-probe → skipped-domain cross-check → pid-file
//!    cross-check, i.e. it already refuses to trust a lone launchd negative)
//!    ⇒ alive-but-unresponsive: [`Verdict::Degraded`] for a hard IPC failure
//!    or a corroborating anomaly, [`Verdict::Unknown`] for a lone
//!    probe-budget-exceeded timeout (see "Probe-budget-exceeded vs
//!    confirmed-unhealthy", #6103, below) — either way, never dead.
//! 3. **`pgrep -x loom-daemon` finds a live process** ⇒ still not dead:
//!    [`Verdict::Degraded`]. This is the third independent signal, for the case
//!    where both launchd *and* the pid file are uninformative (a daemon started
//!    outside the managed wrapper, or a pid file removed by hand).
//! 4. Only when all three are negative is the verdict [`Verdict::Dead`] (exit
//!    `2`).
//!
//! An *undiagnosable* probe (no loom dir resolvable, `pgrep` absent) is
//! [`Verdict::Unknown`] — exit `1`, "I could not tell" — never `2`.
//!
//! # Probe-budget-exceeded vs confirmed-unhealthy (#6103)
//!
//! `cli::health`'s IPC round-trip is bounded far tighter than `status`'s
//! load-scaled 5-30s budget or the watchdog's 15s-per-tick /
//! 3-consecutive-failure budget (`loom-daemon-watchdog.sh`) — deliberately,
//! so a wedged daemon is reported fast rather than waited on. On a busy host
//! that tight budget alone used to manufacture a false alarm: a single
//! round-trip that simply did not complete in time against a daemon 29
//! straight watchdog ticks (and 5/5 immediate manual probes) confirmed was
//! healthy still flipped `overall` to `DEGRADED`/exit `1`. Reconciled without
//! just raising the number (which only shrinks the window, never closes it):
//! [`ipc_error_is_probe_timeout`] classifies a *timeout* apart from a harder
//! IPC failure, `cli::health::query_status` retries exactly once on that
//! classification before ever reporting a failure to this module, and
//! [`assess_liveness`]'s `AliveButUnresponsive` branch reports a lone
//! surviving timeout — with no other corroborating anomaly — as `Unknown`
//! ("could not determine"), not `Degraded` ("confirmed unhealthy"), so it
//! does not by itself flip `overall` to DEGRADED.
//!
//! # Busy vs degraded: `overall` distinguishes the two at the exit-code level (#6191)
//!
//! #6103 (above) kept a lone probe timeout from being *mislabeled* as a
//! confirmed fault at the section level, but it left one gap open: at the
//! **roll-up** level, `Verdict::Unknown` and `Verdict::Degraded` both mapped
//! to the same `exit_code()` (`EXIT_DEGRADED`, `1`). So the exact scenario
//! #6103 fixed — 8 sweeps in flight, host load 20–50, a daemon 29 straight
//! watchdog ticks confirmed healthy — still exited `1`, indistinguishable
//! from a genuine degradation to any caller that (reasonably) branches on the
//! exit code rather than parsing `--json`. Reconciled two ways:
//!
//! 1. `cli::health`'s IPC round-trip now escalates its retry budget (not
//!    just repeats the same one) when local, no-IPC evidence this collector
//!    already gathers — the process is alive AND its heartbeat is fresh, via
//!    [`alive_with_fresh_heartbeat`] — corroborates that the daemon is worth
//!    waiting a little longer for, rather than giving up at the same short
//!    budget a second time. Neither signal alone is enough: a stale or
//!    unreadable heartbeat gets no benefit of the doubt.
//! 2. When every non-green section is attributable to that same exhausted
//!    probe budget against a daemon [`alive_with_fresh_heartbeat`] already
//!    corroborates as running, [`assess`]'s roll-up reports `overall` as the
//!    distinct [`Verdict::IndeterminateBusy`] ("busy, not confirmed
//!    unhealthy") rather than the ordinary [`Verdict::Unknown`], at its own
//!    exit code ([`EXIT_INDETERMINATE_BUSY`], `3`) — so a watch loop (or the
//!    `/loom:watch` tick loop this was written for) can treat it as "try
//!    again shortly" without re-implementing the daemon-side watchdog's own
//!    consecutive-failure streak logic by parsing JSON. A single Unknown
//!    section for an unrelated reason (a missing `gh` binary, an
//!    undiagnosable liveness probe) still falls through to the ordinary
//!    `Unknown`/exit `1` — this distinction is deliberately narrow, not a
//!    general amnesty for "could not determine".
//!
//! # "Busy" has to actually mean busy (#8163)
//!
//! #6191's two mechanisms above were both keyed on a *fixed* budget, and the
//! daemon-side work they wait on ([`crate::ipc::build_daemon_status`]) is
//! `O(registered workspace roots)`. On a host with several dozen registered
//! workspaces the build measured `13.1s`/`14.3s` against a `10s` escalated
//! retry, so **every** `health` call on an idle, demonstrably healthy daemon
//! reported `indeterminate-busy` (exit `3`) with every section downstream of
//! liveness `unknown` — the consolidated probe was useless on exactly the
//! hosts that most need it, and the reported reason (host load) was false.
//! Reconciled on both sides:
//!
//! 1. The budget is derived from the registered root count instead of being
//!    a constant — one shared cost model in [`crate::status_budget`], used by
//!    the daemon to judge its own builds and by `cli::health` to size the
//!    escalated retry.
//! 2. The verdict now requires the host load to *corroborate* the busy story
//!    ([`busy::load_corroborates_busy`]). A timeout that survives a
//!    root-scaled budget on an idle host is not "busy"; it is unexplained,
//!    and the honest roll-up for unexplained is the ordinary `Unknown`. An
//!    unreadable load average cannot refute anything, so it preserves the
//!    pre-#8163 verdict exactly.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::capacity::model_class;
use crate::daemon_install_state::{HeartbeatFreshness, InstallState, InstallStateReport};
use crate::pipeline_snapshot::RepoPipelineSnapshot;
use crate::script_helpers::log_filter::strip_ansi;
use crate::types::{DaemonStatusReport, ObservabilityExportState, RoleTickRecord};

// ============================================================================
// Exit-code contract
// ============================================================================

/// Every section green.
pub const EXIT_HEALTHY: i32 = 0;
/// At least one section is non-green (degraded or undeterminable).
pub const EXIT_DEGRADED: i32 = 1;
/// The daemon is genuinely dead — all three independent liveness signals agree.
pub const EXIT_DEAD: i32 = 2;
/// Every non-green section is attributable to this collector's own IPC probe
/// budget being exhausted against a daemon local, no-IPC signals (process
/// alive, heartbeat fresh — [`alive_with_fresh_heartbeat`]) already
/// corroborate as running — "busy", not "degraded" (Issue #6191). Distinct
/// from [`EXIT_DEGRADED`] so a watch loop can treat this as "try again
/// shortly" rather than an alert, without re-implementing the daemon-side
/// watchdog's own consecutive-failure streak logic by parsing `--json`. See
/// the module-level "Busy vs degraded" doc section above.
pub const EXIT_INDETERMINATE_BUSY: i32 = 3;

/// Default report window (`--since`), used for the role-tick and throughput
/// sections.
pub const DEFAULT_WINDOW_SECS: u64 = 30 * 60;

/// How old the resolved pool's `.ranking` may be before the tokens section
/// reports it stale.
///
/// Six times the default refresh cadence
/// ([`crate::token_ranking_refresh::DEFAULT_TOKEN_RANKING_REFRESH_INTERVAL_SECS`],
/// 600s): generous enough that a couple of skipped refreshes (a rate-limit
/// cooldown, a slow probe) never cries wolf, tight enough that a refresh loop
/// that has actually stopped is caught within the hour — which matters because
/// a stale ranking silently pins the dynamic cap's token axis to a snapshot of
/// the past.
pub const RANKING_STALE_SECS: u64 = 6 * 600;

/// How many work-finder tick intervals a daemon process must have been running
/// before "no tick observed" counts as a fault (Issue #4824).
///
/// A freshly (re)started daemon legitimately has empty per-process tick
/// telemetry until its loop's first tick lands, so for up to one interval after
/// every `loom-daemon restart` — i.e. after every update roll — `health` used to
/// report `dispatch DEGRADED` and exit `1` on a perfectly healthy fleet, paging
/// whatever watchdog was scripted on it. Two intervals is the smallest window
/// that also absorbs one *overrun* tick (the loop measures the next interval
/// from when the previous tick's work finished) without absorbing a second
/// consecutive miss — a work finder that has actually stopped is still caught
/// within ~2 minutes on the default cadence.
pub const WORK_FINDER_TICK_GRACE_INTERVALS: u64 = 2;

/// The commit string a build with no git information available bakes in — the
/// tarball-install case (`build.rs` cannot run `git rev-parse`). Compared as a
/// value rather than matched structurally so both sides of the skew check treat
/// it as "cannot compare" instead of as a real commit that never matches.
const UNKNOWN_BUILD_COMMIT: &str = "unknown";

/// How many open `loom:review-requested` PRs a repo must be carrying before a
/// **zero-merge** window is read as a *review stall* rather than a quiet
/// moment (Issue #5021).
///
/// The pair of conditions is the point. Either alone is ordinary: a repo can
/// hold a few PRs awaiting review while Judge works through them, and an idle
/// window legitimately merges nothing (which is exactly why
/// [`assess_throughput`] treats zero merges as green). What is *not* ordinary
/// is a review queue this deep with nothing coming out the far end — the
/// direct, cause-agnostic observation of "review is not happening" that the
/// 2026-08-03 fleet-wide Judge outage produced on every repo and that the
/// `queued`-only assessment reported as green all day.
///
/// Three is the smallest backlog that cannot be explained by a single burst:
/// one Builder wave lands one or two PRs per repo, so a third simultaneously
/// unreviewed PR means at least one earlier PR was not picked up.
pub const REVIEW_STALL_MIN_BACKLOG: usize = 3;

// ============================================================================
// Verdicts
// ============================================================================

/// One section's health verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Healthy.
    Green,
    /// Non-green for a *known* reason.
    Degraded,
    /// Could not be determined (missing data, failed probe). Non-green — a
    /// watcher must never read "I could not tell" as "fine".
    Unknown,
    /// The daemon is genuinely not running. Only ever produced by
    /// [`assess_liveness`].
    Dead,
    /// Every non-green section is attributable to this collector's own IPC
    /// probe budget being exhausted against a daemon that local, no-IPC
    /// evidence already corroborates as alive and recently active — "busy",
    /// not "degraded" (Issue #6191, see the module-level "Busy vs degraded"
    /// doc section). Only ever produced at the [`HealthReport::overall`]
    /// roll-up ([`probe_budget_busy`]) — no individual `assess_*` section
    /// function produces this for its own `verdict` field.
    #[serde(rename = "indeterminate-busy")]
    IndeterminateBusy,
}

impl Verdict {
    /// The rendered label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Green => "GREEN",
            Verdict::Degraded => "DEGRADED",
            Verdict::Unknown => "UNKNOWN",
            Verdict::Dead => "DEAD",
            Verdict::IndeterminateBusy => "INDETERMINATE-BUSY",
        }
    }

    /// Whether this verdict is healthy.
    #[must_use]
    pub fn is_green(self) -> bool {
        matches!(self, Verdict::Green)
    }
}

/// One rendered section of the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthSection {
    /// Stable machine key (`liveness`, `dispatch`, `tokens`, `roles`,
    /// `queues`, `throughput`, and `observability` when a mismatch is present
    /// — #4830).
    pub key: &'static str,
    /// This section's verdict.
    pub verdict: Verdict,
    /// The one-line human summary.
    pub summary: String,
    /// Machine-readable specifics for `--json` consumers.
    pub detail: serde_json::Value,
}

impl HealthSection {
    fn new(
        key: &'static str,
        verdict: Verdict,
        summary: impl Into<String>,
        detail: serde_json::Value,
    ) -> Self {
        Self {
            key,
            verdict,
            summary: summary.into(),
            detail,
        }
    }
}

/// The consolidated report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthReport {
    /// When the report was collected.
    pub at: DateTime<Utc>,
    /// The `--since` window in seconds (role-tick + throughput sections).
    pub window_secs: u64,
    /// The rolled-up verdict.
    pub overall: Verdict,
    /// One entry per section, in render order.
    pub sections: Vec<HealthSection>,
}

impl HealthReport {
    /// The process exit code for this report — the whole point of the command.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self.overall {
            Verdict::Green => EXIT_HEALTHY,
            Verdict::Dead => EXIT_DEAD,
            Verdict::IndeterminateBusy => EXIT_INDETERMINATE_BUSY,
            Verdict::Degraded | Verdict::Unknown => EXIT_DEGRADED,
        }
    }

    /// Look up a section by key (convenience for tests and consumers).
    #[must_use]
    pub fn section(&self, key: &str) -> Option<&HealthSection> {
        self.sections.iter().find(|s| s.key == key)
    }

    /// Render the human report: one line per section plus a trailing overall
    /// line.
    #[must_use]
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        for section in &self.sections {
            out.push_str(&format!(
                "{:<11} {:<9} {}\n",
                section.key,
                section.verdict.as_str(),
                section.summary
            ));
        }
        out.push_str(&format!(
            "{:<11} {:<9} exit {} (window {})\n",
            "overall",
            self.overall.as_str(),
            self.exit_code(),
            format_window(self.window_secs)
        ));
        out
    }
}

/// Render a window in the same compact form `--since` accepts (`30m`, `2h`,
/// `90s`).
#[must_use]
pub fn format_window(secs: u64) -> String {
    if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Parse a `--since` value: a bare integer (seconds) or `<n>[smhd]`.
///
/// # Errors
/// Returns a message naming the rejected input when it is not a positive
/// duration in one of those forms.
pub fn parse_since(raw: &str) -> Result<Duration, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("--since requires a value (e.g. 30m, 2h, 90s)".to_string());
    }
    let (digits, mult) = match trimmed.chars().last() {
        Some('s') => (&trimmed[..trimmed.len() - 1], 1_u64),
        Some('m') => (&trimmed[..trimmed.len() - 1], 60),
        Some('h') => (&trimmed[..trimmed.len() - 1], 3600),
        Some('d') => (&trimmed[..trimmed.len() - 1], 86400),
        _ => (trimmed, 1),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("could not parse --since {raw:?}; expected e.g. 30m, 2h, 90s"))?;
    if n == 0 {
        return Err(format!("--since {raw:?} must be a positive duration"));
    }
    n.checked_mul(mult)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("--since {raw:?} overflows"))
}

// ============================================================================
// Inputs
// ============================================================================

/// Everything [`assess`] needs, already collected. Keeping this a plain data
/// struct is what makes every verdict rule testable without a daemon, a forge,
/// or a subprocess.
#[derive(Debug, Clone, Default)]
pub struct HealthInputs {
    /// Collection time (all ages/windows are measured from here).
    pub at: DateTime<Utc>,
    /// The `--since` window.
    pub window: Duration,
    /// The daemon's IPC status report, when the round-trip succeeded.
    pub status: Option<DaemonStatusReport>,
    /// Why the IPC round-trip failed, when it did.
    pub ipc_error: Option<String>,
    /// The local install-state classification (launchd + pid-file, #4694).
    /// `None` when no loom dir could be resolved at all — undiagnosable.
    pub install_state: Option<InstallStateReport>,
    /// Live `loom-daemon` pids from `pgrep` — the third liveness signal.
    pub pgrep_pids: Vec<u32>,
    /// A host-local observation of the daemon pid file (Issue #4774), taken
    /// against the path the *daemon* reported ([`DaemonStatusReport::pid_file`])
    /// when it was reachable, else this process's own resolution. `None` when
    /// no path could be resolved at all.
    ///
    /// The pid file is **advisory input, cross-checked but not trusted** —
    /// exactly the rule #4761's collector spec was refined to on 2026-07-31,
    /// after a two-relaunch-stale file and a name-matched `pgrep` hit a `/tmp`
    /// test stub in the same battery. [`assess_liveness`] never *derives*
    /// liveness from this field; it only reports the file's disagreement with
    /// the process that actually answered.
    pub pid_file: Option<crate::daemon_pidfile::PidFileObservation>,
    /// Whether a `.ranking` file was found in the resolved pool.
    pub ranking_present: bool,
    /// Age of the resolved pool's `.ranking` in seconds, when readable.
    pub ranking_age_secs: Option<u64>,
    /// Per-model-class healthy counts for the resolved pool (#8058 Phase 3),
    /// when its `.ranking` was readable.
    ///
    /// Collected client-side by the same collector that fills
    /// [`Self::ranking_present`]/[`Self::ranking_age_secs`] — one more read of
    /// the pool directory it already resolved, never a new probe. `None` (no
    /// readable ranking) and `Some` with an empty
    /// [`crate::capacity::model_class::ClassCapacity::by_class`] (a ranking
    /// with no class-scoped `.bad_tokens` state, the common case) both render
    /// as the exact pre-#8058 single number — see [`assess_tokens`].
    pub token_class_capacity: Option<crate::capacity::model_class::ClassCapacity>,
    /// Per-repo forge snapshot (`queued` + merged-in-window), when collected.
    /// `None` both when the daemon was unreachable (nothing to fan out to)
    /// and when [`Self::gh_unavailable`] is `Some` — the collector skips the
    /// per-repo fan-out entirely once it already knows every call would fail
    /// identically (#5061).
    pub pipeline: Option<Vec<RepoPipelineSnapshot>>,
    /// Set when the collector's one-time [`crate::pipeline_snapshot::probe_gh_availability`]
    /// check (#5061) found the client-side `gh` binary this process would use
    /// for the `queues`/`throughput` forge fan-out missing or non-executable.
    /// This is an **environment fact about this process**, not a forge
    /// outage — [`assess_queues`]/[`assess_throughput`] report it once,
    /// distinctly from a genuine per-repo forge query failure, and
    /// cross-reference [`Self::status`]'s `credential_preflight` (the
    /// daemon's own IPC-answered "is the forge credential OK" verdict) when
    /// available, so the two signals can never silently contradict each
    /// other the way they did before this field existed (a caller with no
    /// `gh` on `PATH` reporting "forge query FAILED for: <every repo>" next
    /// to a daemon reporting "Forge credential: OK").
    pub gh_unavailable: Option<crate::pipeline_snapshot::GhUnavailable>,
    /// The commit **this client process** was built from (Issue #4824) —
    /// [`crate::self_update::BUILT_COMMIT`], threaded in by the collector
    /// rather than read directly here so the skew rules are unit-testable
    /// against synthetic commit pairs. Compared against the daemon's
    /// [`DaemonStatusReport::daemon_build_commit`]; `"unknown"` (a tarball
    /// build) means "cannot compare", never "skew".
    pub cli_build_commit: String,
    /// Age in seconds of the newest `work_finder:` line in the daemon log, when
    /// the log was readable (Issue #4824).
    ///
    /// A **corroborating** signal only, in the same spirit as
    /// [`Self::pid_file`]: it is never used to *derive* dispatch health, only to
    /// refuse to declare the work finder dead while the daemon's own log shows
    /// it ticking. That is exactly the 2026-07-31 disagreement this exists for —
    /// `health` reporting "no work-finder tick observed" while `daemon.log`
    /// carried a `work_finder: tick —` line every ~60s. `None` when the log
    /// could not be resolved/read or carries no such line, which is treated as
    /// "no corroboration either way", never as evidence of death — and also
    /// when the collector did not bother to probe, which both collectors skip
    /// unless the daemon actually reported no tick.
    pub work_finder_log_tick_age_secs: Option<u64>,
    /// This CLI process's own read-only comparison of its built commit against
    /// the source checkout's HEAD ([`crate::self_update::check`], Issue
    /// #6261) — the same call `loom-daemon status --json`'s `.self_update`
    /// carries. Threaded in here (Issue #7584) so [`assess_auto_update`] can
    /// report the *staleness magnitude* (`commits_behind`/`hours_behind`)
    /// alongside the daemon-reported loop state
    /// ([`DaemonStatusReport::auto_update_note`] and friends) — no daemon-side
    /// wire change was needed since the magnitude is cheap to recompute
    /// locally on every invocation, exactly as `status` already does.
    /// `None` only in a fixture that never set it; the real collector always
    /// populates it.
    pub self_update: Option<crate::self_update::SelfUpdateStatus>,
    /// This CLI process's own one-time, non-interactive preflight of a
    /// configured `codesign.identity` (Issue #7605) — see
    /// [`CodesignPreflightResult`] for the full contract. `None` means
    /// "nothing to report", covering both "not configured" and "not
    /// applicable on this platform"; a fixture that never sets this field
    /// also reads as `None`, matching every other optional collector fact
    /// here.
    pub codesign_preflight: Option<CodesignPreflightResult>,
    /// This host's observed 1-minute load average **per logical core**
    /// ([`crate::cpu_headroom::load_per_core`]), read by the collector
    /// without any IPC (Issue #8163).
    ///
    /// A **corroborating** signal only, in the same spirit as
    /// [`Self::pid_file`] and [`Self::work_finder_log_tick_age_secs`]: no
    /// section derives its verdict from it. Its one job is to stop
    /// [`Verdict::IndeterminateBusy`] — literally "the host was too busy to
    /// answer in time" — from being asserted about an idle host whose probe
    /// merely outran a budget. `None` means "no reading available", which
    /// can neither support nor refute the busy story and therefore preserves
    /// the pre-#8163 verdict exactly; see [`busy::load_corroborates_busy`].
    pub load_per_core: Option<f64>,
    /// The collected limit-calibration reading (#8063, threaded by #8349):
    /// the fleet's $-equivalent cost per weekly-limit point and any step
    /// change detected against its trailing baseline. Computed by the
    /// `loom-daemon health` CLI collector via
    /// [`crate::limit_calibration::compute_with_fallback`] — claude-monitor's
    /// `usage_history` where present, else #8347's persisted
    /// `weekly_point_samples` joined by #8348's
    /// [`crate::activity::calibrate`] — and rendered by
    /// [`calibration_section::assess_limit_calibration`].
    ///
    /// `None` means "not collected": neither source was readable on this
    /// host, which reports no section at all (never a non-green line for the
    /// absence of an optional signal), the same rule
    /// [`Self::codesign_preflight`] follows.
    pub limit_calibration: Option<crate::limit_calibration::CalibrationStatus>,
    /// This host's Codex account reading (#8407), collected filesystem-only
    /// by the `loom-daemon health` CLI collector. `None` — a fixture that
    /// never set it, or a host with no resolvable workspace — and a snapshot
    /// with no accounts both render **no section at all**, so a Claude-only
    /// host's report is unchanged.
    pub codex_accounts: Option<codex_accounts::CodexAccountsSnapshot>,
    /// The collected transcript-ingest health snapshot (issue #8477):
    /// whether the background pass is enabled and whether it is keeping up
    /// with transcripts on disk. Computed by the `loom-daemon health` CLI
    /// collector via
    /// [`crate::activity::transcript_ingest::collect_health_status`] and
    /// rendered by
    /// [`transcript_ingest_section::assess_transcript_ingest`]. Unlike
    /// [`Self::limit_calibration`], `None` here means only "a fixture never
    /// set it" — the real collector always populates it, and (unlike an
    /// optional companion tool) an *off* reading still renders a non-green
    /// section rather than none at all, because ingestion being off is
    /// itself the fact this issue exists to surface.
    pub transcript_ingest: Option<crate::activity::transcript_ingest::IngestHealthStatus>,
    /// The collected tmpfs/`shared`-RAM + kernel OOM-kill snapshot (issue
    /// #8572, split from #8512). Computed by the `loom-daemon health` CLI
    /// collector via [`crate::tmpfs_visibility::collect`] and rendered by
    /// [`tmpfs_visibility_section::assess_tmpfs_visibility`]. `None` only
    /// means "a fixture never set it" — the real collector always populates
    /// it; the section itself renders nothing when the collected snapshot has
    /// nothing measurable (e.g. macOS, which has no `/proc` at all), the same
    /// silent-degrade contract [`crate::tmpfs_visibility`] documents.
    pub tmpfs_visibility: Option<crate::tmpfs_visibility::TmpfsVisibilitySnapshot>,
}

// ============================================================================
// Role-tick classification (transient vs persistent)
// ============================================================================

/// One `(root, role)` pair's failure state inside the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleFailure {
    /// The workspace root.
    pub root: PathBuf,
    /// The role name.
    pub role: String,
    /// How many ticks failed for this pair inside the window.
    pub failures: usize,
    /// The length of the *trailing* run of consecutive failures whose `detail`
    /// is byte-identical to `detail` below (the streak that ends at the most
    /// recent record). For a persistent pair this is `>= 1`; for a transient
    /// pair (whose latest record is a success) it is `0`. This is the basis for
    /// escalation (see [`RoleTickSummary::escalated`] and
    /// [`ROLE_TICK_ESCALATION_THRESHOLD`]): a single success — or a failure with
    /// a *different* detail — anywhere in the tail breaks the run and resets the
    /// count, so a self-recovering transient blip can never accumulate a streak.
    pub consecutive_identical: usize,
    /// When the most recent record for this pair landed.
    pub last_at: DateTime<Utc>,
    /// The most recent failure detail.
    pub detail: Option<String>,
}

impl RoleFailure {
    /// `<role> @ <root>` — the label rendered in the summary line.
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "{} @ {}",
            self.role,
            self.root.file_name().map_or_else(
                || self.root.display().to_string(),
                |n| n.to_string_lossy().into_owned()
            )
        )
    }
}

/// How many consecutive identical failures for one `(root, role)` pair escalate
/// it from ordinary "persistent" to "escalated" (#5023).
///
/// **Threshold rationale.** During the 2026-08-03 outage a config-shaped tick
/// failure (`LOOM_RUNTIME_JUDGE=codex`, which could never self-recover without a
/// config or code change) retried silently every interval, producing 6-7
/// consecutive identical failures per repo and ~96 wasted token selections + CLI
/// starts over the day. `5` sits below that observed 6-7 streak — so the same
/// outage would have escalated *before* it ran all day — while staying well
/// above any 1-2 tick transient blip (a slow forge, a one-off network error),
/// which the existing transient/persistent classifier already tolerates.
///
/// **"Identical failure" comparison basis.** Exact `detail`-string match (not a
/// coarser failure-class match): a config-shaped failure emits a byte-identical
/// `detail` every tick (same runtime-rejection reason, same `no-token-pool`
/// sentinel), whereas transient failures vary their detail or are interspersed
/// with successes. Exact-string is therefore the tightest basis that still
/// catches the config-shaped case (e.g. #5001's Codex runtime mismatch) without
/// false-positiving on a flapping transient error whose text differs tick to
/// tick. See [`RoleFailure::consecutive_identical`] for how the streak is
/// counted (and reset).
pub const ROLE_TICK_ESCALATION_THRESHOLD: usize = 5;

/// The windowed role-tick picture.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RoleTickSummary {
    /// Total tick records inside the window.
    pub total: usize,
    /// Successful tick records inside the window.
    pub ok: usize,
    /// `(root, role)` pairs whose **latest** record in the window is a failure
    /// — the only ones that surface as degraded.
    pub persistent: Vec<RoleFailure>,
    /// `(root, role)` pairs that failed at least once but whose latest record
    /// in the window is a success — self-recovered, reported as a count only.
    pub transient: Vec<RoleFailure>,
    /// The **subset** of `persistent` whose trailing run of consecutive
    /// identical failures has reached [`ROLE_TICK_ESCALATION_THRESHOLD`] (#5023)
    /// — a config-shaped failure that cannot self-recover and so must be
    /// surfaced loudly and distinctly, rather than retried identically forever.
    /// Escalated pairs remain in `persistent` too (they *are* persistent); this
    /// list is the "this can never succeed as configured" call-out layered on
    /// top. It empties automatically the moment such a pair ticks successfully
    /// again (the success breaks the streak), so a since-fixed cause never
    /// stays escalated.
    pub escalated: Vec<RoleFailure>,
    /// `(root, role)` pairs whose **latest** record in the window is a
    /// **self-healing** [`crate::role_runner::RoleTickOutcome::PoolExhausted`]
    /// skip (issue #7607) — the resolved pool was present but every account in
    /// it was under a hold that ages out on its own. A pool that is empty
    /// because nothing was ever provisioned, or whose state file will not
    /// parse, is NOT here (#8444): those never clear without an operator, so
    /// they stay in `persistent` (and escalate) like `NoTokenPool` —
    /// see [`crate::role_runner::PoolHold`]. Deliberately **disjoint** from
    /// `persistent`/`transient`:
    /// this is a self-healing, fleet-wide-shared-resource condition, not a
    /// per-role/per-repo defect, so it must never inflate the "N PERSISTENT
    /// failure(s)" count an operator reads as "N broken roles" (the exact
    /// masking effect the issue's incident report describes — 693 identical
    /// exit-78s reading as 693 broken roles).
    pub pool_exhausted: Vec<RoleFailure>,
}

/// Classify a role-tick window into persistent vs transient failures (#4761).
///
/// The rule, stated in the issue: *transient = self-recovered within the same
/// root's next tick; only persistent ones surface*. Concretely, for each
/// `(root, role)` pair inside the window: if its **most recent** record is a
/// failure, the pair is persistent; if it failed at some point but its most
/// recent record is a success, it is transient. A pair that never failed
/// appears in neither list.
///
/// This is deliberately a *client-side* classifier over raw records rather than
/// a daemon-side verdict: which window an operator cares about is their choice,
/// and the daemon's own log-dedup state (#4349's fail-edge/repeat map) is about
/// keeping the log quiet, not about health.
///
/// Records at or after `since` are considered; the rest are ignored. Both
/// output lists are sorted by `(root, role)` for stable rendering.
/// Max characters of a [`RoleFailure::detail`] retained in the **structured**
/// (non-summary) health output (issue #5024). `RoleTickRecord.detail` is
/// already ANSI-stripped and capped at the source by
/// `role_runner::clean_and_cap_detail`, but [`summarize_role_ticks`] cleans
/// it again here defensively — any future code path that lands a raw,
/// ANSI-laden `RoleTickRecord` (a differently-sourced failure, a replayed
/// record from an older binary, …) still cannot leak escapes or an unbounded
/// blob into `health --json`'s `roles.persistent[].detail` field. Deliberately
/// more generous than the tighter per-item cap folded into the one-line
/// `roles.summary` string (see `assess_roles`'s `MAX_SUMMARY_DETAIL_CHARS`) —
/// this is the fuller detail a `--json` consumer actually wants.
const MAX_STRUCTURED_DETAIL_CHARS: usize = 1000;

/// ANSI-strip and length-cap `detail` for storage in [`RoleFailure::detail`].
fn clean_structured_detail(detail: &str) -> String {
    let cleaned = strip_ansi(detail);
    let cleaned = cleaned.trim();
    if cleaned.chars().count() <= MAX_STRUCTURED_DETAIL_CHARS {
        return cleaned.to_string();
    }
    let capped: String = cleaned.chars().take(MAX_STRUCTURED_DETAIL_CHARS).collect();
    format!("{capped}… [truncated]")
}

#[must_use]
pub fn summarize_role_ticks(records: &[RoleTickRecord], since: DateTime<Utc>) -> RoleTickSummary {
    // BTreeMap keyed by (root, role) so the output order is deterministic.
    let mut by_pair: BTreeMap<(PathBuf, String), Vec<&RoleTickRecord>> = BTreeMap::new();
    let mut total = 0_usize;
    let mut ok = 0_usize;
    for record in records.iter().filter(|r| r.at >= since) {
        total += 1;
        if record.ok {
            ok += 1;
        }
        by_pair
            .entry((record.root.clone(), record.role.clone()))
            .or_default()
            .push(record);
    }

    let mut summary = RoleTickSummary {
        total,
        ok,
        ..Default::default()
    };
    for ((root, role), mut entries) in by_pair {
        entries.sort_by_key(|r| r.at);
        let failures = entries.iter().filter(|r| !r.ok).count();
        if failures == 0 {
            continue;
        }
        // `entries` is non-empty (it exists because at least one record was
        // pushed) and now sorted oldest-first, so the last element is the
        // pair's latest record in the window.
        let Some(latest) = entries.last() else {
            continue;
        };
        let detail = entries
            .iter()
            .rev()
            .find(|r| !r.ok)
            .and_then(|r| r.detail.as_deref())
            .map(clean_structured_detail);
        // Escalation streak (#5023): count the trailing run of consecutive
        // failures whose `detail` is byte-identical to the latest record's, from
        // newest backward. A success — or a failure carrying a *different*
        // detail — breaks the run, so this is `0` for a transient pair (latest
        // is a success) and resets to `0` the moment a persistent pair recovers.
        // Exact-string comparison is the deliberate basis (see
        // `ROLE_TICK_ESCALATION_THRESHOLD`). Deliberately compares the *raw*
        // record details rather than the cleaned/capped `detail` above (#5024):
        // capping could make two genuinely different failures compare equal
        // once truncated, inflating the streak.
        let consecutive_identical = if latest.ok {
            0
        } else {
            entries
                .iter()
                .rev()
                .take_while(|r| !r.ok && r.detail == latest.detail)
                .count()
        };
        let failure = RoleFailure {
            root,
            role,
            failures,
            consecutive_identical,
            last_at: latest.at,
            detail,
        };
        if latest.ok {
            summary.transient.push(failure);
        } else if latest.pool_exhausted {
            // Issue #7607: a SELF-HEALING pool-exhausted skip is deliberately
            // routed into its own disjoint bucket rather than `persistent` —
            // see `RoleTickSummary::pool_exhausted`'s doc comment. Never
            // considered for `escalated` either: the detail is intentionally
            // volatile tick to tick (see `record_role_tick_at`'s doc
            // comment), so it could not build a byte-identical streak even
            // if it were. #8444: the flag is `false` for a pool skip that can
            // never self-heal, so those fall through to the branch below and
            // escalate on their (deliberately stable) detail.
            summary.pool_exhausted.push(failure);
        } else {
            // Escalated pairs stay in `persistent` (they are persistent) AND are
            // additionally called out in `escalated` — the loud "config-shaped,
            // cannot self-recover" subset.
            if consecutive_identical >= ROLE_TICK_ESCALATION_THRESHOLD {
                summary.escalated.push(failure.clone());
            }
            summary.persistent.push(failure);
        }
    }
    summary
}

// ============================================================================
// Section assessments
// ============================================================================

/// Whether an IPC failure string represents the round-trip merely exceeding
/// its bounded probe budget — transient host contention that a slightly
/// wider budget or a retry can resolve — as opposed to a harder failure
/// (connection refused, an explicit daemon-side error, an unresolvable
/// socket path).
///
/// Pure string classification over the exact messages
/// `cli::common::query_daemon_bounded` produces (`"connect timed out after
/// {}s"` / `"round-trip timed out after {}s"`), so both `cli::health`'s
/// collector-side retry decision (Issue #6103 AC3) and [`assess_liveness`]'s
/// probe-budget-exceeded verdict (AC2) below classify the exact same
/// evidence the exact same way — they can never disagree about what "just a
/// timeout" means. No new wire field is needed to thread this signal through
/// [`HealthInputs`]: it is derived on demand from [`HealthInputs::ipc_error`],
/// which every existing test fixture already constructs.
#[must_use]
pub fn ipc_error_is_probe_timeout(err: &str) -> bool {
    err.contains("timed out")
}

/// Local, no-IPC-round-trip evidence (Issue #6191) that the daemon process is
/// both **alive** and has ticked its heartbeat **recently** — the two signals
/// [`crate::daemon_install_state::probe`] already collects without ever
/// touching the socket. Used two ways:
///
/// 1. `cli::health::query_status` — to decide whether an IPC timeout is worth
///    retrying at an escalated budget rather than the same short one a second
///    time.
/// 2. [`probe_budget_busy`] below — to decide whether an all-`Unknown` report
///    caused by that same exhausted budget should be reported as `overall:
///    "indeterminate-busy"` rather than the ordinary `"unknown"`.
///
/// Both signals must hold. A process that is merely alive (heartbeat stale,
/// unreadable, or the heartbeat loop disabled) is not enough corroboration —
/// a stale heartbeat is itself grounds for suspicion, not an excuse to wait
/// longer or downgrade the verdict.
#[must_use]
pub fn alive_with_fresh_heartbeat(install_state: Option<&InstallStateReport>) -> bool {
    install_state.is_some_and(|report| {
        report.pid.is_some() && report.heartbeat_freshness == Some(HeartbeatFreshness::Fresh)
    })
}

/// Assess the liveness section — the #4694-pinned precedence (see module docs).
#[must_use]
pub fn assess_liveness(inputs: &HealthInputs) -> HealthSection {
    // 1. A daemon that just answered IPC is alive. No local probe overrules it.
    if let Some(status) = &inputs.status {
        // The pid the daemon itself reported (#4774) is authoritative — it is
        // `std::process::id()` taken inside the answering process. Fall back to
        // the install-state probe's pid only for a pre-#4774 daemon.
        let socket_owner = status.daemon_pid;
        let pid = socket_owner.or_else(|| inputs.install_state.as_ref().and_then(|r| r.pid));

        // Cross-check the pid file against that owner (#4774 AC3). A daemon
        // that is demonstrably alive with a pid file naming someone else is
        // still GREEN on *liveness itself* — but the file is a booby trap for
        // every other consumer that reads it (the watchdog, the #4694
        // fallback, an operator's `cat`), so the section degrades and names it.
        let pid_state = inputs
            .pid_file
            .as_ref()
            .map(|obs| crate::daemon_pidfile::classify(obs, socket_owner));
        // `note()` is `None` for every non-anomalous verdict, so it is the
        // single gate on both the message and the degrade below — no separate
        // `is_stale()` test that could drift out of step with it.
        let stale_note = inputs
            .pid_file
            .as_ref()
            .zip(pid_state.as_ref())
            .and_then(|(obs, state)| state.note(&obs.path));

        let base = match pid {
            Some(p) => format!("daemon alive — IPC round-trip ok (pid {p})"),
            None => "daemon alive — IPC round-trip ok".to_string(),
        };
        let detail = serde_json::json!({
            "signal": "ipc",
            "ipc_reachable": true,
            "pid": pid,
            "socket_owner_pid": socket_owner,
            "pgrep_pids": inputs.pgrep_pids,
            "pid_file": inputs.pid_file.as_ref().map(|o| o.path.display().to_string()),
            "pid_file_recorded_pid": inputs.pid_file.as_ref().and_then(|o| o.recorded_pid),
            "pid_file_state": pid_state.as_ref().map(crate::daemon_pidfile::PidFileState::as_str),
        });

        return match stale_note {
            Some(note) => {
                HealthSection::new("liveness", Verdict::Degraded, format!("{base}; {note}"), detail)
            }
            None => HealthSection::new("liveness", Verdict::Green, base, detail),
        };
    }

    let ipc_error = inputs
        .ipc_error
        .clone()
        .unwrap_or_else(|| "unreachable".to_string());

    // 2. The install-state classification already refuses to trust a lone
    //    launchd negative (launchd → skipped-domain cross-check → pid file).
    if let Some(report) = &inputs.install_state {
        let detail = report.liveness_detail.clone().unwrap_or_default();
        match report.state {
            InstallState::AliveStarting => {
                return HealthSection::new(
                    "liveness",
                    Verdict::Degraded,
                    format!(
                        "process alive, still STARTING (age {}s) — IPC not bound yet: {ipc_error}{}",
                        report.process_age_secs.unwrap_or_default(),
                        // A daemon that has not bound yet has not claimed the
                        // pid file yet either (#4774 writes it after the bind),
                        // so a disagreement here is expected-and-transient —
                        // reported, but not dressed up as a fault.
                        suffix_note(report.pid_file_stale_note.as_deref())
                    ),
                    liveness_detail_json("install-state", report, inputs, false),
                );
            }
            InstallState::AliveButUnresponsive => {
                // #6103: a lone bounded-probe MISS against a daemon this
                // install-state classification already cross-checked as alive
                // (launchd → skipped-domain → pid-file, see the module-level
                // #4694 precedence doc) is not, by itself, evidence the daemon
                // is unhealthy — it is evidence this collector's probe budget
                // was exceeded, which a busy host can do to a perfectly fine
                // daemon (the reported incident: 29 consecutive watchdog OK
                // ticks + 5/5 immediate manual IPC probes against a daemon
                // `health` called DEGRADED). Report that case as `Unknown`
                // ("could not determine"), not `Degraded` ("confirmed
                // unhealthy"), so it does not by itself flip `overall` to
                // DEGRADED (see `assess`'s roll-up). `Degraded` is reserved for
                // a harder failure (`ipc_error_is_probe_timeout` says `false` —
                // e.g. `connect failed`, an explicit daemon error) or a second,
                // independently corroborating anomaly (a stale pid file) —
                // either of which is real evidence beyond "ran out of time".
                let probe_budget_exceeded =
                    ipc_error_is_probe_timeout(&ipc_error) && report.pid_file_stale_note.is_none();
                let verdict = if probe_budget_exceeded {
                    Verdict::Unknown
                } else {
                    Verdict::Degraded
                };
                let headline = if probe_budget_exceeded {
                    "process ALIVE, IPC probe budget exceeded — NOT confirmed unhealthy"
                } else {
                    "process ALIVE but not answering IPC — NOT dead"
                };
                return HealthSection::new(
                    "liveness",
                    verdict,
                    format!(
                        "{headline} ({detail}); ipc: {ipc_error}{}",
                        suffix_note(report.pid_file_stale_note.as_deref())
                    ),
                    liveness_detail_json("install-state", report, inputs, false),
                );
            }
            InstallState::ExpectedButDead | InstallState::NotExpected => {
                // 3. Both launchd domains and the pid file came back negative.
                //    One more independent signal before declaring death.
                if !inputs.pgrep_pids.is_empty() {
                    return HealthSection::new(
                        "liveness",
                        Verdict::Degraded,
                        format!(
                            "no launchd/pid-file evidence ({detail}), but pgrep finds live \
                             loom-daemon pid(s) {:?} — NOT declaring dead; ipc: {ipc_error}",
                            inputs.pgrep_pids
                        ),
                        liveness_detail_json("pgrep", report, inputs, false),
                    );
                }
                let summary = if report.state == InstallState::NotExpected {
                    format!(
                        "daemon DEAD — no autonomy-desired marker, no live pid file, no \
                         loom-daemon process (deliberately stopped?); ipc: {ipc_error}"
                    )
                } else {
                    format!(
                        "daemon DEAD — marker present (started {}) but {detail}, and no \
                         loom-daemon process; ipc: {ipc_error}",
                        report.started_at.as_deref().unwrap_or("unknown")
                    )
                };
                return HealthSection::new(
                    "liveness",
                    Verdict::Dead,
                    summary,
                    liveness_detail_json("all-negative", report, inputs, true),
                );
            }
        }
    }

    // 4. Undiagnosable: no install-state classification at all. `pgrep` is the
    //    only remaining signal, and its absence is not evidence of death.
    if inputs.pgrep_pids.is_empty() {
        HealthSection::new(
            "liveness",
            Verdict::Unknown,
            format!(
                "could not classify liveness (no loom dir resolvable) and no loom-daemon \
                 process found — NOT declaring dead without a marker/pid-file verdict; ipc: \
                 {ipc_error}"
            ),
            serde_json::json!({
                "signal": "undiagnosable",
                "ipc_reachable": false,
                "ipc_error": ipc_error,
                "pgrep_pids": inputs.pgrep_pids,
            }),
        )
    } else {
        HealthSection::new(
            "liveness",
            Verdict::Degraded,
            format!(
                "could not classify liveness (no loom dir resolvable), but pgrep finds live \
                 loom-daemon pid(s) {:?}; ipc: {ipc_error}",
                inputs.pgrep_pids
            ),
            serde_json::json!({
                "signal": "pgrep",
                "ipc_reachable": false,
                "ipc_error": ipc_error,
                "pgrep_pids": inputs.pgrep_pids,
            }),
        )
    }
}

/// Append an operator-facing pid-file note (#4774) to a summary line, or
/// nothing at all when there is no anomaly — so the overwhelmingly common
/// healthy case reads exactly as it did before this issue.
fn suffix_note(note: Option<&str>) -> String {
    note.map(|n| format!("; {n}")).unwrap_or_default()
}

fn liveness_detail_json(
    signal: &str,
    report: &InstallStateReport,
    inputs: &HealthInputs,
    dead: bool,
) -> serde_json::Value {
    serde_json::json!({
        "signal": signal,
        "ipc_reachable": false,
        "ipc_error": inputs.ipc_error,
        "install_state": report.state.as_str(),
        "liveness_detail": report.liveness_detail,
        "pid": report.pid,
        "process_age_secs": report.process_age_secs,
        "pgrep_pids": inputs.pgrep_pids,
        "declared_dead": dead,
        // #4774: the pid file as *evidence*, never as the verdict. On this
        // unreachable path there is no `daemon_pid` to arbitrate with, so the
        // note (when any) comes from the install-state probe's launchd
        // cross-check, and the raw observation is carried for `--json`.
        "pid_file": inputs.pid_file.as_ref().map(|o| o.path.display().to_string()),
        "pid_file_recorded_pid": inputs.pid_file.as_ref().and_then(|o| o.recorded_pid),
        "pid_file_recorded_pid_alive": inputs.pid_file.as_ref().map(|o| o.recorded_pid_alive),
        "pid_file_stale_note": report.pid_file_stale_note,
    })
}

// ============================================================================
// #4824 — telling CLI/daemon build skew and a warming-up daemon apart from a
// dead work finder
// ============================================================================

/// The comparison between this client's build commit and the running daemon's
/// ([`DaemonStatusReport::daemon_build_commit`], #4824).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "daemon_commit")]
pub enum BuildSkew {
    /// Both commits are known and identical — the client is talking to a daemon
    /// built from its own source. Only in this state can a client read the
    /// daemon's absent telemetry as a real fault.
    Match,
    /// Both commits are known and differ. The daemon may simply predate the
    /// telemetry the client expects.
    Skew(String),
    /// The daemon reported no commit at all ⇒ it predates #4824, so it also
    /// predates every field added since. Indistinguishable from `Skew` for
    /// health purposes, but reported separately because there is no sha to name.
    DaemonUnknown,
    /// No comparison is possible — this client (or the daemon) is a build with
    /// no git commit baked in. Never claim skew from a non-comparison.
    Incomparable,
}

impl BuildSkew {
    /// Whether the two builds are known to differ (or cannot be shown to
    /// match), i.e. whether missing daemon-side telemetry might be explained by
    /// the daemon binary predating it.
    #[must_use]
    pub const fn may_predate_client(&self) -> bool {
        matches!(self, Self::Skew(_) | Self::DaemonUnknown)
    }
}

/// Compare a client's build commit against the daemon-reported one (#4824).
///
/// `"unknown"` on either side (a tarball build with no git information) yields
/// [`BuildSkew::Incomparable`] rather than a false skew.
#[must_use]
pub fn classify_build_skew(cli_commit: &str, daemon_commit: Option<&str>) -> BuildSkew {
    if cli_commit.is_empty() || cli_commit == UNKNOWN_BUILD_COMMIT {
        return BuildSkew::Incomparable;
    }
    match daemon_commit {
        None => BuildSkew::DaemonUnknown,
        Some(c) if c.is_empty() || c == UNKNOWN_BUILD_COMMIT => BuildSkew::Incomparable,
        Some(c) if c == cli_commit => BuildSkew::Match,
        Some(c) => BuildSkew::Skew(c.to_string()),
    }
}

/// Why the daemon reported no work-finder tick (#4824).
///
/// Only [`MissingTick::Dead`] is a fault. The other three are the false-DEGRADED
/// modes that made `health` exit `1` on a demonstrably dispatching fleet, each
/// reported as its own condition instead of being flattened into "the work
/// finder is dead".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum MissingTick {
    /// The daemon has not been up long enough for a tick to be due yet.
    WarmingUp {
        /// The live daemon process's age in seconds.
        age_secs: u64,
        /// The grace window this age was compared against.
        grace_secs: u64,
    },
    /// The client and daemon were built from different commits, so the daemon
    /// binary may simply predate the tick telemetry the client is looking for.
    BuildSkew {
        /// The daemon's build commit, when it reported one.
        daemon_commit: Option<String>,
    },
    /// The status report carries no tick, but the daemon log shows recent
    /// `work_finder:` activity — the loop is running, the telemetry is not
    /// reaching the status report.
    LogCorroborated {
        /// Age in seconds of the newest `work_finder:` log line.
        age_secs: u64,
    },
    /// Nothing explains the silence: a matching build, a daemon well past the
    /// grace window, and no corroborating log activity.
    Dead,
}

/// The grace window (seconds) a missing tick is tolerated for after a daemon
/// (re)start: [`WORK_FINDER_TICK_GRACE_INTERVALS`] × the daemon's own resolved
/// tick interval, falling back to the default cadence for a pre-#4824 daemon
/// that does not report one.
#[must_use]
pub fn work_finder_grace_secs(status: &DaemonStatusReport) -> u64 {
    status
        .work_finder_interval_secs
        .filter(|s| *s > 0)
        .unwrap_or(crate::work_finder::DEFAULT_WORK_FINDER_INTERVAL_SECS)
        .saturating_mul(WORK_FINDER_TICK_GRACE_INTERVALS)
}

/// Classify a missing work-finder tick (#4824), in strict precedence order:
///
/// 1. **Warming up** — the daemon process is younger than the grace window, so
///    no tick is due yet. Checked first because it is the one explanation that
///    is true regardless of build state, and it is what an operator hits
///    immediately after every `loom-daemon restart`.
/// 2. **Build skew** — the client and daemon binaries differ (or the daemon
///    predates the field entirely), so its silence may just be an older binary
///    that never had the counter. A client cannot distinguish "older daemon"
///    from "dead loop" from the status payload alone, so it must not assert the
///    stronger claim.
/// 3. **Log corroboration** — the daemon log shows `work_finder:` activity
///    within the grace window. Advisory evidence only, and it can only *soften*
///    the verdict, never harden it.
/// 4. **Dead** — none of the above; report the fault.
#[must_use]
pub fn classify_missing_tick(inputs: &HealthInputs, status: &DaemonStatusReport) -> MissingTick {
    let grace_secs = work_finder_grace_secs(status);

    if let Some(age_secs) = inputs
        .install_state
        .as_ref()
        .and_then(|r| r.process_age_secs)
        .filter(|age| *age < grace_secs)
    {
        return MissingTick::WarmingUp {
            age_secs,
            grace_secs,
        };
    }

    let skew = classify_build_skew(&inputs.cli_build_commit, status.daemon_build_commit.as_deref());
    if skew.may_predate_client() {
        return MissingTick::BuildSkew {
            daemon_commit: match skew {
                BuildSkew::Skew(c) => Some(c),
                _ => None,
            },
        };
    }

    if let Some(age) = inputs
        .work_finder_log_tick_age_secs
        .filter(|age| *age <= grace_secs)
    {
        return MissingTick::LogCorroborated { age_secs: age };
    }

    MissingTick::Dead
}

/// Whether `disk_headroom` is the **strictly** binding term of the dynamic
/// concurrency cap `min(disk_headroom, ram_headroom, configured_max)` (#5177;
/// token axis removed / ram_headroom added in #5270 — see
/// [`crate::work_finder::resolve_dynamic_max_concurrent`]).
///
/// True when disk headroom is at most RAM headroom (a tie resolves to disk,
/// matching [`crate::calibrate::binding_term`]'s tie-break order) AND
/// strictly smaller than the operator's configured admission ceiling — i.e.
/// the scratch volume is throttling dispatch below what `configured_max`
/// would otherwise allow. A tie **with the ceiling** is deliberately NOT
/// flagged: that ceiling is the operator's own deliberate choice, not a disk
/// fault. Mirrors the input shape of
/// [`crate::work_finder::resolve_dynamic_max_concurrent`] so the two agree on
/// what "binding" means.
#[must_use]
pub fn disk_binds_cap(disk_headroom: usize, ram_headroom: usize, configured_max: usize) -> bool {
    disk_headroom <= ram_headroom && disk_headroom < configured_max
}

/// Whether `ram_headroom` is the **uniquely** binding term of the dynamic
/// concurrency cap (#5270) — the RAM-headroom mirror of [`disk_binds_cap`].
///
/// True only when RAM headroom is strictly smaller than **both** the disk
/// headroom and the operator's configured admission ceiling. A tie with disk
/// resolves to [`disk_binds_cap`] instead (mirroring
/// [`crate::calibrate::binding_term`]'s tie-break order), keeping the two
/// predicates mutually exclusive so `assess_dispatch` never double-reports
/// the same throttling event under both names.
#[must_use]
pub fn ram_binds_cap(disk_headroom: usize, ram_headroom: usize, configured_max: usize) -> bool {
    ram_headroom < disk_headroom && ram_headroom < configured_max
}

/// Assess the dispatch section: in-flight occupancy against the dynamic cap,
/// plus the last work-finder tick's dispatch/skip-reason summary.
#[must_use]
pub fn assess_dispatch(inputs: &HealthInputs) -> HealthSection {
    let Some(status) = &inputs.status else {
        return unknown_section("dispatch", &no_status_reason(inputs));
    };

    let in_flight = status.in_flight.len();
    let cap = status.dynamic_cap;
    let mut degraded: Vec<String> = Vec::new();

    if status.work_finder_enabled == Some(false) {
        degraded.push("work finder DISABLED — no autonomous dispatch".to_string());
    }
    if status.main_health_gate_halted {
        degraded.push("main-health gate HALTED".to_string());
    }
    if status.draining {
        degraded.push("DRAINING".to_string());
    }
    if status.host_breaker.as_ref().is_some_and(|b| b.suppressed) {
        degraded.push("host-distress breaker TRIPPED".to_string());
    }
    if status
        .rate_limit_breaker
        .as_ref()
        .is_some_and(|b| b.suppressed)
    {
        degraded.push("GitHub rate-limit breaker TRIPPED".to_string());
    }
    if status.preflight_advisory_active {
        degraded.push(
            status
                .preflight_advisory_message
                .clone()
                .unwrap_or_else(|| "claude-wrapper preflight tripwire active".to_string()),
        );
    }
    // #5177: a small-disk host silently decays as merged worktrees leak their
    // multi-GB target/ dirs, until disk headroom becomes the binding term of the
    // cap and throttles the host to a fraction of its configured capacity. That
    // used to present as a green daemon dispatching at, say, cap 2 — a fault an
    // operator had to notice in the `min(...)` breakdown. Name it as degraded.
    if disk_binds_cap(status.disk_headroom, status.ram_headroom, status.configured_max) {
        degraded.push(format!(
            "disk headroom is throttling dispatch (cap {} bound by disk headroom {} — free scratch \
             disk; e.g. `loom-daemon clean --aggressive` / `--deep`)",
            cap, status.disk_headroom
        ));
    }
    // #5270: the RAM-headroom mirror of the #5177 disk case above — the second
    // "dumb mode" machine-headroom axis, so a critically-low-memory host is
    // named as degraded the same way a critically-low-disk host already is.
    if ram_binds_cap(status.disk_headroom, status.ram_headroom, status.configured_max) {
        degraded.push(format!(
            "RAM headroom is throttling dispatch (cap {} bound by ram headroom {} — free memory \
             or lower LOOM_PER_WORKTREE_RAM_GB, #5270)",
            cap, status.ram_headroom
        ));
    }
    // #5270: name the machine axis (max/disk/RAM/CPU) currently HOLDING new
    // admissions outright — the CPU saturation admission brake is a
    // point-in-time gate outside the `min(...)` cap formula (see
    // `crate::admission_brake`'s module docs for why), so it cannot be named
    // by `disk_binds_cap` / `ram_binds_cap` above; it is checked independently
    // here, mirroring the host-distress breaker check above it.
    if status.admission_brake.as_ref().is_some_and(|b| b.held) {
        let load = status
            .admission_brake
            .as_ref()
            .and_then(|b| b.load_per_core)
            .map_or_else(|| "n/a".to_string(), |l| format!("{l:.2}"));
        let threshold = status
            .admission_brake
            .as_ref()
            .map_or(0.0, |b| b.load_per_core_threshold);
        degraded.push(format!(
            "CPU saturation admission brake HOLDING new admissions (load {load}/core \u{2265} \
             {threshold:.2} — in-flight sweeps are untouched; releases automatically once load \
             drops, #5270/#4903)"
        ));
    }

    // #4824: why the tick is missing, when it is. `None` whenever a tick was
    // reported (or the loop is disabled — the DISABLED line above is already the
    // whole story) so `--json` never carries a reason for a non-condition.
    let mut missing_tick: Option<MissingTick> = None;
    let tick_line = match &status.last_work_finder_tick {
        Some(tick) => {
            if tick.errors > 0 {
                degraded.push(format!("{} dispatch error(s) last tick", tick.errors));
            }
            format!(
                "last tick {} ago: {}",
                format_age((inputs.at - tick.at).num_seconds()),
                tick.reason_summary()
            )
        }
        None => {
            // Only a fault when the loop is supposed to be running...
            if status.work_finder_enabled == Some(false) {
                "no work-finder tick observed".to_string()
            } else {
                // ...and only when nothing else explains the silence (#4824).
                // A newer CLI against an older daemon, and a daemon that has
                // not been up long enough for its first tick, are both states
                // in which "no tick" says nothing about the work finder — and
                // reporting them as a dead loop paged operators after every
                // update roll.
                let reason = classify_missing_tick(inputs, status);
                let line = match &reason {
                    MissingTick::WarmingUp {
                        age_secs,
                        grace_secs,
                    } => format!(
                        "no tick yet — daemon warming up (up {}, first tick due within {})",
                        format_age(i64::try_from(*age_secs).unwrap_or(i64::MAX)),
                        format_age(i64::try_from(*grace_secs).unwrap_or(i64::MAX)),
                    ),
                    MissingTick::BuildSkew { daemon_commit } => format!(
                        "no tick telemetry: daemon build {} ≠ CLI build {} — daemon predates \
                         tick telemetry; update via loom-daemon-update.sh",
                        daemon_commit.as_deref().unwrap_or("<unreported>"),
                        inputs.cli_build_commit,
                    ),
                    MissingTick::LogCorroborated { age_secs } => format!(
                        "no tick in status, but daemon log shows work_finder activity {} ago \
                         (tick telemetry not published)",
                        format_age(i64::try_from(*age_secs).unwrap_or(i64::MAX)),
                    ),
                    MissingTick::Dead => {
                        degraded.push(
                            "no work-finder tick observed in this daemon process".to_string(),
                        );
                        "no work-finder tick observed".to_string()
                    }
                };
                missing_tick = Some(reason);
                line
            }
        }
    };

    let summary = if degraded.is_empty() {
        format!("{in_flight} in-flight / cap {cap}; {tick_line}")
    } else {
        format!("{in_flight} in-flight / cap {cap}; {}; {tick_line}", degraded.join("; "))
    };
    let verdict = if degraded.is_empty() {
        Verdict::Green
    } else {
        Verdict::Degraded
    };
    HealthSection::new(
        "dispatch",
        verdict,
        summary,
        serde_json::json!({
            "in_flight": in_flight,
            "dynamic_cap": cap,
            "capacity_bound": status.capacity_bound,
            "work_finder_enabled": status.work_finder_enabled,
            "halted": status.main_health_gate_halted,
            "draining": status.draining,
            "last_tick": status.last_work_finder_tick,
            // #4824 — the build-skew / warm-up / log-corroboration evidence a
            // `--json` consumer needs to see *why* a missing tick was (or was
            // not) treated as a fault.
            "missing_tick": missing_tick,
            "daemon_build_commit": status.daemon_build_commit,
            "cli_build_commit": inputs.cli_build_commit,
            "work_finder_interval_secs": status.work_finder_interval_secs,
            "work_finder_log_tick_age_secs": inputs.work_finder_log_tick_age_secs,
            "issues": degraded,
        }),
    )
}

/// Assess the token-pool section: healthy/total, exhausted count, and
/// `.ranking` staleness.
#[must_use]
pub fn assess_tokens(inputs: &HealthInputs) -> HealthSection {
    let Some(status) = &inputs.status else {
        return unknown_section("tokens", &no_status_reason(inputs));
    };
    let cap = &status.capacity;
    let mut degraded: Vec<String> = Vec::new();

    if cap.total_accounts == 0 {
        degraded.push("EMPTY token pool".to_string());
    } else if cap.healthy_accounts == 0 {
        degraded.push("ZERO healthy accounts — dispatch is token-starved".to_string());
    }
    if !inputs.ranking_present {
        degraded.push("no .ranking — token axis falls back to the raw pool size".to_string());
    } else if let Some(age) = inputs.ranking_age_secs {
        if age > RANKING_STALE_SECS {
            degraded.push(format!(
                ".ranking STALE ({} old, threshold {})",
                format_age(age.try_into().unwrap_or(i64::MAX)),
                format_age(RANKING_STALE_SECS.try_into().unwrap_or(i64::MAX))
            ));
        }
    }

    let ranking_age = match (inputs.ranking_present, inputs.ranking_age_secs) {
        (true, Some(age)) => {
            format!("ranking {} old", format_age(age.try_into().unwrap_or(i64::MAX)))
        }
        (true, None) => "ranking present (age unknown)".to_string(),
        (false, _) => "no ranking".to_string(),
    };
    // Per-model-class breakdown (#8058 Phase 3). Empty — no class-scoped
    // `.bad_tokens` state, or no readable ranking — splices in as nothing, so
    // this line is byte-identical to its pre-#8058 form on every pool that has
    // never recorded a class-scoped hold. When it is non-empty it is the
    // answer to the question the single number cannot give: whether `2/20
    // healthy` means a dead pool or one class at its ceiling.
    let class_suffix = model_class::summary_suffix_of(inputs.token_class_capacity.as_ref());
    let base = format!(
        "{}/{} healthy{class_suffix} ({} exhausted), {ranking_age}",
        cap.healthy_accounts, cap.total_accounts, cap.exhausted_accounts
    );

    // Per-repo ranking staleness (#5269). `inputs.ranking_present`/
    // `ranking_age_secs` above cover only the daemon's single
    // `fallback_root`-anchored pool (`status.token_pool_dir`) — on a
    // multi-repo daemon that can be a *different* repo's pool than the one an
    // operator asking about a specific registered repo actually cares about.
    // `status.per_repo` carries each registered repo's OWN resolved pool +
    // staleness (`RepoStatus::token_pool_dir`/`ranking_present`/
    // `ranking_age_secs`, populated via the unanchored
    // `resolve_tokens_dir(&repo.root)` — the same resolution
    // `token_ranking_refresh.rs`'s self-refresh loop uses), independent of
    // the daemon's launch CWD. Grouped by (repo, own pool age) rather than by
    // pool path — this is the answer to "is THIS repo's own pool fresh"
    // regardless of whether it happens to share a directory with another
    // registered repo's pool.
    let per_repo_detail: Vec<serde_json::Value> = status
        .per_repo
        .iter()
        .map(|r| {
            let stale = r.ranking_present
                && r.ranking_age_secs
                    .is_some_and(|age| age > RANKING_STALE_SECS);
            serde_json::json!({
                "root": r.root,
                "pool_path": r.token_pool_dir,
                "ranking_present": r.ranking_present,
                "ranking_age_secs": r.ranking_age_secs,
                "stale": stale,
            })
        })
        .collect();
    let affected_repos: Vec<&crate::types::RepoStatus> = status
        .per_repo
        .iter()
        .filter(|r| {
            !r.ranking_present
                || r.ranking_age_secs
                    .is_some_and(|age| age > RANKING_STALE_SECS)
        })
        .collect();
    if !affected_repos.is_empty() {
        // Bounded summary line (unlike the full `per_repo` JSON detail below,
        // which is always complete) — mirrors the roles section's
        // `MAX_ROLES_SUMMARY_LINE_CHARS` rationale: an operator with many
        // registered repos sharing one stale pool should see a short count in
        // the human summary, not a repeated per-repo essay.
        degraded.push(format!(
            "{} of {} registered repo(s) have their OWN pool's .ranking stale/missing (see tokens.per_repo detail)",
            affected_repos.len(),
            status.per_repo.len()
        ));
    }

    let (verdict, summary) = if degraded.is_empty() {
        (Verdict::Green, base)
    } else {
        (Verdict::Degraded, format!("{base}; {}", degraded.join("; ")))
    };
    HealthSection::new(
        "tokens",
        verdict,
        summary,
        serde_json::json!({
            "healthy": cap.healthy_accounts,
            // `class -> healthy` (#8058 Phase 3); `{}` when the pool carries
            // no class-scoped `.bad_tokens` state, so the pre-#8058 shape of
            // every other field is untouched and a consumer can read this one
            // unconditionally.
            "healthy_by_class": model_class::detail_of(inputs.token_class_capacity.as_ref()),
            // claude-monitor's predictive `class -> utilization` (issue
            // #8297); `{}` when `healthy_by_class` above is also `{}` (same
            // gate — see `capacity::model_class`'s degradation contract) or
            // no fresh monitor sidecar exists. Report-only: never consulted
            // by the selector, never changes `healthy`/`healthy_by_class`.
            "monitor_utilization_by_class": model_class::monitor_detail_of(inputs.token_class_capacity.as_ref()),
            "total": cap.total_accounts,
            "exhausted": cap.exhausted_accounts,
            "token_axis_limit": cap.token_axis_limit,
            "token_bound": cap.token_bound,
            // The single pool this section's headline numbers above are
            // scoped to — the daemon's `fallback_root`-anchored primary
            // workspace pool (#5269 AC2), NOT necessarily any particular
            // registered repo's own pool. See `per_repo` for that.
            "pool_path": status.token_pool_dir,
            "ranking_present": inputs.ranking_present,
            "ranking_age_secs": inputs.ranking_age_secs,
            "ranking_stale_threshold_secs": RANKING_STALE_SECS,
            "issues": degraded,
            "per_repo": per_repo_detail,
        }),
    )
}

/// Max characters of a single failure's detail folded into the one-line
/// `roles.summary` string (issue #5024). The full detail — already
/// ANSI-stripped and capped at the source by
/// `role_runner::clean_and_cap_detail` — still lives in the structured
/// `persistent[].detail` field of the section's JSON payload; only the
/// *summary line* gets this tighter cap so N simultaneously-failing
/// `(root, role)` pairs cannot multiply the line's length (the 2026-08-03
/// 12-repo outage produced a tens-of-kilobytes summary line before this fix).
const MAX_SUMMARY_DETAIL_CHARS: usize = 60;

/// Hard cap on the fully-assembled `roles.summary` line, applied after
/// joining every persistent failure's (already-capped) detail. A second line
/// of defense: even if a future code path folds many failures/details into
/// one line without going through [`MAX_SUMMARY_DETAIL_CHARS`], the summary
/// line itself still cannot grow unbounded.
const MAX_ROLES_SUMMARY_LINE_CHARS: usize = 2000;

/// ANSI-strip (defensive — the detail should already be clean from the
/// source) and length-cap `detail` for inline use in the `roles.summary`
/// line.
fn summary_detail(detail: &str) -> String {
    let cleaned = strip_ansi(detail);
    let cleaned = cleaned.trim();
    if cleaned.chars().count() <= MAX_SUMMARY_DETAIL_CHARS {
        return cleaned.to_string();
    }
    let capped: String = cleaned.chars().take(MAX_SUMMARY_DETAIL_CHARS).collect();
    format!("{capped}…")
}

/// Cap the fully-assembled `roles.summary` line length (see
/// [`MAX_ROLES_SUMMARY_LINE_CHARS`]).
fn cap_summary_line(line: String) -> String {
    if line.chars().count() <= MAX_ROLES_SUMMARY_LINE_CHARS {
        return line;
    }
    let capped: String = line.chars().take(MAX_ROLES_SUMMARY_LINE_CHARS).collect();
    format!("{capped}… [truncated]")
}

/// Assess the role-tick section: only *persistent* failures surface; transient
/// (self-recovered) ones are reported as a count so they are visible without
/// being alarming.
#[must_use]
pub fn assess_roles(inputs: &HealthInputs) -> HealthSection {
    let Some(status) = &inputs.status else {
        return unknown_section("roles", &no_status_reason(inputs));
    };
    let since = inputs.at
        - chrono::Duration::from_std(inputs.window).unwrap_or_else(|_| chrono::Duration::zero());
    let summary = summarize_role_ticks(&status.role_tick_records, since);

    if summary.total == 0 {
        return HealthSection::new(
            "roles",
            Verdict::Green,
            "no role ticks in window (role runner idle or disabled)",
            serde_json::json!({ "total": 0, "ok": 0, "persistent": [], "transient": [] }),
        );
    }

    let transient_note = if summary.transient.is_empty() {
        String::new()
    } else {
        format!("; {} transient (self-recovered)", summary.transient.len())
    };
    // Escalation call-out (#5023): a pair whose latest run of consecutive
    // identical failures has reached `ROLE_TICK_ESCALATION_THRESHOLD` is
    // config-shaped — it cannot self-recover without an operator config/code
    // change, so it is surfaced distinctly from ordinary persistent noise rather
    // than left to retry identically forever (burning a token slot each tick).
    // The verdict stays `Degraded` (no new tier), but the summary and the JSON
    // `escalated` list make the "actionable now" subset unmistakable.
    let escalated_note = if summary.escalated.is_empty() {
        String::new()
    } else {
        let names: Vec<String> = summary
            .escalated
            .iter()
            .map(|f| {
                let detail = f.detail.as_deref().unwrap_or("failed");
                format!(
                    "{} ({} consecutive identical: {detail})",
                    f.label(),
                    f.consecutive_identical
                )
            })
            .collect();
        format!(
            "; {} ESCALATED (>={ROLE_TICK_ESCALATION_THRESHOLD} consecutive identical failures — \
             config-shaped, cannot self-recover, needs an operator config/code change): {}",
            summary.escalated.len(),
            names.join(", ")
        )
    };
    // Issue #7607: a "pool exhausted (N role(s) held)" call-out, deliberately
    // worded so it can never be confused with "N PERSISTENT failure(s)" —
    // the exact masking `assess_roles` must not produce when a fleet-wide
    // shared token pool runs dry (693 identical exit-78 skips must read as
    // "pool exhausted", never as "693 broken roles").
    let pool_exhausted_note = if summary.pool_exhausted.is_empty() {
        String::new()
    } else {
        format!("; pool exhausted ({} role(s) held)", summary.pool_exhausted.len())
    };
    let (verdict, line) = if summary.persistent.is_empty() && summary.pool_exhausted.is_empty() {
        (
            Verdict::Green,
            format!("{}/{} ticks ok{transient_note}", summary.ok, summary.total),
        )
    } else if summary.persistent.is_empty() {
        // Only pool-exhausted skips this window, zero genuine role failures
        // — still `Degraded` (a fleet-wide exhausted pool is real,
        // operator-actionable information), but the summary text never says
        // "PERSISTENT failure(s)" for this state (#7607).
        (
            Verdict::Degraded,
            cap_summary_line(format!(
                "{}/{} ticks ok{pool_exhausted_note}{transient_note}",
                summary.ok, summary.total
            )),
        )
    } else {
        let names: Vec<String> = summary
            .persistent
            .iter()
            .map(|f| {
                let detail = f
                    .detail
                    .as_deref()
                    .map_or_else(|| "failed".to_string(), summary_detail);
                format!("{} ({} ticks, {detail})", f.label(), f.failures)
            })
            .collect();
        (
            Verdict::Degraded,
            cap_summary_line(format!(
                "{}/{} ticks ok; {} PERSISTENT failure(s): {}{escalated_note}{pool_exhausted_note}{transient_note}",
                summary.ok,
                summary.total,
                summary.persistent.len(),
                names.join(", ")
            )),
        )
    };
    HealthSection::new(
        "roles",
        verdict,
        line,
        serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null),
    )
}

// ============================================================================
// Role liveness (#6201) — "is this role ticking at all", not "did its last
// ticks succeed" (that question is `assess_roles` above).
// ============================================================================

/// Multiplier applied to a role's own expected tick interval before
/// [`assess_role_liveness`] treats its silence as abnormal (issue #6201 AC3:
/// "has not ticked within k× its interval"). `4` sits comfortably above
/// ordinary single-tick jitter (a skipped interval from the in-progress guard
/// (#4364), the concurrent-role-agent ceiling (#6102), or the GitHub
/// rate-limit cooldown (#4429) — each skips AT MOST the current tick, never
/// several in a row under normal operation) while still catching #6201's
/// actual incident (nine days of total silence for `curator`, whose 300s
/// interval makes even `4x` a 20-minute threshold) by more than three orders
/// of magnitude of margin.
pub const ROLE_LIVENESS_STALE_MULTIPLIER: u32 = 4;

/// One `(root, role)` pair [`assess_role_liveness`] flags as having gone
/// silent — configured to tick, has ticked before, but has not ticked in
/// over [`ROLE_LIVENESS_STALE_MULTIPLIER`]x its own expected interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StaleRole {
    /// The workspace root this role is configured to tick for.
    pub root: PathBuf,
    /// The role name.
    pub role: String,
    /// When this pair last completed a tick (of either outcome).
    pub last_tick_at: DateTime<Utc>,
    /// How long it has been silent, in seconds.
    pub silent_for_secs: u64,
    /// The role's own expected tick interval, in seconds (before the
    /// [`ROLE_LIVENESS_STALE_MULTIPLIER`] is applied).
    pub expected_interval_secs: u64,
}

impl StaleRole {
    /// `<role> @ <root>` — the label rendered in the summary line, matching
    /// [`RoleFailure::label`]'s shape.
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "{} @ {}",
            self.role,
            self.root.file_name().map_or_else(
                || self.root.display().to_string(),
                |n| n.to_string_lossy().into_owned()
            )
        )
    }
}

/// One `(root, role)` pair [`assess_role_liveness`] flags as **stuck**
/// (#6239) — it IS ticking on schedule (so it is not [`StaleRole`]), but its
/// last [`ROLE_TICK_ESCALATION_THRESHOLD`]-or-more consecutive ticks all
/// failed with a byte-identical detail: the exact shape of a pre-spawn skip
/// (`ModelRuntimeMismatch`, `NoTokenPool`, `RuntimeRejected`) that can never
/// self-recover without an operator config/code change, and that #6201's
/// staleness check alone reads as perfectly alive because it never stops
/// ticking. Deliberately excludes the **self-healing**
/// [`crate::role_runner::RoleTickOutcome::PoolExhausted`] hold (#7607): that
/// state's `detail` is intentionally volatile tick to tick (the spawnable
/// count and next-clear estimate change every check), so it can never build
/// a byte-identical streak here — correctly, since pool exhaustion is
/// expected to self-heal, unlike the three states above.
///
/// The two pool holds that are NOT self-healing (#8444 — no account
/// provisioned at all, or a pool state file that will not parse) are
/// deliberately *included*: they are the same "can never succeed as
/// configured" shape as the three states above, and their `detail` is
/// stable by construction so the streak forms. See
/// [`crate::role_runner::PoolHold`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StuckRole {
    /// The workspace root this role ticks for.
    pub root: PathBuf,
    /// The role name.
    pub role: String,
    /// When the (still-recent) failing tick landed.
    pub last_tick_at: DateTime<Utc>,
    /// The length of the trailing run of consecutive identical failures.
    pub consecutive_identical_failures: usize,
    /// The failure detail repeating every tick.
    pub detail: Option<String>,
}

impl StuckRole {
    /// `<role> @ <root>` — the label rendered in the summary line, matching
    /// [`RoleFailure::label`]'s shape.
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "{} @ {}",
            self.role,
            self.root.file_name().map_or_else(
                || self.root.display().to_string(),
                |n| n.to_string_lossy().into_owned()
            )
        )
    }
}

/// Assess whether any role-runner-enabled root's configured role has gone
/// **silent** ([`StaleRole`]) — has not ticked at all in over
/// [`ROLE_LIVENESS_STALE_MULTIPLIER`]x its own expected interval — OR is
/// **stuck** ([`StuckRole`], #6239) — is still ticking on schedule but has
/// failed identically on its last [`ROLE_TICK_ESCALATION_THRESHOLD`]-or-more
/// consecutive ticks — despite being configured to run there (issue #6201,
/// extended #6239).
///
/// This answers a different question than [`assess_roles`]: that section
/// classifies the *outcomes* of ticks that DID happen, within a bounded,
/// client-chosen window sourced from the shared, capacity-bounded tick ring
/// ([`crate::role_runner::role_tick_records`]) — a role that stops ticking
/// entirely, or that ticks on schedule but never does anything (a pre-spawn
/// skip, detected and recorded BEFORE any spawn — see
/// [`crate::role_runner::RoleTickOutcome::ModelRuntimeMismatch`]), while
/// several other roles/roots keep ticking normally, has its ring entries
/// evicted within hours, at which point that section sees zero (or too few)
/// records for the pair and reports a clean bill of health instead of the
/// silent or stuck gap (the exact incidents #6201 and #6239 were filed for).
/// This section instead reads [`DaemonStatusReport::role_last_tick`] — a
/// never-evicted last-tick timestamp AND outcome/streak per `(root, role)`
/// pair — cross-referenced against each registered root's own
/// `role_runner_enabled` + `role_runner_roles` (what SHOULD be ticking
/// there, populated by `role_runner::resolve_roles` regardless of whether
/// the root is currently enabled).
///
/// A `(root, role)` pair that has **never** ticked at all is deliberately
/// NOT flagged — it may simply have been enabled moments ago (a fresh config
/// edit, a newly-registered workspace), and this section has no daemon-uptime
/// signal to distinguish that from a role that has been broken since before
/// this process started. Only a pair with a KNOWN prior tick that has since
/// gone silent for the threshold, or is ticking but stuck, is reported, so
/// this can never false-positive on daemon startup or a config change alone.
/// A pair already reported stale is never also reported stuck — silence is
/// the stronger, more informative verdict for the same underlying pair.
#[must_use]
pub fn assess_role_liveness(inputs: &HealthInputs) -> HealthSection {
    let Some(status) = &inputs.status else {
        return unknown_section("role_liveness", &no_status_reason(inputs));
    };
    let mut stale: Vec<StaleRole> = Vec::new();
    let mut stuck: Vec<StuckRole> = Vec::new();
    let mut checked = 0_usize;
    for repo in &status.per_repo {
        if !repo.role_runner_enabled {
            continue;
        }
        for role_name in &repo.role_runner_roles {
            let Some(spec) = crate::role_runner::DEFAULT_ROLES
                .iter()
                .find(|s| s.name == role_name.as_str())
            else {
                // Unknown role name (should not happen — `role_runner_roles`
                // is itself filtered against `DEFAULT_ROLES` — but this
                // section must never panic on a future drift): nothing to
                // compare an interval against, so skip rather than guess.
                continue;
            };
            checked += 1;
            let Some(last_tick) = status
                .role_last_tick
                .iter()
                .find(|r| r.root == repo.root && r.role == *role_name)
            else {
                continue; // never observed a tick — not flagged (see doc comment)
            };
            // Issue #7238: compare against the interval the role runner
            // itself ACTUALLY resolved for this role (env override >
            // `autonomous.roleRunner.intervalSecs` > this role's own
            // built-in default — see `role_runner::resolve_interval_for_role`),
            // not the bare built-in `spec.default_interval_secs` alone.
            // Falls back to the built-in when this root's
            // `role_runner_intervals` map is absent/missing this role's
            // entry (pre-#7238 wire data from an older daemon), preserving
            // the exact pre-fix behavior in that case.
            let expected_interval_secs = repo
                .role_runner_intervals
                .get(role_name.as_str())
                .copied()
                .unwrap_or(spec.default_interval_secs);
            let threshold_secs =
                expected_interval_secs.saturating_mul(u64::from(ROLE_LIVENESS_STALE_MULTIPLIER));
            let silent_for = inputs.at - last_tick.at;
            let silent_for_secs = u64::try_from(silent_for.num_seconds()).unwrap_or(0);
            if silent_for_secs > threshold_secs {
                stale.push(StaleRole {
                    root: repo.root.clone(),
                    role: role_name.clone(),
                    last_tick_at: last_tick.at,
                    silent_for_secs,
                    expected_interval_secs,
                });
            } else if !last_tick.ok
                && last_tick.consecutive_identical_failures >= ROLE_TICK_ESCALATION_THRESHOLD
            {
                // #6239: still ticking on schedule (else it would be `stale`
                // above), but its never-evicted streak — immune to the
                // shared ring's eviction — shows it has done nothing but
                // repeat the identical failure for at least
                // `ROLE_TICK_ESCALATION_THRESHOLD` ticks running.
                stuck.push(StuckRole {
                    root: repo.root.clone(),
                    role: role_name.clone(),
                    last_tick_at: last_tick.at,
                    consecutive_identical_failures: last_tick.consecutive_identical_failures,
                    detail: last_tick.detail.clone(),
                });
            }
        }
    }
    if stale.is_empty() && stuck.is_empty() {
        return HealthSection::new(
            "role_liveness",
            Verdict::Green,
            format!("{checked} role/workspace pair(s) checked, none silent or stuck"),
            serde_json::json!({ "checked": checked, "stale": [], "stuck": [] }),
        );
    }
    let mut parts: Vec<String> = Vec::new();
    if !stale.is_empty() {
        let names: Vec<String> = stale
            .iter()
            .map(|s| {
                format!(
                    "{} (silent {}, expected every {}s)",
                    s.label(),
                    format_window(s.silent_for_secs),
                    s.expected_interval_secs
                )
            })
            .collect();
        parts.push(format!(
            "{} role(s) SILENT beyond {ROLE_LIVENESS_STALE_MULTIPLIER}x their interval: {}",
            stale.len(),
            names.join(", ")
        ));
    }
    if !stuck.is_empty() {
        let names: Vec<String> = stuck
            .iter()
            .map(|s| {
                let detail = s.detail.as_deref().unwrap_or("failed");
                format!(
                    "{} ({} consecutive identical: {detail})",
                    s.label(),
                    s.consecutive_identical_failures
                )
            })
            .collect();
        parts.push(format!(
            "{} role(s) STUCK (>={ROLE_TICK_ESCALATION_THRESHOLD} consecutive identical \
             pre-spawn failures, ticking but never actually running — config-shaped, cannot \
             self-recover, needs an operator config/code change): {}",
            stuck.len(),
            names.join(", ")
        ));
    }
    HealthSection::new(
        "role_liveness",
        Verdict::Degraded,
        parts.join("; "),
        serde_json::json!({ "checked": checked, "stale": stale, "stuck": stuck }),
    )
}

/// Add one optionally-observed count into a running total, **without** ever
/// inventing a zero.
///
/// `None` on the input means "this metric was not observed for this repo" —
/// the forge query failed, or the caller masked the metric off
/// ([`crate::pipeline_snapshot::PipelineMetrics`]). Either way it must not be
/// folded in as `0`, or an unobserved review queue would report as an empty
/// one: the precise misreading Issue #5021 exists to prevent. The accumulator
/// therefore stays `None` until at least one repo actually reports, and from
/// then on sums only the repos that did.
fn accumulate_observed(total: &mut Option<usize>, observed: Option<usize>) {
    if let Some(n) = observed {
        *total = Some(total.unwrap_or(0) + n);
    }
}

/// Whether this repo's review pipeline looks *stalled*: at least
/// [`REVIEW_STALL_MIN_BACKLOG`] PRs sitting in `loom:review-requested` while
/// the window merged nothing at all.
///
/// Both inputs must be *observed* (`Some`) — an unobserved axis yields `false`
/// ("cannot tell"), never a stall verdict and never a clean bill of health;
/// the enclosing section reports the unobserved axis as `null`/`?` so the gap
/// is visible rather than silently resolved either way.
fn is_review_stalled(snap: &RepoPipelineSnapshot) -> bool {
    matches!(
        (snap.review_requested, snap.merged_24h),
        (Some(backlog), Some(0)) if backlog >= REVIEW_STALL_MIN_BACKLOG
    )
}

/// Assess the queue-depth section: per-root ready (dispatchable `loom:issue`,
/// excluding park-labeled rows — see [`crate::pipeline_snapshot::RepoPipelineSnapshot::queued`])
/// counts, **plus** the review-side axes (`review_requested`,
/// `changes_requested`, `changes_requested_unclaimed`, `approved`) that the
/// same snapshot already carries.
///
/// The review axes are reported for their own sake and are also the input to a
/// per-repo *review stall* check ([`is_review_stalled`]): a deep
/// `loom:review-requested` backlog against a window that merged nothing is a
/// direct, cause-agnostic observation that review is not happening. Before
/// Issue #5021 those three fields were collected and discarded, so a fleet-wide
/// Judge outage — which by construction moves `review_requested` and leaves
/// `queued` untouched — read green here all day.
///
/// `changes_requested_unclaimed` (Issue #5272) is the no-owner subset of
/// `changes_requested` — see
/// [`crate::pipeline_snapshot::RepoPipelineSnapshot::changes_requested_unclaimed`].
/// It does not (yet) feed a dedicated stall/degraded verdict the way the
/// review axis does; it is surfaced so the state #5272 fixes (a
/// `loom:changes-requested` PR with no sweep and no standalone Doctor tick
/// ever picking it up) is visible in the summary/JSON if it regresses,
/// rather than requiring an operator to cross-reference `loom:treating` by
/// hand.
///
/// Verdict precedence when both a stall and a failed forge query are present:
/// **stall wins** (`Degraded` over `Unknown`). Both are non-green and share an
/// exit code, but a stall is a *known, actionable finding*, and burying it
/// under the `Unknown` a single flaky `gh` call produces is the same masking
/// this check exists to remove. The failed repos are still named in the
/// summary either way.
#[must_use]
pub fn assess_queues(inputs: &HealthInputs) -> HealthSection {
    if let Some(gh) = &inputs.gh_unavailable {
        return gh_unavailable_section("queues", gh, inputs);
    }
    let Some(pipeline) = &inputs.pipeline else {
        return unknown_section("queues", "forge snapshot not collected");
    };
    if pipeline.is_empty() {
        return HealthSection::new(
            "queues",
            Verdict::Green,
            "no managed repos",
            serde_json::json!({ "repos": [] }),
        );
    }

    let mut parts: Vec<String> = Vec::with_capacity(pipeline.len());
    let mut total = 0_usize;
    let mut failed: Vec<String> = Vec::new();
    let mut review_requested: Option<usize> = None;
    let mut changes_requested: Option<usize> = None;
    let mut changes_requested_unclaimed: Option<usize> = None;
    let mut approved: Option<usize> = None;
    let mut stalled: Vec<String> = Vec::new();
    for snap in pipeline {
        let name = repo_label(&snap.root);
        match snap.queued {
            Some(n) => {
                total += n;
                parts.push(format!("{name} {n}"));
            }
            None => {
                parts.push(format!("{name} ?"));
                failed.push(name.clone());
            }
        }
        accumulate_observed(&mut review_requested, snap.review_requested);
        accumulate_observed(&mut changes_requested, snap.changes_requested);
        accumulate_observed(&mut changes_requested_unclaimed, snap.changes_requested_unclaimed);
        accumulate_observed(&mut approved, snap.approved);
        if is_review_stalled(snap) {
            stalled.push(format!(
                "{name} ({} awaiting review, 0 merged)",
                snap.review_requested.unwrap_or(0)
            ));
        }
    }

    // The review clause is appended only for axes some repo actually reported,
    // so a caller that masked them off (or a total forge failure) renders the
    // historical `queued`-only line instead of a fabricated "0 awaiting review".
    let review_clause = {
        let mut axes: Vec<String> = Vec::with_capacity(4);
        if let Some(n) = review_requested {
            axes.push(format!("{n} awaiting review"));
        }
        if let Some(n) = changes_requested {
            axes.push(format!("{n} changes-requested"));
        }
        // #5272: the no-owner subset of `changes_requested` — a PR carrying
        // `loom:changes-requested` with no active Doctor claim and no
        // park/hold label. Reported as its own axis (not folded into the
        // `changes_requested` clause above) so a regression here — this
        // count climbing and staying nonzero, meaning the standalone Doctor
        // dispatch isn't draining the queue — is visible without having to
        // cross-reference the `loom:treating` label by hand.
        if let Some(n) = changes_requested_unclaimed {
            axes.push(format!("{n} changes-requested-no-owner"));
        }
        if let Some(n) = approved {
            axes.push(format!("{n} approved"));
        }
        if axes.is_empty() {
            String::new()
        } else {
            format!("; {}", axes.join(", "))
        }
    };
    let stall_clause = if stalled.is_empty() {
        String::new()
    } else {
        format!("; REVIEW STALLED: {}", stalled.join(", "))
    };
    let failed_clause = if failed.is_empty() {
        String::new()
    } else {
        format!("; forge query FAILED for: {}", failed.join(", "))
    };

    let verdict = if !stalled.is_empty() {
        Verdict::Degraded
    } else if failed.is_empty() {
        Verdict::Green
    } else {
        Verdict::Unknown
    };
    let ready_prefix = if failed.is_empty() {
        format!("{total} ready")
    } else {
        format!("{total}+ ready")
    };
    let summary = format!(
        "{ready_prefix} across {} repo(s) ({}){review_clause}{stall_clause}{failed_clause}",
        pipeline.len(),
        parts.join(", ")
    );

    HealthSection::new(
        "queues",
        verdict,
        summary,
        serde_json::json!({
            "total_ready": total,
            "total_review_requested": review_requested,
            "total_changes_requested": changes_requested,
            "total_changes_requested_unclaimed": changes_requested_unclaimed,
            "total_approved": approved,
            "review_stall_min_backlog": REVIEW_STALL_MIN_BACKLOG,
            "review_stalled": pipeline
                .iter()
                .filter(|s| is_review_stalled(s))
                .map(|s| repo_label(&s.root))
                .collect::<Vec<_>>(),
            "repos": pipeline
                .iter()
                .map(|s| serde_json::json!({
                    "root": s.root,
                    "ready": s.queued,
                    "review_requested": s.review_requested,
                    "changes_requested": s.changes_requested,
                    "changes_requested_unclaimed": s.changes_requested_unclaimed,
                    "approved": s.approved,
                    "merged": s.merged_24h,
                    "review_stalled": is_review_stalled(s),
                    "error": s.error,
                }))
                .collect::<Vec<_>>(),
        }),
    )
}

/// Assess the throughput section: merges across managed repos inside the
/// window.
///
/// A *zero* merge count is deliberately **green**: an idle window (an empty
/// backlog, a quiet 4am hour) is not a fault, and a health check that cries
/// wolf on it teaches a watcher to ignore it. Only a failed forge query is
/// non-green here.
#[must_use]
pub fn assess_throughput(inputs: &HealthInputs) -> HealthSection {
    if let Some(gh) = &inputs.gh_unavailable {
        return gh_unavailable_section("throughput", gh, inputs);
    }
    let Some(pipeline) = &inputs.pipeline else {
        return unknown_section("throughput", "forge snapshot not collected");
    };
    if pipeline.is_empty() {
        return HealthSection::new(
            "throughput",
            Verdict::Green,
            "no managed repos",
            serde_json::json!({ "repos": [] }),
        );
    }

    let window = format_window(inputs.window.as_secs());
    let mut total = 0_usize;
    let mut failed: Vec<String> = Vec::new();
    for snap in pipeline {
        match snap.merged_24h {
            Some(n) => total += n,
            None => failed.push(repo_label(&snap.root)),
        }
    }
    let (verdict, summary) = if failed.is_empty() {
        (
            Verdict::Green,
            format!("{total} merged in {window} across {} repo(s)", pipeline.len()),
        )
    } else {
        (
            Verdict::Unknown,
            format!(
                "{total}+ merged in {window} across {} repo(s); forge query FAILED for: {}",
                pipeline.len(),
                failed.join(", ")
            ),
        )
    };
    HealthSection::new(
        "throughput",
        verdict,
        summary,
        serde_json::json!({
            "window_secs": inputs.window.as_secs(),
            "merged": total,
            "repos": pipeline
                .iter()
                .map(|s| serde_json::json!({
                    "root": s.root,
                    "merged": s.merged_24h,
                    "error": s.error,
                }))
                .collect::<Vec<_>>(),
        }),
    )
}

// ============================================================================
// Operator-attention section (Issue #8091) — sibling module
// ============================================================================
//
// `assess_operator_attention` lives in the `operator_attention` sibling
// module and is re-exported here, mirroring `holds` (#7990) just above: this
// file sits at its `.loom/docs/file-size-policy.md` ratchet, so new
// assessment logic goes in a sibling module and this file only carries the
// dispatch line.

pub mod operator_attention;
pub use operator_attention::assess_operator_attention;

// ============================================================================
// Peer-coordination section (Issue #6157)
// ============================================================================

/// Assess the peer-coordination section: whether this host's safehouse RPC
/// socket is reachable, and whether the peer-claim RECEIVE path looks
/// alive — DEGRADED when either invariant from the 2026-08-13 incident
/// fires:
///
/// 1. The socket itself is unreachable/rejected
///    ([`crate::types::SafehouseStatus::state`]) — coordination cannot even
///    be attempted.
/// 2. This host has been advertising sustained peer claims with no receive
///    in return
///    ([`crate::peer_claims::PeerClaimView::evaluate_coordination`], surfaced
///    via [`crate::types::PeerClaimStatus::coordination`]).
///
/// This section is diagnostic-only as of Epic #6165 Phase 4 (#6317):
/// [`crate::claim_reconciliation::forge::reconcile_workspace`] used to
/// refuse stale-claim reclamation while DEGRADED (Issue #6157), on the
/// theory that a one-way channel's silence could not be trusted as evidence
/// of anything. That gate has been removed — the lease record (Epic #6165
/// Phase 2, Issue #6286) is the sole fleet-scoped reclamation gate now, and
/// it needs no receipt from this channel at all — so a DEGRADED verdict
/// here reports a real transport problem worth an operator's attention, but
/// no longer changes reclamation behavior.
///
/// Green (not Unknown) when safehouse is not configured at all — there is
/// nothing broken about a host that was never asked to coordinate with
/// peers, and reporting `Unknown` there would make every non-fleet
/// single-host repo look perpetually "could not determine".
#[must_use]
pub fn assess_peer_coordination(inputs: &HealthInputs) -> HealthSection {
    let Some(status) = &inputs.status else {
        return unknown_section("peer_coordination", &no_status_reason(inputs));
    };

    if let Some(safehouse) = &status.safehouse {
        if matches!(safehouse.state.as_str(), "unreachable" | "send_rejected") {
            let detail = safehouse
                .reason
                .clone()
                .or_else(|| safehouse.socket.as_ref().map(|p| p.display().to_string()));
            return HealthSection::new(
                "peer_coordination",
                Verdict::Degraded,
                format!(
                    "safehouse RPC socket {}{} — peer-coordination liveness is unknowable \
                     (#6157)",
                    safehouse.state,
                    detail
                        .as_ref()
                        .map(|d| format!(": {d}"))
                        .unwrap_or_default()
                ),
                serde_json::json!({
                    "safehouse_state": safehouse.state,
                    "detail": detail,
                }),
            );
        }
    }

    let Some(peer_claims) = &status.peer_claims else {
        return HealthSection::new(
            "peer_coordination",
            Verdict::Green,
            "safehouse not configured — nothing to report".to_string(),
            serde_json::json!({ "configured": false }),
        );
    };
    let c = &peer_claims.coordination;
    // Issue #6242: the resolved claims-room identity, so comparing two
    // hosts' `health`/`status` output makes a `rooms.claims` /
    // `LOOM_SAFEHOUSE_ROOM_CLAIMS` mismatch a one-line diff. Visibility
    // only — no cross-host comparison is performed here.
    let room = peer_claims.claims_room.as_deref().unwrap_or("none");
    // Issue #6243: the repo-sharding verification AC ("zero cross-host claims
    // on the SAME issue over a 24h window") is read by an operator off this
    // section. Append it to the human-readable summary only when it is
    // NON-zero, so the healthy/default rendering — and every pre-#6243
    // assertion on that wording — is byte-for-byte unchanged, while a fleet
    // that IS colliding says so without needing `--json`.
    let collisions_note = if peer_claims.same_issue_collisions > 0 {
        format!(
            ", {} same-issue cross-host claim(s) in 24h (#6243)",
            peer_claims.same_issue_collisions
        )
    } else {
        String::new()
    };
    if c.degraded {
        return HealthSection::new(
            "peer_coordination",
            Verdict::Degraded,
            format!(
                "peer-claim receive path DEGRADED ({} received / {} advertised, room: {room}), \
                 degraded for {} — {}/{} sustained receive(s) toward recovery (#6157){collisions_note}",
                peer_claims.received,
                peer_claims.advertised,
                c.degraded_for_secs
                    .map(|s| format_age(i64::try_from(s).unwrap_or(i64::MAX)))
                    .unwrap_or_else(|| "?".to_string()),
                c.consecutive_receives_toward_recovery,
                c.recovery_threshold,
            ),
            serde_json::json!({
                "advertised": peer_claims.advertised,
                "received": peer_claims.received,
                "claims_room": peer_claims.claims_room,
                "degraded_for_secs": c.degraded_for_secs,
                "consecutive_receives_toward_recovery": c.consecutive_receives_toward_recovery,
                "recovery_threshold": c.recovery_threshold,
                // Issue #6243: surfaced here as a plain counter — this
                // section's DEGRADED/Green verdict above is #6157's mesh-
                // liveness question ("is the receive path alive"), a
                // different question from "did repo-sharding actually keep
                // cross-host claims on the same issue rare" this counter
                // answers. Not folded into the verdict (that is #6242's
                // scope, tracked separately) — visibility only.
                "same_issue_collisions_24h": peer_claims.same_issue_collisions,
            }),
        );
    }
    HealthSection::new(
        "peer_coordination",
        Verdict::Green,
        format!(
            "peer-claim receive path healthy ({} received / {} advertised, room: {room})\
             {collisions_note}",
            peer_claims.received, peer_claims.advertised
        ),
        serde_json::json!({
            "advertised": peer_claims.advertised,
            "received": peer_claims.received,
            "claims_room": peer_claims.claims_room,
            // Issue #6243: see the DEGRADED branch's comment above.
            "same_issue_collisions_24h": peer_claims.same_issue_collisions,
        }),
    )
}

// ============================================================================
// Stale-untracked-sweep section (Issue #7529)
// ============================================================================

/// Assess [`DaemonStatusReport::stale_sweeps`]: a hard, non-tick-dependent
/// finding for any in-flight sweep that has crossed the age + log-silence
/// sanity thresholds while unreachable by either the startup-hang (#3887) or
/// review-stall (#3910) watchdog — see the module doc on
/// `crate::sweep_registry::watchdog`'s stale-sweep section for the full root
/// cause. Computed by the daemon fresh on every `DaemonStatus` round-trip
/// (`ipc::build_daemon_status`), so this section is `Green`/`Degraded`
/// regardless of whether the watchdog tick task has ever run.
#[must_use]
pub fn assess_stale_sweeps(inputs: &HealthInputs) -> HealthSection {
    let Some(status) = &inputs.status else {
        return unknown_section("stale_sweeps", &no_status_reason(inputs));
    };

    if status.stale_sweeps.is_empty() {
        return HealthSection::new(
            "stale_sweeps",
            Verdict::Green,
            "no stale untracked sweeps",
            serde_json::json!({ "count": 0 }),
        );
    }

    let summary = status
        .stale_sweeps
        .iter()
        .map(|f| {
            format!("#{} (pid {}, {}s, {})", f.issue, f.pid, f.elapsed_secs, repo_label(&f.root))
        })
        .collect::<Vec<_>>()
        .join(", ");
    HealthSection::new(
        "stale_sweeps",
        Verdict::Degraded,
        format!(
            "{} stale untracked sweep(s) with zero watchdog coverage (age + log silence past \
             sanity thresholds, #7529): {summary}",
            status.stale_sweeps.len()
        ),
        serde_json::json!({
            "count": status.stale_sweeps.len(),
            "sweeps": status.stale_sweeps,
        }),
    )
}

// ============================================================================
// Auto-update section (Issue #7584) — unconditional
// ============================================================================

/// The wrong-repo-resolution escalation (Issue #8513) this section applies
/// on top of the staleness rules below — a sibling module so this file, over
/// `.loom/docs/file-size-policy.md`'s threshold, does not grow to hold it.
mod auto_update_stale_repo;

/// Assess the auto_update section: the autonomous self-update loop's own
/// state ([`DaemonStatusReport::auto_update_enabled`] and friends, Issue
/// #4055) plus this CLI process's own staleness magnitude
/// ([`HealthInputs::self_update`], Issue #6261).
///
/// **Unconditional** — wired directly into [`assess`]'s `sections` literal,
/// like `liveness`/`dispatch`, not the `Option`-returning
/// "only-when-non-green" pattern [`assess_observability`] uses. The whole
/// point of #7584 is that a dirty-tree (or backing-off, or terminal) block
/// silently starves the fleet's autonomous rebuild pipeline for days with
/// *nothing* on the normal `health`/`status` surfaces to notice — a section
/// that only appears once that has already happened defeats the purpose just
/// as thoroughly as no section at all.
///
/// Verdict rule, reusing [`crate::self_update::resolve_stale_warn_commits`] /
/// [`crate::self_update::resolve_stale_warn_hours`] (via
/// [`crate::self_update::staleness_warning_default`]) rather than inventing a
/// second staleness threshold:
///
/// - [`Verdict::Unknown`] — the daemon is unreachable (no loop state to
///   report at all), same precedence every other status-derived section here
///   already follows.
/// - [`Verdict::Green`] — the loop is disabled (a deliberate opt-out), OR the
///   running binary is up to date / staleness is undecidable, OR it is stale
///   but still under **both** warn thresholds.
/// - [`Verdict::Degraded`] — `auto_update_terminal_reason` is set (stuck
///   until a new source commit lands, regardless of how stale that commit
///   currently is), OR the staleness crosses the warn thresholds **and** the
///   loop is actively blocked from converging on its own — a dirty source
///   tree ([`crate::self_update::source_tree_clean`]'s refusal, this issue's
///   own trigger) or an active build-failure backoff. Merely "stale but
///   still settling/deferring" (the loop is progressing normally toward its
///   next roll) does not escalate — only a block that requires operator
///   intervention (clean the tree) or represents repeated failure (backoff)
///   does. The summary names the block reason **verbatim** from
///   `auto_update_note` — never a generic "stale" message — because that
///   verbatim reason is the whole point of this issue (#7584). Also
///   [`Verdict::Degraded`] when
///   [`auto_update_stale_repo::stalled_summary`] fires (#8513) — a distinct
///   hard finding from the staleness checks above, since it can fire even
///   while the SOURCE checkout is perfectly current.
#[must_use]
pub fn assess_auto_update(inputs: &HealthInputs) -> HealthSection {
    let Some(status) = &inputs.status else {
        return unknown_section("auto_update", &no_status_reason(inputs));
    };

    let commits_behind = inputs.self_update.as_ref().and_then(|s| s.commits_behind);
    let hours_behind = inputs.self_update.as_ref().and_then(|s| s.hours_behind);
    let update_available = inputs.self_update.as_ref().and_then(|s| s.update_available);
    let staleness_warning =
        crate::self_update::staleness_warning_default(commits_behind, hours_behind);

    let detail = serde_json::json!({
        "enabled": status.auto_update_enabled,
        "last_check": status.auto_update_last_check,
        "last_roll": status.auto_update_last_roll,
        "consecutive_failures": status.auto_update_consecutive_failures,
        "backoff_secs": status.auto_update_backoff_secs,
        "terminal_reason": status.auto_update_terminal_reason,
        "note": status.auto_update_note,
        "update_available": update_available,
        "commits_behind": commits_behind,
        "hours_behind": hours_behind,
        // Issue #7609: the artifact-first tick's view — what release artifact
        // is available for this host's platform (`null` when none resolved),
        // reported next to the installed version so a fleet-wide version
        // drift is visible from `loom-daemon health` alone.
        "installed_version": env!("CARGO_PKG_VERSION"),
        "artifact_available": status.auto_update_artifact_version.as_ref().map(|version| {
            serde_json::json!({
                "version": version,
                "published_at": status.auto_update_artifact_published_at,
            })
        }),
        // Issue #8513: the wrong-repo-resolution streak, next to the fields
        // above so an operator sees the "which repo?" answer in the same
        // place as everything else this section already reports.
        "stale_repo_ticks": status.auto_update_stale_repo_ticks,
        "stale_repo": status.auto_update_stale_repo,
    });

    if !status.auto_update_enabled {
        return HealthSection::new(
            "auto_update",
            Verdict::Green,
            "auto_update loop disabled (opted out)",
            detail,
        );
    }

    if let Some(reason) = &status.auto_update_terminal_reason {
        return HealthSection::new(
            "auto_update",
            Verdict::Degraded,
            format!(
                "auto_update TERMINALLY stuck — not retrying until a new source commit lands: \
                 {reason}"
            ),
            detail,
        );
    }

    // Issue #8513: a persisting stale-repo resolution is a hard finding in
    // its own right, independent of the staleness-magnitude checks below —
    // those compare against the SOURCE checkout's HEAD, which says nothing
    // about a release-artifact-path wrong-repo resolution.
    if let Some(summary) = auto_update_stale_repo::stalled_summary(status) {
        return HealthSection::new("auto_update", Verdict::Degraded, summary, detail);
    }

    // A block that requires operator intervention (a dirty tree) or reflects
    // repeated failure (an active backoff) — as opposed to the loop merely
    // still settling/deferring on its way to a normal roll.
    let note = status.auto_update_note.as_deref().unwrap_or_default();
    let actively_blocked = status.auto_update_backoff_secs.is_some() || note.contains("dirty");

    if staleness_warning.is_some() && actively_blocked {
        return HealthSection::new(
            "auto_update",
            Verdict::Degraded,
            format!(
                "auto_update blocked while stale ({}): {note}",
                staleness_warning.as_deref().unwrap_or("stale")
            ),
            detail,
        );
    }

    let summary = if let Some(warning) = &staleness_warning {
        // Stale past the warn thresholds but still progressing normally
        // (settling / deferring past in-flight sweeps) — not yet a fault.
        format!("stale but not blocked ({warning}); note: {note}")
    } else if update_available == Some(true) {
        format!("stale but below warn thresholds; note: {note}")
    } else {
        note.to_string()
    };
    HealthSection::new("auto_update", Verdict::Green, summary, detail)
}

// ============================================================================
// Sections for held/backed-off activity (Issues #7590, #7708)
// ============================================================================
//
// `assess_worktree_reaper` (#7590) and `assess_pool_hold` (#7708) both live in
// the `holds` sibling module and are re-exported here, so every caller and
// test keeps addressing them as `health::assess_*` — see that module's doc for
// why they are grouped and why this file only carries the dispatch line.

pub mod holds;
pub use holds::{assess_pool_hold, assess_worktree_reaper};

// ============================================================================
// Observability section (Issue #4830) — conditional
// ============================================================================

/// Assess the observability exporter: `Some(DEGRADED)` when telemetry is
/// demonstrably going wrong, else `None`.
///
/// **Deliberately conditional**, unlike every other section. There is nothing
/// to say when the exporter is disabled, still starting, or exporting under the
/// right identity — which is all but a handful of daemons — so a permanent
/// `observability GREEN — ok` line would be pure noise on a surface whose whole
/// value is that every line printed is worth reading. That anomaly-only rule
/// (#4830) is preserved verbatim; the *positive* confirmation an operator needs
/// lives on `loom-daemon status` instead (`Observability: OK — …`, #5083) and,
/// machine-readably, in `DaemonStatusReport::observability_export`.
///
/// Four conditions qualify as anomalies:
///
/// 1. **host-identity mismatch** (#4830) — the daemon has confirmed its ingest
///    key is bound to a *different* `host_id` than it reports for itself, so
///    every record it pushes is filed under the wrong host. Kept first and
///    byte-for-byte as it was, including its `detail` keys.
/// 2. **never exported** (#5083) — the exporter has been running well past its
///    flush cadence and has still never had a single batch acked. This is the
///    silent failure this section previously rendered *identically to healthy*:
///    as nothing at all.
/// 3. **export failing** (#5083) — flushes are actively erroring, so the queue
///    is backing up and telemetry is going stale.
/// 4. **misconfigured** (#5337) — `enabled: true` but the exporter never
///    started because a required piece of config (endpoint, ingest key file,
///    or a readable/non-empty key) could not be resolved. Distinct from the
///    silent, no-section `Disabled` state below: this is a config error an
///    operator should fix, not a deliberate opt-out.
///
/// Read straight off [`DaemonStatusReport`] rather than through a dedicated
/// [`HealthInputs`] field: this is *daemon-process* state (only the daemon
/// holds both halves — its own identity and the backend's responses), and
/// `health` runs in a separate CLI process, so the IPC status report is the
/// only place it can come from. A parallel collector field would just copy it
/// and add a way for the two to disagree.
#[must_use]
pub fn assess_observability(inputs: &HealthInputs) -> Option<HealthSection> {
    let status = inputs.status.as_ref()?;
    // Positive facts, when the daemon is new enough to report them (#5083) —
    // folded into the mismatch note's `detail` below so a machine consumer
    // reading a DEGRADED section still learns whether anything is landing at
    // all, and used on its own for conditions 2 and 3.
    //
    // `state` is re-stamped from this report's own `at` before serialization:
    // the daemon classified it at status-build time, and a section whose
    // verdict said one thing while its `detail.state` said another would be a
    // new way for the two halves of the same answer to disagree — precisely
    // the failure mode this issue is about.
    let export = status.observability_export.as_ref().map(|e| {
        let mut classified = e.clone();
        classified.state = classified.classify(inputs.at);
        classified
    });
    let export = export.as_ref();
    let export_detail = export.map_or(serde_json::Value::Null, |e| {
        serde_json::to_value(e).unwrap_or(serde_json::Value::Null)
    });

    if let Some(mismatch) = status.observability_host_id_mismatch.as_ref() {
        let age = inputs
            .at
            .signed_duration_since(mismatch.first_seen_at)
            .num_seconds()
            .max(0);
        return Some(HealthSection::new(
            "observability",
            Verdict::Degraded,
            format!(
                "telemetry is being filed under {} — the ingest key on this host is bound to that \
                 id, not to {} (first seen {} ago)",
                mismatch.ingest_host_id,
                mismatch.daemon_host_id,
                format_window(u64::try_from(age).unwrap_or(0))
            ),
            serde_json::json!({
                "daemon_host_id": mismatch.daemon_host_id,
                "ingest_host_id": mismatch.ingest_host_id,
                "first_seen_at": mismatch.first_seen_at,
                "first_seen_age_secs": age,
                "export": export_detail,
            }),
        ));
    }

    let export = export?;
    match export.classify(inputs.at) {
        // The #5083 headline: configured, running, and has never once
        // succeeded. Called out only after the grace window
        // (`never_exported_grace_secs`) so a freshly-restarted daemon is never
        // reported as broken for its first flush interval.
        ObservabilityExportState::NeverExported => {
            Some(HealthSection::new(
                "observability",
                Verdict::Degraded,
                format!(
                "exporter has been running {} as {} and has NEVER had a batch acked — telemetry \
                 is not reaching {}{}",
                format_window(export.uptime_secs(inputs.at).unwrap_or(0)),
                export.host_id.as_deref().unwrap_or("unknown-host"),
                export.endpoint.as_deref().unwrap_or("the configured endpoint"),
                export
                    .last_failure_detail
                    .as_deref()
                    .map_or_else(String::new, |d| format!(" (last error: {d})")),
            ),
                export_detail,
            ))
        }
        ObservabilityExportState::Failing => Some(HealthSection::new(
            "observability",
            Verdict::Degraded,
            format!(
                "{} consecutive failed flush(es) as {}; last successful export {}{}",
                export.consecutive_failures,
                export.host_id.as_deref().unwrap_or("unknown-host"),
                export.last_success_age_secs(inputs.at).map_or_else(
                    || "never".to_string(),
                    |age| format!("{} ago", format_window(age))
                ),
                export
                    .last_failure_detail
                    .as_deref()
                    .map_or_else(String::new, |d| format!(" (last error: {d})")),
            ),
            export_detail,
        )),
        // `enabled: true` but a required piece of config could not be
        // resolved (Issue #5337) — a real, operator-actionable config error,
        // not the benign `Disabled` steady state, so it earns a section same
        // as `NeverExported`/`Failing` above.
        ObservabilityExportState::Misconfigured => Some(HealthSection::new(
            "observability",
            Verdict::Degraded,
            format!(
                "enabled but not exporting — configuration is incomplete or unreadable{}",
                export
                    .last_failure_detail
                    .as_deref()
                    .map_or_else(String::new, |d| format!(": {d}")),
            ),
            export_detail,
        )),
        // Disabled / Starting / Healthy / HostIdMismatch (handled above) — no
        // section, exactly as before #5083.
        _ => None,
    }
}

// ============================================================================
// Codesign identity preflight (Issue #7605) — conditional
// ============================================================================

/// The conditional `codesign_identity` section and its preflight fact
/// (#7605), including the DEGRADED wording that names the invocation context
/// the preflight actually ran in (#8286).
mod codesign;

pub use codesign::{assess_codesign_identity, CodesignPreflightResult};

// ============================================================================
// Limit calibration (Issues #8063 / #8349) — conditional
// ============================================================================

/// The conditional `limit_calibration` section: the fleet's $-eq per
/// weekly-limit point plus its step-change warning, rendered only when the
/// collector actually produced a reading on this host.
mod calibration_section;

pub use calibration_section::assess_limit_calibration;

/// The `transcript_ingest` section (#8477): whether the background
/// transcript-token-ingest pass is enabled and keeping up with transcripts on
/// disk.
mod transcript_ingest_section;

pub use transcript_ingest_section::assess_transcript_ingest;

/// The `tmpfs_visibility` section (issue #8572, split from #8512): tmpfs/
/// `shared`-RAM usage and the cumulative kernel OOM-kill count.
mod tmpfs_visibility_section;

pub use tmpfs_visibility_section::assess_tmpfs_visibility;

// ============================================================================
// Codex accounts (Issue #8407) — conditional
// ============================================================================

/// The conditional `codex` section: per-subscription Codex availability, the
/// provider-scoped sibling of the Claude `tokens` section. Rendered only on a
/// host that actually has Codex accounts.
pub mod codex_accounts;

// ============================================================================
// Roll-up
// ============================================================================

/// The `indeterminate-busy` verdict's corroboration rules (#6191, #8163) —
/// `probe_budget_busy` and the host-load reading it now requires.
mod busy;

/// Assemble the full report from already-collected inputs (pure).
///
/// A [`Verdict::Dead`] liveness verdict short-circuits: the remaining sections
/// are reported as [`Verdict::Unknown`] (they are all downstream of an IPC
/// round-trip that could not happen), and the overall verdict is `Dead` so the
/// exit code is `2` rather than a misleading `1`.
///
/// Every section is unconditional except `observability` (#4830),
/// `codesign_identity` (#7605), `limit_calibration` (#8063/#8349), `codex`
/// (#8407) and `tmpfs_visibility` (#8572), each of which is appended only
/// when there is something to report — see [`assess_observability`],
/// [`assess_codesign_identity`], [`assess_limit_calibration`],
/// [`codex_accounts::assess`] and [`assess_tmpfs_visibility`].
///
/// # `IndeterminateBusy` (#6191, #8163)
///
/// When no section is [`Verdict::Degraded`] (so this is not, and cannot mask,
/// a genuine fault) and [`busy::probe_budget_busy`] says the entire non-green
/// state traces back to an exhausted IPC probe budget against a daemon already
/// corroborated as alive with a fresh heartbeat **on a host whose load
/// corroborates the busy story** (#8163), `overall` is
/// [`Verdict::IndeterminateBusy`] rather than the ordinary
/// [`Verdict::Unknown`] — its own exit code ([`EXIT_INDETERMINATE_BUSY`])
/// distinct from [`EXIT_DEGRADED`]. See the module-level "Busy vs degraded"
/// doc section for the full rationale.
#[must_use]
pub fn assess(inputs: &HealthInputs) -> HealthReport {
    let liveness = assess_liveness(inputs);
    let dead = liveness.verdict == Verdict::Dead;
    let mut sections = vec![
        liveness,
        assess_dispatch(inputs),
        assess_tokens(inputs),
        assess_roles(inputs),
        assess_role_liveness(inputs),
        assess_queues(inputs),
        assess_throughput(inputs),
        assess_operator_attention(inputs),
        assess_peer_coordination(inputs),
        assess_stale_sweeps(inputs),
        assess_auto_update(inputs),
        assess_worktree_reaper(inputs),
        assess_pool_hold(inputs),
    ];
    sections.extend(codex_accounts::assess(inputs));
    sections.extend(assess_observability(inputs));
    sections.extend(assess_codesign_identity(inputs));
    sections.extend(assess_limit_calibration(inputs));
    sections.extend(assess_transcript_ingest(inputs));
    sections.extend(assess_tmpfs_visibility(inputs));
    let overall = if dead {
        Verdict::Dead
    } else if sections.iter().all(|s| s.verdict.is_green()) {
        Verdict::Green
    } else if sections.iter().any(|s| s.verdict == Verdict::Degraded) {
        Verdict::Degraded
    } else if busy::probe_budget_busy(inputs) {
        Verdict::IndeterminateBusy
    } else {
        Verdict::Unknown
    };
    HealthReport {
        at: inputs.at,
        window_secs: inputs.window.as_secs(),
        overall,
        sections,
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn unknown_section(key: &'static str, why: &str) -> HealthSection {
    HealthSection::new(
        key,
        Verdict::Unknown,
        why.to_string(),
        serde_json::json!({ "unavailable": why }),
    )
}

/// Why `dispatch`/`tokens`/`roles` could not be collected at all — every one
/// of them is derived solely from the same single [`DaemonStatusReport`], so
/// a missing `status` explains all three identically (Issue #6103 AC4).
/// Distinguishes a *transient probe miss* (this collection's IPC round-trip
/// merely exceeded its bounded budget — see [`ipc_error_is_probe_timeout`]
/// and `assess_liveness`'s own probe-budget-exceeded treatment) from a
/// genuinely unreachable/unresolvable daemon, so an operator reading these
/// three sections is pointed at the same distinction `liveness` already
/// makes rather than a flat, undifferentiated "IPC unreachable".
fn no_status_reason(inputs: &HealthInputs) -> String {
    match &inputs.ipc_error {
        Some(err) if ipc_error_is_probe_timeout(err) => {
            "no daemon status — IPC probe budget exceeded (see liveness); not necessarily unhealthy"
                .to_string()
        }
        _ => "no daemon status (IPC unreachable)".to_string(),
    }
}

/// Render `queues`/`throughput` for a missing/non-executable `gh` (#5061):
/// one distinct, environment-attributed reason instead of the pre-#5061
/// "forge query FAILED for: <every managed repo>" — which duplicated the
/// same environment fact once per repo and read exactly like a forge outage.
///
/// Still [`Verdict::Unknown`] (not [`Verdict::Degraded`]): the queue depth /
/// merge count genuinely could not be determined, so the existing "unknown
/// != healthy" exit-code contract (exit `1`) is unchanged — only the
/// *reason string* changes.
///
/// When the daemon answered IPC and reported its own `credential_preflight`
/// verdict, that signal is cross-referenced by name so an operator never has
/// to manually reconcile "forge query FAILED" here against "Forge
/// credential: OK" from `status`/`--json`'s `credential_preflight` — the
/// exact disagreement that prompted #5061.
fn gh_unavailable_section(
    key: &'static str,
    gh: &crate::pipeline_snapshot::GhUnavailable,
    inputs: &HealthInputs,
) -> HealthSection {
    let credential_preflight = inputs
        .status
        .as_ref()
        .and_then(|s| s.credential_preflight.as_ref());
    let cred_note = match credential_preflight {
        Some(c) if c.ok => format!(
            " (the daemon's own IPC reports its forge credential OK via {} — this is a \
             caller-side PATH problem in the process running `health`, not a forge outage or a \
             bad credential)",
            c.mechanism
        ),
        Some(c) => format!(
            " (the daemon's own IPC separately reports its forge credential as DEGRADED: {} — \
             but that is the daemon's credential, not this caller's missing `gh`)",
            c.message
        ),
        None => String::new(),
    };
    HealthSection::new(
        key,
        Verdict::Unknown,
        format!("{}{cred_note}", gh.reason),
        serde_json::json!({
            "unavailable": "gh not found on PATH or not executable",
            "gh_bin": gh.gh_bin,
            "reason": gh.reason,
            "observed_path": gh.observed_path,
            "daemon_credential_preflight": credential_preflight,
        }),
    )
}

/// The short repo label rendered in the queue/throughput lines — the root's
/// final path component, which is the repo name for every managed workspace.
fn repo_label(root: &std::path::Path) -> String {
    root.file_name()
        .map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned())
}

// ============================================================================
// #4824 — the daemon-log corroborating signal
// ============================================================================

/// The substring every work-finder log line carries (`work_finder: tick — …`,
/// `work_finder: dispatching issue #…`).
const WORK_FINDER_LOG_MARKER: &str = "work_finder:";

/// How much of the daemon log's tail to read for the corroborating probe.
///
/// The log rotates at 10 MiB and the work finder writes at least one line per
/// tick, so a quarter-megabyte tail always spans far more than the grace window
/// this signal is compared against — while keeping the probe a single bounded
/// read rather than a scan of the whole file.
const DAEMON_LOG_TAIL_BYTES: u64 = 256 * 1024;

/// The daemon log's line-prefix timestamp format, as written by the daemon's
/// `env_logger` format hook (`[2026-07-31T14:27:33.950] [INFO] …`) — a **local**
/// naive stamp with no offset, which is why the comparison below is done in
/// local time rather than UTC.
const DAEMON_LOG_STAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f";

/// Parse the leading `[<stamp>]` of one daemon-log line.
fn parse_log_line_stamp(line: &str) -> Option<chrono::NaiveDateTime> {
    let (stamp, _) = line.strip_prefix('[')?.split_once(']')?;
    chrono::NaiveDateTime::parse_from_str(stamp, DAEMON_LOG_STAMP_FORMAT).ok()
}

/// Age in seconds of the newest `work_finder:` line in a daemon-log tail,
/// measured against `now_local` (Issue #4824).
///
/// Pure over the log text so the corroboration rule is unit-testable without a
/// daemon or a real log file. `None` when the tail carries no parseable
/// `work_finder:` line — honestly "no corroboration", never "the loop is dead".
/// A stamp in the future (clock skew) reads as age `0` rather than underflowing.
#[must_use]
pub fn work_finder_log_tick_age_secs(
    log_tail: &str,
    now_local: chrono::NaiveDateTime,
) -> Option<u64> {
    let stamp = log_tail
        .lines()
        .rev()
        .filter(|line| line.contains(WORK_FINDER_LOG_MARKER))
        .find_map(parse_log_line_stamp)?;
    Some(u64::try_from((now_local - stamp).num_seconds()).unwrap_or(0))
}

/// Resolve the daemon log path the way the daemon itself does: `LOOM_DAEMON_LOG`
/// (full override) when set, else `<loom dir>/daemon.log` where the loom dir is
/// `LOOM_SOCKET_PATH`'s parent (test isolation) or `$HOME/.loom`.
///
/// Mirrors the binary-side `daemon_service::resolve_log_path` /
/// `resolve_loom_dir` pair (#4010), which is private to the binary crate and so
/// unreachable from this library module and from `cli::health`.
#[must_use]
pub fn resolve_daemon_log_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_DAEMON_LOG") {
        return Some(PathBuf::from(path));
    }
    let loom_dir = match std::env::var("LOOM_SOCKET_PATH") {
        Ok(socket) => PathBuf::from(socket).parent()?.to_path_buf(),
        Err(_) => dirs::home_dir()?.join(".loom"),
    };
    Some(loom_dir.join("daemon.log"))
}

/// Probe the daemon log for the newest `work_finder:` line's age (Issue #4824)
/// — the I/O half of [`work_finder_log_tick_age_secs`].
///
/// Best-effort like every other collector input: any failure (no resolvable
/// path, unreadable file, no matching line) is `None`, which the classifier
/// treats as "no corroboration either way".
#[must_use]
pub fn probe_work_finder_log_tick_age() -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};

    let path = resolve_daemon_log_path()?;
    let mut file = std::fs::File::open(&path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > DAEMON_LOG_TAIL_BYTES {
        file.seek(SeekFrom::Start(len - DAEMON_LOG_TAIL_BYTES))
            .ok()?;
    }
    let mut buf = Vec::with_capacity(DAEMON_LOG_TAIL_BYTES as usize);
    file.take(DAEMON_LOG_TAIL_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    work_finder_log_tick_age_secs(
        &String::from_utf8_lossy(&buf),
        chrono::Local::now().naive_local(),
    )
}

/// Render an age in seconds compactly (`43s`, `7m`, `2h`, `3d`). Negative ages
/// (a clock skew between the daemon's stamp and this process) render as `0s`
/// rather than a nonsensical negative.
#[must_use]
pub fn format_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 90 {
        format!("{s}s")
    } else if s < 5400 {
        format!("{}m", s / 60)
    } else if s < 172_800 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
