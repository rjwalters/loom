//! Work-finder **build back-off** (WIP limit) on review + merge debt, with
//! hysteresis (issue #9410, Phase 2b of #9391), **per repo** since #10624.
//!
//! When a repo's PR debt is high, its finished work is piling up faster than
//! Judge / Doctor / Champion drain it, so admitting another issue build in that
//! repo only makes the pile bigger. While a repo's back-off is **engaged** the
//! work finder admits no new unstarred issue builds **in that repo**; other
//! repos are untouched, sweeps already in flight are untouched, and the freed
//! slots go to whatever else is ready.
//!
//! - **Per repo (#10624).** Each registered root has its own hysteresis state
//!   ([`BuildBackoffs`]), fed by that root's own debt
//!   ([`DemandLedger::repo_debt`]). One repo's backlog never holds another's
//!   builds — the #9410 host-wide total did, starving zero-debt repos.
//! - **Optional host ceiling.** `hostHigh` / `hostLow` (absent = off) apply
//!   the pre-#10624 host-total rule on top: while engaged, every repo's
//!   unstarred builds are held. For an operator who wants the fleet to spend
//!   more effort reviewing than building.
//! - **Input.** The #9392 demand ledger
//!   ([`crate::role_runner::demand::global`]), read once per root per tick
//!   with `autonomous.roleRunner.demandWidth.staleSecs`. `debt = review +
//!   changes + merge` over the axes that have a fresh entry. **No forge call**
//!   is made here: the ledger is filled only by listings the role runner
//!   already does.
//! - **Hysteresis.** Engage when `debt > high`; release when `debt < low`;
//!   between the two the previous state holds ([`BuildBackoff::observe`]).
//! - **Fail open.** An unobserved repo (or host, for the ceiling) — role
//!   runner off, demand width disabled, every entry older than `staleSecs` —
//!   never engages, and releases an engaged back-off. A partly observed one
//!   sums what it has, which can only err toward not engaging.
//! - **Bypass.** `loom:operator-priority` (starred) and verified red-main-fix
//!   candidates are admitted anyway (see `tick_multi_with_build_backoff`).
//!
//! Config: `autonomous.workFinder.buildBackoff.{enabled, high, low, hostHigh,
//! hostLow}`, re-read every tick from the daemon's primary workspace. No env
//! tier.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::ready_queue::{self, TickQueueRow};
use super::PriorityCandidate;
use crate::role_runner::demand::{self, DemandLedger, HostDebt};
use crate::types::QueueDisposition;

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
    /// The optional host-wide ceiling (`hostHigh` / `hostLow`, #10624);
    /// `None` (the default) is off.
    pub host: Option<HostCeiling>,
}

/// `buildBackoff.{hostHigh, hostLow}`: the pre-#10624 host-total rule, kept
/// as an opt-in ceiling on top of the per-repo limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCeiling {
    /// `hostHigh`: engage when the host total is strictly above it.
    pub high: usize,
    /// `hostLow`: release when the host total is strictly below it; `< high`.
    pub low: usize,
}

impl Default for BuildBackoffConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            high: DEFAULT_HIGH,
            low: DEFAULT_LOW,
            host: None,
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
    /// The `(hostHigh, hostLow)` rejected — crossed, or only one of the two
    /// set — leaving the host ceiling off (#10624).
    pub rejected_host_pair: Option<(Option<usize>, Option<usize>)>,
}

