//! The daemon-side entry points: startup + periodic `doctor`, dispatch and
//! spawn admission, and the cached last-doctor file `status` reads.
//!
//! | Entry point | Call | On failure (`enforcement.api = required`) |
//! |---|---|---|
//! | daemon startup, then every [`resolve_interval`] | `doctor` | log each finding + remedy; event `forge.egress.drift` on change |
//! | sweep dispatch ([`dispatch_refusal`]) | fresh `assert` | refuse; event `sweep.blocked` `reason=forge-egress` |
//! | worker spawn ([`spawn_refusal`]) | fresh `assert` | do not spawn (exit 78) |
//!
//! `observe` logs the same findings and admits. Unconfigured admits silently
//! with no subprocess at all. An unreadable or unknown-version policy is
//! never observe-only: it fails closed.
//!
//! The daemon stays up either way — local-only commands, `status` and the
//! IPC surface keep working; only forge work is refused.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::policy::PolicySources;
use super::report::Finding;
use super::{Mode, Report, Section};
use crate::event_bus::EventBus;

/// Event topic for a dispatch refused by the gate.
pub const SWEEP_BLOCKED_TOPIC: &str = "sweep.blocked";
/// Event topic for a change in the periodic doctor's routing verdict.
pub const DRIFT_TOPIC: &str = "forge.egress.drift";
/// `source` field on both events (the bus's `Generic`-topic rule).
pub const EVENT_SOURCE: &str = "forge-egress";
/// Env override for the periodic doctor cadence.
pub const INTERVAL_ENV: &str = "LOOM_FORGE_EGRESS_DOCTOR_INTERVAL_SECS";
/// Default cadence — the worktree reaper's.
pub const DEFAULT_INTERVAL_SECS: u64 = 900;
/// Host-level cache of the daemon's last doctor report, under `<loom_dir>`.
pub const CACHE_FILENAME: &str = "forge-egress-doctor.json";
/// The repair command named in every refusal.
pub const REPAIR_COMMAND: &str = "loom-daemon forge egress doctor";

/// Why forge work was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub codes: Vec<String>,
    pub exit_code: i32,
    pub policy_origin: String,
    /// The first routing finding's remedy.
    pub remedy: String,
}

impl Refusal {
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "forge-egress: routing admission refused (enforcement.api=required, exit {}): [{}] — \
             fix: {} — run `{REPAIR_COMMAND}` for the full report",
            self.exit_code,
            self.codes.join(", "),
            self.remedy
        )
    }

    /// The `sweep.blocked` payload for `kind`.
    #[must_use]
    pub fn event_payload(&self, kind: Value) -> Value {
        json!({
            "source": EVENT_SOURCE,
            "reason": "forge-egress",
            "kind": kind,
            "codes": self.codes,
            "exit_code": self.exit_code,
            "policy_origin": self.policy_origin,
        })
    }
}

/// Admission verdict for one report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Unconfigured,
    Aligned,
    /// Findings, but `enforcement.api = observe`: logged, admitted.
    Observed(Vec<String>),
    Refused(Refusal),
}

fn policy_origin(report: &Report) -> String {
    match &report.policy {
        super::PolicyState::Loaded { origin, .. }
        | super::PolicyState::Unreadable { origin, .. } => (*origin).to_string(),
        super::PolicyState::Unconfigured => "unconfigured".to_string(),
    }
}

/// Map a report to an admission verdict (pure).
#[must_use]
pub fn admission_for(report: &Report) -> Admission {
    if !report.is_configured() {
        return Admission::Unconfigured;
    }
    let code = report.exit_code();
    if code == 0 {
        return Admission::Aligned;
    }
    let codes = report.routing_codes();
    if report.observe_only() {
        return Admission::Observed(codes);
    }
    Admission::Refused(Refusal {
        codes,
        exit_code: code,
        policy_origin: policy_origin(report),
        remedy: report
            .routing
            .iter()
            .find(|f| !f.remedy.is_empty())
            .map(|f| super::report::redact(&f.remedy))
            .unwrap_or_default(),
    })
}

/// Log each routing finding once per distinct code set (so a busy dispatch
/// path does not repeat the same warning every call).
fn log_findings_once(context: &str, report: &Report) {
    static LAST: Mutex<Option<(String, Vec<String>)>> = Mutex::new(None);
    let codes = report.routing_codes();
    if let Ok(mut last) = LAST.lock() {
        let key = (context.to_string(), codes.clone());
        if last.as_ref() == Some(&key) {
            return;
        }
        *last = Some(key);
    }
    log_findings(context, report);
}

fn log_findings(context: &str, report: &Report) {
    let mode = if report.observe_only() {
        "observe"
    } else {
        "required"
    };
    for f in &report.routing {
        log::warn!(
            "forge_egress ({context}, enforcement.api={mode}): {} {} — {}; fix: {} (repair: \
             `{REPAIR_COMMAND}`)",
            f.severity.as_str().to_uppercase(),
            f.code,
            f.invariant,
            super::report::redact(&f.remedy)
        );
    }
}

fn admit(context: &str, report: &Report) -> Option<Refusal> {
    match admission_for(report) {
        Admission::Unconfigured | Admission::Aligned => None,
        Admission::Observed(_) => {
            log_findings_once(context, report);
            None
        }
        Admission::Refused(r) => {
            log_findings_once(context, report);
            Some(r)
        }
    }
}

/// Sweep dispatch admission: a fresh `assert` for `workspace`. `Some` ⇒ do
/// not dispatch, and publish [`SWEEP_BLOCKED_TOPIC`] with
/// [`Refusal::event_payload`].
#[must_use]
pub fn dispatch_refusal(workspace: &Path) -> Option<Refusal> {
    dispatch_refusal_with(&PolicySources::from_process(Some(workspace)), workspace)
}

