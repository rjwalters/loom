//! Context-aware failure reporting (Issue #8997 gaps b, c, d).
//!
//! [`report_failure`] is the one path every failure hook takes:
//!
//! 1. Classify. Non-rate-limit text (auth, network, spawn, timeout) and a
//!    disabled breaker return `None` — existing treatment untouched.
//! 2. **Storm guard**: an already-cooling breaker returns `None` before any
//!    probe, so repeats during a cooldown never re-probe.
//! 3. **Trip first.** The cooldown starts *before* any probe, from the failing
//!    response's own headers when the caller captured them (authoritative,
//!    no probe at all) or the configured fallback otherwise. Every concurrent
//!    or re-entrant caller — including anything the probe itself might
//!    trigger — now sees a suppressed breaker, so a notification can never
//!    recurse into another probe, and exactly one caller (the one whose trip
//!    won) goes on to probe.
//! 4. Refine with a probe run under the **failing call's** context (working
//!    directory → owner `GH_CONFIG_DIR`, injected `gh` program, explicit
//!    credential dir), so the reading belongs to the credential that was
//!    refused rather than the host's ambient one. The reading is filtered by
//!    [`super::evidence::from_probe`]. [`ProbeMode::Background`] runs this on
//!    a detached thread so an async caller (safehouse's event loop) or the
//!    dispatch path never waits on a bounded-but-slow `gh`.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use chrono::{DateTime, Utc};

use super::evidence::{self, ResetEvidence};
use super::{indicates_rate_limit, BudgetSnapshot, SharedRateLimitBreaker, Transition};

/// What the breaker knows about the call that failed. Every field is
/// optional; [`FailureContext::default`] is the pre-#8997 ambient probe.
/// Holds no secrets — only paths and the program name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureContext {
    /// Working directory of the failing call. Keys the per-owner
    /// `GH_CONFIG_DIR` lookup exactly as the call itself did.
    pub root: Option<PathBuf>,
    /// The `gh` program the failing call ran (an injected stub or launcher).
    pub program: Option<String>,
    /// An explicit `GH_CONFIG_DIR` the failing call ran under.
    pub config_dir: Option<PathBuf>,
    /// Raw response head (`gh api -i` style) from the failing call, when the
    /// caller captured one — the authoritative reset source.
    pub response_head: Option<String>,
}

impl FailureContext {
    /// Context for a call that ran in `root` with `program`.
    #[must_use]
    pub fn for_root(root: impl Into<PathBuf>, program: impl Into<String>) -> Self {
        Self {
            root: Some(root.into()),
            program: Some(program.into()),
            ..Self::default()
        }
    }
}

/// Reads the live budget for a failure context. The production
/// implementation is [`ForgeProbe`]; tests inject counters/fixtures.
pub trait BudgetProbe: Send + Sync {
    fn probe(&self, ctx: &FailureContext, now: DateTime<Utc>) -> Option<BudgetSnapshot>;
}

/// The real probe: `gh api rate_limit` + the GraphQL `rateLimit` query
/// through the `gh` facade, under the failure's context.
#[derive(Debug, Clone, Copy, Default)]
pub struct ForgeProbe;

impl BudgetProbe for ForgeProbe {
    fn probe(&self, ctx: &FailureContext, now: DateTime<Utc>) -> Option<BudgetSnapshot> {
        super::forge::probe_budget_ctx(ctx, now)
    }
}

/// Whether the refine probe runs in the reporting call or on its own thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeMode {
    /// Probe in the caller (sync polling loops that already block on `gh`).
    Inline,
    /// Probe on a detached thread; the trip itself is immediate.
    Background,
}

/// A breaker plus the probe and mode a call site reports through. Injected
/// by tests; production call sites use [`BreakerHandle::global`].
#[derive(Clone)]
pub struct BreakerHandle {
    pub breaker: Arc<SharedRateLimitBreaker>,
    pub probe: Arc<dyn BudgetProbe>,
    pub mode: ProbeMode,
}