impl BuildBackoffConfig {
    /// Parse a `buildBackoff` object (anything else is all-defaults). Each key
    /// falls back on its own: a non-bool `enabled`, or a zero, negative or
    /// non-integer threshold drops only that key. Then a `low >= high` pair is
    /// rejected as a whole and both fall back to `40/25`. The host ceiling is
    /// on only when both `hostHigh` and `hostLow` resolve and
    /// `hostLow < hostHigh`; any other pair that names either key is rejected
    /// and leaves it off.
    #[must_use]
    pub fn parse(block: &Value) -> ParsedConfig {
        let d = Self::default();
        let Some(obj) = block.as_object() else {
            return ParsedConfig {
                config: d,
                rejected_pair: None,
                rejected_host_pair: None,
            };
        };
        let positive = |key: &str| {
            obj.get(key)
                .and_then(Value::as_u64)
                .filter(|&n| n > 0)
                .and_then(|n| usize::try_from(n).ok())
        };
        let enabled = obj
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(d.enabled);
        let (host, rejected_host_pair) = match (positive("hostHigh"), positive("hostLow")) {
            (Some(high), Some(low)) if low < high => (Some(HostCeiling { high, low }), None),
            (None, None) if !obj.contains_key("hostHigh") && !obj.contains_key("hostLow") => {
                (None, None)
            }
            pair => (None, Some(pair)),
        };
        let high = positive("high").unwrap_or(d.high);
        let low = positive("low").unwrap_or(d.low);
        let (config, rejected_pair) = if low >= high {
            (Self { enabled, host, ..d }, Some((high, low)))
        } else {
            (
                Self {
                    enabled,
                    high,
                    low,
                    host,
                },
                None,
            )
        };
        ParsedConfig {
            config,
            rejected_pair,
            rejected_host_pair,
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
/// crossed final pair falls back to the default thresholds (40/25), keeping
/// `enabled` and the host ceiling (which the overlay does not touch) — the
/// same soft contract [`BuildBackoffConfig::parse`] applies to a legacy-only pair;
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
            config: BuildBackoffConfig {
                enabled: config.enabled,
                host: config.host,
                ..BuildBackoffConfig::default()
            },
            rejected_pair: Some((config.high, config.low)),
            ..parsed
        };
    }
    ParsedConfig {
        config,
        rejected_pair: None,
        ..parsed
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

/// The debt a host (or, since #10624, one repo's) aggregate carries, or `None`
/// when every axis is unobserved.
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
    /// `enabled: false` (or, for the host ceiling, `hostHigh`/`hostLow` unset).
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

/// Which hysteresis state an [`Edge`] belongs to, with its thresholds (#10624).
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// One repo's per-repo limit (`high` / `low`).
    Repo(&'a Path, &'a BuildBackoffConfig),
    /// The optional host-wide ceiling (`hostHigh` / `hostLow`).
    Host(HostCeiling),
}

impl Scope<'_> {
    /// `(who, debt qualifier, high key=value, low key=value, whose builds)`.
    fn words(&self) -> (String, &'static str, String, String, &'static str) {
        match self {
            Self::Repo(root, cfg) => (
                format!("repo {}", root.display()),
                "",
                format!("high={}", cfg.high),
                format!("low={}", cfg.low),
                "in this repo",
            ),
            Self::Host(h) => (
                "the host ceiling".to_string(),
                "host-wide ",
                format!("hostHigh={}", h.high),
                format!("hostLow={}", h.low),
                "in every repo",
            ),
        }
    }
}

impl Edge {
    /// The INFO line for this edge, naming the repo (or the host ceiling).
    #[must_use]
    pub fn log_line(&self, scope: &Scope<'_>) -> String {
        let (who, wide, high, low, whose) = scope.words();
        let head = format!("work_finder: build back-off for {who}");
        match self {
            Self::Engaged(d) => format!(
                "{head} ENGAGED — {wide}review+changes+merge debt {} ({}) > {high}; new issue \
                 builds {whose} held until < {low} (#9410, #10624)",
                d.total,
                d.split()
            ),
            Self::Released(ReleaseReason::BelowLow, Some(d)) => format!(
                "{head} RELEASED — {wide}review+changes+merge debt {} ({}) < {low} ({high}); \
                 new issue builds {whose} admitted again (#9410, #10624)",
                d.total,
                d.split()
            ),
            Self::Released(ReleaseReason::Disabled, _) => format!(
                "{head} RELEASED — disabled by config ({high}, {low}); new issue builds {whose} \
                 admitted again (#9410, #10624)"
            ),
            Self::Released(..) => format!(
                "{head} RELEASED — debt unobserved — failing open ({high}, {low}); new issue \
                 builds {whose} admitted again (#9410, #10624)"
            ),
        }
    }
}

