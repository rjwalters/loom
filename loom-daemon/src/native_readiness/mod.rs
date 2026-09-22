//! Provider-free startup-readiness measurement for guarded native harnesses
//! (issue #8581, following #8529/#8522/#8568).
//!
//! # What this measures, and what it deliberately refuses to measure
//!
//! An isolated guarded OpenCode canary took roughly 157s to reach
//! initialization and 162s to create a session, leaving ~17s of a 180s wall
//! deadline. That run proved a *wall time*, and nothing about its cause: no
//! HTTP status, no provider error, no assistant message. This module exists to
//! turn that single opaque number into **named, separately-timed boundaries**
//! that can be re-measured cheaply, without spending a token.
//!
//! The boundary set is [`Stage`]. Three of its members
//! ([`Stage::FirstProviderEvent`], [`Stage::FirstTool`], [`Stage::Completion`])
//! are **structurally unobservable** to a provider-free probe, and this module
//! records them as [`Observation::Unknown`] with a reason, forever. It never
//! synthesizes a duration, a span, or an interpolated estimate for work it did
//! not watch happen. A fabricated provider span is worse than a missing one:
//! it launders a guess into evidence.
//!
//! # The `#8568` interaction (why cold start is not canary-specific)
//!
//! `native_tools::provision::state::create` allocates a **fresh
//! `uuid::Uuid::new_v4()` directory per launch** under the per-workspace base,
//! and `State::configure` points `XDG_CACHE_HOME`, `XDG_DATA_HOME`,
//! `XDG_CONFIG_HOME` and `XDG_STATE_HOME` at subdirectories of it. Every
//! guarded OpenCode launch therefore starts with an **empty package cache** and
//! re-resolves the pinned `@opencode-ai/plugin` dependency, whether or not it
//! is a canary with a synthetic `HOME`. That is a property of the landed
//! private-state layout, read directly off the source — not an inference from
//! the single fresh-home experiment in #8529, which could not have
//! distinguished the two.
//!
//! This module does **not** change that layout. It measures it, and it provides
//! [`package_cache`], a keyed user-home cache for *package artifacts only*, so
//! the cold/warm difference can be observed before anything in the production
//! launch path is touched.
//!
//! # Safety properties, enforced in code rather than documented
//!
//! * No model call, ever — [`probe`] refuses any argv token outside a
//!   provider-free allowlist, so `run`, `--prompt`, `--model` and `--auto`
//!   cannot be reached even by an operator flag.
//! * No credential reaches the child — the probe builds its environment from
//!   `env_clear()` plus a fixed allowlist, and asserts the result contains
//!   nothing credential-shaped.
//! * No forge contact — no `GH_TOKEN`/`GITHUB_TOKEN`/`GITEA_TOKEN` is passed
//!   and no forge client is constructed on this path.
//! * No paid retry — a failed or timed-out attempt is reported, never retried
//!   against a provider.
//! * Timeout output carries the **stage** plus byte counts and a closed-set
//!   [`Classification`], never child output, argv or environment values.

pub mod measure;
pub mod package_cache;
pub mod probe;

use serde::Serialize;

/// An observable startup boundary of a guarded native launch.
///
/// Ordered as a launch traverses them. The order is load-bearing: a failure at
/// one boundary renders every later boundary [`Observation::NotReached`] rather
/// than zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// `<cli> --version` answered and parsed into a supported major.
    BinaryProbe,
    /// The guarded bindings (plugin source + pinned package manifest + private
    /// XDG tree) were written into this attempt's isolated state directory.
    BindingProvision,
    /// The pinned plugin package set was resolved and made loadable.
    PackageResolution,
    /// A provider-free readiness invocation of the real CLI returned.
    ServerSessionReady,
    /// First provider/assistant event. Unobservable without a model call.
    FirstProviderEvent,
    /// First tool part. Unobservable without a model call.
    FirstTool,
    /// Run completion. Unobservable without a model call.
    Completion,
}

impl Stage {
    /// Every boundary, in traversal order.
    pub const ALL: [Stage; 7] = [
        Stage::BinaryProbe,
        Stage::BindingProvision,
        Stage::PackageResolution,
        Stage::ServerSessionReady,
        Stage::FirstProviderEvent,
        Stage::FirstTool,
        Stage::Completion,
    ];

