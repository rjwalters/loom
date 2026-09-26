//! Ordered runtime preference with fall-through (Issue #8436).
//!
//! Runtime selection used to be **static**: `LOOM_RUNTIME` >
//! `runtimes.roles.<role>` / `runtimes.default` > built-in `claude`, decided
//! with no reference to whether the chosen runtime's credentials can actually
//! serve a launch. Exhaustion was handled per runtime and the only response
//! was to stop — the #7708 host-level pool-exhaustion hold for sweeps, the
//! #6201/#8408 pre-spawn skip for role ticks. A host with a dead Claude pool,
//! valid Codex seats, and a working pay-per-use endpoint therefore sat idle.
//!
//! Operator direction (2026-09-20): **prefer the Claude and Codex subscription
//! accounts whenever they can serve the work; use a pay-per-use
//! OpenAI-compatible endpoint through a native runtime only as a backstop.**
//! That is an ordered preference with fall-through, which the static model
//! cannot express — `runtimes.default: "opencode"` would send *all* work to
//! the metered endpoint and strand the seats already paid for.
//!
//! ```jsonc
//! "runtimes": {
//!   "preference": ["claude", "codex", {"runtime": "opencode", "modelProfile": "zai-metered"}],
//!   "rolePreference": { "judge": ["codex", "claude"] },
//!   "backstopCeiling": { "maxConcurrent": 2, "appliesFrom": 2 }
//! }
//! ```
//!
//! # What this module is, and is not
//!
//! It started as only the **resolution half** of #8436: config parsing, the
//! shared availability mapping ([`availability`]), and the pure ordered walk
//! ([`resolve`]). Issue #8554 wired it into dispatch, so a configured
//! `runtimes.preference` now changes real launches at three seams:
//!
//! - `sweep_registry::dispatch` resolves a sweep's runtime through
//!   [`resolve_for_dispatch`] — a one-for-one substitution for
//!   [`crate::runtime_admission::resolve_and_admit`].
//! - `work_finder::pool_preflight`'s #7708 host-level hold arms only when the
//!   **whole** list is unavailable, instead of whenever the Claude pool is dry.
//! - `role_runner::runtime_preflight` lets the list choose a role tick's tap,
//!   keeping the #6201/#8408 pre-spawn gate as the fail-closed reporter.
//!
//! Issue #8555 added the fourth piece: the per-host admission bound on the
//! metered backstop tier ([`ceiling`]), asked during the same walk and applied
//! at the first two of those seams — `resolve_for_dispatch` hands the sweep
//! path a [`ceiling::Reservation`] to attach to the child it spawns. Issue
//! #8599 carried the chosen tier into the `role_tick.outcome` record /
//! per-sweep launch record, via [`PreferenceStamp`] and the shared
//! `crate::launch_env::apply_launch_env` pin site.
//!
//! **A tap's `modelProfile` gates *and* pins (#8602).** [`Tap`]'s optional
//! profile is honoured when [`availability`] decides whether the tap can
//! serve (it reads exactly that profile's provider + credential pool), and
//! [`PreferenceStamp::model_profile`] carries the same value out to
//! [`crate::launch_env::apply_launch_env`], which pins `LOOM_MODEL_PROFILE`
//! beside `LOOM_RUNTIME` so the launched child resolves the identical profile
//! availability just checked. Bare-runtime entries — every tap in the shipped
//! examples that is not the metered backstop — pin nothing, matching
//! `Tap::model_profile`'s own `None`, so "absent config is byte-identical"
//! holds for this field too.
//!
//! # Invariants
//!
//! - **Absent config is byte-identical.** With no `preference`/`rolePreference`
//!   key, [`resolve_runtime`] returns exactly what
//!   [`crate::runtime_admission::resolve_and_admit`] returns, having consulted
//!   no pool at all.
//! - **An operator pin wins outright and disables fall-through.** An explicit
//!   per-dispatch runtime, `LOOM_RUNTIME_<ROLE>`, or `LOOM_RUNTIME` short-
//!   circuits to static resolution even when a preference list is configured.
//!   A pin is a deliberate act; silently routing around it would make it
//!   useless for the debugging it exists for.
//! - **Preference is never an admission override.** A runtime a role cannot be
//!   admitted onto is skipped, never forced. `defaults/runtimes/codex.json`
//!   declares `worktreeIsolation: "partial"` and `builder.json`/`doctor.json`
//!   require it, so for build work `["claude","codex","opencode"]` is
//!   effectively `claude -> opencode`.
//! - **The backstop ceiling bounds spend, it never gates on a human.** A
//!   governed tap that is at its per-host ceiling (or that this work is not
//!   eligible for) is skipped with a recorded reason exactly like an
//!   unavailable tap; the walk continues, a recovered higher tap is still
//!   preferred, and nothing waits for approval. Absent config touches no state
//!   at all.
//! - **Fail-closed stays fail-closed.** When every listed tap is skipped the
//!   result carries no choice, and the caller holds/skips exactly as it does
//!   today. The #7708 hold becomes "hold when the *whole list* is exhausted".
//!
//! # One sweep, one runtime
//!
//! A sweep uses one runtime throughout (`runtime_admission`'s own module
//! doc). Fall-through is therefore decided at **dispatch**, never
//! mid-sweep: a sweep that exhausts its runtime in flight fails and is
//! re-dispatched, where it re-resolves. Hysteresis falls out of that for free
//! — once a higher-preference pool recovers, new spawns return to it while
//! in-flight backstop sweeps finish where they are. No pinning mechanism is
//! needed, and none is provided.
//!
//! # Judge independence
//!
//! A native sweep runs every phase in one session with no subagents, so a
//! preference list that lands Builder and Judge on the same single-session
//! runtime weakens review independence. `rolePreference.judge` exists to keep
//! Judge on a different tap from the one that built the change; prefer
//! configuring it over relying on the fleet-wide order.

