//! The pure ordered-preference resolver (Issue #8436).
//!
//! Nothing in this file reads a file, an environment variable, or a clock. It
//! takes an ordered list of [`Tap`]s and two caller-supplied questions —
//! *"is this tap admitted for the role?"* and *"does it have a spawnable
//! credential right now?"* — and answers *"which tap, and why not the ones
//! above it?"*.
//!
//! Keeping the walk pure is what makes the interesting cases testable without
//! provisioning a Claude token pool, a Codex seat, and a metered endpoint on
//! the test host: the availability source is a closure, so a test can say
//! "Claude is dry, Codex is fine" in one line.
//!
//! # Order of the two questions
//!
//! Admission is asked **first**, and a tap that fails it is skipped without
//! its credential source ever being read. Two reasons:
//!
//! 1. **Admission is the harder constraint.** A preference list is a
//!    preference, never an admission override — `defaults/runtimes/codex.json`
//!    declares `worktreeIsolation: "partial"`, so Codex is not admitted for
//!    Builder/Doctor no matter how many Codex seats are free, and reporting
//!    "codex: no spawnable credential" for it would be actively misleading.
//! 2. **Reading a pool has a cost** (directory scans, JSON parses) that a tap
//!    which could never run should not pay.

use std::fmt;

/// One ordered entry in a preference list: a runtime, optionally with the
/// model profile that pins *which provider and credential source* that
/// runtime draws from.
///
/// The operator's framing (2026-09-20) is that the unit being ordered is a
/// **tap** — `(runtime, credential source)` — not a bare runtime id: the same
/// model family is reachable through a flat-rate subscription and through a
/// metered pay-per-token endpoint, under different provider ids, with
/// completely different economics. A bare runtime name is the shorthand for
/// "that runtime with whatever profile it would have chosen anyway", which is
/// why [`Tap::model_profile`] is optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tap {
    /// The runtime id, as it appears in `defaults/runtimes/<id>.json`.
    pub runtime: String,
    /// The model profile whose `providers[runtime]` + credential mapping this
    /// tap draws on. `None` ⇒ the runtime's own default resolution applies
    /// (for the non-native runtimes, which have no profile, always `None`).
    pub model_profile: Option<String>,
}

impl Tap {
    /// A tap naming only a runtime — the shorthand form of a preference entry.
    #[must_use]
    pub fn runtime(runtime: &str) -> Self {
        Self {
            runtime: runtime.to_string(),
            model_profile: None,
        }
    }

    /// A tap naming a runtime and the model profile that binds its provider.
    #[must_use]
    pub fn with_profile(runtime: &str, model_profile: &str) -> Self {
        Self {
            runtime: runtime.to_string(),
            model_profile: Some(model_profile.to_string()),
        }
    }
}

impl fmt::Display for Tap {
    /// `claude`, or `opencode:zai-metered` when a profile pins the tap. This
    /// rendering is the stable identity used in every log line and telemetry
    /// field, so an operator can grep one string across the launch record and
    /// the role log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.model_profile {
            Some(profile) => write!(f, "{}:{profile}", self.runtime),
            None => f.write_str(&self.runtime),
        }
    }
}

/// Which form of backstop-ceiling refusal passed a tap over (#8555).
///
/// Three kinds rather than one, because they call for three different operator
/// responses: "the host is at its metered ceiling" is working as designed,
/// "this work is below the eligibility tier" is a policy verdict on *this*
/// dispatch, and "the ceiling state could not be read" is a host fault that
/// must be repaired before the metered tier can be used again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CeilingSkip {
    /// The configured per-host concurrency ceiling is already held.
    AtCapacity { live: u32, limit: u32 },
    /// The work is below the configured eligibility tier, so it never reaches
    /// the metered tap at all.
    Ineligible,
    /// The ceiling is configured but its live count could not be established
    /// (no resolvable lease directory, an unwritable store, an unreadable
    /// one). Fail closed: an unknown count must never read as "there is room".
    Unknown,
}

