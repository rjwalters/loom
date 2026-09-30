//! Work-finder **build back-off** (WIP limit) on review + merge debt, with
//! hysteresis (issue #9410, Phase 2b of #9391).
//!
//! When the host's PR debt is high, finished work is piling up faster than
//! Judge / Doctor / Champion drain it, so admitting another issue build only
//! makes the pile bigger. While the back-off is **engaged** the work finder
//! admits no new unstarred issue builds; sweeps already in flight are
//! untouched, and the freed host resources (token pool, load) go to the role
//! runner's PR roles, which #9392 already sizes to the same debt.
//!
//! - **Input.** The #9392 demand ledger
//!   ([`crate::role_runner::demand::global`]), read once per tick with
//!   `autonomous.roleRunner.demandWidth.staleSecs`. `debt = review + changes +
//!   merge` over the axes that have a fresh entry. **No forge call** is made
//!   here: the ledger is filled only by listings the role runner already does.
//! - **Hysteresis.** Engage when `debt > high`; release when `debt < low`;
//!   between the two the previous state holds ([`BuildBackoff::observe`]).
//! - **Fail open.** A fully unobserved ledger (role runner off, demand width
//!   disabled, every entry older than `staleSecs`) never engages, and releases
//!   an engaged back-off. A partly observed ledger sums what it has, which can
//!   only err toward not engaging.
//! - **Bypass.** `loom:operator-priority` (starred) and verified red-main-fix
//!   candidates are admitted anyway (see `tick_multi_with_build_backoff`).
//!
//! Config: `autonomous.workFinder.buildBackoff.{enabled, high, low}`, re-read
//! every tick from the daemon's primary workspace. No env tier.

use std::path::Path;

use serde_json::Value;

use crate::role_runner::demand::{self, DemandLedger, HostDebt};

/// Default `high` (`W`): engage when the debt is strictly above it.
pub const DEFAULT_HIGH: usize = 40;
/// Default `low` (`W_low`): release when the debt is strictly below it.
pub const DEFAULT_LOW: usize = 25;

/// `autonomous.workFinder.buildBackoff`, resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildBackoffConfig {
    /// `enabled` — `false` is exactly the pre-#9410 admission.
    pub enabled: bool,
    /// `high` (`W`).
    pub high: usize,
    /// `low` (`W_low`), always `< high` once parsed.
    pub low: usize,
}

impl Default for BuildBackoffConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            high: DEFAULT_HIGH,
            low: DEFAULT_LOW,
        }
    }
}

/// A parsed config plus the `(high, low)` pair it rejected, if any: a
/// `low >= high` pair falls back to the defaults and is WARN-worthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedConfig {
    /// The config to run with.
    pub config: BuildBackoffConfig,
    /// The per-key-resolved `(high, low)` rejected because `low >= high`.
    pub rejected_pair: Option<(usize, usize)>,
}

impl BuildBackoffConfig {
    /// Parse a `buildBackoff` object (anything else is all-defaults). Each key
    /// falls back on its own: a non-bool `enabled`, or a zero, negative or
    /// non-integer threshold drops only that key. Then a `low >= high` pair is
    /// rejected as a whole and both fall back to `40/25`.
    #[must_use]
    pub fn parse(block: &Value) -> ParsedConfig {
        let d = Self::default();
        let Some(obj) = block.as_object() else {
            return ParsedConfig {
                config: d,
                rejected_pair: None,
            };
        };
        let positive = |key: &str, default: usize| {
            obj.get(key)
                .and_then(Value::as_u64)
                .filter(|&n| n > 0)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(default)
        };
        let enabled = obj
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(d.enabled);
        let (high, low) = (positive("high", d.high), positive("low", d.low));
        if low >= high {
            return ParsedConfig {
                config: Self { enabled, ..d },
                rejected_pair: Some((high, low)),
            };
        }
        ParsedConfig {
            config: Self { enabled, high, low },
            rejected_pair: None,
        }
    }