    /// Stable snake_case name, matching the serialized form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::BinaryProbe => "binary_probe",
            Stage::BindingProvision => "binding_provision",
            Stage::PackageResolution => "package_resolution",
            Stage::ServerSessionReady => "server_session_ready",
            Stage::FirstProviderEvent => "first_provider_event",
            Stage::FirstTool => "first_tool",
            Stage::Completion => "completion",
        }
    }

    /// Whether a probe that makes no model call can observe this boundary.
    #[must_use]
    pub fn provider_free_observable(self) -> bool {
        !matches!(self, Stage::FirstProviderEvent | Stage::FirstTool | Stage::Completion)
    }

    /// Why an unobservable boundary is reported unknown.
    ///
    /// `None` for boundaries this module does measure — asking for a reason
    /// there is a caller bug, not a fallback to a plausible-sounding string.
    #[must_use]
    pub fn unknown_reason(self) -> Option<&'static str> {
        match self {
            Stage::FirstProviderEvent => Some(
                "no provider request is issued by a provider-free probe; provider latency is unmeasured, not zero",
            ),
            Stage::FirstTool => Some(
                "a tool part requires an assistant turn, which needs a model call this probe never makes",
            ),
            Stage::Completion => Some(
                "run completion requires a model call this probe never makes",
            ),
            _ => None,
        }
    }
}

/// Why a boundary produced no duration.
///
/// A closed set on purpose: it is the only thing derived from a child that
/// reaches the report, so it cannot carry a substring of the child's output,
/// argv or environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// The hard deadline elapsed; the child's process group was terminated.
    Timeout,
    /// The child ran and exited nonzero.
    NonzeroExit,
    /// The child died on a signal.
    SignalDeath,
    /// The child could not be started at all.
    SpawnFailed,
    /// The child started but its exit could not be observed.
    ExitUnobserved,
    /// The child answered but its version output did not parse.
    UnparsableVersion,
    /// This probe refused to run the boundary (policy, not failure).
    Refused,
    /// A local filesystem/provisioning step failed before any child ran.
    LocalSetupFailed,
}

/// What was learned about one boundary in one attempt.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Observation {
    /// The boundary completed; `millis` is the wall time spent inside it.
    Measured { millis: u64 },
    /// Structurally unobservable here. Carries the reason, never a number.
    Unknown { reason: &'static str },
    /// Entered and did not complete. Nothing from the child's bytes is
    /// included beyond their counts.
    Failed {
        classification: Classification,
        elapsed_millis: u64,
        stdout_bytes: usize,
        stderr_bytes: usize,
    },
    /// An earlier boundary failed, so this one was never entered.
    NotReached,
}

impl Observation {
    /// The measured duration, if this boundary was measured.
    #[must_use]
    pub fn millis(&self) -> Option<u64> {
        match self {
            Observation::Measured { millis } => Some(*millis),
            _ => None,
        }
    }
}

/// Whether the shared package-artifact cache was consulted, and what happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheOutcome {
    /// Cold mode: the cache was deliberately not consulted.
    Bypassed,
    /// Warm mode, nothing published yet: this attempt built and published.
    Miss,
    /// Warm mode: a valid keyed entry was reused.
    Hit,
    /// Warm mode: an entry existed but failed validation and was quarantined.
    Invalidated,
}

/// Cold (fresh state per attempt) or warm (shared keyed package artifacts).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Cold,
    Warm,
}

impl Mode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Cold => "cold",
            Mode::Warm => "warm",
        }
    }
}

/// Whether the child was allowed to reach the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// The host's ordinary network configuration is inherited.
    Allowed,
    /// Proxy/registry environment variables are pointed at a closed loopback
    /// port. This is an **environment-level** denial for well-behaved HTTP
    /// clients, not a kernel namespace: a client that ignores `*_proxy` can
    /// still reach the network, so a pass under this mode is weaker evidence
    /// than a failure under it.
    DeniedByEnv,
}

impl NetworkMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NetworkMode::Allowed => "allowed",
            NetworkMode::DeniedByEnv => "denied_by_env",
        }
    }
}

/// One traversal of the boundary set.
#[derive(Clone, Debug, Serialize)]
pub struct AttemptReport {
    pub index: usize,
    pub mode: Mode,
    pub cache: CacheOutcome,
    pub total_millis: u64,
    /// Whether this attempt's readiness invocation loaded the guarded plugin,
    /// observed from the receipt the binding writes rather than inferred from
    /// an exit status. `None` when the invocation did not complete, so nothing
    /// was observable. See [`probe::Probe::readiness`].
    pub plugin_load_observed: Option<bool>,
    pub stages: Vec<StageObservation>,
}

/// A boundary paired with what was observed about it.
#[derive(Clone, Debug, Serialize)]
pub struct StageObservation {
    pub stage: Stage,
    #[serde(flatten)]
    pub observation: Observation,
}

impl AttemptReport {
    /// Look up one boundary's observation.
    #[must_use]
    pub fn stage(&self, stage: Stage) -> Option<&Observation> {
        self.stages
            .iter()
            .find(|s| s.stage == stage)
            .map(|s| &s.observation)
    }
}