pub mod availability;
pub mod ceiling;
pub mod handoff;
pub mod resolve;

pub use availability::{availability, Availability, CredentialSource};
pub use ceiling::{BackstopCeiling, ComplexityTier, Intent, Reservation};
pub use resolve::{
    CeilingSkip, ChosenTap, PreferenceStamp, Resolution, SkipReason, SkippedTap, Tap,
    PREFERENCE_LOG_MARKER,
};

use crate::runtime_admission::{canonical_role, ResolvedRuntime, RuntimeRejection, RuntimeSource};
use serde_json::Value;
use std::path::Path;

/// Which config key supplied the preference list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreferenceSource {
    /// `runtimes.rolePreference.<role>` — the per-role override.
    RolePreference,
    /// `runtimes.preference` — the fleet-wide default order.
    FleetPreference,
}

impl PreferenceSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RolePreference => "role-preference",
            Self::FleetPreference => "preference",
        }
    }
}

/// Why resolution stayed on the pre-#8436 static path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticReason {
    /// No `preference`/`rolePreference` key applies to this role.
    NoPreferenceConfigured,
    /// An operator pin is in force; the named source disables fall-through.
    OperatorPin(RuntimeSource),
}

/// The result of asking "what runtime should this launch use?".
#[derive(Debug)]
pub enum Decision {
    /// Static resolution, byte-identical to pre-#8436 behaviour. No credential
    /// pool was read.
    Static {
        reason: StaticReason,
        result: Result<ResolvedRuntime, RuntimeRejection>,
    },
    /// The ordered preference list decided. `resolution.chosen == None` means
    /// every listed tap was skipped: fail closed, exactly as today.
    Preference {
        source: PreferenceSource,
        resolution: Resolution<ResolvedRuntime>,
        /// The backstop slot this decision holds (#8555), present only when
        /// the chosen tap is a governed backstop tier **and** a concurrency
        /// ceiling is configured.
        ///
        /// **The launch path must [`ceiling::Reservation::attach`] it to the
        /// spawned worker's PID.** Until it does, the slot is owned by *this*
        /// process and is released when the `Decision` is dropped — so a
        /// caller that resolves and then does not launch cannot leak it, and a
        /// caller that launches without attaching runs uncounted rather than
        /// pinning a slot forever.
        backstop: Option<ceiling::Reservation>,
    },
}