impl fmt::Debug for BreakerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BreakerHandle")
            .field("breaker", &self.breaker)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl BreakerHandle {
    /// The process-global breaker with the real probe in `mode`; `None` when
    /// no breaker is registered (unchanged behavior).
    #[must_use]
    pub fn global(mode: ProbeMode) -> Option<Self> {
        super::global().map(|breaker| Self {
            breaker,
            probe: Arc::new(ForgeProbe),
            mode,
        })
    }

    /// Whether the breaker is suppressing forge calls right now.
    #[must_use]
    pub fn is_suppressed(&self) -> bool {
        self.breaker.is_suppressed(Utc::now())
    }

    /// [`report_failure`] through this handle.
    pub fn report(&self, error_text: &str, source: &str, ctx: FailureContext) -> Option<Reported> {
        report_failure(self, error_text, source, ctx, Utc::now())
    }
}

/// A trip this report caused.
#[derive(Debug)]
pub struct Reported {
    /// The trip (for [`Inline`](ProbeMode::Inline), with the refined
    /// release time).
    pub transition: Transition,
    /// The background refine thread, when one was started.
    pub refine: Option<JoinHandle<()>>,
}

/// Classify, trip and (when needed) refine — see the module docs.
pub fn report_failure(
    handle: &BreakerHandle,
    error_text: &str,
    source: &str,
    ctx: FailureContext,
    now: DateTime<Utc>,
) -> Option<Reported> {
    let breaker = &handle.breaker;
    if !breaker.config().enabled || !indicates_rate_limit(error_text) {
        return None;
    }
    if breaker.is_suppressed(now) {
        return None;
    }
    let header_evidence = ctx
        .response_head
        .as_deref()
        .and_then(|head| evidence::from_failure_headers(head, now));
    let provisional = header_evidence.clone().unwrap_or(ResetEvidence::Fallback {
        reason: "awaiting contextual budget probe",
    });
    let transition = breaker.trip(source, &provisional, now)?;
    if header_evidence.is_some() {
        log::warn!(
            "rate_limit_breaker: {} — {} [{}]",
            transition.kind.as_str(),
            transition.reason,
            provisional.describe()
        );
        // Authoritative headers, no probe: the trip span carries no
        // attribution (#10022).
        super::export_trip(source, now, transition.until, None, || None);
        return Some(Reported {
            transition,
            refine: None,
        });
    }
    match handle.mode {
        ProbeMode::Inline => {
            let until = refine(
                breaker,
                handle.probe.as_ref(),
                error_text,
                source,
                transition.until,
                &ctx,
                now,
            );
            let transition = breaker.retitle(transition, until);
            log::warn!("rate_limit_breaker: {} — {}", transition.kind.as_str(), transition.reason);
            Some(Reported {
                transition,
                refine: None,
            })
        }
        ProbeMode::Background => {
            log::warn!(
                "rate_limit_breaker: {} — {} (provisional; refining from a contextual probe)",
                transition.kind.as_str(),
                transition.reason
            );
            let breaker = Arc::clone(breaker);
            let probe = Arc::clone(&handle.probe);
            let text = error_text.to_owned();
            let source = source.to_owned();
            let provisional_until = transition.until;
            let spawned = std::thread::Builder::new()
                .name("rate-limit-probe".to_owned())
                .spawn(move || {
                    refine(
                        &breaker,
                        probe.as_ref(),
                        &text,
                        &source,
                        provisional_until,
                        &ctx,
                        now,
                    );
                });
            Some(Reported {
                transition,
                refine: spawned.ok(),
            })
        }
    }
}

/// Probe under `ctx`, select evidence, and narrow/extend the trip started at
/// `tripped_at`. Returns the resulting release time. Exports the trip span
/// with the probe's attribution once the release time is known (#10022) —
/// the trip's one export, since a re-trip while cooling never reaches here.
fn refine(
    breaker: &SharedRateLimitBreaker,
    probe: &dyn BudgetProbe,
    error_text: &str,
    source: &str,
    provisional_until: Option<DateTime<Utc>>,
    ctx: &FailureContext,
    tripped_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let budget = probe.probe(ctx, Utc::now().max(tripped_at));
    let (ev, reading_trusted) = evidence::from_probe(error_text, budget.as_ref(), tripped_at);
    let until = breaker.refine(tripped_at, &ev, budget.filter(|_| reading_trusted));
    if let Some(until) = until {
        log::info!("rate_limit_breaker: cooldown until {until} from {}", ev.describe());
    }
    super::export_trip(
        source,
        tripped_at,
        until.or(provisional_until),
        budget.as_ref().map(|b| (b, reading_trusted)),
        || crate::forge_call_stats::consumed_in_window(tripped_at),
    );
    until
}
