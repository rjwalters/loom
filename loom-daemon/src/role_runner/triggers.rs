//! Event-driven launch triggers for curator, auditor and guide (issue #10816,
//! slice 3 of #10630).
//!
//! Before this slice those three roles launched an agent on every interval
//! tick whether or not anything had changed: a quiet repository rebuilt and
//! re-audited the same `main` commit about 144 times a day. With
//! `autonomous.roleRunner.eventTriggers.enabled` the interval still fires at
//! its usual cadence (`intervalSecs` becomes the *floor* on how often the
//! trigger is re-checked), but the trigger decides whether an agent runs:
//!
//! - **auditor**: launches when `origin/main` (the already-fetched local
//!   remote-tracking ref, no forge call) differs from the last SHA a run of
//!   it completed successfully on, for this root.
//! - **curator**: launches when the repository has at least one open
//!   `loom:triage` issue (the raw intake queue; one ETag-cached listing page).
//! - **guide**: launches when the set of open ready (`loom:issue`) and backlog
//!   (`loom:curated`) issue numbers differs from the set its last successful
//!   run saw.
//!
//! Two rules keep the gate from starving a role:
//!
//! - **Fail open.** An input that could not be observed (a git error, a
//!   listing failure) launches with trigger [`Trigger::Floor`]; the gate never
//!   skips on missing information.
//! - **Quiet ceiling.** A role that has not launched for a root in
//!   `eventTriggers.maxQuietSecs` (default one day) launches with trigger
//!   [`Trigger::Floor`] whatever its input says, so work outside the trigger
//!   (curator's approved-but-uncurated and blocked re-check passes, a primary
//!   clone that is never fetched) still gets a periodic pass.
//!
//! A skipped tick ends [`RoleTickOutcome::QueueEmpty`]: no agent is spent, it
//! is not a failure, it never reaches the failure sentinel, and the dispatcher
//! may give its slot to a deferred root, exactly like the judge/doctor queue
//! gate (#9391). The last-seen state lives in a small per-`(root, role)`
//! [`TriggerLedger`], kept apart from the demand ledger's `DebtAxis` set.
//! With the flag off (the default) nothing here runs and dispatch is unchanged.

use super::{concurrent_dispatch, RoleTickOutcome};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Host env override for `autonomous.roleRunner.eventTriggers.enabled`
/// (env > config > default).
pub const EVENT_TRIGGERS_ENV: &str = "LOOM_ROLE_EVENT_TRIGGERS";

/// Default `eventTriggers.maxQuietSecs`: one day.
pub const DEFAULT_MAX_QUIET_SECS: u64 = 86_400;

/// `forge_call_stats` caller name for the trigger listings.
pub const TRIGGER_CALLER: &str = "role_event_trigger";

/// The curator's debt axis name, as it appears in a trigger label.
pub const UNTRIAGED_AXIS: &str = "untriaged";

/// Labels that mean Curator has queued work: new issues and Champion's
/// revision requests (#10753), which never carry `loom:triage`.
pub const CURATOR_WORK_LABELS: [&str; 2] = ["loom:triage", "loom:needs-revision"];

/// The resolved `autonomous.roleRunner.eventTriggers` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventTriggerConfig {
    /// Whether the gate runs at all.
    pub enabled: bool,
    /// The quiet ceiling, in seconds.
    pub max_quiet_secs: u64,
}

impl Default for EventTriggerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_quiet_secs: DEFAULT_MAX_QUIET_SECS,
        }
    }
}

impl EventTriggerConfig {
    /// The quiet ceiling as a [`Duration`].
    #[must_use]
    pub fn max_quiet(self) -> Duration {
        Duration::from_secs(self.max_quiet_secs)
    }
}