impl Decision {
    /// Take the backstop reservation out of this decision, for the launch path
    /// to attach to the worker it spawns.
    pub fn take_backstop(&mut self) -> Option<ceiling::Reservation> {
        match self {
            Self::Static { .. } => None,
            Self::Preference { backstop, .. } => backstop.take(),
        }
    }

    /// The admitted runtime, when one was chosen.
    #[must_use]
    pub fn admitted(&self) -> Option<&ResolvedRuntime> {
        match self {
            Self::Static { result, .. } => result.as_ref().ok(),
            Self::Preference { resolution, .. } => {
                resolution.chosen.as_ref().map(|chosen| &chosen.admitted)
            }
        }
    }

    /// The `# LOOM_RUNTIME_PREFERENCE …` marker for this decision, or `None`
    /// on the static path (which has no tiers to report and whose log output
    /// must not change).
    #[must_use]
    pub fn marker_line(&self) -> Option<String> {
        match self {
            Self::Static { .. } => None,
            Self::Preference {
                source,
                resolution,
                backstop,
            } => {
                let mut line = format!("{} source={}", resolution.marker_line(), source.as_str());
                // How much of the metered ceiling this launch consumed, so
                // "the fleet is pinned at its backstop ceiling" is greppable
                // from the same marker that records the fall-through itself.
                if let Some(reservation) = backstop {
                    line.push_str(&format!(" backstop={}", reservation.summary()));
                }
                Some(line)
            }
        }
    }

    /// The [`PreferenceStamp`] to carry on the admitted runtime (#8599), or
    /// `None` on the static path (no tiers to report) and in the fail-closed
    /// case (no chosen tap). Stamping the admission is what gets the chosen
    /// tier out of the daemon log and into the per-launch records: the launch
    /// surfaces read it off [`ResolvedRuntime::preference`] rather than
    /// re-resolving anything.
    ///
    /// The stamped marker is [`Self::marker_line`] verbatim — the exact line
    /// the daemon logs, including the `backstop=` reservation summary (#8555)
    /// when the chosen tap holds a metered slot — so the launch record and the
    /// daemon log can never disagree.
    #[must_use]
    pub fn stamp(&self) -> Option<PreferenceStamp> {
        let Self::Preference {
            source, resolution, ..
        } = self
        else {
            return None;
        };
        let mut stamp = resolution.stamp(source.as_str())?;
        if let Some(line) = self.marker_line() {
            stamp.marker = line;
        }
        Some(stamp)
    }

    /// Collapse this decision into the ordinary admission shape every
    /// existing dispatch call site already expects, so wiring the preference
    /// resolver in is a straight substitution for
    /// [`crate::runtime_admission::resolve_and_admit`] (#8554).
    ///
    /// The static path's `result` passes through unchanged — including its
    /// `Err` shape — preserving the "absent config is byte-identical"
    /// invariant all the way to the error a caller sees. The preference
    /// path's fail-closed case (every tap skipped) is reported as a
    /// [`RuntimeRejection`] whose `reason` is
    /// [`Resolution::exhausted_diagnostic`], naming every skipped tap and
    /// why, so a refused dispatch stays as diagnosable as the single-runtime
    /// rejection it replaces.
    ///
    /// # Errors
    /// The static path's own rejection, or — on the preference path, when
    /// every listed tap was skipped — the fail-closed rejection built from
    /// [`Resolution::exhausted_diagnostic`].
    pub fn into_admission(self, role: &str) -> Result<ResolvedRuntime, RuntimeRejection> {
        match self {
            Self::Static { result, .. } => result,
            Self::Preference { resolution, .. } => match resolution.chosen {
                Some(chosen) => Ok(chosen.admitted),
                None => Err(RuntimeRejection {
                    role: role.to_string(),
                    runtime: String::new(),
                    source: RuntimeSource::Preference,
                    unmet_capabilities: vec![],
                    reason: resolution.exhausted_diagnostic(role),
                }),
            },
        }
    }
}