/// [`dispatch_refusal`] against explicit `sources` (hermetic tests; #9999).
#[must_use]
pub fn dispatch_refusal_with(sources: &PolicySources, workspace: &Path) -> Option<Refusal> {
    admit("dispatch", &super::run_with(sources, workspace, Mode::Assert))
}

/// Worker spawn admission (`loom-daemon spawn-worker`, i.e.
/// `spawn-worker.sh`): `Some(message)` ⇒ do not exec the runtime.
#[must_use]
pub fn spawn_refusal(workspace: &Path) -> Option<String> {
    spawn_refusal_with(&PolicySources::from_process(Some(workspace)), workspace)
}

/// [`spawn_refusal`] against explicit `sources` (hermetic tests; #9999).
#[must_use]
pub fn spawn_refusal_with(sources: &PolicySources, workspace: &Path) -> Option<String> {
    admit("spawn-worker", &super::run_with(sources, workspace, Mode::Assert)).map(|r| r.message())
}

/// `<loom_dir>`: the parent of `LOOM_SOCKET_PATH` when set, else `~/.loom`
/// (the same rule as `fleet_store::pending_restart`).
fn loom_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        return PathBuf::from(path).parent().map(Path::to_path_buf);
    }
    dirs::home_dir().map(|h| h.join(".loom"))
}

/// Path of the cached last-doctor file.
#[must_use]
pub fn cache_path() -> Option<PathBuf> {
    loom_dir().map(|d| d.join(CACHE_FILENAME))
}

/// Write `report` as the daemon's last doctor (atomic rename).
pub fn write_cache(path: &Path, workspace: &Path, report: &Report) -> std::io::Result<()> {
    let body = json!({
        "written_at": chrono::Utc::now().to_rfc3339(),
        "daemon_pid": std::process::id(),
        "workspace": workspace.display().to_string(),
        "report": report.to_json(),
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&body)?)?;
    std::fs::rename(&tmp, path)
}

/// Read the cached last doctor, if any.
#[must_use]
pub fn read_cache(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Remove a stale cache (the policy went away).
fn clear_cache(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Resolve the periodic cadence — env > default.
#[must_use]
pub fn resolve_interval() -> Duration {
    std::env::var(INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map_or(Duration::from_secs(DEFAULT_INTERVAL_SECS), Duration::from_secs)
}

/// The `forge.egress.drift` payload, or `None` when nothing changed.
#[must_use]
pub fn drift_payload(previous: Option<&BTreeSet<String>>, report: &Report) -> Option<Value> {
    let current: BTreeSet<String> = report.routing_codes().into_iter().collect();
    let first_clean = previous.is_none() && current.is_empty();
    if first_clean || previous == Some(&current) {
        return None;
    }
    Some(json!({
        "source": EVENT_SOURCE,
        "codes": current,
        "previous_codes": previous,
        "exit_code": report.exit_code(),
        "policy_origin": policy_origin(report),
        "enforcement": if report.observe_only() { "observe" } else { "required" },
        "sections": Section::ALL.iter().map(|s| (s.as_str(), super::report::exit_code(report.section(*s)))).collect::<std::collections::BTreeMap<_, _>>(),
    }))
}

/// One doctor pass: run, cache, log on change, publish drift on change.
/// Returns the new code set (or `None` when unconfigured).
fn pass(
    workspace: &Path,
    previous: Option<&BTreeSet<String>>,
    bus: Option<&EventBus>,
) -> Option<BTreeSet<String>> {
    let report = super::doctor_for(workspace);
    let cache = cache_path();
    if !report.is_configured() {
        if let Some(c) = &cache {
            clear_cache(c);
        }
        return None;
    }
    if let Some(c) = &cache {
        if let Err(e) = write_cache(c, workspace, &report) {
            log::warn!("forge_egress: could not write {} ({e})", c.display());
        }
    }
    if let Some(payload) = drift_payload(previous, &report) {
        log_findings("doctor", &report);
        if report.routing.is_empty() {
            log::info!("forge_egress: routing aligned (doctor)");
        } else if !report.observe_only() {
            log::warn!(
                "forge_egress: enforcement.api=required — forge work (sweep dispatch, worker \
                 spawn) is refused until routing is aligned; local-only commands keep working"
            );
        }
        if let Some(bus) = bus {
            let _ = bus.publish_generic(DRIFT_TOPIC, payload);
        }
    }
    Some(report.routing_codes().into_iter().collect())
}

/// Daemon startup: spawn the doctor loop (first pass immediately, then every
/// [`resolve_interval`]). Each pass runs on a blocking thread — the canary
/// may take seconds — so it never parks a runtime worker or delays startup.
pub fn start(workspace: &Path, bus: Option<Arc<EventBus>>) -> tokio::task::JoinHandle<()> {
    let workspace = workspace.to_path_buf();
    let interval = resolve_interval();
    tokio::spawn(async move {
        let mut previous: Option<BTreeSet<String>> = None;
        loop {
            let ws = workspace.clone();
            let prev = previous.clone();
            let bus_for_pass = bus.clone();
            match tokio::task::spawn_blocking(move || {
                pass(&ws, prev.as_ref(), bus_for_pass.as_deref())
            })
            .await
            {
                Ok(next) => previous = next,
                Err(e) => log::error!("forge_egress: doctor pass panicked ({e}); continuing"),
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// The first remedy-bearing finding, for one-line renderers.
#[must_use]
pub fn first_remedy(findings: &[Finding]) -> Option<&Finding> {
    findings.iter().find(|f| !f.remedy.is_empty())
}