/// Parse `eventTriggers` out of an `autonomous.roleRunner` block, then apply
/// the `env` override to `enabled`. A non-bool `enabled` or a zero, negative
/// or non-integer `maxQuietSecs` drops only that key to its default.
#[must_use]
pub fn parse_event_trigger_config(
    role_runner_block: &serde_json::Value,
    env: Option<bool>,
) -> EventTriggerConfig {
    let d = EventTriggerConfig::default();
    let obj = role_runner_block
        .get("eventTriggers")
        .and_then(serde_json::Value::as_object);
    let enabled = obj
        .and_then(|o| o.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(d.enabled);
    let max_quiet_secs = obj
        .and_then(|o| o.get("maxQuietSecs"))
        .and_then(serde_json::Value::as_u64)
        .filter(|&n| n > 0)
        .unwrap_or(d.max_quiet_secs);
    EventTriggerConfig {
        enabled: env.unwrap_or(enabled),
        max_quiet_secs,
    }
}

/// Parse an [`EVENT_TRIGGERS_ENV`] value: truthy or falsy words override the
/// config; anything else (unset, blank, unrecognised) leaves it alone.
#[must_use]
pub fn parse_env_override(value: Option<&str>) -> Option<bool> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `root`'s own resolved trigger config, re-read every tick (live).
#[must_use]
pub fn read_event_trigger_config(root: &Path) -> EventTriggerConfig {
    let env = std::env::var(EVENT_TRIGGERS_ENV).ok();
    parse_event_trigger_config(
        &concurrent_dispatch::role_runner_block(root),
        parse_env_override(env.as_deref()),
    )
}

/// Whether `role` has an event trigger. Every other role is dispatched
/// exactly as before.
#[must_use]
pub fn is_triggered_role(role: &str) -> bool {
    matches!(role, "curator" | "auditor" | "guide")
}

/// Why a role launched (or which trigger a skip found absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Something the role watches changed (auditor: `main`; guide: the
    /// ready/backlog set).
    Event,
    /// The role has queued work on a debt axis (curator: `untriaged`).
    Debt(&'static str),
    /// An idle-edge run (#4364), which the gate never holds back.
    Idle,
    /// The re-check floor: an unobserved input (fail open) or the quiet
    /// ceiling.
    Floor,
}

impl std::fmt::Display for Trigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Event => f.write_str("event"),
            Self::Debt(axis) => write!(f, "debt:{axis}"),
            Self::Idle => f.write_str("idle"),
            Self::Floor => f.write_str("floor"),
        }
    }
}

/// What a probe saw for one `(root, role)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// A state fingerprint (auditor: the `origin/main` SHA; guide: the
    /// ready/backlog set).
    Fingerprint(String),
    /// A queue count (curator: open `loom:triage` issues).
    Count(usize),
    /// The input could not be read; the payload says why.
    Unobserved(String),
}

/// Everything [`should_launch`] decides from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerInputs {
    /// This tick's observation.
    pub observation: Observation,
    /// The fingerprint the last successful run recorded, if any.
    pub last_fingerprint: Option<String>,
    /// Time since this role last launched successfully for this root (or
    /// since the gate first saw it, before any launch).
    pub quiet_for: Duration,
    /// The quiet ceiling.
    pub max_quiet: Duration,
}

/// The gate's verdict for one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerDecision {
    /// Launch the agent.
    Launch {
        /// Why.
        trigger: Trigger,
        /// A short human-readable reason.
        reason: String,
    },
    /// Do not launch this tick.
    Skip {
        /// The trigger that was checked and found absent.
        trigger: Trigger,
        /// A short human-readable reason.
        reason: String,
    },
}

impl TriggerDecision {
    /// Whether the agent launches.
    #[must_use]
    pub fn launches(&self) -> bool {
        matches!(self, Self::Launch { .. })
    }

    fn launch(trigger: Trigger, reason: impl Into<String>) -> Self {
        Self::Launch {
            trigger,
            reason: reason.into(),
        }
    }

    fn skip(trigger: Trigger, reason: impl Into<String>) -> Self {
        Self::Skip {
            trigger,
            reason: reason.into(),
        }
    }
}