/// One resolution's answer to "what does this launch run on, and what did
/// choosing it cost?" — the admitted runtime together with the metered
/// backstop slot that settling on it took (#8555).
///
/// The two travel together, by value, because the slot has to reach the
/// spawned child's PID and the runtime has to reach the spawn arguments: they
/// are the same journey. A caller moves this whole value along whatever
/// intermediate already crosses its own resolve→spawn seam
/// (`PreparedIssueDispatch` on the sweep path, the role tick's local on the
/// role-runner path) and calls [`handoff::attach`] at the far end. **Dropping
/// it releases the slot**, so every early return between resolution and spawn
/// is correct with no unwinding code — see [`handoff`] for why this is carried
/// rather than parked in shared state.
///
/// `admitted` is `Option` because a caller may have no runtime to name at all:
/// `sweep_registry`'s hermetic fixtures skip admission entirely, and
/// `role_runner::runtime_preflight` reports "keep the admission you already
/// had" the same way. Neither can hold a backstop slot, so an absent runtime
/// always comes with an absent reservation.
#[derive(Debug, Default)]
pub struct DispatchAdmission {
    /// The runtime this launch should use, when this resolution named one.
    pub admitted: Option<ResolvedRuntime>,
    /// The metered slot the choice consumed, present only when the chosen tap
    /// is a governed backstop tier **and** a ceiling is configured.
    pub backstop: Option<ceiling::Reservation>,
}

impl DispatchAdmission {
    /// "No runtime named here, and nothing metered" — the shape a caller that
    /// opted out of admission gets back.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }
}

/// Resolve the runtime for one dispatch and collapse the answer to the
/// ordinary admission shape, logging the `# LOOM_RUNTIME_PREFERENCE` marker
/// on the way when a preference list actually decided (#8554).
///
/// The point of this wrapper is that a dispatch call site swaps
/// [`crate::runtime_admission::resolve_and_admit`] for it with no branching of
/// its own — same arity, same `Err` shape on the static path — so wiring
/// preference into a call site cannot drift from the marker/collapse handling
/// every other call site does.
///
/// `now` is read here rather than taken as a parameter because every
/// production caller wants the wall clock; a test that needs a pinned clock
/// calls [`resolve_runtime`] directly, which is the seam that takes one.
///
/// # Side effect
/// This is the **dispatch-intent** entry point ([`Intent::Dispatch`]), so
/// settling on a governed backstop tap takes a metered slot (#8555). That is
/// why the return type is [`DispatchAdmission`] rather than a bare
/// [`ResolvedRuntime`]: the slot rides back to the caller, which **must** hand
/// it to the spawned child with [`handoff::attach`]. A caller that drops it
/// instead releases the slot — correct for a dispatch that never launches,
/// and merely uncounted for one that does.
///
/// # Errors
/// The static path's own rejection, a malformed-preference rejection, or —
/// when every listed tap was skipped — the fail-closed rejection
/// [`Decision::into_admission`] builds from
/// [`Resolution::exhausted_diagnostic`].
pub fn resolve_for_dispatch(
    root: &Path,
    role: &str,
    explicit: Option<&str>,
) -> Result<DispatchAdmission, RuntimeRejection> {
    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    let context = DispatchContext {
        complexity: None,
        intent: Intent::Dispatch,
    };
    let mut decision = resolve_runtime_for(root, role, explicit, now, context)?;
    if let Some(marker) = decision.marker_line() {
        log::info!("runtime_preference: {role} resolved by preference list — {marker} (#8554)");
    }
    // #8599: stamp the decision onto the admission so the launch surfaces can
    // report the chosen tier without re-resolving it. `None` on the static
    // path keeps "absent config is byte-identical" intact all the way to the
    // child's environment.
    let stamp = decision.stamp();
    let backstop = decision.take_backstop();
    let mut admitted = decision.into_admission(role)?;
    admitted.preference = stamp;
    Ok(DispatchAdmission {
        admitted: Some(admitted),
        backstop,
    })
}