    /// Read `root`'s effective `autonomous.workFinder.buildBackoff`, with the
    /// `hyperparameters.rework` layer (Issue #9683) overlaid per field.
    #[must_use]
    pub fn read(root: &Path) -> ParsedConfig {
        let effective = crate::config_resolver::resolve_effective_config(root);
        let block = crate::config_resolver::get_path(&effective, "autonomous")
            .and_then(|a| a.get("workFinder"))
            .and_then(|w| w.get("buildBackoff"))
            .cloned()
            .unwrap_or(Value::Null);
        apply_hyperparams(&effective, Self::parse(&block))
    }
}

/// Overlay `hyperparameters.rework.{buildBackoffHigh,buildBackoffLow}`
/// (Issue #9683) onto the legacy-parsed pair, then re-run the crossed-pair
/// rejection against the **final** pair: a layer-supplied `high` must still
/// sit above whatever `low` resolved to, whichever tier each came from. A
/// crossed final pair falls back to the defaults (40/25) — the same soft
/// contract [`BuildBackoffConfig::parse`] applies to a legacy-only pair;
/// `hyperparams::startup_init` is the strict gate that names a crossed pair
/// supplied through the `hyperparameters` surface at daemon startup. An
/// overlay that lands clears any legacy rejected-pair note — it no longer
/// describes the pair that will run.
fn apply_hyperparams(effective: &Value, parsed: ParsedConfig) -> ParsedConfig {
    let layer = crate::hyperparams::overlay_from_effective(effective);
    let Some(rework) = layer.get("rework").filter(|r| !r.is_null()) else {
        return parsed;
    };
    let overlay_high = rework
        .get("buildBackoffHigh")
        .filter(|v| !v.is_null())
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok());
    let overlay_low = rework
        .get("buildBackoffLow")
        .filter(|v| !v.is_null())
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok());
    if overlay_high.is_none() && overlay_low.is_none() {
        return parsed;
    }
    let mut config = parsed.config;
    if let Some(high) = overlay_high {
        config.high = high;
    }
    if let Some(low) = overlay_low {
        config.low = low;
    }
    if config.low >= config.high {
        return ParsedConfig {
            config: BuildBackoffConfig::default(),
            rejected_pair: Some((config.high, config.low)),
        };
    }
    ParsedConfig {
        config,
        rejected_pair: None,
    }
}

/// The debt one tick saw: the total over the observed axes plus the split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebtReading {
    /// `review + changes + merge` over the axes that are `Some`.
    pub total: usize,
    /// `loom:review-requested` (`None`: unobserved).
    pub review: Option<usize>,
    /// `loom:changes-requested` (`None`: unobserved).
    pub changes: Option<usize>,
    /// `loom:pr`, excluding operator-held PRs (`None`: unobserved).
    pub merge: Option<usize>,
}

impl DebtReading {
    /// `review=28 changes=0 merge=59`, `?` for an unobserved axis.
    #[must_use]
    pub fn split(&self) -> String {
        let f = |v: Option<usize>| v.map_or_else(|| "?".to_string(), |n| n.to_string());
        format!("review={} changes={} merge={}", f(self.review), f(self.changes), f(self.merge))
    }
}

/// The debt a host aggregate carries, or `None` when every axis is unobserved.
#[must_use]
pub fn debt_from(host: &HostDebt) -> Option<DebtReading> {
    let (review, changes, merge) = (
        host.review.map(|a| a.total),
        host.changes.map(|a| a.total),
        host.merge.map(|a| a.total),
    );
    if review.is_none() && changes.is_none() && merge.is_none() {
        return None;
    }
    Some(DebtReading {
        total: review.unwrap_or(0) + changes.unwrap_or(0) + merge.unwrap_or(0),
        review,
        changes,
        merge,
    })
}

/// Why an engaged back-off released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseReason {
    /// The debt fell below `low`.
    BelowLow,
    /// The ledger went unobserved (fail open).
    Unobserved,
    /// `enabled: false`.
    Disabled,
}

/// A state change — the only thing that is logged at INFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// `debt > high` from released.
    Engaged(DebtReading),
    /// Released, with the reading that released it (`None`: unobserved or
    /// disabled without a reading).
    Released(ReleaseReason, Option<DebtReading>),
}