/// One hysteresis state: a repo's, or the host ceiling's.
#[derive(Debug, Default)]
pub struct BuildBackoff {
    engaged: bool,
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
        self.observe_band(debt, cfg.enabled, cfg.high, cfg.low)
    }

    /// [`Self::observe`] against an explicit `(enabled, high, low)` band — the
    /// host ceiling's is `(enabled && host set, hostHigh, hostLow)`.
    pub fn observe_band(
        &mut self,
        debt: Option<DebtReading>,
        enabled: bool,
        high: usize,
        low: usize,
    ) -> Option<Edge> {
        let was = self.engaged;
        let next = match (enabled, debt) {
            (false, _) | (true, None) => false,
            (true, Some(d)) if was => d.total >= low,
            (true, Some(d)) => d.total > high,
        };
        self.engaged = next;
        match (was, next) {
            (false, true) => debt.map(Edge::Engaged),
            (true, false) => {
                let reason = match (enabled, debt) {
                    (false, _) => ReleaseReason::Disabled,
                    (true, None) => ReleaseReason::Unobserved,
                    (true, Some(_)) => ReleaseReason::BelowLow,
                };
                Some(Edge::Released(reason, debt))
            }
            _ => None,
        }
    }
}

/// One tick's verdict, parallel to the roots [`BuildBackoffs::step`] was given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Holds {
    /// Per workspace: held by its own repo's limit **or** the host ceiling.
    pub per_workspace: Vec<bool>,
    /// Repos held by their own limit (whatever the host ceiling says).
    pub repos_engaged: usize,
    /// The host ceiling is engaged.
    pub host: bool,
}

impl Holds {
    /// Whether any workspace is held this tick.
    #[must_use]
    pub fn any(&self) -> bool {
        self.per_workspace.contains(&true)
    }
}

impl std::fmt::Display for Holds {
    /// `2/14 repos`, plus ` + host ceiling` while it is engaged.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{} repos", self.repos_engaged, self.per_workspace.len())?;
        if self.host {
            write!(f, " + host ceiling")?;
        }
        Ok(())
    }
}

/// The per-repo hysteresis states plus the host ceiling's, held by the
/// work-finder loop across ticks (#10624).
#[derive(Debug, Default)]
pub struct BuildBackoffs {
    repos: HashMap<PathBuf, BuildBackoff>,
    host: BuildBackoff,
    /// The last host ceiling configured, named by a release after it is unset.
    last_host: Option<HostCeiling>,
    /// The last rejected `(high, low)` / host pair warned about, so a bad
    /// config is warned once per distinct value rather than every tick.
    warned: Option<(usize, usize)>,
    warned_host: Option<(Option<usize>, Option<usize>)>,
    /// The repos named by the last deferral breakdown logged at INFO.
    last_deferred: Vec<String>,
}

impl BuildBackoffs {
    /// Whether `root`'s own limit is engaged (the host ceiling aside).
    #[must_use]
    pub fn repo_held(&self, root: &Path) -> bool {
        self.repos.get(root).is_some_and(BuildBackoff::held)
    }

    /// One production tick: read the config from `primary` (warning once per
    /// distinct bad pair), drop the state of roots no longer registered, read
    /// each root's own debt — and the host total, when the ceiling is set —
    /// from `ledger` with the demand ledger's own `staleSecs`, advance, log any
    /// edge at INFO, and return the verdict parallel to `roots`.
    pub fn step(&mut self, primary: &Path, roots: &[PathBuf], ledger: &DemandLedger) -> Holds {
        let parsed = BuildBackoffConfig::read(primary);
        self.warn(&parsed);
        let cfg = parsed.config;
        // A root that left the registry stops holding anything.
        self.repos.retain(|root, b| {
            let keep = roots.contains(root);
            if !keep && b.held() {
                log::info!(
                    "work_finder: build back-off for repo {} RELEASED — repo left the \
                     registry (#10624)",
                    root.display()
                );
            }
            keep
        });
        // Disabled: no ledger read at all, exactly the pre-#9410 admission.
        let stale = cfg
            .enabled
            .then(|| demand::read_demand_config(primary).stale());
        let mut per_workspace = Vec::with_capacity(roots.len());
        for root in roots {
            let debt = stale.and_then(|s| debt_from(&ledger.repo_debt(root, s)));
            let state = self.repos.entry(root.clone()).or_default();
            if let Some(edge) = state.observe(debt, &cfg) {
                log::info!("{}", edge.log_line(&Scope::Repo(root, &cfg)));
            }
            per_workspace.push(state.held());
        }
        let repos_engaged = per_workspace.iter().filter(|&&h| h).count();
        let ceiling = cfg.host.filter(|_| cfg.enabled);
        let host_debt = stale.filter(|_| ceiling.is_some());
        let host_debt = host_debt.and_then(|s| debt_from(&ledger.host_debt(s)));
        // An unset ceiling still names its last thresholds when it releases.
        self.last_host = ceiling.or(self.last_host);
        let band = self.last_host.unwrap_or(HostCeiling { high: 0, low: 0 });
        if let Some(edge) =
            self.host
                .observe_band(host_debt, ceiling.is_some(), band.high, band.low)
        {
            log::info!("{}", edge.log_line(&Scope::Host(band)));
        }
        let host = self.host.held();
        if host {
            per_workspace.iter_mut().for_each(|h| *h = true);
        }
        Holds {
            per_workspace,
            repos_engaged,
            host,
        }
    }