/// Parse one preference-list entry: a bare runtime id, or an object naming the
/// runtime and the model profile that binds its provider + credential source.
fn parse_tap(entry: &Value, path: &str, index: usize) -> Result<Tap, String> {
    let nonempty = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    match entry {
        Value::String(runtime) => nonempty(Some(runtime))
            .map(|runtime| Tap {
                runtime,
                model_profile: None,
            })
            .ok_or_else(|| format!("{path}[{index}] is an empty runtime name")),
        Value::Object(map) => {
            let runtime = nonempty(map.get("runtime").and_then(Value::as_str))
                .ok_or_else(|| format!("{path}[{index}] must name a non-empty \"runtime\""))?;
            let model_profile = match map.get("modelProfile") {
                None | Some(Value::Null) => None,
                Some(Value::String(profile)) => Some(
                    nonempty(Some(profile))
                        .ok_or_else(|| format!("{path}[{index}] has an empty \"modelProfile\""))?,
                ),
                Some(_) => {
                    return Err(format!("{path}[{index}] \"modelProfile\" must be a string"))
                }
            };
            let unknown: Vec<&str> = map
                .keys()
                .map(String::as_str)
                .filter(|key| !matches!(*key, "runtime" | "modelProfile"))
                .collect();
            if !unknown.is_empty() {
                return Err(format!(
                    "{path}[{index}] has unknown key(s): {} (known: runtime, modelProfile)",
                    unknown.join(", ")
                ));
            }
            Ok(Tap {
                runtime,
                model_profile,
            })
        }
        _ => Err(format!(
            "{path}[{index}] must be a runtime name or an object with a \"runtime\" key"
        )),
    }
}

/// Parse a JSON array of preference entries. An **empty** array is `Ok(None)`
/// — "unset, fall through to the next tier" — matching the established
/// empty-value semantics of `runtimes.roles.<role>: ""`.
fn parse_list(value: &Value, path: &str) -> Result<Option<Vec<Tap>>, String> {
    let Some(entries) = value.as_array() else {
        return Err(format!("{path} must be an array of runtime names"));
    };
    if entries.is_empty() {
        return Ok(None);
    }
    let taps = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| parse_tap(entry, path, index))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen = std::collections::BTreeSet::new();
    for tap in &taps {
        if !seen.insert(tap.to_string()) {
            return Err(format!(
                "{path} lists {tap} more than once; a duplicate entry can never be reached and \
                 is more likely a typo than an intent"
            ));
        }
    }
    Ok(taps.into())
}

/// The preference list that applies to `role`, if any:
/// `runtimes.rolePreference.<role>` first, then `runtimes.preference`.
///
/// Fail-closed shape validation, in the spirit of `runtimes.roles` (#4494): an
/// unknown `rolePreference` key, a non-array value, a malformed entry, or a
/// duplicated tap is an **error**, not a silently-ignored entry. A
/// misconfigured preference list that degraded silently would strand work on
/// the very tier the operator was trying to route around.
///
/// # Errors
/// A formatted message naming the offending key, for the caller to surface as
/// a [`RuntimeRejection`] or a `loom-daemon validate` finding.
pub fn preference_for(
    config: &Value,
    role: &str,
) -> Result<Option<(PreferenceSource, Vec<Tap>)>, String> {
    if let Some(per_role) = crate::config_resolver::get_path(config, "runtimes.rolePreference") {
        let Some(map) = per_role.as_object() else {
            return Err(
                "runtimes.rolePreference must be an object mapping role names to preference lists"
                    .to_string(),
            );
        };
        let mut unknown: Vec<String> = map
            .keys()
            .filter(|key| canonical_role(key).is_none())
            .cloned()
            .collect();
        unknown.sort();
        if !unknown.is_empty() {
            return Err(format!(
                "unknown role name(s) in runtimes.rolePreference: {}",
                unknown.join(", ")
            ));
        }
        // Validate EVERY list, not just the requested role's — the same
        // whole-map discipline `validate_runtimes_roles_shape` applies, so a
        // typo in a sibling role's list surfaces on the next launch of any
        // role rather than only when that role happens to tick.
        for (key, value) in map {
            parse_list(value, &format!("runtimes.rolePreference.{key}"))?;
        }
        if let Some(value) = map.get(role) {
            if let Some(taps) = parse_list(value, &format!("runtimes.rolePreference.{role}"))? {
                return Ok(Some((PreferenceSource::RolePreference, taps)));
            }
        }
    }
    let Some(fleet) = crate::config_resolver::get_path(config, "runtimes.preference") else {
        return Ok(None);
    };
    Ok(parse_list(fleet, "runtimes.preference")?
        .map(|taps| (PreferenceSource::FleetPreference, taps)))
}