impl Edge {
    /// The INFO line for this edge.
    #[must_use]
    pub fn log_line(&self, cfg: &BuildBackoffConfig) -> String {
        let (high, low) = (cfg.high, cfg.low);
        match self {
            Self::Engaged(d) => format!(
                "work_finder: build back-off ENGAGED — review+changes+merge debt {} ({}) > \
                 high={high}; new issue builds held until < low={low} (#9410)",
                d.total,
                d.split()
            ),
            Self::Released(ReleaseReason::BelowLow, Some(d)) => format!(
                "work_finder: build back-off RELEASED — review+changes+merge debt {} ({}) < \
                 low={low} (high={high}); new issue builds admitted again (#9410)",
                d.total,
                d.split()
            ),
            Self::Released(ReleaseReason::Disabled, _) => format!(
                "work_finder: build back-off RELEASED — buildBackoff.enabled is false \
                 (high={high}, low={low}); new issue builds admitted again (#9410)"
            ),
            Self::Released(..) => format!(
                "work_finder: build back-off RELEASED — debt unobserved — failing open \
                 (high={high}, low={low}); new issue builds admitted again (#9410)"
            ),
        }
    }
}

/// The hysteresis state, held by the work-finder loop across ticks.
#[derive(Debug, Default)]
pub struct BuildBackoff {
    engaged: bool,
    /// The last rejected `(high, low)` pair warned about, so a bad config is
    /// warned once per distinct value rather than every tick.
    warned: Option<(usize, usize)>,
}

impl BuildBackoff {
    /// Whether new unstarred issue builds are held.
    #[must_use]
    pub fn held(&self) -> bool {
        self.engaged
    }

    /// Advance the state machine one tick. Pure: returns the edge, if any.
    ///
    /// | current | input | next |
    /// |---|---|---|
    /// | any | `enabled: false` | released |
    /// | any | unobserved (`None`) | released (fail open) |
    /// | released | `debt > high` | **engaged** |
    /// | engaged | `debt < low` | **released** |
    /// | otherwise | | unchanged |
    pub fn observe(&mut self, debt: Option<DebtReading>, cfg: &BuildBackoffConfig) -> Option<Edge> {
        let was = self.engaged;
        let next = match (cfg.enabled, debt) {
            (false, _) | (true, None) => false,
            (true, Some(d)) if was => d.total >= cfg.low,
            (true, Some(d)) => d.total > cfg.high,
        };
        self.engaged = next;
        match (was, next) {
            (false, true) => debt.map(Edge::Engaged),
            (true, false) => {
                let reason = match (cfg.enabled, debt) {
                    (false, _) => ReleaseReason::Disabled,
                    (true, None) => ReleaseReason::Unobserved,
                    (true, Some(_)) => ReleaseReason::BelowLow,
                };
                Some(Edge::Released(reason, debt))
            }
            _ => None,
        }
    }

    /// One production tick: read the config (warning once per distinct bad
    /// pair), read the host debt from `ledger` with the demand ledger's own
    /// `staleSecs`, advance, log any edge at INFO, and return [`Self::held`].
    pub fn step(&mut self, root: &Path, ledger: &DemandLedger) -> bool {
        let parsed = BuildBackoffConfig::read(root);
        if parsed.rejected_pair != self.warned {
            if let Some((high, low)) = parsed.rejected_pair {
                log::warn!(
                    "work_finder: autonomous.workFinder.buildBackoff low={low} >= high={high} \
                     — rejected, using the defaults high={DEFAULT_HIGH} low={DEFAULT_LOW} (#9410)"
                );
            }
            self.warned = parsed.rejected_pair;
        }
        let cfg = parsed.config;
        // Disabled: no ledger read at all, exactly the pre-#9410 admission.
        let debt = if cfg.enabled {
            debt_from(&ledger.host_debt(demand::read_demand_config(root).stale()))
        } else {
            None
        };
        if let Some(edge) = self.observe(debt, &cfg) {
            log::info!("{}", edge.log_line(&cfg));
        }
        self.held()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "build_backoff_tests.rs"]
mod tests;