/// The first 12 characters of a fingerprint, for log lines.
fn short(fingerprint: &str) -> &str {
    fingerprint.get(..12).unwrap_or(fingerprint)
}

/// Decide whether `role` launches, from `inputs` alone (pure).
#[must_use]
pub fn should_launch(role: &str, inputs: &TriggerInputs) -> TriggerDecision {
    if let Observation::Unobserved(why) = &inputs.observation {
        return TriggerDecision::launch(
            Trigger::Floor,
            format!("input unobserved ({why}); failing open"),
        );
    }
    if inputs.quiet_for >= inputs.max_quiet {
        return TriggerDecision::launch(
            Trigger::Floor,
            format!(
                "no launch for {}s, at or past the {}s quiet ceiling",
                inputs.quiet_for.as_secs(),
                inputs.max_quiet.as_secs()
            ),
        );
    }
    let last = inputs.last_fingerprint.as_deref();
    match (role, &inputs.observation) {
        ("auditor", Observation::Fingerprint(sha)) => match last {
            Some(prev) if prev == sha => TriggerDecision::skip(
                Trigger::Event,
                format!("origin/main unchanged at {} since the last audit", short(sha)),
            ),
            Some(prev) => TriggerDecision::launch(
                Trigger::Event,
                format!("origin/main moved {} -> {}", short(prev), short(sha)),
            ),
            None => TriggerDecision::launch(
                Trigger::Event,
                format!("no audited main SHA recorded; origin/main at {}", short(sha)),
            ),
        },
        ("guide", Observation::Fingerprint(set)) => match last {
            Some(prev) if prev == set => TriggerDecision::skip(
                Trigger::Event,
                format!("ready/backlog set unchanged ({set})"),
            ),
            Some(prev) => TriggerDecision::launch(
                Trigger::Event,
                format!("ready/backlog set changed ({prev} -> {set})"),
            ),
            None => TriggerDecision::launch(
                Trigger::Event,
                format!("no ready/backlog set recorded; now {set}"),
            ),
        },
        ("curator", Observation::Count(n)) => {
            let axis = Trigger::Debt(UNTRIAGED_AXIS);
            if *n > 0 {
                TriggerDecision::launch(axis, format!("{n} untriaged issue(s) open"))
            } else {
                TriggerDecision::skip(axis, "no untriaged issues open")
            }
        }
        (role, observation) => TriggerDecision::launch(
            Trigger::Floor,
            format!("no trigger rule for {role} with {observation:?}; failing open"),
        ),
    }
}

// -- ledger -------------------------------------------------------------------

#[derive(Debug, Clone)]
struct LedgerEntry {
    fingerprint: Option<String>,
    quiet_since: Instant,
}

/// Per-`(root, role)` trigger state: the last fingerprint a successful run
/// recorded and when the role last launched successfully. In memory only: a
/// daemon restart forgets it, so the first tick after a restart launches.
#[derive(Debug, Default)]
pub struct TriggerLedger {
    entries: Mutex<HashMap<(PathBuf, &'static str), LedgerEntry>>,
}

impl TriggerLedger {
    fn with_entry<T>(
        &self,
        root: &Path,
        role: &'static str,
        now: Instant,
        f: impl FnOnce(&mut LedgerEntry) -> T,
    ) -> T {
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = map
            .entry((root.to_path_buf(), role))
            .or_insert_with(|| LedgerEntry {
                fingerprint: None,
                quiet_since: now,
            });
        f(entry)
    }

    /// The last recorded fingerprint and how long the role has been quiet.
    /// The first call for a `(root, role)` starts its quiet clock at `now`.
    #[must_use]
    pub fn read(
        &self,
        root: &Path,
        role: &'static str,
        now: Instant,
    ) -> (Option<String>, Duration) {
        self.with_entry(root, role, now, |e| {
            (e.fingerprint.clone(), now.saturating_duration_since(e.quiet_since))
        })
    }

