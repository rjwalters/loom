//! `loom-daemon eta doctor` (#10391): gather the [`Facts`] the pure checks in
//! [`super::doctor`] judge. **Read-only**: it reads this host's config, state
//! files and snapshots and nothing else. It never makes a forge call, never
//! spawns a process of its own, never arms the captain's singleton job and
//! never creates or alters a file (a source test pins this).

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::path::Path;

use super::doctor::{
    ConfigFacts, DataFacts, FitFacts, Gate, HeuristicTally, OutcomeFacts, PairFacts, RepoFacts,
    ServingFacts,
};
use super::{calibration_log, config, fit, fleet, fleet_refresh, health, shadow, Kind, Registry};
use crate::eta::doctor::Facts;
use crate::eta::score::EstimateSummary;
use crate::observability::{self, ExporterKind};

/// Gather every fact for the checks, as of `now`, for the host `host_id`.
#[must_use]
pub fn gather(root: &Path, host_id: &str, now: DateTime<Utc>) -> Facts {
    let eta = config::read(root);
    let obs = observability::read_config(root);
    let exporters = if observability::resolve_enabled(&obs) {
        observability::resolve_exporters(&obs)
    } else {
        Vec::new()
    };
    let config_facts = ConfigFacts {
        eta_enabled: eta.enabled,
        fit_enabled: eta.fit_enabled,
        fleet_refresh_enabled: eta.fleet_refresh.enabled,
        interval_secs: eta.fleet_refresh.interval_secs,
        otlp_exporter: exporters.iter().any(|e| e.kind == ExporterKind::Otlp),
        native_exporter: exporters.iter().any(|e| e.kind == ExporterKind::Https),
    };

    // The read-only gate resolver: it must never arm the singleton job.
    let gate = match crate::fleet_captain::resolve_gate_for_root(root, host_id) {
        crate::fleet_captain::CaptainGate::Armed { .. } => Gate::Captain,
        crate::fleet_captain::CaptainGate::Refused { captain, .. } => Gate::StandDown { captain },
        crate::fleet_captain::CaptainGate::NoCaptainDeclared => Gate::NoCaptain,
    };
    let targets = crate::observability::eta_fleet_refresh::repo_targets(
        root,
        &[],
        |r| crate::forge_etag_store::remote_identity(r),
        |repo, host| {
            crate::forge_identity::read_credential(repo, host)
                .map(|(dir, app_id)| crate::eta::fleet_fetch::Reader { app_id, dir })
        },
    );
    let repos = targets
        .iter()
        .map(|t| RepoFacts {
            repo: t.repo.clone(),
            has_reader: t.reader.is_ok(),
            snapshot_as_of: fleet::read(&fleet::snapshot_path(root, &t.repo)).map(|s| s.as_of),
            backfill_since: fleet_refresh::read_state(&fleet_refresh::state_path(root, &t.repo))
                .and_then(|s| s.pass)
                .filter(|p| p.kind == fleet_refresh::PassKind::Backfill)
                .map(|p| p.listed_at),
        })
        .collect();
    let data = DataFacts {
        gate,
        repos,
        refresh_cycle: health::read_refresh_cycle(root),
    };

    let latest =
        fit::coeffs::load_latest(root, now + chrono::Duration::days(1)).map(|f| (f.id, f.as_of));
    let today = fit::run::midnight(now);
    let fit_facts = FitFacts {
        latest,
        today_exists: fit::coeffs::read(
            &fit::coeffs::fit_dir(root).join(fit::coeffs::path_for(today)),
        )
        .is_some(),
        last_check: health::read_fit_check(root),
    };

    let pending = read_pending(root);
    let registry = Registry::load(root, now + chrono::Duration::days(1));
    let mut shadows = Vec::new();
    let mut tallies = Vec::new();
    for kind in [Kind::Start, Kind::Finish, Kind::Land] {
        let current = registry.current(kind, eta.current(kind)).id();
        for h in registry.for_kind(kind) {
            if h.id() != current {
                shadows.push(format!("{}:{}", kind.as_str(), h.id()));
            }
            let mut newest: BTreeMap<(&str, u32), &EstimateSummary> = BTreeMap::new();
            for p in pending
                .iter()
                .filter(|p| p.kind == kind && p.heuristic == h.id())
            {
                let key = (p.repo.as_str(), p.issue);
                if newest.get(&key).is_none_or(|n| n.as_of < p.as_of) {
                    newest.insert(key, p);
                }
            }
            if newest.is_empty() {
                continue;
            }
            let mut tally = HeuristicTally {
                kind: kind.as_str().to_string(),
                heuristic: h.id().to_string(),
                current: h.id() == current,
                answered: 0,
                refused: BTreeMap::new(),
            };
            for p in newest.values() {
                match p.no_estimate_reason {
                    None => tally.answered += 1,
                    Some(r) => *tally.refused.entry(r.as_str().to_string()).or_default() += 1,
                }
            }
            tallies.push(tally);
        }
    }
    let serving = ServingFacts {
        fit_loaded: registry.fit().is_some(),
        shadows,
        tallies,
    };

    let ledger = shadow::read_ledger(&shadow::ledger_path(root)).unwrap_or_default();
    let outcomes = OutcomeFacts {
        calibration_newest: calibration_log::read(&calibration_log::path(root))
            .iter()
            .map(|o| o.as_of)
            .max(),
        pairs: ledger
            .pairs
            .iter()
            .map(|(key, sums)| PairFacts {
                key: key.clone(),
                pairs: u64::try_from(sums.pairs).unwrap_or(u64::MAX),
            })
            .collect(),
        oldest_pending: pending.iter().map(|p| p.as_of).min(),
        pending: u64::try_from(pending.len()).unwrap_or(u64::MAX),
    };

    Facts {
        now,
        config: config_facts,
        data,
        fit: fit_facts,
        serving,
        outcomes,
    }
}

fn read_pending(root: &Path) -> Vec<EstimateSummary> {
    std::fs::read_to_string(observability::eta::pending_path(root))
        .map(|text| {
            text.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}
