//! Side-listing filters and the host's cap terms (#9244, re-scoped by
//! #11508).
//!
//! Priority (`loom:important` / `loom:very-important`, #11103) changes
//! dispatch **order only, never eligibility** (operator ruling on #11508):
//! there is no priority side listing, so a priority issue is dispatched only
//! through the normal `loom:issue` path, and no priority label exempts a
//! candidate from any gate (build back-off, file overlap, the caps).
//!
//! What remains here:
//!
//! - [`excluded_from_side_listing`]: the filter the unpromoted red-main-fix
//!   listing (#10118) applies to rows from outside `loom:issue`.
//! - [`CapTerms`]: the configured vs headroom terms of the concurrency cap.
//! - [`OverflowSlot`]: **inert** since #11508. Nothing is admitted over the
//!   cap any more; the slot only reports whether an overflow sweep started
//!   by an older daemon is still live (`plan.slots.overflow_free`).
//! - [`warn_retired_config`]: the one startup warning for a host config
//!   that still carries the removed `autonomous.operatorPriority` block
//!   (#11104: the star-liveness subsystem that read it is gone).

use std::path::Path;

use super::{WorkDispatcher, WorkItem, BUILDING_LABEL};

/// The in-progress Curator claim. A side-listed row carrying it is being
/// curated right now, so it is left alone this tick.
pub const CURATING_LABEL: &str = "loom:curating";

/// Labels that route an issue through Champion rather than the Builder: an
/// epic, and the three proposal kinds. A side-listed row carrying one is not
/// build work, so it is skipped. Derived from the label registry's
/// `champion_path` property (#10013).
pub static CHAMPION_PATH_LABELS: crate::label_registry::LabelSet =
    crate::label_registry::LabelSet::new(|| crate::label_registry::embedded_set("champion_path"));

/// Whether a row from a listing other than `loom:issue` (the unpromoted
/// red-main-fix ones, #10118) is left out of the candidates: already claimed
/// ([`BUILDING_LABEL`]), being curated ([`CURATING_LABEL`]), or on the
/// Champion path ([`CHAMPION_PATH_LABELS`]).
#[must_use]
pub fn excluded_from_side_listing(item: &WorkItem) -> bool {
    item.labels.iter().any(|l| {
        l == BUILDING_LABEL || l == CURATING_LABEL || CHAMPION_PATH_LABELS.contains(&l.as_str())
    })
}

/// The two kinds of term in this tick's concurrency cap (#9244 §5, #5270).
///
/// The dynamic cap is `min(disk headroom, configured)` (RAM term removed in
/// #12330): the configured term is the operator's queue limit, the headroom
/// term says the host cannot hold another worktree. Both are published on the tick summary
/// (Issue #10214) so an alert can say which term holds the cap down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapTerms {
    /// The configured `maxConcurrent` (after env / config resolution).
    pub configured: usize,
    /// Disk headroom: how many sweeps the host can hold.
    pub headroom: usize,
}

impl CapTerms {
    /// The terms from this tick's configured cap and its disk probe.
    #[must_use]
    pub fn new(configured: usize, disk_headroom: usize) -> Self {
        Self {
            configured,
            headroom: disk_headroom,
        }
    }

    /// [`Self::new`] for a production tick: also records the raw terms for
    /// the published tick summary (Issue #10214), so the liveness pass and
    /// the fleet alert can say which term holds the cap down.
    #[must_use]
    pub fn observed(configured: usize, disk_headroom: usize) -> Self {
        super::tick_summary::record_cap(crate::types::CapView::from_terms(
            configured,
            disk_headroom,
        ));
        Self::new(configured, disk_headroom)
    }

    /// The effective cap every non-overflow admission is held to.
    #[must_use]
    pub fn effective(&self) -> usize {
        self.configured.min(self.headroom)
    }
}

impl From<usize> for CapTerms {
    /// A cap with no known resource headroom: every term is configured. The
    /// plain `tick*` entry points use this; the production loops build the
    /// real terms from their disk and RAM probes.
    fn from(configured: usize) -> Self {
        Self {
            configured,
            headroom: usize::MAX,
        }
    }
}

/// The per-host overflow slot (#9244 §5), **inert since #11508**.
///
/// It used to let one starred candidate run as this host's single over-limit
/// sweep. Priority no longer changes admission (operator ruling on #11508),
/// so nothing takes the slot; it is kept only so the tick still reports
/// whether an overflow sweep an older daemon started is live
/// ([`Self::is_free`], `plan.slots.overflow_free`) and so the effective cap
/// comes from one place ([`Self::open`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverflowSlot {
    taken: bool,
}

impl OverflowSlot {
    /// The tick's effective cap plus its slot, for the single-workspace tick;
    /// `already_live` is whether an overflow sweep is still running.
    #[must_use]
    pub fn open(already_live: bool, terms: CapTerms) -> (usize, Self) {
        (
            terms.effective(),
            Self {
                taken: already_live,
            },
        )
    }