/// Proactively check a resolved config's preference keys for the same
/// fail-closed problems [`preference_for`] rejects at resolution time, so
/// `loom-daemon validate` can surface them before any launch hits them — the
/// counterpart of
/// [`crate::runtime_admission::check_runtimes_roles_config`].
#[must_use]
pub fn check_runtimes_preference_config(config: &Value) -> Vec<String> {
    // `sweep-lifecycle` is only a probe role here: `preference_for` validates
    // the whole `rolePreference` map plus `runtimes.preference` regardless of
    // which role is asked about.
    let mut findings = match preference_for(config, "sweep-lifecycle") {
        Ok(_) => Vec::new(),
        Err(message) => vec![format!("runtimes preference: {message}")],
    };
    findings.extend(ceiling::check_config(config));
    findings
}

/// What dispatch knows about the *work* being launched, as opposed to the role
/// launching it.
///
/// Only the backstop ceiling's optional eligibility filter reads this today
/// (#8555): "low-value work never reaches the metered tap" is a property of the
/// issue, not of the role. It is a struct rather than a bare argument so a
/// later admission question can be added without re-churning every call site.
#[derive(Debug, Clone, Copy, Default)]
pub struct DispatchContext<'a> {
    /// The issue's `<!-- loom:complexity=<tier> -->` stratum, as the work
    /// finder already carries it through `dispatch(issue, complexity)`.
    /// `None` ⇒ treated as `routine`.
    pub complexity: Option<&'a str>,
    /// Whether this resolution precedes a real launch (and so may take a
    /// metered slot) or is a read-only probe. Defaults to
    /// [`Intent::Probe`] — `work_finder::pool_preflight` re-resolves on every
    /// tick for every workspace purely to decide whether to hold, and a probe
    /// that consumed capacity would refuse the very dispatch it was asked
    /// about.
    pub intent: Intent,
}

/// The operator pin in force for `role`, if any — the three tiers that
/// outrank a preference list and disable fall-through.
fn operator_pin(canonical: &str, explicit: Option<&str>) -> Option<RuntimeSource> {
    let set = |value: Option<String>| value.is_some_and(|v| !v.trim().is_empty());
    if set(explicit.map(str::to_string)) {
        return Some(RuntimeSource::Explicit);
    }
    let role_env = format!("LOOM_RUNTIME_{}", canonical.replace('-', "_").to_ascii_uppercase());
    if set(std::env::var(role_env).ok()) {
        return Some(RuntimeSource::RoleEnvironment);
    }
    if set(std::env::var("LOOM_RUNTIME").ok()) {
        return Some(RuntimeSource::GlobalEnvironment);
    }
    None
}

/// Resolve the runtime for `role`, walking the configured preference list
/// against live admission and credential availability.
///
/// `now` is epoch seconds, threaded through to the codex pool's cooldown
/// arithmetic so a test can pin it.
///
/// # Errors
/// A [`RuntimeRejection`] when the preference configuration itself is
/// malformed — resolution fails closed rather than degrading to a silently
/// different order.
pub fn resolve_runtime(
    root: &Path,
    role: &str,
    explicit: Option<&str>,
    now: u64,
) -> Result<Decision, RuntimeRejection> {
    resolve_runtime_for(root, role, explicit, now, DispatchContext::default())
}