/// Why a higher-preference tap was passed over.
///
/// Recorded per skipped tap rather than collapsed into a count, because
/// "Claude was dry" and "Codex is not admitted for this role" call for
/// completely different operator responses, and a fleet that silently drifts
/// onto its metered backstop for the *second* reason is a misconfiguration,
/// not an outage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Capability admission refused this runtime for this role. Carries the
    /// named unmet capabilities when admission reported any (the
    /// `worktreeIsolation` case), and always the admission diagnostic.
    NotAdmitted { unmet: Vec<String>, detail: String },
    /// Admitted, but its credential source has nothing spawnable right now.
    /// `source` is the wire name of the pool that was read.
    Unavailable { source: String, detail: String },
    /// Admitted and spawnable, but the backstop-tier admission bound (#8555)
    /// passed it over. **This is a resource bound, never an approval gate**:
    /// nothing waits for a human, the walk simply continues to the next tap
    /// and fails closed if nothing below qualifies.
    Ceiling { skip: CeilingSkip, detail: String },
}

impl SkipReason {
    /// Stable wire token for the skip kind, for telemetry and log greps.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotAdmitted { .. } => "not-admitted",
            Self::Unavailable { .. } => "unavailable",
            Self::Ceiling { skip, .. } => match skip {
                CeilingSkip::AtCapacity { .. } => "ceiling-at-capacity",
                CeilingSkip::Ineligible => "ceiling-ineligible",
                CeilingSkip::Unknown => "ceiling-unknown",
            },
        }
    }

    /// The parenthetical detail rendered after [`Self::kind`]. Deliberately
    /// short: the full diagnostic already went to the log.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::NotAdmitted { unmet, detail } if unmet.is_empty() => detail.clone(),
            Self::NotAdmitted { unmet, .. } => unmet.join("+"),
            Self::Unavailable { source, detail } => format!("{source}: {detail}"),
            Self::Ceiling { detail, .. } => detail.clone(),
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.kind(), self.summary())
    }
}

/// A tap the walk passed over, with its position in the list preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedTap {
    /// 0-based position in the preference list.
    pub tier: usize,
    pub tap: Tap,
    pub reason: SkipReason,
}

impl fmt::Display for SkippedTap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.tap, self.reason)
    }
}

/// The tap the walk settled on, with whatever the admission question produced
/// for it (in production, the admitted `ResolvedRuntime`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChosenTap<T> {
    /// 0-based position in the preference list. `0` is the most-preferred
    /// entry — the "nothing fell through" case, and the one that must stay
    /// byte-identical to pre-#8436 behaviour for a healthy fleet.
    pub tier: usize,
    pub tap: Tap,
    pub admitted: T,
}

/// The outcome of one walk: the chosen tap (if any) and every tap above it,
/// with the reason each was passed over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution<T> {
    pub chosen: Option<ChosenTap<T>>,
    /// Taps passed over, in list order. Only taps *above* the chosen one — the
    /// walk short-circuits, so lower-preference taps are never evaluated and
    /// must not be reported as skipped.
    pub skipped: Vec<SkippedTap>,
    /// The list the walk ran over, for diagnostics.
    pub order: Vec<Tap>,
}

impl<T> Resolution<T> {
    /// `true` when the chosen tap is not the most-preferred one — i.e. the
    /// fall-through actually fired. The question "how much work is going to
    /// the backstop" reduces to counting these.
    #[must_use]
    pub fn fell_through(&self) -> bool {
        self.chosen.as_ref().is_some_and(|chosen| chosen.tier > 0)
    }