    fn warn(&mut self, parsed: &ParsedConfig) {
        if parsed.rejected_pair != self.warned {
            if let Some((high, low)) = parsed.rejected_pair {
                log::warn!(
                    "work_finder: autonomous.workFinder.buildBackoff low={low} >= high={high} \
                     — rejected, using the defaults high={DEFAULT_HIGH} low={DEFAULT_LOW} (#9410)"
                );
            }
            self.warned = parsed.rejected_pair;
        }
        if parsed.rejected_host_pair != self.warned_host {
            if let Some((high, low)) = parsed.rejected_host_pair {
                let show = |v: Option<usize>| v.map_or_else(|| "unset".into(), |n| n.to_string());
                log::warn!(
                    "work_finder: autonomous.workFinder.buildBackoff hostHigh={} hostLow={} \
                     — rejected (both must be positive integers with hostLow < hostHigh), \
                     host ceiling off (#10624)",
                    show(high),
                    show(low)
                );
            }
            self.warned_host = parsed.rejected_host_pair;
        }
    }

    /// Log which repos this tick's build back-off deferrals came from: INFO
    /// when the set of repos changes, DEBUG otherwise, so a starved repo is
    /// visible without a line per tick (#10624).
    pub fn log_deferred(&mut self, queue: &[TickQueueRow], roots: &[PathBuf]) {
        let counts = deferred_by_repo(queue, roots);
        let names: Vec<String> = counts.iter().map(|(n, _)| n.clone()).collect();
        let total: usize = counts.iter().map(|(_, c)| c).sum();
        let list: Vec<String> = counts.iter().map(|(n, c)| format!("{n}={c}")).collect();
        let line = format!(
            "work_finder: build back-off deferred {total} issue(s) in {} repo(s) this tick: \
             [{}] (#10624)",
            counts.len(),
            list.join(", ")
        );
        if names == self.last_deferred {
            log::debug!("{line}");
        } else {
            log::info!("{line}");
            self.last_deferred = names;
        }
    }
}

/// Whether pass 2 defers `cand`: its workspace is held (by its own repo's
/// debt or the host ceiling) and it is neither starred nor a verified
/// red-main fix — the two bypasses #9410 defined. `held` is parallel to the
/// tick's workspaces; an index past its end is not held.
#[must_use]
pub fn defers(held: &[bool], cand: &PriorityCandidate) -> bool {
    held.get(cand.workspace_idx) == Some(&true) && !(cand.operator_priority || cand.main_red_fix)
}

/// This tick's build back-off deferrals per repo, most-deferred first (ties
/// by name), naming each repo as the ready queue does.
#[must_use]
pub fn deferred_by_repo(queue: &[TickQueueRow], roots: &[PathBuf]) -> Vec<(String, usize)> {
    let mut by_idx: HashMap<usize, usize> = HashMap::new();
    for row in queue {
        if row.disposition == Some(QueueDisposition::DeferredBuildBackoff) {
            *by_idx.entry(row.key.workspace_idx).or_default() += 1;
        }
    }
    let mut out: Vec<(String, usize)> = by_idx
        .into_iter()
        .map(|(idx, n)| (ready_queue::repo_names(&[idx], roots).remove(0), n))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "build_backoff_tests.rs"]
mod tests;
