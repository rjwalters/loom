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
//! | `1`  | degraded — at least one section is non-green (including "could not determine") |
//! | `2`  | the daemon is **genuinely** dead |
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
//!    ⇒ alive-but-unresponsive: [`Verdict::Degraded`], never dead.
//! 3. **`pgrep -x loom-daemon` finds a live process** ⇒ still not dead:
//!    [`Verdict::Degraded`]. This is the third independent signal, for the case
//!    where both launchd *and* the pid file are uninformative (a daemon started
//!    outside the managed wrapper, or a pid file removed by hand).
//! 4. Only when all three are negative is the verdict [`Verdict::Dead`] (exit
//!    `2`).
//!
//! An *undiagnosable* probe (no loom dir resolvable, `pgrep` absent) is
//! [`Verdict::Unknown`] — exit `1`, "I could not tell" — never `2`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::daemon_install_state::{InstallState, InstallStateReport};
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
                return HealthSection::new(
                    "liveness",
                    Verdict::Degraded,
                    format!(
                        "process ALIVE but not answering IPC — NOT dead ({detail}); ipc: {ipc_error}{}",
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
/// concurrency cap `min(token_axis, disk_headroom, configured_max)` (#5177).
///
/// True only when disk headroom is smaller than *both* other terms — i.e. the
/// scratch volume is throttling dispatch below what the token pool and the
/// operator's configured admission ceiling would otherwise allow. A tie with
/// `configured_max` is deliberately NOT flagged: that ceiling is the operator's
/// own deliberate choice, not a disk fault. Mirrors the input shape of
/// [`crate::work_finder::resolve_dynamic_max_concurrent`] so the two agree on
/// what "binding" means.
#[must_use]
pub fn disk_binds_cap(
    token_axis_limit: usize,
    per_token_concurrency: usize,
    disk_headroom: usize,
    configured_max: usize,
) -> bool {
    let token_axis = token_axis_limit.saturating_mul(per_token_concurrency.max(1));
    disk_headroom < token_axis && disk_headroom < configured_max
}

/// Assess the dispatch section: in-flight occupancy against the dynamic cap,
/// plus the last work-finder tick's dispatch/skip-reason summary.
#[must_use]
pub fn assess_dispatch(inputs: &HealthInputs) -> HealthSection {
    let Some(status) = &inputs.status else {
        return unknown_section("dispatch", "no daemon status (IPC unreachable)");
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
    // cap and throttles the host to a fraction of its token/config capacity. That
    // used to present as a green daemon dispatching at, say, cap 2 — a fault an
    // operator had to notice in the `min(...)` breakdown. Name it as degraded.
    if disk_binds_cap(
        status.capacity.token_axis_limit,
        status.per_token_concurrency,
        status.disk_headroom,
        status.configured_max,
    ) {
        degraded.push(format!(
            "disk headroom is throttling dispatch (cap {} bound by disk headroom {} — free scratch \
             disk; e.g. `loom-daemon clean --aggressive` / `--deep`)",
            cap, status.disk_headroom
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
        return unknown_section("tokens", "no daemon status (IPC unreachable)");
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
    let base = format!(
        "{}/{} healthy ({} exhausted), {ranking_age}",
        cap.healthy_accounts, cap.total_accounts, cap.exhausted_accounts
    );
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
            "total": cap.total_accounts,
            "exhausted": cap.exhausted_accounts,
            "token_axis_limit": cap.token_axis_limit,
            "token_bound": cap.token_bound,
            "ranking_present": inputs.ranking_present,
            "ranking_age_secs": inputs.ranking_age_secs,
            "ranking_stale_threshold_secs": RANKING_STALE_SECS,
            "issues": degraded,
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
        return unknown_section("roles", "no daemon status (IPC unreachable)");
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
    let (verdict, line) = if summary.persistent.is_empty() {
        (
            Verdict::Green,
            format!("{}/{} ticks ok{transient_note}", summary.ok, summary.total),
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
                "{}/{} ticks ok; {} PERSISTENT failure(s): {}{escalated_note}{transient_note}",
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
/// `changes_requested`, `approved`) that the same snapshot already carries.
///
/// The review axes are reported for their own sake and are also the input to a
/// per-repo *review stall* check ([`is_review_stalled`]): a deep
/// `loom:review-requested` backlog against a window that merged nothing is a
/// direct, cause-agnostic observation that review is not happening. Before
/// Issue #5021 those three fields were collected and discarded, so a fleet-wide
/// Judge outage — which by construction moves `review_requested` and leaves
/// `queued` untouched — read green here all day.
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
        let mut axes: Vec<String> = Vec::with_capacity(3);
        if let Some(n) = review_requested {
            axes.push(format!("{n} awaiting review"));
        }
        if let Some(n) = changes_requested {
            axes.push(format!("{n} changes-requested"));
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
/// Three conditions qualify as anomalies:
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
        // Disabled / Starting / Healthy / HostIdMismatch (handled above) — no
        // section, exactly as before #5083.
        _ => None,
    }
}

// ============================================================================
// Roll-up
// ============================================================================

/// Assemble the full report from already-collected inputs (pure).
///
/// A [`Verdict::Dead`] liveness verdict short-circuits: the remaining sections
/// are reported as [`Verdict::Unknown`] (they are all downstream of an IPC
/// round-trip that could not happen), and the overall verdict is `Dead` so the
/// exit code is `2` rather than a misleading `1`.
///
/// Every section is unconditional except `observability` (#4830), which is
/// appended only when there is a mismatch to report — see
/// [`assess_observability`].
#[must_use]
pub fn assess(inputs: &HealthInputs) -> HealthReport {
    let liveness = assess_liveness(inputs);
    let dead = liveness.verdict == Verdict::Dead;
    let mut sections = vec![
        liveness,
        assess_dispatch(inputs),
        assess_tokens(inputs),
        assess_roles(inputs),
        assess_queues(inputs),
        assess_throughput(inputs),
    ];
    sections.extend(assess_observability(inputs));
    let overall = if dead {
        Verdict::Dead
    } else if sections.iter().all(|s| s.verdict.is_green()) {
        Verdict::Green
    } else if sections.iter().any(|s| s.verdict == Verdict::Degraded) {
        Verdict::Degraded
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
mod tests {
    use super::*;
    use crate::daemon_install_state::HeartbeatFreshness;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-31T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn install_report(state: InstallState) -> InstallStateReport {
        InstallStateReport {
            state,
            started_at: Some("2026-07-30T00:00:00Z".to_string()),
            pid: matches!(state, InstallState::AliveStarting | InstallState::AliveButUnresponsive)
                .then_some(4321),
            liveness_detail: Some("launchd job gui/501/com.loom.daemon alive".to_string()),
            heartbeat_freshness: Some(HeartbeatFreshness::Fresh),
            heartbeat_age_secs: Some(5),
            heartbeat_stale_threshold_secs: Some(120),
            process_age_secs: Some(9),
            startup_grace_threshold_secs: Some(45),
            watchdog_log_path: PathBuf::from("/tmp/watchdog.log"),
            pid_file_stale_note: None,
        }
    }

    fn healthy_status() -> DaemonStatusReport {
        let mut status = DaemonStatusReport {
            capacity: crate::types::CapacityReport {
                ranking_present: true,
                total_accounts: 8,
                healthy_accounts: 6,
                exhausted_accounts: 2,
                token_axis_limit: 6,
                token_bound: false,
            },
            dynamic_cap: 7,
            work_finder_enabled: Some(true),
            // #4824: the healthy baseline is a daemon built from the SAME
            // commit as the client asking, on the default tick cadence — the
            // only state in which missing telemetry is attributable to the
            // daemon rather than to build skew.
            daemon_build_commit: Some(CLI_COMMIT.to_string()),
            work_finder_interval_secs: Some(60),
            ..Default::default()
        };
        status.last_work_finder_tick = Some(crate::types::WorkFinderTickSummary {
            at: now() - chrono::Duration::seconds(30),
            max_concurrent: 7,
            seen: 12,
            dispatched: 2,
            skipped_in_flight: 10,
            ..Default::default()
        });
        status
    }

    fn healthy_inputs() -> HealthInputs {
        HealthInputs {
            at: now(),
            window: Duration::from_secs(DEFAULT_WINDOW_SECS),
            status: Some(healthy_status()),
            ipc_error: None,
            install_state: Some(install_report(InstallState::AliveButUnresponsive)),
            pgrep_pids: vec![4321],
            pid_file: None,
            ranking_present: true,
            ranking_age_secs: Some(240),
            pipeline: Some(vec![RepoPipelineSnapshot {
                root: PathBuf::from("/repos/loom"),
                queued: Some(5),
                merged_24h: Some(3),
                ..Default::default()
            }]),
            gh_unavailable: None,
            cli_build_commit: CLI_COMMIT.to_string(),
            work_finder_log_tick_age_secs: None,
        }
    }

    /// The client's build commit in every fixture below. A synthetic value, not
    /// the real [`crate::self_update::BUILT_COMMIT`], so the skew assertions do
    /// not depend on how the test binary happened to be built.
    const CLI_COMMIT: &str = "18887b5c";

    // ===================================================================
    // #4694 regression pins — liveness precedence
    // ===================================================================

    /// The single most important assertion in this file: a reachable daemon is
    /// GREEN even when the local install-state probe reports it dead. This is
    /// the #4694 false negative — the launchd domain probe declaring a live,
    /// dispatching daemon dead — and it must never be able to override an
    /// answered IPC round-trip.
    #[test]
    fn ipc_reachable_beats_a_dead_launchd_verdict() {
        let mut inputs = healthy_inputs();
        inputs.install_state = Some(install_report(InstallState::ExpectedButDead));
        let section = assess_liveness(&inputs);
        assert_eq!(section.verdict, Verdict::Green);
        assert_eq!(section.detail["signal"], "ipc");
    }

    // ===================================================================
    // #4774 regression pins — the pid file is advisory, cross-checked
    // ===================================================================

    /// Build a pid-file observation for the assess tests.
    fn pid_obs(recorded: Option<u32>, alive: bool) -> crate::daemon_pidfile::PidFileObservation {
        crate::daemon_pidfile::PidFileObservation {
            path: PathBuf::from("/home/.loom/.daemon.pid"),
            present: recorded.is_some(),
            recorded_pid: recorded,
            recorded_pid_alive: alive,
        }
    }

    /// An inputs fixture whose daemon answered IPC and reported its own pid —
    /// the post-#4774 wire shape every assertion below builds on.
    fn reachable_inputs_with_socket_owner(pid: u32) -> HealthInputs {
        let mut inputs = healthy_inputs();
        let mut status = healthy_status();
        status.daemon_pid = Some(pid);
        inputs.status = Some(status);
        inputs
    }

    /// The healthy case must be *unchanged* by #4774: a pid file naming the
    /// process that answered adds no note and no verdict change.
    #[test]
    fn a_pid_file_matching_the_socket_owner_stays_green() {
        let mut inputs = reachable_inputs_with_socket_owner(99917);
        inputs.pid_file = Some(pid_obs(Some(99917), true));
        let section = assess_liveness(&inputs);
        assert_eq!(section.verdict, Verdict::Green);
        assert_eq!(section.detail["pid_file_state"], "matches");
        assert_eq!(section.detail["socket_owner_pid"], 99917);
        assert!(!section.summary.contains("STALE"), "{}", section.summary);
    }

    /// THE #4774 pin. The 2026-07-31 incident, exactly: the file says 13724,
    /// the daemon answering the socket says 99917. Liveness is not in doubt —
    /// the daemon just answered — but the file is a booby trap for every other
    /// consumer, so the section degrades and names both pids.
    #[test]
    fn a_pid_file_naming_a_different_process_degrades_and_is_named() {
        let mut inputs = reachable_inputs_with_socket_owner(99917);
        inputs.pid_file = Some(pid_obs(Some(13724), true));
        let section = assess_liveness(&inputs);
        assert_eq!(
            section.verdict,
            Verdict::Degraded,
            "a stale pid file must not be reported as healthy: {}",
            section.summary
        );
        assert_eq!(section.detail["pid_file_state"], "mismatch");
        assert!(
            section.summary.contains("13724") && section.summary.contains("99917"),
            "the summary must name both the recorded and the real pid: {}",
            section.summary
        );
        // Liveness itself is still positively established.
        assert_eq!(section.detail["ipc_reachable"], true);
    }

    /// A reachable daemon whose pid file records a pid that is neither the
    /// socket owner nor a live process at all — a relaunch after the old pid
    /// was recycled away. `classify()` still calls this "mismatch" (a live
    /// socket owner is known, so it always outranks the file's own liveness
    /// check) — the name reflects the verdict actually produced, not "dead".
    #[test]
    fn a_pid_file_naming_a_mismatched_dead_process_degrades() {
        let mut inputs = reachable_inputs_with_socket_owner(99917);
        inputs.pid_file = Some(pid_obs(Some(13724), false));
        let section = assess_liveness(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert_eq!(section.detail["pid_file_state"], "mismatch");
    }

    /// An absent pid file makes no false claim, so it is not an anomaly — a
    /// daemon started outside the managed start path legitimately has none.
    #[test]
    fn an_absent_pid_file_does_not_degrade_a_reachable_daemon() {
        let mut inputs = reachable_inputs_with_socket_owner(99917);
        inputs.pid_file = Some(pid_obs(None, false));
        let section = assess_liveness(&inputs);
        assert_eq!(section.verdict, Verdict::Green);
        assert_eq!(section.detail["pid_file_state"], "absent");
    }

    /// Backward compatibility: a **pre-#4774 daemon** answers without a
    /// `daemon_pid`, so there is nothing to cross-check against. That must read
    /// as "unverified", never as a mismatch — an upgrade-order false alarm would
    /// be worse than the bug this issue fixes, since it would fire on every
    /// fleet host until the last daemon rolled.
    #[test]
    fn a_pre_4774_daemon_never_produces_a_false_mismatch() {
        let mut inputs = healthy_inputs();
        let mut status = healthy_status();
        status.daemon_pid = None;
        inputs.status = Some(status);
        inputs.pid_file = Some(pid_obs(Some(13724), true));
        let section = assess_liveness(&inputs);
        assert_eq!(section.verdict, Verdict::Green);
        assert_eq!(section.detail["pid_file_state"], "unverified");
        assert!(section.detail["socket_owner_pid"].is_null());
    }

    /// The daemon's self-reported pid outranks the install-state probe's pid
    /// for the rendered `pid` — it is `std::process::id()` from inside the
    /// answering process, versus a launchd/pid-file inference from outside.
    #[test]
    fn the_reported_pid_prefers_the_daemons_own_over_the_probes() {
        // `install_report` pins the probe's pid at 4321.
        let inputs = reachable_inputs_with_socket_owner(99917);
        let section = assess_liveness(&inputs);
        assert_eq!(section.detail["pid"], 99917);
        assert!(section.summary.contains("99917"), "{}", section.summary);
    }

    /// On the *unreachable* path there is no `daemon_pid` to arbitrate with, so
    /// the note comes from the install-state probe's own launchd cross-check —
    /// and it must reach the operator-facing summary, not just the JSON.
    #[test]
    fn an_unreachable_daemons_stale_note_reaches_the_summary() {
        let mut inputs = healthy_inputs();
        inputs.status = None;
        inputs.ipc_error = Some("round-trip timed out".to_string());
        let mut report = install_report(InstallState::AliveButUnresponsive);
        report.pid_file_stale_note = Some("STALE pid file /home/.loom/.daemon.pid: …".to_string());
        inputs.install_state = Some(report);
        let section = assess_liveness(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(
            section.summary.contains("STALE pid file"),
            "the install-state probe's #4774 note must be surfaced: {}",
            section.summary
        );
        // Regression pin: a hand-joined literal + suffix_note() must not
        // leave a double-space run in the operator-facing summary — fmt and
        // clippy cannot see this class of bug, only a runtime assertion can.
        assert!(!section.summary.contains("  "), "double space in summary: {}", section.summary);
    }

    /// The launchd domain probe alone can never produce a DEAD verdict: with
    /// IPC unreachable and the install-state classification negative, a live
    /// `pgrep` pid still holds the verdict at DEGRADED.
    #[test]
    fn pgrep_blocks_a_dead_verdict_when_launchd_and_pidfile_are_negative() {
        let mut inputs = healthy_inputs();
        inputs.status = None;
        inputs.ipc_error = Some("connect failed".to_string());
        inputs.install_state = Some(install_report(InstallState::ExpectedButDead));
        inputs.pgrep_pids = vec![9911];
        let section = assess_liveness(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert_eq!(section.detail["signal"], "pgrep");
        assert_eq!(section.detail["declared_dead"], false);
    }

    /// An install-state classification of "alive but unresponsive" is DEGRADED,
    /// never DEAD — the daemon is running, it is just not answering.
    #[test]
    fn alive_but_unresponsive_is_degraded_not_dead() {
        let mut inputs = healthy_inputs();
        inputs.status = None;
        inputs.ipc_error = Some("round-trip timed out".to_string());
        inputs.pgrep_pids = vec![];
        let section = assess_liveness(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("NOT dead"));
        // Regression pin: the hand-joined literal must not leave a double
        // space before {ipc_error} — fmt/clippy cannot see this class of bug.
        assert!(!section.summary.contains("  "), "double space in summary: {}", section.summary);
    }

    #[test]
    fn alive_starting_is_degraded_not_dead() {
        let mut inputs = healthy_inputs();
        inputs.status = None;
        inputs.ipc_error = Some("connect failed".to_string());
        inputs.install_state = Some(install_report(InstallState::AliveStarting));
        inputs.pgrep_pids = vec![];
        let section = assess_liveness(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("STARTING"));
        // Regression pin: the hand-joined literal must not leave a double
        // space before {ipc_error} — fmt/clippy cannot see this class of bug.
        assert!(!section.summary.contains("  "), "double space in summary: {}", section.summary);
    }

    /// Only when *all three* signals are negative is the daemon declared dead.
    #[test]
    fn all_three_signals_negative_declares_dead() {
        let mut inputs = healthy_inputs();
        inputs.status = None;
        inputs.ipc_error = Some("connect failed".to_string());
        inputs.install_state = Some(install_report(InstallState::ExpectedButDead));
        inputs.pgrep_pids = vec![];
        let section = assess_liveness(&inputs);
        assert_eq!(section.verdict, Verdict::Dead);
        assert_eq!(section.detail["signal"], "all-negative");
    }

    /// An undiagnosable probe is UNKNOWN (exit 1), never DEAD (exit 2).
    #[test]
    fn undiagnosable_is_unknown_never_dead() {
        let mut inputs = healthy_inputs();
        inputs.status = None;
        inputs.ipc_error = Some("connect failed".to_string());
        inputs.install_state = None;
        inputs.pgrep_pids = vec![];
        let section = assess_liveness(&inputs);
        assert_eq!(section.verdict, Verdict::Unknown);
        assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
    }

    // ===================================================================
    // Exit-code contract
    // ===================================================================

    #[test]
    fn healthy_inputs_exit_zero() {
        let report = assess(&healthy_inputs());
        assert_eq!(report.overall, Verdict::Green, "{}", report.render_human());
        assert_eq!(report.exit_code(), EXIT_HEALTHY);
    }

    #[test]
    fn any_degraded_section_exits_one() {
        let mut inputs = healthy_inputs();
        inputs.status.as_mut().unwrap().capacity.healthy_accounts = 0;
        let report = assess(&inputs);
        assert_eq!(report.overall, Verdict::Degraded);
        assert_eq!(report.exit_code(), EXIT_DEGRADED);
    }

    #[test]
    fn dead_daemon_exits_two_and_marks_other_sections_unknown() {
        let mut inputs = healthy_inputs();
        inputs.status = None;
        inputs.ipc_error = Some("connect failed".to_string());
        inputs.install_state = Some(install_report(InstallState::ExpectedButDead));
        inputs.pgrep_pids = vec![];
        let report = assess(&inputs);
        assert_eq!(report.overall, Verdict::Dead);
        assert_eq!(report.exit_code(), EXIT_DEAD);
        assert_eq!(report.section("dispatch").unwrap().verdict, Verdict::Unknown);
        assert_eq!(report.section("tokens").unwrap().verdict, Verdict::Unknown);
    }

    // ===================================================================
    // Observability section (#4830)
    // ===================================================================

    /// An inputs fixture whose daemon has confirmed a host-identity mismatch —
    /// the live 2026-07-31 shape (the Studio's key bound to `robb-pro`).
    fn mismatched_inputs(age_secs: i64) -> HealthInputs {
        let mut inputs = healthy_inputs();
        inputs
            .status
            .as_mut()
            .unwrap()
            .observability_host_id_mismatch = Some(crate::types::ObservabilityHostIdMismatch {
            daemon_host_id: "robb-studio".to_string(),
            ingest_host_id: "robb-pro".to_string(),
            first_seen_at: now() - chrono::Duration::seconds(age_secs),
        });
        inputs
    }

    #[test]
    fn no_observability_section_when_the_host_ids_agree() {
        // AC: "no behavior change when they match" — a healthy daemon's report
        // is byte-for-byte what it was before #4830.
        let report = assess(&healthy_inputs());
        assert!(report.section("observability").is_none());
        assert_eq!(report.overall, Verdict::Green);
        assert_eq!(report.exit_code(), EXIT_HEALTHY);
    }

    #[test]
    fn no_observability_section_for_a_disabled_or_keyless_exporter() {
        // A disabled exporter never registers a status handle, so the field is
        // `None` on the wire — indistinguishable, by design, from "enabled and
        // correctly bound".
        let mut inputs = healthy_inputs();
        inputs
            .status
            .as_mut()
            .unwrap()
            .observability_host_id_mismatch = None;
        assert!(assess_observability(&inputs).is_none());
    }

    #[test]
    fn no_observability_section_when_the_daemon_is_unreachable() {
        let mut inputs = healthy_inputs();
        inputs.status = None;
        assert!(assess_observability(&inputs).is_none());
    }

    #[test]
    fn a_host_id_mismatch_is_a_degraded_observability_note() {
        let report = assess(&mismatched_inputs(3600));
        let section = report.section("observability").expect("section present");
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(
            section.summary.contains("robb-pro") && section.summary.contains("robb-studio"),
            "the note must name BOTH identities: {}",
            section.summary
        );
        assert_eq!(section.detail["daemon_host_id"], "robb-studio");
        assert_eq!(section.detail["ingest_host_id"], "robb-pro");
        assert_eq!(section.detail["first_seen_age_secs"], 3600);
        assert_eq!(report.overall, Verdict::Degraded);
        assert_eq!(report.exit_code(), EXIT_DEGRADED);
    }

    #[test]
    fn the_observability_section_is_appended_last_and_only_when_present() {
        let with_mismatch = assess(&mismatched_inputs(60));
        let keys: Vec<&str> = with_mismatch.sections.iter().map(|s| s.key).collect();
        assert_eq!(keys.last(), Some(&"observability"));
        assert_eq!(keys.len(), 7);
        assert_eq!(assess(&healthy_inputs()).sections.len(), 6);
    }

    // ===================================================================
    // Observability export liveness (#5083)
    // ===================================================================

    /// Inputs whose daemon reports a running exporter with the given export
    /// record. `now()` is the report's `at`, so ages are exact.
    fn exporting_inputs(
        mutate: impl FnOnce(&mut crate::types::ObservabilityExportStatus),
    ) -> HealthInputs {
        let mut inputs = healthy_inputs();
        let mut export = crate::types::ObservabilityExportStatus {
            state: ObservabilityExportState::Starting,
            host_id: Some("robb-studio".to_string()),
            ingest_host_id: None,
            endpoint: Some("https://dashboard.example/ingest".to_string()),
            exporter: Some("https".to_string()),
            started_at: Some(now() - chrono::Duration::hours(4)),
            last_success_at: None,
            last_failure_at: None,
            last_failure_detail: None,
            records_exported: 0,
            consecutive_failures: 0,
            flush_interval_secs: Some(30),
        };
        mutate(&mut export);
        inputs.status.as_mut().unwrap().observability_export = Some(export);
        inputs
    }

    #[test]
    fn a_healthy_exporter_still_renders_no_observability_section() {
        // The #4830 guarantee this issue must NOT reverse: exporting normally
        // stays silent on the anomaly-only surface. The positive confirmation
        // lives on `loom-daemon status` instead.
        let inputs = exporting_inputs(|e| {
            e.last_success_at = Some(now() - chrono::Duration::seconds(12));
            e.records_exported = 3481;
        });
        let report = assess(&inputs);
        assert!(report.section("observability").is_none());
        assert_eq!(report.overall, Verdict::Green);
        assert_eq!(report.sections.len(), 6);
    }

    #[test]
    fn a_disabled_exporter_still_renders_no_observability_section() {
        let mut inputs = healthy_inputs();
        inputs.status.as_mut().unwrap().observability_export =
            Some(crate::types::ObservabilityExportStatus::disabled());
        assert!(assess_observability(&inputs).is_none());
    }

    #[test]
    fn a_starting_exporter_is_not_yet_called_out() {
        // A daemon restarted 20 seconds ago has not had a fair chance to
        // flush; reporting it as broken would make every roll look like an
        // outage.
        let inputs = exporting_inputs(|e| {
            e.started_at = Some(now() - chrono::Duration::seconds(20));
        });
        assert!(assess_observability(&inputs).is_none());
    }

    #[test]
    fn a_never_exporting_host_is_a_degraded_observability_note() {
        // THE gap this issue exists for: configured, running for four hours,
        // and has never landed a single batch — which before #5083 rendered
        // exactly like a healthy host: no section at all.
        let inputs = exporting_inputs(|_| {});
        let report = assess(&inputs);
        let section = report.section("observability").expect("section present");
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(
            section.summary.contains("NEVER"),
            "the note must name the condition unambiguously: {}",
            section.summary
        );
        assert!(
            section.summary.contains("robb-studio")
                && section.summary.contains("dashboard.example"),
            "the note must name the host identity AND the endpoint: {}",
            section.summary
        );
        // Machine-readable for a watch loop (AC5).
        assert_eq!(section.detail["state"], "never_exported");
        assert_eq!(section.detail["host_id"], "robb-studio");
        assert!(section.detail["last_success_at"].is_null());
        assert_eq!(report.overall, Verdict::Degraded);
        assert_eq!(report.exit_code(), EXIT_DEGRADED);
    }

    #[test]
    fn a_failing_exporter_is_a_degraded_observability_note() {
        let inputs = exporting_inputs(|e| {
            e.last_success_at = Some(now() - chrono::Duration::hours(2));
            e.records_exported = 900;
            e.consecutive_failures = 4;
            e.last_failure_at = Some(now() - chrono::Duration::seconds(30));
            e.last_failure_detail = Some("sink rejected batch: HTTP 401 — denied".to_string());
        });
        let report = assess(&inputs);
        let section = report.section("observability").expect("section present");
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(
            section.summary.contains("HTTP 401"),
            "the exporter's own error must reach the operator: {}",
            section.summary
        );
        assert!(
            section.summary.contains("2h"),
            "a regression must be distinguishable from never-worked: {}",
            section.summary
        );
        assert_eq!(section.detail["state"], "failing");
        assert_eq!(section.detail["consecutive_failures"], 4);
    }

    #[test]
    fn a_mismatch_still_wins_and_now_carries_the_export_facts() {
        // #4830 regression guard: the mismatch note is unchanged, and the
        // positive facts ride along in `detail.export` so a machine consumer
        // reading a DEGRADED section still learns whether anything is landing.
        let mut inputs = mismatched_inputs(3600);
        inputs.status.as_mut().unwrap().observability_export =
            Some(crate::types::ObservabilityExportStatus {
                state: ObservabilityExportState::HostIdMismatch,
                host_id: Some("robb-studio".to_string()),
                ingest_host_id: Some("robb-pro".to_string()),
                last_success_at: Some(now() - chrono::Duration::seconds(12)),
                records_exported: 77,
                started_at: Some(now() - chrono::Duration::hours(4)),
                ..Default::default()
            });
        let section = assess_observability(&inputs).expect("section present");
        assert_eq!(section.verdict, Verdict::Degraded);
        // Unchanged #4830 keys.
        assert_eq!(section.detail["daemon_host_id"], "robb-studio");
        assert_eq!(section.detail["ingest_host_id"], "robb-pro");
        assert_eq!(section.detail["first_seen_age_secs"], 3600);
        // Additive #5083 payload.
        assert_eq!(section.detail["export"]["records_exported"], 77);
        assert_eq!(section.detail["export"]["state"], "host_id_mismatch");
    }

    #[test]
    fn a_pre_5083_daemon_reporting_no_export_field_renders_nothing() {
        // An older daemon cannot answer, so the section stays absent — the
        // pre-#5083 baseline, never a fabricated "never exported".
        let mut inputs = healthy_inputs();
        let status = inputs.status.as_mut().unwrap();
        status.observability_export = None;
        status.observability_host_id_mismatch = None;
        assert!(assess_observability(&inputs).is_none());
    }

    #[test]
    fn report_always_has_all_six_sections() {
        let report = assess(&healthy_inputs());
        let keys: Vec<&str> = report.sections.iter().map(|s| s.key).collect();
        assert_eq!(
            keys,
            vec![
                "liveness",
                "dispatch",
                "tokens",
                "roles",
                "queues",
                "throughput"
            ]
        );
    }

    #[test]
    fn render_human_is_one_line_per_section_plus_overall() {
        let report = assess(&healthy_inputs());
        let rendered = report.render_human();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 7);
        assert!(lines[6].starts_with("overall"));
    }

    #[test]
    fn json_serialization_round_trips() {
        let report = assess(&healthy_inputs());
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["overall"], "green");
        assert_eq!(value["sections"].as_array().unwrap().len(), 6);
    }

    // ===================================================================
    // Dispatch section
    // ===================================================================

    #[test]
    fn dispatch_reports_last_tick_reason_summary() {
        let section = assess_dispatch(&healthy_inputs());
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section
            .summary
            .contains("12 seen, 2 dispatched, 10 in-flight-skip"));
    }

    /// #5177 AC4: `disk_binds_cap` is the strict-minimum test — disk must be
    /// smaller than BOTH other terms, never merely tied.
    #[test]
    fn disk_binds_cap_only_when_strictly_smallest() {
        // token axis = 6 × 2 = 12, configured 12, disk 2 → disk binds.
        assert!(disk_binds_cap(6, 2, 2, 12));
        // disk ties configured_max → operator's own ceiling, not a disk fault.
        assert!(!disk_binds_cap(6, 2, 12, 12));
        // disk larger than the token axis → tokens bind, not disk.
        assert!(!disk_binds_cap(1, 1, 5, 12));
        // per-token 0 is clamped to 1 (mirrors resolve_dynamic_max_concurrent).
        assert!(disk_binds_cap(6, 0, 2, 12));
        // the healthy default fixture (disk 0, configured 0) must NOT trip it.
        assert!(!disk_binds_cap(6, 0, 0, 0));
    }

    /// #5177 AC4: when disk headroom is the binding cap term, the dispatch
    /// section is Degraded and names disk as the cause — not a silent green
    /// daemon dispatching at a fraction of its token/config capacity.
    #[test]
    fn disk_bound_cap_produces_degraded_verdict_naming_disk() {
        let mut inputs = healthy_inputs();
        {
            let status = inputs.status.as_mut().unwrap();
            status.capacity.token_axis_limit = 6;
            status.per_token_concurrency = 2; // token axis = 12
            status.configured_max = 12;
            status.disk_headroom = 2; // strictly the smallest term
            status.dynamic_cap = 2;
        }
        let section = assess_dispatch(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(
            section.summary.to_lowercase().contains("disk"),
            "summary must name disk: {}",
            section.summary
        );
    }

    /// #5177 AC4 (negative): a cap bound by the operator's configured ceiling
    /// (disk headroom ample) stays green — disk is not the culprit there.
    #[test]
    fn config_bound_cap_is_not_flagged_as_disk() {
        let mut inputs = healthy_inputs();
        {
            let status = inputs.status.as_mut().unwrap();
            status.capacity.token_axis_limit = 6;
            status.per_token_concurrency = 2; // token axis = 12
            status.configured_max = 4; // operator's own ceiling binds
            status.disk_headroom = 50; // plenty of disk
            status.dynamic_cap = 4;
        }
        let section = assess_dispatch(&inputs);
        assert_eq!(section.verdict, Verdict::Green);
    }

    /// Inputs with no tick reported, a daemon well past the warm-up grace
    /// window, a matching build, and no corroborating log line — the one state
    /// in which "no tick" really does mean the work finder is dead (#4824).
    fn missing_tick_inputs() -> HealthInputs {
        let mut inputs = healthy_inputs();
        inputs.status.as_mut().unwrap().last_work_finder_tick = None;
        let mut report = install_report(InstallState::AliveButUnresponsive);
        report.process_age_secs = Some(3600);
        inputs.install_state = Some(report);
        inputs
    }

    #[test]
    fn dispatch_missing_tick_is_degraded_when_work_finder_is_enabled() {
        let section = assess_dispatch(&missing_tick_inputs());
        assert_eq!(section.verdict, Verdict::Degraded);
    }

    #[test]
    fn dispatch_missing_tick_is_green_when_work_finder_is_disabled_and_nothing_else_is_wrong() {
        let mut inputs = healthy_inputs();
        let status = inputs.status.as_mut().unwrap();
        status.last_work_finder_tick = None;
        status.work_finder_enabled = Some(false);
        // The DISABLED work finder is itself the (single) degradation.
        let section = assess_dispatch(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("DISABLED"));
        assert!(!section
            .summary
            .contains("no work-finder tick observed in this daemon process"));
    }

    // ===================================================================
    // #4824 regression pins — build skew and the post-restart grace window
    // must not be reported as a dead work finder
    // ===================================================================

    /// The message a genuinely dead work finder produces. Asserted absent in
    /// every false-DEGRADED case below.
    const DEAD_WORK_FINDER_MSG: &str = "no work-finder tick observed in this daemon process";

    /// THE #4824 pin, mode 1. A `health` built from HEAD querying a daemon
    /// built one commit earlier: the daemon cannot report a counter it does not
    /// have, and rendering that absence as "work finder dead" paged operators
    /// on a fleet that was demonstrably dispatching.
    #[test]
    fn dispatch_missing_tick_reports_build_skew_not_a_dead_work_finder() {
        let mut inputs = missing_tick_inputs();
        inputs.status.as_mut().unwrap().daemon_build_commit = Some("105f9c12".to_string());
        let section = assess_dispatch(&inputs);
        assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
        assert!(section.summary.contains("105f9c12"), "{}", section.summary);
        assert!(section.summary.contains("predates tick telemetry"), "{}", section.summary);
        assert!(section.summary.contains("loom-daemon-update.sh"), "{}", section.summary);
        assert!(!section.summary.contains(DEAD_WORK_FINDER_MSG), "{}", section.summary);
        assert_eq!(section.detail["missing_tick"]["reason"], "build_skew");
    }

    /// A daemon predating #4824 reports no commit at all. That is *also* skew
    /// (it necessarily predates every field added since), and must be named as
    /// such rather than collapsed into a dead-loop verdict.
    #[test]
    fn dispatch_missing_tick_from_a_daemon_with_no_reported_commit_is_skew() {
        let mut inputs = missing_tick_inputs();
        inputs.status.as_mut().unwrap().daemon_build_commit = None;
        let section = assess_dispatch(&inputs);
        assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
        assert!(section.summary.contains("<unreported>"), "{}", section.summary);
        assert!(!section.summary.contains(DEAD_WORK_FINDER_MSG), "{}", section.summary);
    }

    /// AC4: exit code stays 0 under skew when everything else is green.
    #[test]
    fn build_skew_alone_does_not_change_the_exit_code() {
        let mut inputs = missing_tick_inputs();
        inputs.status.as_mut().unwrap().daemon_build_commit = Some("105f9c12".to_string());
        assert_eq!(assess(&inputs).exit_code(), EXIT_HEALTHY);
    }

    /// THE #4824 pin, mode 2. Immediately after `loom-daemon restart` the
    /// per-process telemetry is legitimately empty for up to one tick interval.
    #[test]
    fn dispatch_missing_tick_within_the_grace_window_reports_warming_up() {
        let mut inputs = missing_tick_inputs();
        let mut report = install_report(InstallState::AliveButUnresponsive);
        report.process_age_secs = Some(30);
        inputs.install_state = Some(report);
        let section = assess_dispatch(&inputs);
        assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
        assert!(section.summary.contains("warming up"), "{}", section.summary);
        assert!(!section.summary.contains(DEAD_WORK_FINDER_MSG), "{}", section.summary);
        assert_eq!(section.detail["missing_tick"]["reason"], "warming_up");
        assert_eq!(assess(&inputs).exit_code(), EXIT_HEALTHY);
    }

    /// The grace window is `2 ×` the daemon's OWN resolved interval, not the
    /// default: a daemon on a 300s cadence must not false-alarm for its whole
    /// first interval after a roll.
    #[test]
    fn the_grace_window_scales_with_the_daemons_own_tick_interval() {
        let mut inputs = missing_tick_inputs();
        let mut report = install_report(InstallState::AliveButUnresponsive);
        report.process_age_secs = Some(400);
        inputs.install_state = Some(report);

        // Default 60s cadence ⇒ 120s grace ⇒ 400s old is well past it.
        inputs.status.as_mut().unwrap().work_finder_interval_secs = Some(60);
        assert_eq!(assess_dispatch(&inputs).verdict, Verdict::Degraded);

        // 300s cadence ⇒ 600s grace ⇒ still warming up.
        inputs.status.as_mut().unwrap().work_finder_interval_secs = Some(300);
        let section = assess_dispatch(&inputs);
        assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
        assert!(section.summary.contains("warming up"), "{}", section.summary);
    }

    /// A pre-#4824 daemon reports no interval; fall back to the default cadence
    /// rather than treating the absence as "no grace at all".
    #[test]
    fn an_unreported_tick_interval_falls_back_to_the_default_cadence() {
        let mut status = healthy_status();
        status.work_finder_interval_secs = None;
        assert_eq!(
            work_finder_grace_secs(&status),
            crate::work_finder::DEFAULT_WORK_FINDER_INTERVAL_SECS
                * WORK_FINDER_TICK_GRACE_INTERVALS
        );
    }

    /// AC5: the daemon log is consulted as a corroborating signal before the
    /// work finder is declared dead — the exact 2026-07-31 disagreement, where
    /// `health` said "no tick" while `daemon.log` carried one every ~60s.
    #[test]
    fn recent_work_finder_log_activity_blocks_a_dead_verdict() {
        let mut inputs = missing_tick_inputs();
        inputs.work_finder_log_tick_age_secs = Some(45);
        let section = assess_dispatch(&inputs);
        assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
        assert!(section.summary.contains("daemon log"), "{}", section.summary);
        assert!(!section.summary.contains(DEAD_WORK_FINDER_MSG), "{}", section.summary);
        assert_eq!(section.detail["missing_tick"]["reason"], "log_corroborated");
    }

    /// Corroboration can only *soften* the verdict, never harden it — and a log
    /// line older than the grace window corroborates nothing.
    #[test]
    fn stale_work_finder_log_activity_does_not_block_a_dead_verdict() {
        let mut inputs = missing_tick_inputs();
        inputs.work_finder_log_tick_age_secs = Some(9_999);
        assert_eq!(assess_dispatch(&inputs).verdict, Verdict::Degraded);
    }

    /// THE regression guard. With a matching build, a daemon long past the
    /// grace window, and no corroborating log activity, a missing tick is still
    /// a fault — this fix must not be able to silence a genuinely dead work
    /// finder.
    #[test]
    fn a_genuinely_dead_work_finder_is_still_degraded() {
        let inputs = missing_tick_inputs();
        let section = assess_dispatch(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains(DEAD_WORK_FINDER_MSG), "{}", section.summary);
        assert_eq!(section.detail["missing_tick"]["reason"], "dead");
        assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
    }

    /// Warm-up outranks skew: a just-restarted daemon on a *different* commit
    /// is reported as warming up, the explanation that is true regardless of
    /// build state.
    #[test]
    fn warming_up_outranks_build_skew() {
        let mut inputs = missing_tick_inputs();
        inputs.status.as_mut().unwrap().daemon_build_commit = Some("105f9c12".to_string());
        let mut report = install_report(InstallState::AliveButUnresponsive);
        report.process_age_secs = Some(5);
        inputs.install_state = Some(report);
        assert_eq!(
            classify_missing_tick(&inputs, inputs.status.as_ref().unwrap()),
            MissingTick::WarmingUp {
                age_secs: 5,
                grace_secs: 120
            }
        );
    }

    // -------------------------------------------------------------------
    // #4824 — build-skew classification
    // -------------------------------------------------------------------

    #[test]
    fn identical_commits_are_a_match() {
        assert_eq!(classify_build_skew("abc1234", Some("abc1234")), BuildSkew::Match);
        assert!(!BuildSkew::Match.may_predate_client());
    }

    #[test]
    fn differing_commits_are_skew() {
        assert_eq!(
            classify_build_skew("abc1234", Some("def5678")),
            BuildSkew::Skew("def5678".to_string())
        );
        assert!(classify_build_skew("abc1234", Some("def5678")).may_predate_client());
    }

    #[test]
    fn an_absent_daemon_commit_is_daemon_unknown() {
        assert_eq!(classify_build_skew("abc1234", None), BuildSkew::DaemonUnknown);
        assert!(BuildSkew::DaemonUnknown.may_predate_client());
    }

    /// A tarball build bakes in `"unknown"`. It must never be read as a commit
    /// that happens to differ from every real one — that would permanently
    /// suppress the dead-work-finder verdict on such an install.
    #[test]
    fn an_unknown_commit_on_either_side_is_incomparable() {
        assert_eq!(classify_build_skew("unknown", Some("def5678")), BuildSkew::Incomparable);
        assert_eq!(classify_build_skew("abc1234", Some("unknown")), BuildSkew::Incomparable);
        assert!(!BuildSkew::Incomparable.may_predate_client());
    }

    /// …and an incomparable build must therefore still produce a dead verdict
    /// when nothing else explains the silence.
    #[test]
    fn an_incomparable_build_still_reports_a_dead_work_finder() {
        let mut inputs = missing_tick_inputs();
        inputs.cli_build_commit = "unknown".to_string();
        assert_eq!(assess_dispatch(&inputs).verdict, Verdict::Degraded);
    }

    // -------------------------------------------------------------------
    // #4824 — the daemon-log corroborating probe (pure half)
    // -------------------------------------------------------------------

    fn log_now() -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str("2026-07-31T14:30:00.000", DAEMON_LOG_STAMP_FORMAT)
            .unwrap()
    }

    #[test]
    fn log_probe_reads_the_newest_work_finder_line() {
        let log = "\
[2026-07-31T14:20:00.000] [INFO] work_finder: tick — cap 16; 12 seen, 0 dispatched
[2026-07-31T14:29:30.500] [INFO] work_finder: tick — cap 16; 12 seen, 2 dispatched
[2026-07-31T14:29:59.000] [INFO] sweep_registry: reaped 1 finished sweep
";
        assert_eq!(work_finder_log_tick_age_secs(log, log_now()), Some(29));
    }

    #[test]
    fn log_probe_returns_none_without_a_work_finder_line() {
        let log = "[2026-07-31T14:29:59.000] [INFO] sweep_registry: reaped 1 finished sweep\n";
        assert_eq!(work_finder_log_tick_age_secs(log, log_now()), None);
    }

    /// The tail read starts mid-line, so the first line is routinely a fragment
    /// with no parseable `[stamp]`. It must be skipped, not abort the scan.
    #[test]
    fn log_probe_skips_an_unparseable_partial_first_line() {
        let log = "ck — cap 16; 12 seen, 0 dispatched  work_finder: partial\n\
[2026-07-31T14:28:00.000] [INFO] work_finder: tick — cap 16; 12 seen, 1 dispatched\n";
        assert_eq!(work_finder_log_tick_age_secs(log, log_now()), Some(120));
    }

    /// Clock skew (the log stamped ahead of this process) reads as age 0, never
    /// an underflow.
    #[test]
    fn log_probe_clamps_a_future_stamp_to_zero() {
        let log = "[2026-07-31T14:35:00.000] [INFO] work_finder: tick — cap 16\n";
        assert_eq!(work_finder_log_tick_age_secs(log, log_now()), Some(0));
    }

    #[test]
    fn dispatch_flags_a_halted_gate() {
        let mut inputs = healthy_inputs();
        inputs.status.as_mut().unwrap().main_health_gate_halted = true;
        let section = assess_dispatch(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("HALTED"));
    }

    #[test]
    fn dispatch_flags_tick_errors() {
        let mut inputs = healthy_inputs();
        inputs
            .status
            .as_mut()
            .unwrap()
            .last_work_finder_tick
            .as_mut()
            .unwrap()
            .errors = 3;
        let section = assess_dispatch(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("3 dispatch error(s)"));
    }

    // ===================================================================
    // Tokens section
    // ===================================================================

    #[test]
    fn tokens_green_when_healthy_and_ranking_fresh() {
        let section = assess_tokens(&healthy_inputs());
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.starts_with("6/8 healthy (2 exhausted)"));
    }

    #[test]
    fn tokens_degraded_when_all_exhausted() {
        let mut inputs = healthy_inputs();
        let cap = &mut inputs.status.as_mut().unwrap().capacity;
        cap.healthy_accounts = 0;
        cap.exhausted_accounts = 8;
        let section = assess_tokens(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("token-starved"));
    }

    #[test]
    fn tokens_degraded_when_ranking_is_stale() {
        let mut inputs = healthy_inputs();
        inputs.ranking_age_secs = Some(RANKING_STALE_SECS + 1);
        let section = assess_tokens(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("STALE"));
    }

    #[test]
    fn tokens_degraded_when_ranking_is_absent() {
        let mut inputs = healthy_inputs();
        inputs.ranking_present = false;
        inputs.ranking_age_secs = None;
        let section = assess_tokens(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("no .ranking"));
    }

    #[test]
    fn tokens_degraded_on_an_empty_pool() {
        let mut inputs = healthy_inputs();
        let cap = &mut inputs.status.as_mut().unwrap().capacity;
        cap.total_accounts = 0;
        cap.healthy_accounts = 0;
        cap.exhausted_accounts = 0;
        let section = assess_tokens(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("EMPTY token pool"));
    }

    // ===================================================================
    // Role-tick classification
    // ===================================================================

    fn record(role: &str, root: &str, ago_secs: i64, ok: bool) -> RoleTickRecord {
        RoleTickRecord {
            root: PathBuf::from(root),
            role: role.to_string(),
            at: now() - chrono::Duration::seconds(ago_secs),
            ok,
            detail: (!ok).then(|| "boom".to_string()),
        }
    }

    #[test]
    fn a_failure_followed_by_a_success_is_transient() {
        let records = vec![
            record("curator", "/r/loom", 600, false),
            record("curator", "/r/loom", 300, true),
        ];
        let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
        assert!(summary.persistent.is_empty());
        assert_eq!(summary.transient.len(), 1);
        assert_eq!(summary.transient[0].failures, 1);
    }

    #[test]
    fn a_failure_that_is_still_the_latest_record_is_persistent() {
        let records = vec![
            record("champion", "/r/loom", 600, true),
            record("champion", "/r/loom", 300, false),
            record("champion", "/r/loom", 60, false),
        ];
        let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
        assert_eq!(summary.persistent.len(), 1);
        assert_eq!(summary.persistent[0].failures, 2);
        assert_eq!(summary.persistent[0].detail.as_deref(), Some("boom"));
        assert!(summary.transient.is_empty());
    }

    #[test]
    fn each_root_role_pair_is_classified_independently() {
        let records = vec![
            record("curator", "/r/loom", 300, false),
            record("curator", "/r/anvil", 300, false),
            record("curator", "/r/anvil", 100, true),
        ];
        let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
        assert_eq!(summary.persistent.len(), 1);
        assert_eq!(summary.persistent[0].root, PathBuf::from("/r/loom"));
        assert_eq!(summary.transient.len(), 1);
        assert_eq!(summary.transient[0].root, PathBuf::from("/r/anvil"));
    }

    #[test]
    fn records_outside_the_window_are_ignored() {
        let records = vec![record("guide", "/r/loom", 7200, false)];
        let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
        assert_eq!(summary.total, 0);
        assert!(summary.persistent.is_empty());
    }

    #[test]
    fn only_persistent_failures_make_the_roles_section_degraded() {
        let mut inputs = healthy_inputs();
        inputs.status.as_mut().unwrap().role_tick_records = vec![
            record("curator", "/r/loom", 600, false),
            record("curator", "/r/loom", 300, true),
        ];
        let section = assess_roles(&inputs);
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("1 transient (self-recovered)"));

        inputs.status.as_mut().unwrap().role_tick_records =
            vec![record("curator", "/r/loom", 60, false)];
        let section = assess_roles(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("PERSISTENT"));
        assert!(section.summary.contains("curator @ loom"));
    }

    #[test]
    fn no_role_ticks_in_window_is_green() {
        let section = assess_roles(&healthy_inputs());
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("no role ticks in window"));
    }

    // ------- Escalation on N consecutive identical failures (#5023) -------

    /// A failure record with an explicit `detail`, for the identical-vs-varying
    /// escalation tests (#5023) and the summary-bounding tests (#5024) — the
    /// plain `record` helper always uses `"boom"`.
    fn record_detail(role: &str, root: &str, ago_secs: i64, detail: &str) -> RoleTickRecord {
        RoleTickRecord {
            root: PathBuf::from(root),
            role: role.to_string(),
            at: now() - chrono::Duration::seconds(ago_secs),
            ok: false,
            detail: Some(detail.to_string()),
        }
    }

    #[test]
    fn n_consecutive_identical_failures_escalate() {
        // Exactly ROLE_TICK_ESCALATION_THRESHOLD identical failures, oldest
        // first — the config-shaped case the 2026-08-03 outage produced.
        let n = ROLE_TICK_ESCALATION_THRESHOLD;
        let records: Vec<RoleTickRecord> = (0..n)
            .map(|i| record("judge", "/r/loom", ((n - i) * 10) as i64, false))
            .collect();
        let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
        assert_eq!(summary.persistent.len(), 1);
        assert_eq!(summary.escalated.len(), 1, "N identical failures must escalate");
        assert_eq!(summary.escalated[0].consecutive_identical, n);
        assert_eq!(summary.escalated[0].detail.as_deref(), Some("boom"));
    }

    #[test]
    fn n_minus_one_failures_interspersed_with_a_success_do_not_escalate() {
        // (N-1) identical failures, one success, then (N-1) identical failures:
        // the latest record is a failure (persistent) but the trailing run of
        // *consecutive* identical failures is only N-1 — the success in the
        // middle reset the streak, so escalation must NOT trigger.
        let n = ROLE_TICK_ESCALATION_THRESHOLD;
        let mut records = Vec::new();
        let mut ago = ((2 * n) * 10) as i64;
        for _ in 0..(n - 1) {
            records.push(record("judge", "/r/loom", ago, false));
            ago -= 10;
        }
        records.push(record("judge", "/r/loom", ago, true));
        ago -= 10;
        for _ in 0..(n - 1) {
            records.push(record("judge", "/r/loom", ago, false));
            ago -= 10;
        }
        let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
        assert_eq!(summary.persistent.len(), 1);
        assert_eq!(summary.persistent[0].consecutive_identical, n - 1);
        assert!(
            summary.escalated.is_empty(),
            "a success within the tail must reset the streak below the threshold"
        );
    }

    #[test]
    fn escalation_clears_once_the_tick_succeeds_again() {
        // N identical failures (would escalate) followed by a success: the pair
        // is now transient, `escalated` is empty, and the streak resets to 0 —
        // no permanent lockout from a since-fixed cause.
        let n = ROLE_TICK_ESCALATION_THRESHOLD;
        let mut records: Vec<RoleTickRecord> = (0..n)
            .map(|i| record("judge", "/r/loom", ((n + 1 - i) * 10) as i64, false))
            .collect();
        records.push(record("judge", "/r/loom", 5, true));
        let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
        assert!(summary.escalated.is_empty());
        assert!(summary.persistent.is_empty());
        assert_eq!(summary.transient.len(), 1);
        assert_eq!(summary.transient[0].consecutive_identical, 0);
    }

    #[test]
    fn a_different_failure_detail_breaks_the_identical_streak() {
        // N-1 identical failures then one failure with a DIFFERENT detail: the
        // trailing run of *identical* failures is only 1, so exact-detail
        // matching keeps a flapping (varying-message) failure out of escalation
        // even though the pair is persistent.
        let n = ROLE_TICK_ESCALATION_THRESHOLD;
        let mut records: Vec<RoleTickRecord> = (0..(n - 1))
            .map(|i| record("judge", "/r/loom", ((n - i) * 10) as i64, false))
            .collect();
        records.push(record_detail("judge", "/r/loom", 5, "a-different-transient-error"));
        let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
        assert_eq!(summary.persistent.len(), 1);
        assert_eq!(summary.persistent[0].consecutive_identical, 1);
        assert!(summary.escalated.is_empty());
    }

    #[test]
    fn escalated_failures_surface_distinctly_in_the_roles_section() {
        let n = ROLE_TICK_ESCALATION_THRESHOLD;
        let mut inputs = healthy_inputs();
        inputs.status.as_mut().unwrap().role_tick_records = (0..n)
            .map(|i| record("judge", "/r/loom", ((n - i) * 10) as i64, false))
            .collect();
        let section = assess_roles(&inputs);
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(
            section.summary.contains("ESCALATED"),
            "escalated pairs must be called out distinctly: {}",
            section.summary
        );
        assert!(section.summary.contains("judge @ loom"));
        // The escalated subset is machine-readable for --json / dashboard
        // consumers (#5022 will export it externally).
        assert_eq!(section.detail["escalated"].as_array().map(Vec::len), Some(1));
    }

    // -- roles.summary bounding + ANSI cleanup (#5024) ----------------------

    #[test]
    fn roles_summary_line_stays_bounded_for_an_oversized_ansi_laden_detail() {
        let mut inputs = healthy_inputs();
        let noisy = format!("\x1b[31m{}\x1b[0m", "x".repeat(20_000));
        inputs.status.as_mut().unwrap().role_tick_records =
            vec![record_detail("curator", "/r/loom", 60, &noisy)];

        let section = assess_roles(&inputs);

        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(
            section.summary.chars().count() <= MAX_ROLES_SUMMARY_LINE_CHARS + 100,
            "summary line was not bounded: {} chars",
            section.summary.chars().count()
        );
        assert!(
            !section.summary.contains('\u{1b}'),
            "summary line still contains a raw ANSI escape byte"
        );
    }

    #[test]
    fn roles_summary_line_does_not_multiply_with_many_simultaneous_failures() {
        // The 2026-08-03 12-repo outage: every repo fails identically with a
        // large detail. The summary line must stay bounded regardless of how
        // many `(root, role)` pairs are failing at once.
        let noisy = format!("\x1b[31m{}\x1b[0m", "boom ".repeat(2000));
        let mut inputs = healthy_inputs();
        inputs.status.as_mut().unwrap().role_tick_records = (0..12)
            .map(|i| record_detail("curator", &format!("/r/repo{i}"), 60, &noisy))
            .collect();

        let section = assess_roles(&inputs);

        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("12 PERSISTENT failure(s)"));
        assert!(
            section.summary.chars().count() <= MAX_ROLES_SUMMARY_LINE_CHARS + 100,
            "summary line was not bounded across 12 failures: {} chars",
            section.summary.chars().count()
        );
    }

    #[test]
    fn roles_structured_detail_still_carries_the_capped_cleaned_content() {
        let mut inputs = healthy_inputs();
        let noisy = format!("\x1b[31m{}\x1b[0m", "x".repeat(200));
        inputs.status.as_mut().unwrap().role_tick_records =
            vec![record_detail("curator", "/r/loom", 60, &noisy)];

        let section = assess_roles(&inputs);

        // The summary line is bounded/short...
        assert!(section.summary.chars().count() < noisy.chars().count());
        // ...but the structured `detail` field still carries the (ANSI-clean)
        // failure content — moved out of the summary line, not dropped.
        let structured_detail = section.detail["persistent"][0]["detail"]
            .as_str()
            .expect("persistent[0].detail should be a string");
        assert!(structured_detail.contains('x'));
        assert!(
            !structured_detail.contains('\u{1b}'),
            "structured detail should not carry raw ANSI escapes: {structured_detail:?}"
        );
    }

    #[test]
    fn roles_summary_detail_round_trips_short_clean_text_unchanged() {
        let mut inputs = healthy_inputs();
        inputs.status.as_mut().unwrap().role_tick_records = vec![record_detail(
            "curator",
            "/r/loom",
            60,
            "connection refused",
        )];

        let section = assess_roles(&inputs);

        assert!(section.summary.contains("connection refused"));
    }

    // ===================================================================
    // Queues + throughput
    // ===================================================================

    #[test]
    fn queues_sum_ready_counts_per_repo() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = Some(vec![
            RepoPipelineSnapshot {
                root: PathBuf::from("/r/loom"),
                queued: Some(4),
                merged_24h: Some(1),
                ..Default::default()
            },
            RepoPipelineSnapshot {
                root: PathBuf::from("/r/anvil"),
                queued: Some(2),
                merged_24h: Some(0),
                ..Default::default()
            },
        ]);
        let section = assess_queues(&inputs);
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("6 ready across 2 repo(s)"));
        assert!(section.summary.contains("loom 4"));
        assert!(section.summary.contains("anvil 2"));
    }

    #[test]
    fn a_failed_queue_query_is_unknown_not_green() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = Some(vec![RepoPipelineSnapshot {
            root: PathBuf::from("/r/loom"),
            queued: None,
            merged_24h: Some(1),
            error: Some("rate limited".to_string()),
            ..Default::default()
        }]);
        let section = assess_queues(&inputs);
        assert_eq!(section.verdict, Verdict::Unknown);
        assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
    }

    // ===================================================================
    // A missing/non-executable `gh` is one fact, not N per-repo failures
    // (#5061)
    // ===================================================================

    fn gh_unavailable_fixture() -> crate::pipeline_snapshot::GhUnavailable {
        crate::pipeline_snapshot::GhUnavailable {
            gh_bin: "gh".to_string(),
            reason: "`gh` not found on PATH — cannot assess queue depth / merge throughput. \
                     This looks like a non-login shell missing PATH entries a login shell would \
                     add — the same failure class as #4875."
                .to_string(),
            observed_path: Some("/usr/bin:/bin".to_string()),
        }
    }

    fn credential_preflight_ok() -> crate::types::CredentialPreflightReport {
        crate::types::CredentialPreflightReport {
            ok: true,
            mechanism: "keyring".to_string(),
            fingerprint: Some("rjwalters".to_string()),
            message: "authenticated as rjwalters via keyring".to_string(),
            checked_at: now(),
        }
    }

    /// The core #5061 regression pin: a missing `gh` collapses to a single
    /// section-level reason naming PATH, never a per-repo "forge query
    /// FAILED for: repoA, repoB, ..." list — even though `pipeline` itself is
    /// `None` (the collector never ran the fan-out).
    #[test]
    fn a_missing_gh_is_one_fact_not_a_per_repo_failure_list() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = None;
        inputs.gh_unavailable = Some(gh_unavailable_fixture());

        let queues = assess_queues(&inputs);
        assert_eq!(queues.verdict, Verdict::Unknown);
        assert!(queues.summary.contains("PATH"), "{}", queues.summary);
        assert!(queues.summary.contains("#4875"), "{}", queues.summary);
        assert!(
            !queues.summary.contains("forge query FAILED for:"),
            "must not regress to the per-repo phrasing: {}",
            queues.summary
        );

        let throughput = assess_throughput(&inputs);
        assert_eq!(throughput.verdict, Verdict::Unknown);
        assert!(
            !throughput.summary.contains("forge query FAILED for:"),
            "must not regress to the per-repo phrasing: {}",
            throughput.summary
        );

        // exit-code contract is unchanged: still "could not determine", exit 1.
        assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
    }

    /// AC: cross-reference the daemon's own IPC-answered credential verdict
    /// so the two signals never silently contradict each other.
    #[test]
    fn a_missing_gh_cross_references_a_healthy_daemon_credential() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = None;
        inputs.gh_unavailable = Some(gh_unavailable_fixture());
        inputs.status.as_mut().unwrap().credential_preflight = Some(credential_preflight_ok());

        let queues = assess_queues(&inputs);
        assert!(
            queues.summary.contains("credential OK"),
            "should cross-reference the daemon's own OK verdict: {}",
            queues.summary
        );
        assert!(queues.detail["daemon_credential_preflight"]["ok"] == true);
    }

    /// Without a daemon-reported credential verdict (unreachable IPC, or a
    /// pre-#4005 daemon), the section must still render — no cross-reference
    /// clause, not a panic or an empty summary.
    #[test]
    fn a_missing_gh_with_no_daemon_credential_signal_still_renders() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = None;
        inputs.gh_unavailable = Some(gh_unavailable_fixture());
        inputs.status = None;

        let queues = assess_queues(&inputs);
        assert_eq!(queues.verdict, Verdict::Unknown);
        assert!(!queues.summary.is_empty());
        assert!(queues.detail["daemon_credential_preflight"].is_null());
    }

    // ===================================================================
    // Queues: the review-side axes (#5021)
    // ===================================================================

    /// One repo's snapshot with every axis observed — the fixture the review
    /// tests vary a single field of.
    fn review_snapshot(
        name: &str,
        queued: usize,
        review_requested: usize,
        merged: usize,
    ) -> RepoPipelineSnapshot {
        RepoPipelineSnapshot {
            root: PathBuf::from(format!("/r/{name}")),
            queued: Some(queued),
            review_requested: Some(review_requested),
            changes_requested: Some(0),
            approved: Some(0),
            merged_24h: Some(merged),
            ..Default::default()
        }
    }

    /// AC1: the section reports the three review-side axes, not just `queued`.
    #[test]
    fn queues_report_the_review_side_axes_per_repo() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = Some(vec![RepoPipelineSnapshot {
            root: PathBuf::from("/r/loom"),
            queued: Some(1),
            review_requested: Some(2),
            changes_requested: Some(1),
            approved: Some(3),
            merged_24h: Some(5),
            ..Default::default()
        }]);
        let section = assess_queues(&inputs);

        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("2 awaiting review"), "{}", section.summary);
        assert!(section.summary.contains("1 changes-requested"), "{}", section.summary);
        assert!(section.summary.contains("3 approved"), "{}", section.summary);

        assert_eq!(section.detail["total_review_requested"], serde_json::json!(2));
        assert_eq!(section.detail["total_changes_requested"], serde_json::json!(1));
        assert_eq!(section.detail["total_approved"], serde_json::json!(3));
        let repo = &section.detail["repos"][0];
        assert_eq!(repo["review_requested"], serde_json::json!(2));
        assert_eq!(repo["changes_requested"], serde_json::json!(1));
        assert_eq!(repo["approved"], serde_json::json!(3));
        assert_eq!(repo["review_stalled"], serde_json::json!(false));
    }

    /// AC2: a review backlog against a window that merged nothing is non-green
    /// — the 2026-08-03 Judge-outage shape, reported without knowing the cause.
    #[test]
    fn a_review_backlog_with_no_merges_is_degraded() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = Some(vec![review_snapshot("loom", 1, REVIEW_STALL_MIN_BACKLOG, 0)]);
        let section = assess_queues(&inputs);

        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("REVIEW STALLED"), "{}", section.summary);
        assert!(section.summary.contains("loom"), "{}", section.summary);
        assert_eq!(section.detail["review_stalled"], serde_json::json!(["loom"]));
        assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
    }

    /// AC3 (the inverse): a flowing review pipeline stays green even with the
    /// same backlog depth — the merge axis is what distinguishes them.
    #[test]
    fn a_review_backlog_that_is_still_merging_stays_green() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = Some(vec![review_snapshot("loom", 1, REVIEW_STALL_MIN_BACKLOG + 5, 2)]);
        let section = assess_queues(&inputs);

        assert_eq!(section.verdict, Verdict::Green);
        assert!(!section.summary.contains("REVIEW STALLED"), "{}", section.summary);
    }

    /// A shallow backlog in a quiet window is *not* a stall — the same
    /// cry-wolf rule `assess_throughput` follows for a zero-merge window.
    #[test]
    fn a_shallow_review_backlog_in_a_quiet_window_stays_green() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = Some(vec![review_snapshot("loom", 1, REVIEW_STALL_MIN_BACKLOG - 1, 0)]);
        assert_eq!(assess_queues(&inputs).verdict, Verdict::Green);
    }

    /// Edge case: unobserved review axes must not be read as "zero backlog" —
    /// they render as `null`, never `0`, and cannot produce a stall verdict.
    #[test]
    fn unobserved_review_axes_are_null_not_zero() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = Some(vec![RepoPipelineSnapshot {
            root: PathBuf::from("/r/loom"),
            queued: Some(1),
            merged_24h: Some(0),
            ..Default::default()
        }]);
        let section = assess_queues(&inputs);

        assert_eq!(section.verdict, Verdict::Green);
        assert_eq!(section.detail["total_review_requested"], serde_json::Value::Null);
        assert_eq!(section.detail["total_changes_requested"], serde_json::Value::Null);
        assert_eq!(section.detail["total_approved"], serde_json::Value::Null);
        assert!(
            !section.summary.contains("awaiting review"),
            "an unobserved axis must not be summarized at all: {}",
            section.summary
        );
    }

    /// Edge case: a repo with **zero** ready issues but a stalled review queue
    /// must still be caught — the all-repos-summed `total_ready` says 0 across
    /// the fleet, which is exactly the "looks idle, is broken" masking #5004
    /// reported.
    #[test]
    fn an_empty_ready_queue_does_not_mask_a_review_stall() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = Some(vec![
            review_snapshot("anvil", 0, 0, 0),
            review_snapshot("loom", 0, REVIEW_STALL_MIN_BACKLOG, 0),
        ]);
        let section = assess_queues(&inputs);

        assert_eq!(section.detail["total_ready"], serde_json::json!(0));
        assert_eq!(section.verdict, Verdict::Degraded);
        assert_eq!(section.detail["review_stalled"], serde_json::json!(["loom"]));
    }

    /// A stall is a *known* finding and outranks the `Unknown` a single flaky
    /// forge query produces — but the failed repo is still named.
    #[test]
    fn a_review_stall_outranks_a_failed_query_but_still_names_it() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = Some(vec![
            RepoPipelineSnapshot {
                root: PathBuf::from("/r/anvil"),
                queued: None,
                error: Some("rate limited".to_string()),
                ..Default::default()
            },
            review_snapshot("loom", 2, REVIEW_STALL_MIN_BACKLOG, 0),
        ]);
        let section = assess_queues(&inputs);

        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("REVIEW STALLED"), "{}", section.summary);
        assert!(section.summary.contains("forge query FAILED for: anvil"), "{}", section.summary);
    }

    /// The whole-fleet shape of the 2026-08-03 outage: every repo idle on the
    /// `queued` axis, nothing merging, PRs piling up awaiting review. The
    /// pre-#5021 assessment reported this green.
    #[test]
    fn a_fleet_wide_judge_outage_is_not_green() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = Some(
            (0..12)
                .map(|i| review_snapshot(&format!("repo{i}"), 0, 4, 0))
                .collect(),
        );
        let section = assess_queues(&inputs);

        assert_ne!(section.verdict, Verdict::Green);
        assert_eq!(section.detail["total_review_requested"], serde_json::json!(48));
        assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
    }

    #[test]
    fn zero_merges_in_window_is_green() {
        let mut inputs = healthy_inputs();
        inputs.pipeline.as_mut().unwrap()[0].merged_24h = Some(0);
        let section = assess_throughput(&inputs);
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("0 merged in 30m"));
    }

    #[test]
    fn a_failed_throughput_query_is_unknown() {
        let mut inputs = healthy_inputs();
        inputs.pipeline.as_mut().unwrap()[0].merged_24h = None;
        let section = assess_throughput(&inputs);
        assert_eq!(section.verdict, Verdict::Unknown);
    }

    #[test]
    fn an_empty_fleet_is_green_not_unknown() {
        let mut inputs = healthy_inputs();
        inputs.pipeline = Some(vec![]);
        assert_eq!(assess_queues(&inputs).verdict, Verdict::Green);
        assert_eq!(assess_throughput(&inputs).verdict, Verdict::Green);
        assert_eq!(assess(&inputs).exit_code(), EXIT_HEALTHY);
    }

    // ===================================================================
    // --since parsing / formatting
    // ===================================================================

    #[test]
    fn parse_since_accepts_suffixed_and_bare_values() {
        assert_eq!(parse_since("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_since("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_since("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_since("1d").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_since(" 45 ").unwrap(), Duration::from_secs(45));
    }

    #[test]
    fn parse_since_rejects_junk_and_zero() {
        assert!(parse_since("").is_err());
        assert!(parse_since("later").is_err());
        assert!(parse_since("0m").is_err());
        assert!(parse_since("-5m").is_err());
    }

    #[test]
    fn format_window_round_trips_common_values() {
        assert_eq!(format_window(1800), "30m");
        assert_eq!(format_window(7200), "2h");
        assert_eq!(format_window(45), "45s");
    }

    #[test]
    fn format_age_is_compact_and_never_negative() {
        assert_eq!(format_age(-10), "0s");
        assert_eq!(format_age(43), "43s");
        assert_eq!(format_age(600), "10m");
        assert_eq!(format_age(7200), "2h");
        assert_eq!(format_age(400_000), "4d");
    }
}