/// [`resolve_runtime`] with what dispatch knows about the work itself — the
/// form the work finder and role runner call once they carry a
/// [`DispatchContext`].
///
/// # Side effect
/// Unlike the rest of this module, choosing a **governed backstop tap** under a
/// configured ceiling *takes a slot* (a lease file under
/// [`ceiling::lease_dir`]): the count and the choice have to be atomic or two
/// concurrent dispatches both admit at `limit - 1`. The slot rides on the
/// returned [`Decision`] and is released when it drops, so a caller that
/// resolves without launching cannot leak it.
///
/// # Errors
/// As [`resolve_runtime`].
pub fn resolve_runtime_for(
    root: &Path,
    role: &str,
    explicit: Option<&str>,
    now: u64,
    context: DispatchContext<'_>,
) -> Result<Decision, RuntimeRejection> {
    let Some(canonical) = canonical_role(role) else {
        // Unknown roles are not this module's error to shape: hand straight
        // back the rejection `resolve_and_admit` already produces for them.
        return Ok(Decision::Static {
            reason: StaticReason::NoPreferenceConfigured,
            result: crate::runtime_admission::resolve_and_admit(root, role, explicit),
        });
    };
    if let Some(pin) = operator_pin(canonical, explicit) {
        return Ok(Decision::Static {
            reason: StaticReason::OperatorPin(pin),
            result: crate::runtime_admission::resolve_and_admit(root, role, explicit),
        });
    }
    let config = crate::config_resolver::resolve_effective_config(root);
    let listed = preference_for(&config, canonical).map_err(|reason| RuntimeRejection {
        role: canonical.to_string(),
        runtime: String::new(),
        source: RuntimeSource::Preference,
        unmet_capabilities: vec![],
        reason,
    })?;
    let Some((source, taps)) = listed else {
        return Ok(Decision::Static {
            reason: StaticReason::NoPreferenceConfigured,
            result: crate::runtime_admission::resolve_and_admit(root, role, None),
        });
    };
    let backstop_ceiling = ceiling::configured(&config).map_err(|reason| RuntimeRejection {
        role: canonical.to_string(),
        runtime: String::new(),
        source: RuntimeSource::Preference,
        unmet_capabilities: vec![],
        reason,
    })?;
    // Filled in by the availability closure below when — and only when — the
    // walk settles on a governed backstop tap. `resolve` short-circuits on the
    // first `Ok`, so at most one slot is ever taken per walk, and it always
    // belongs to the tap actually chosen.
    let mut reservation: Option<ceiling::Reservation> = None;
    let resolution = resolve::resolve(
        &taps,
        |tap| {
            crate::runtime_admission::resolve_and_admit(root, canonical, Some(&tap.runtime))
                .map(|mut admitted| {
                    // The walk chose among candidates; it is not the operator
                    // pin `Explicit` denotes, even though each candidate was
                    // offered to admission as an explicit runtime.
                    admitted.source = RuntimeSource::Preference;
                    admitted
                })
                .map_err(|rejection| SkipReason::NotAdmitted {
                    unmet: rejection.unmet_capabilities.clone(),
                    detail: rejection.reason.clone(),
                })
        },
        |tier, tap, admitted| {
            match availability::availability(root, tap, admitted, now) {
                state if state.is_spawnable() => {}
                state => {
                    return Err(state
                        .skip_reason()
                        .expect("a non-spawnable availability always yields a skip reason"))
                }
            }
            // The ceiling is asked LAST, and only for the tiers it governs:
            // credentials first means a tap that could not have run anyway is
            // never charged a metered slot, and asking last means the slot is
            // taken only for a tap the walk is about to return.
            let Some(bound) = backstop_ceiling.as_ref().filter(|c| c.governs(tier)) else {
                return Ok(());
            };
            match ceiling::admit(bound, canonical, tap, context.complexity, context.intent) {
                ceiling::Verdict::Admitted(slot) => {
                    reservation = slot;
                    Ok(())
                }
                ceiling::Verdict::Refused(reason) => Err(reason),
            }
        },
    );
    Ok(Decision::Preference {
        source,
        resolution,
        backstop: reservation,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