    /// [`Self::open`] for the multi-workspace tick: the slot is taken when
    /// any workspace already has a live overflow sweep.
    #[must_use]
    pub fn for_workspaces<S, D: WorkDispatcher>(
        workspaces: &[(S, D)],
        terms: CapTerms,
    ) -> (usize, Self) {
        let live = workspaces.iter().any(|(_, d)| d.overflow_in_flight());
        Self::open(live, terms)
    }

    /// Whether no overflow sweep is live (Issue #9288's
    /// `plan.slots.overflow_free`).
    #[must_use]
    pub fn is_free(&self) -> bool {
        !self.taken
    }
}

/// The `autonomous` sub-block the removed star-liveness subsystem read
/// (`propagate`, `noProgressMinutes`, `poolsExhaustedGraceMinutes`,
/// `intervalSecs`, `escalate`, `materializeLabels`, `levelCaps`). Removed by
/// #11104; a config that still carries it is ignored with one warning.
pub const RETIRED_CONFIG_BLOCK: &str = "operatorPriority";

/// The retired `autonomous.operatorPriority` keys present in `effective` (an
/// effective config document), sorted. Empty when the block is absent. Pure.
#[must_use]
pub fn retired_config_keys(effective: &serde_json::Value) -> Vec<String> {
    let Some(block) = crate::config_resolver::get_path(effective, "autonomous")
        .and_then(|a| a.get(RETIRED_CONFIG_BLOCK))
    else {
        return Vec::new();
    };
    let mut keys: Vec<String> = match block.as_object() {
        Some(obj) if !obj.is_empty() => obj
            .keys()
            .map(|k| format!("autonomous.{RETIRED_CONFIG_BLOCK}.{k}"))
            .collect(),
        _ => vec![format!("autonomous.{RETIRED_CONFIG_BLOCK}")],
    };
    keys.sort();
    keys
}

/// Log one warning when the workspace at `root` still configures the removed
/// `autonomous.operatorPriority` block (#11104). The keys are otherwise
/// ignored: nothing reads them, so the daemon starts exactly as without them.
/// Returns the keys warned about.
pub fn warn_retired_config(root: &Path) -> Vec<String> {
    let keys = retired_config_keys(&crate::config_resolver::resolve_effective_config(root));
    if !keys.is_empty() {
        log::warn!(
            "config: ignoring {} (removed with the star-liveness subsystem, #11104); \
             delete them from .loom/config.json",
            keys.join(", ")
        );
    }
    keys
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod retired_config_tests {
    use super::*;

    #[test]
    fn a_config_without_the_block_has_nothing_to_warn_about() {
        assert!(retired_config_keys(&serde_json::json!({})).is_empty());
        let other = serde_json::json!({"autonomous": {"workFinder": {"maxConcurrent": 2}}});
        assert!(retired_config_keys(&other).is_empty());
    }

    #[test]
    fn every_retired_key_is_named_once() {
        let cfg = serde_json::json!({"autonomous": {"operatorPriority": {
            "propagate": true, "noProgressMinutes": 30, "poolsExhaustedGraceMinutes": 5
        }}});
        assert_eq!(
            retired_config_keys(&cfg),
            [
                "autonomous.operatorPriority.noProgressMinutes",
                "autonomous.operatorPriority.poolsExhaustedGraceMinutes",
                "autonomous.operatorPriority.propagate",
            ]
        );
        let odd = serde_json::json!({"autonomous": {"operatorPriority": false}});
        assert_eq!(retired_config_keys(&odd), ["autonomous.operatorPriority"]);
    }

    /// AC (#11104): a host config that still carries the removed keys starts
    /// cleanly: they resolve to a warning, and the work-finder config the
    /// daemon starts from is the same as without them.
    #[test]
    fn a_workspace_still_carrying_the_keys_starts_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        let with = serde_json::json!({"autonomous": {
            "workFinder": {"enabled": true, "maxConcurrent": 3},
            "operatorPriority": {"propagate": true, "noProgressMinutes": 45,
                                 "poolsExhaustedGraceMinutes": 10, "levelCaps": {"2": 1}}
        }});
        std::fs::write(dir.path().join(".loom/config.json"), with.to_string()).unwrap();
        let warned = warn_retired_config(dir.path());
        for key in [
            "propagate",
            "noProgressMinutes",
            "poolsExhaustedGraceMinutes",
            "levelCaps",
        ] {
            let key = format!("autonomous.operatorPriority.{key}");
            assert!(warned.contains(&key), "{key} not in {warned:?}");
        }
        let effective = crate::config_resolver::resolve_effective_config(dir.path());
        assert_eq!(
            crate::config_resolver::get_path(&effective, "autonomous.workFinder.maxConcurrent"),
            Some(&serde_json::json!(3)),
            "the rest of the config still resolves"
        );
    }
}