    /// Comma-joined `<tap>:<kind>(<detail>)` for every skipped tap, or the
    /// empty string when nothing was skipped.
    #[must_use]
    pub fn skip_summary(&self) -> String {
        self.skipped
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Comma-joined preference list, as configured.
    #[must_use]
    pub fn order_summary(&self) -> String {
        self.order
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The `# LOOM_RUNTIME_PREFERENCE …` log marker recording which tier was
    /// chosen and why every higher tier was skipped (#8436 acceptance
    /// criterion 5).
    ///
    /// **A sibling line, not an extension of `# LOOM_RUNTIME_RESOLVED`.**
    /// `sweep_registry::crash_signals::resolved_runtime_after` reads that
    /// marker by taking *the entire rest of the line* as the runtime name
    /// (`crash_signals.rs`), so appending ` tier=2` to it would silently make
    /// every crash-signal read report a runtime called `"opencode tier=2"`.
    /// The preference record therefore gets its own marker, which is purely
    /// additive: a reader that does not know about it is unaffected.
    #[must_use]
    pub fn marker_line(&self) -> String {
        let mut line = format!("{PREFERENCE_LOG_MARKER}order={}", self.order_summary());
        match &self.chosen {
            Some(chosen) => {
                line.push_str(&format!(" tier={} tap={}", chosen.tier, chosen.tap));
            }
            None => line.push_str(" tier=none tap=none"),
        }
        let skipped = self.skip_summary();
        if !skipped.is_empty() {
            line.push_str(&format!(" skipped={skipped}"));
        }
        line
    }

    /// Operator-facing text for the fail-closed case: every listed tap was
    /// skipped, so the caller holds/skips exactly as it does today.
    ///
    /// The whole point of naming each tap's reason here is that "the whole
    /// list is unavailable" is a materially different situation from "the
    /// Claude pool is dry": the first is a genuine host-level stall, the
    /// second is the case this feature exists to route around.
    #[must_use]
    pub fn exhausted_diagnostic(&self, role: &str) -> String {
        let mut out = format!(
            "No runtime in the preference list can serve role {role:?} right now \
             (fail-closed, exactly as before #8436).\n  preference order: {}",
            self.order_summary()
        );
        for skipped in &self.skipped {
            out.push_str(&format!("\n  {} -> {}", skipped.tap, skipped.reason));
        }
        out
    }
}

/// Leading tag of the preference log marker. See [`Resolution::marker_line`]
/// for why this is a sibling of `# LOOM_RUNTIME_RESOLVED` rather than extra
/// fields on it.
pub const PREFERENCE_LOG_MARKER: &str = "# LOOM_RUNTIME_PREFERENCE ";

/// Walk `taps` in order and take the first that is both admitted for the role
/// and has a spawnable credential right now.
///
/// `admit` is asked first for every tap (see the module doc for why);
/// `available` is asked only for taps that were admitted, and receives the
/// admission result so it can consult the admitted runtime's own manifest
/// without re-resolving it.
///
/// `available` also receives the tap's **tier** — its 0-based position in the
/// list — because one admission question is positional: the backstop-tier
/// concurrency ceiling (#8555) governs only the taps *below* the most-
/// preferred one, which is the same "did this fall through" predicate
/// [`Resolution::fell_through`] answers after the fact. It is asked inside
/// `available` rather than as a fourth walk stage so a tap that takes a
/// ceiling slot is, by construction, the tap the walk then returns: `available`
/// returning `Ok` short-circuits the loop, so no slot can be taken for a tap
/// that is subsequently passed over.
///
/// An empty `taps` yields `chosen: None` with no skips — the caller must treat
/// that identically to "every tap was skipped", i.e. fail closed. Callers are
/// expected never to produce it: an empty configured list is parsed as *unset*
/// and never reaches here.
pub fn resolve<T, A, V>(taps: &[Tap], mut admit: A, mut available: V) -> Resolution<T>
where
    A: FnMut(&Tap) -> Result<T, SkipReason>,
    V: FnMut(usize, &Tap, &T) -> Result<(), SkipReason>,
{
    let mut skipped = Vec::new();
    for (tier, tap) in taps.iter().enumerate() {
        let admitted = match admit(tap) {
            Ok(admitted) => admitted,
            Err(reason) => {
                skipped.push(SkippedTap {
                    tier,
                    tap: tap.clone(),
                    reason,
                });
                continue;
            }
        };
        if let Err(reason) = available(tier, tap, &admitted) {
            skipped.push(SkippedTap {
                tier,
                tap: tap.clone(),
                reason,
            });
            continue;
        }
        return Resolution {
            chosen: Some(ChosenTap {
                tier,
                tap: tap.clone(),
                admitted,
            }),
            skipped,
            order: taps.to_vec(),
        };
    }
    Resolution {
        chosen: None,
        skipped,
        order: taps.to_vec(),
    }
}
