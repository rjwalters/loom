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
use crate::types::{DaemonStatusReport, RoleTickRecord};

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
    /// `queues`, `throughput`).
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
    pub pipeline: Option<Vec<RepoPipelineSnapshot>>,
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
            .and_then(|r| r.detail.clone());
        let failure = RoleFailure {
            root,
            role,
            failures,
            last_at: latest.at,
            detail,
        };
        if latest.ok {
            summary.transient.push(failure);
        } else {
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
                        "process alive, still STARTING (age {}s) — IPC not bound yet:                          {ipc_error}{}",
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
                        "process ALIVE but not answering IPC — NOT dead ({detail}); ipc:                          {ipc_error}{}",
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
            // Only a fault when the loop is supposed to be running.
            if status.work_finder_enabled != Some(false) {
                degraded.push("no work-finder tick observed in this daemon process".to_string());
            }
            "no work-finder tick observed".to_string()
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
                let detail = f.detail.as_deref().unwrap_or("failed");
                format!("{} ({} ticks, {detail})", f.label(), f.failures)
            })
            .collect();
        (
            Verdict::Degraded,
            format!(
                "{}/{} ticks ok; {} PERSISTENT failure(s): {}{transient_note}",
                summary.ok,
                summary.total,
                summary.persistent.len(),
                names.join(", ")
            ),
        )
    };
    HealthSection::new(
        "roles",
        verdict,
        line,
        serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null),
    )
}

/// Assess the queue-depth section: per-root ready (`loom:issue`) counts.
#[must_use]
pub fn assess_queues(inputs: &HealthInputs) -> HealthSection {
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
    for snap in pipeline {
        let name = repo_label(&snap.root);
        match snap.queued {
            Some(n) => {
                total += n;
                parts.push(format!("{name} {n}"));
            }
            None => {
                parts.push(format!("{name} ?"));
                failed.push(name);
            }
        }
    }
    let (verdict, summary) = if failed.is_empty() {
        (
            Verdict::Green,
            format!("{total} ready across {} repo(s) ({})", pipeline.len(), parts.join(", ")),
        )
    } else {
        (
            Verdict::Unknown,
            format!(
                "{total}+ ready across {} repo(s) ({}); forge query FAILED for: {}",
                pipeline.len(),
                parts.join(", "),
                failed.join(", ")
            ),
        )
    };
    HealthSection::new(
        "queues",
        verdict,
        summary,
        serde_json::json!({
            "total_ready": total,
            "repos": pipeline
                .iter()
                .map(|s| serde_json::json!({
                    "root": s.root,
                    "ready": s.queued,
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
// Roll-up
// ============================================================================

/// Assemble the full report from already-collected inputs (pure).
///
/// A [`Verdict::Dead`] liveness verdict short-circuits: the remaining sections
/// are reported as [`Verdict::Unknown`] (they are all downstream of an IPC
/// round-trip that could not happen), and the overall verdict is `Dead` so the
/// exit code is `2` rather than a misleading `1`.
#[must_use]
pub fn assess(inputs: &HealthInputs) -> HealthReport {
    let liveness = assess_liveness(inputs);
    let dead = liveness.verdict == Verdict::Dead;
    let sections = vec![
        liveness,
        assess_dispatch(inputs),
        assess_tokens(inputs),
        assess_roles(inputs),
        assess_queues(inputs),
        assess_throughput(inputs),
    ];
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

/// The short repo label rendered in the queue/throughput lines — the root's
/// final path component, which is the repo name for every managed workspace.
fn repo_label(root: &std::path::Path) -> String {
    root.file_name()
        .map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned())
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
        }
    }

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

    /// A reachable daemon whose pid file records a pid that is not a live
    /// process at all — a relaunch after the old pid was recycled away.
    #[test]
    fn a_pid_file_naming_a_dead_process_degrades() {
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

    #[test]
    fn dispatch_missing_tick_is_degraded_when_work_finder_is_enabled() {
        let mut inputs = healthy_inputs();
        inputs.status.as_mut().unwrap().last_work_finder_tick = None;
        let section = assess_dispatch(&inputs);
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