/// Distribution of one boundary's measured durations across a phase.
///
/// Deliberately min/median/max and a count, with no mean and no ratio: a mean
/// over three runs on a contended host invites a speedup claim the data cannot
/// support, and a ratio is exactly the claim this issue forbids.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Aggregate {
    pub measured: usize,
    pub min_millis: u64,
    pub median_millis: u64,
    pub max_millis: u64,
}

/// Median of `values`, or `None` when empty. Even counts take the lower of the
/// two central samples rather than averaging: an averaged median of wall times
/// is a number no run produced.
#[must_use]
pub fn median(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[(values.len() - 1) / 2])
}

/// Aggregate one boundary's measured durations over a set of attempts.
#[must_use]
pub fn aggregate(attempts: &[AttemptReport], stage: Stage) -> Option<Aggregate> {
    let mut values: Vec<u64> = attempts
        .iter()
        .filter_map(|a| a.stage(stage).and_then(Observation::millis))
        .collect();
    if values.is_empty() {
        return None;
    }
    let median_millis = median(&mut values)?;
    Some(Aggregate {
        measured: values.len(),
        min_millis: *values.first()?,
        median_millis,
        max_millis: *values.last()?,
    })
}

/// Host conditions sampled around a phase, so a later reader can tell whether
/// two phases are even comparable.
#[derive(Clone, Debug, Serialize)]
pub struct HostConditions {
    pub os: &'static str,
    pub arch: &'static str,
    pub logical_cpus: usize,
    pub loadavg_1m_before: Option<f64>,
    pub loadavg_1m_after: Option<f64>,
}

impl HostConditions {
    /// Sample the pre-phase side. `finish` fills in the post-phase side.
    #[must_use]
    pub fn begin() -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            logical_cpus: crate::cpu_headroom::logical_cpu_count(),
            loadavg_1m_before: crate::cpu_headroom::read_loadavg_1m(),
            loadavg_1m_after: None,
        }
    }

    /// Sample the post-phase load average.
    pub fn finish(&mut self) {
        self.loadavg_1m_after = crate::cpu_headroom::read_loadavg_1m();
    }

    /// Largest observed 1-minute load average across the phase, if readable.
    #[must_use]
    pub fn peak_loadavg(&self) -> Option<f64> {
        match (self.loadavg_1m_before, self.loadavg_1m_after) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }
}

/// One mode's attempts plus its own host conditions.
#[derive(Clone, Debug, Serialize)]
pub struct PhaseReport {
    pub mode: Mode,
    pub host: HostConditions,
    pub attempts: Vec<AttemptReport>,
    pub per_stage: Vec<StageAggregate>,
}

/// A boundary paired with its distribution over a phase.
#[derive(Clone, Debug, Serialize)]
pub struct StageAggregate {
    pub stage: Stage,
    #[serde(flatten)]
    pub aggregate: Aggregate,
}

impl PhaseReport {
    /// Build a phase report, aggregating every measured boundary.
    #[must_use]
    pub fn new(mode: Mode, host: HostConditions, attempts: Vec<AttemptReport>) -> Self {
        let per_stage = Stage::ALL
            .iter()
            .filter_map(|&stage| {
                aggregate(&attempts, stage).map(|aggregate| StageAggregate { stage, aggregate })
            })
            .collect();
        Self {
            mode,
            host,
            attempts,
            per_stage,
        }
    }
}

/// A boundary this run could not observe, with the reason it could not.
#[derive(Clone, Debug, Serialize)]
pub struct UnknownBoundary {
    pub stage: Stage,
    pub reason: &'static str,
}

/// The whole timing report.
///
/// Every field that could be read as a claim about provider behaviour, cost or
/// improvement is a constant `false`/`0`/`null` here, present precisely so a
/// consumer cannot mistake silence for an absent risk.
#[derive(Clone, Debug, Serialize)]
pub struct ReadinessReport {
    pub schema: u32,
    /// Always 0. A provider-free probe makes none.
    pub model_calls: u32,
    /// Always false. A failed attempt is reported, never retried for money.
    pub paid_retry: bool,
    /// Always false. No forge client is constructed on this path.
    pub forge_contact: bool,
    /// Always false. This report states durations; it does not claim a win.
    pub speedup_claimed: bool,
    pub network: NetworkMode,
    /// The provider-free readiness argv actually used. Every token is drawn
    /// from [`probe::READINESS_ALLOWLIST`], so it cannot carry a secret.
    pub readiness_command: Vec<String>,
    /// Whether this probe shape loaded the guarded plugin set against the CLI
    /// that was actually probed.
    ///
    /// Derived from [`plugin_load_verdict`] over the attempts' own receipts —
    /// never from an exit status, and never from a constant flipped by hand.
    /// A run against OpenCode 1.18.31 with the default `debug config` argv
    /// reports `true`; the same run with `--version`, or any run against a CLI
    /// that does not load the binding, reports `false`.
    pub plugin_load_proven: Option<bool>,
    /// The CLI's own reported version line, sanitized and bounded.
    pub cli_version: Option<String>,
    pub cache_base: Option<String>,
    pub phases: Vec<PhaseReport>,
    pub unknown_boundaries: Vec<UnknownBoundary>,
    /// Whether the phases ran under load averages close enough to compare.
    /// `None` when the load average could not be read on this platform.
    pub host_conditions_comparable: Option<bool>,
    pub notes: Vec<&'static str>,
}