    /// Record a successful launch: reset the quiet clock and, when given,
    /// advance the fingerprint.
    pub fn record_success(
        &self,
        root: &Path,
        role: &'static str,
        fingerprint: Option<String>,
        now: Instant,
    ) {
        self.with_entry(root, role, now, |e| {
            e.quiet_since = now;
            if fingerprint.is_some() {
                e.fingerprint = fingerprint;
            }
        });
    }
}

/// The process-wide trigger ledger.
#[must_use]
pub fn global() -> &'static TriggerLedger {
    static LEDGER: OnceLock<TriggerLedger> = OnceLock::new();
    LEDGER.get_or_init(TriggerLedger::default)
}

// -- probes -------------------------------------------------------------------

/// Observes one `(root, role)` input. Blocking: called on the run's own
/// blocking thread, never in the dispatcher's synchronous walk.
pub type TriggerProbe = Arc<dyn Fn(&Path, &'static str) -> Observation + Send + Sync>;

/// A probe that observes nothing, so the gate always fails open (the test
/// dispatcher default: it never touches git or the forge).
#[must_use]
pub fn no_trigger_probe() -> TriggerProbe {
    Arc::new(|_, _| Observation::Unobserved("no trigger probe configured".to_string()))
}

/// The production probe: `git rev-parse` for auditor, ETag-cached REST
/// listings for curator and guide.
#[must_use]
pub fn forge_trigger_probe() -> TriggerProbe {
    Arc::new(|root, role| match role {
        "auditor" => observe_origin_main(root),
        "curator" => observe_untriaged(root),
        "guide" => observe_ready_backlog(root),
        other => Observation::Unobserved(format!("{other} has no trigger probe")),
    })
}

/// The SHA of the already-fetched `origin/main` ref in `root` (no fetch, no
/// forge call).
#[must_use]
pub fn observe_origin_main(root: &Path) -> Observation {
    let output = std::process::Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/main^{commit}",
        ])
        .current_dir(root)
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let sha = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if sha.is_empty() {
                Observation::Unobserved("git rev-parse origin/main printed nothing".to_string())
            } else {
                Observation::Fingerprint(sha)
            }
        }
        Ok(o) => Observation::Unobserved(format!(
            "git rev-parse origin/main exited {}",
            o.status
                .code()
                .map_or_else(|| "by signal".to_string(), |c| c.to_string())
        )),
        Err(e) => Observation::Unobserved(format!("could not run git rev-parse: {e}")),
    }
}

/// Open `loom:triage` issues (pull requests excluded) on the first listing
/// page: a count of at least one is all the curator gate needs, so one
/// ETag-cached page per tick, usually a free `304`.
fn observe_untriaged(root: &Path) -> Observation {
    let gh = concurrent_dispatch::gate_gh_bin();
    let mut total = 0;
    // Champion's NEEDS REVISION requests (`loom:needs-revision`, #10753) are
    // Curator work that never carries `loom:triage`, so they count too.
    for label in CURATOR_WORK_LABELS {
        match crate::forge_listing::list_issues_cached_as(
            TRIGGER_CALLER,
            &gh,
            Some(root),
            None,
            label,
            "open",
        ) {
            Ok(rows) => total += count_issue_rows(rows.iter().map(|r| r.is_pull_request)),
            Err(e) => return Observation::Unobserved(format!("{label} listing failed: {e}")),
        }
    }
    Observation::Count(total)
}

/// Issues (not pull requests) among the listed rows.
fn count_issue_rows(is_pull_request: impl Iterator<Item = bool>) -> usize {
    is_pull_request.filter(|pr| !pr).count()
}