/// Fractional 1-minute-load-average drift between phases above which their
/// measurements are reported as not comparable.
const COMPARABLE_LOAD_DRIFT: f64 = 0.25;

/// Whether two phases' peak load averages are close enough that their timings
/// can be compared at all.
///
/// Relative to the larger of the two, so this is scale-free: 0.4 → 0.5 on an
/// idle host is a big relative jump and is reported as not comparable, which
/// is the honest answer even though both numbers are small.
#[must_use]
pub fn comparable_load(phases: &[PhaseReport]) -> Option<bool> {
    let peaks: Vec<f64> = phases.iter().filter_map(PhaseReport::peak).collect();
    if peaks.len() < 2 {
        return None;
    }
    let min = peaks.iter().copied().fold(f64::INFINITY, f64::min);
    let max = peaks.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if max <= 0.0 {
        return Some(true);
    }
    Some((max - min) / max <= COMPARABLE_LOAD_DRIFT)
}

impl PhaseReport {
    fn peak(&self) -> Option<f64> {
        self.host.peak_loadavg()
    }
}

/// Whether every readiness invocation this run could observe loaded the
/// guarded plugin.
///
/// `None` when no attempt produced an observation at all — a run whose
/// readiness boundary never completed has no opinion on plugin load, and must
/// not report one. A mixed run (some attempts loaded it, some did not) is
/// `Some(false)`: "this probe shape loads the guarded plugin" is a claim about
/// every invocation of it, and one counterexample refutes it.
#[must_use]
pub fn plugin_load_verdict(phases: &[PhaseReport]) -> Option<bool> {
    let mut seen = false;
    let mut all = true;
    for observed in phases
        .iter()
        .flat_map(|phase| phase.attempts.iter())
        .filter_map(|attempt| attempt.plugin_load_observed)
    {
        seen = true;
        all &= observed;
    }
    seen.then_some(all)
}

impl ReadinessReport {
    /// Assemble the report, filling in the unknown-boundary ledger and the
    /// standing disclosures.
    #[must_use]
    pub fn new(
        network: NetworkMode,
        readiness_command: Vec<String>,
        cli_version: Option<String>,
        cache_base: Option<String>,
        phases: Vec<PhaseReport>,
    ) -> Self {
        let unknown_boundaries = Stage::ALL
            .iter()
            .filter_map(|&stage| {
                stage
                    .unknown_reason()
                    .map(|reason| UnknownBoundary { stage, reason })
            })
            .collect();
        let host_conditions_comparable = comparable_load(&phases);
        let plugin_load_proven = plugin_load_verdict(&phases);
        Self {
            schema: 1,
            model_calls: 0,
            paid_retry: false,
            forge_contact: false,
            speedup_claimed: false,
            network,
            readiness_command,
            plugin_load_proven,
            cli_version,
            cache_base,
            phases,
            unknown_boundaries,
            host_conditions_comparable,
            notes: vec![
                "Durations are wall times on one uncontrolled host; they are not a benchmark and no speedup is claimed.",
                "The historical ~157s cold run in issue #8529 is a single observation and is not reused as a baseline here.",
                "A provider-free readiness probe returning successfully does not prove inference readiness.",
                "plugin_load_proven comes from a receipt the guarded binding writes, not from the exit status: OpenCode 1.18.31 logs `failed to load plugin` and still exits 0 (issue #8600).",
                "Guarded launches allocate a fresh per-launch state directory (native_tools::provision::state), so XDG_CACHE_HOME is empty on every launch, not only for canaries.",
            ],
        }
    }
}

/// Reduce a child's own version line to something safe to embed in a report:
/// the first nonempty line, printable ASCII only, bounded length.
///
/// Same shape as `worker_spawn::opencode_version::reported`, duplicated rather
/// than shared because that one is private to the launch path's error strings
/// and widening its visibility to serve a report would couple a refusal
/// message to a measurement artifact.
#[must_use]
pub fn sanitize_version_line(stdout: &str) -> Option<String> {
    let line: String = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(40)
        .collect();
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