/// The open ready (`loom:issue`) and backlog (`loom:curated`) issue-number
/// set, fingerprinted. Every page is read (all-or-nothing): a partial set
/// would read as a change.
fn observe_ready_backlog(root: &Path) -> Observation {
    let gh = concurrent_dispatch::gate_gh_bin();
    let mut numbers = Vec::new();
    for label in ["loom:issue", "loom:curated"] {
        match crate::forge_listing::list_issues_cached_all_as(
            TRIGGER_CALLER,
            &gh,
            Some(root),
            None,
            label,
            "open",
        ) {
            Ok(rows) => {
                numbers.extend(rows.iter().filter(|r| !r.is_pull_request).map(|r| r.number));
            }
            Err(e) => return Observation::Unobserved(format!("{label} listing failed: {e}")),
        }
    }
    Observation::Fingerprint(set_fingerprint(numbers))
}

/// A stable fingerprint of an issue-number set (order and duplicates do not
/// matter): its size plus a hash of the sorted numbers.
#[must_use]
pub fn set_fingerprint(mut numbers: Vec<u32>) -> String {
    numbers.sort_unstable();
    numbers.dedup();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    numbers.hash(&mut hasher);
    format!("{} issue(s), set {:016x}", numbers.len(), hasher.finish())
}

// -- gate ---------------------------------------------------------------------

/// The one log line per gated tick: role, root, trigger and reason.
#[must_use]
pub fn decision_log_line(role: &str, root: &Path, decision: &TriggerDecision) -> String {
    let (verb, trigger, reason) = match decision {
        TriggerDecision::Launch { trigger, reason } => ("launch", trigger, reason),
        TriggerDecision::Skip { trigger, reason } => ("skip", trigger, reason),
    };
    format!(
        "role_runner: event trigger {verb} — role={role} root={} trigger={trigger} reason={reason} \
         (autonomous.roleRunner.eventTriggers, #10816)",
        root.display()
    )
}

/// Run `run` for `(root, role)` behind the event trigger.
///
/// A role without a trigger, or a root with `eventTriggers.enabled` false,
/// runs `run` directly with no extra read (today's behaviour). Otherwise the
/// probe observes the input, [`should_launch`] decides, and the decision is
/// logged: a skip returns [`RoleTickOutcome::QueueEmpty`] without calling
/// `run`; a launch calls it and, only if it ends
/// [`RoleTickOutcome::Success`], records the observed fingerprint and resets
/// the quiet clock (a failed run advances nothing, so the next tick retries).
pub fn run_with_trigger_gate(
    config: EventTriggerConfig,
    probe: &TriggerProbe,
    ledger: &TriggerLedger,
    root: &Path,
    role: &'static str,
    run: impl FnOnce() -> RoleTickOutcome,
) -> RoleTickOutcome {
    if !config.enabled || !is_triggered_role(role) {
        return run();
    }
    let observation = probe(root, role);
    let (last_fingerprint, quiet_for) = ledger.read(root, role, Instant::now());
    let fingerprint = match &observation {
        Observation::Fingerprint(f) => Some(f.clone()),
        Observation::Count(_) | Observation::Unobserved(_) => None,
    };
    let decision = should_launch(
        role,
        &TriggerInputs {
            observation,
            last_fingerprint,
            quiet_for,
            max_quiet: config.max_quiet(),
        },
    );
    let line = decision_log_line(role, root, &decision);
    if !decision.launches() {
        log::debug!("{line}");
        return RoleTickOutcome::QueueEmpty;
    }
    log::info!("{line}");
    let outcome = run();
    if outcome.is_success() {
        ledger.record_success(root, role, fingerprint, Instant::now());
    }
    outcome
}

/// The idle-edge path's trigger line (#4364): an idle run is never held back,
/// but with the flag on it is logged with trigger [`Trigger::Idle`] so every
/// launch of a gated role names its trigger.
pub fn log_idle_launch(root: &Path, role: &'static str) {
    if is_triggered_role(role) && read_event_trigger_config(root).enabled {
        let decision = TriggerDecision::launch(Trigger::Idle, "idle edge");
        log::info!("{}", decision_log_line(role, root, &decision));
    }
}
