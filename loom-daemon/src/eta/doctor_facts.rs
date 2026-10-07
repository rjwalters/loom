//! `loom-daemon eta doctor` (#10391): gather the [`Facts`] the pure checks in
//! [`super::doctor`] judge. **Read-only**: it reads this host's config, state
//! files and snapshots and nothing else. It never makes a forge call, never
//! spawns a process of its own, never arms the captain's singleton job and
//! never creates or alters a file (a source test pins this).
//!
//! It does spawn one local process indirectly: resolving each repo's identity
//! goes through `forge_etag_store::remote_identity`, which runs the read-only
//! `git remote get-url origin`. That is why the source test says "of its own";
//! do not tighten it to forbid the transitive `git` read.
//!
//! Reader Apps are resolved against this `root` directly
//! ([`crate::forge_identity::read_credential_in`]), never through
//! `forge_identity::read_credential`: that one needs the workspace root the
//! daemon registers at startup, which a CLI subcommand never has, so it would
//! report `no_reader` for every repo.

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::path::Path;

use super::doctor::{
    AuthorityFacts, BacktestFacts, ConfigFacts, DataFacts, DriftFacts, FitFacts, Gate,
    HeuristicTally, OutcomeFacts, PairFacts, RepoFacts, ServingFacts,
};
use super::heuristics::{
    CALIBRATION_BASE, CALIBRATION_BASES, LAND_BRISK_PETREL, LAND_TWIN_OTTER_B,
};
use super::{
    calibration_log, config, fit, fleet, fleet_refresh, health, nightly_folds, regime, shadow,
    Kind, Registry, Stage,
};
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
        authority: {
            let r = crate::eta::authority::resolve_with(root, host_id, |k| std::env::var(k).ok());
            AuthorityFacts {
                host: r.authority.host.clone(),
                reason: r.authority.reason.as_str().to_string(),
                is_local: r.is_authority(),
                others: r.authority.others.clone(),
                detail: r.describe(),
            }
        },
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
        |repo, host| reader_in(root, repo, host),
    );
    let repos = targets
        .iter()
        .map(|t| RepoFacts {
            repo: t.repo.clone(),
            has_reader: t.reader.is_ok(),
            unsupported_forge: matches!(
                t.reader,
                Err(crate::eta::fleet_fetch::NoReader::UnsupportedForge)
            ),
            snapshot_as_of: fleet::read(&fleet::snapshot_path(root, &t.repo)).map(|s| s.as_of),
            backfill_since: fleet_refresh::read_state(&fleet_refresh::state_path(root, &t.repo))
                .and_then(|s| s.pass)
                .filter(|p| p.kind == fleet_refresh::PassKind::Backfill)
                .map(|p| p.listed_at),
            // #10520: persisted by the refresh cycle; never re-derived here.
            history: fleet_refresh::read_state(&fleet_refresh::state_path(root, &t.repo))
                .and_then(|s| s.history),
        })
        .collect();
    let data = DataFacts {
        gate,
        repos,
        refresh_cycle: health::read_refresh_cycle(root),
    };

    // Cut off at `now`, the same point-in-time rule the estimator's serving
    // path applies (`fit::load_latest(.., listed_at)`): a fit dated in the
    // future is one the estimator cannot serve yet, so the doctor must not
    // report it (#10407 review).
    let latest = fit::coeffs::load_latest(root, now).map(|f| (f.id, f.as_of));
    let today = fit::run::midnight(now);
    let fit_facts = FitFacts {
        latest,
        today_exists: fit::coeffs::read(
            &fit::coeffs::fit_dir(root).join(fit::coeffs::path_for(today)),
        )
        .is_some(),
        last_check: health::read_fit_check(root),
        published: fit::publish::read_status(root),
        published_v2: fit::publish_v2::read_status_v2(root),
    };

    let pending = read_pending(root);
    let registry = Registry::load(root, now);
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
    let calibration = calibration_log::read(&calibration_log::path(root));
    // Drift is checked on one heuristic's track only (#10563 review): pooling
    // every logged heuristic would let a change in the *mix* of heuristics
    // trip the CUSUM with no real regime change. That track is the serving
    // `land` heuristic's when the log records it, else the calibration base.
    // brisk-petrel (#10528) adjusts on twin-otter-b's track, so that is the
    // track its drift is checked on.
    let serving_land = registry.current(Kind::Land, eta.current(Kind::Land)).id();
    let adjusted = serving_land == LAND_BRISK_PETREL;
    let drift_heuristic = if adjusted {
        LAND_TWIN_OTTER_B
    } else if CALIBRATION_BASES.contains(&serving_land) {
        serving_land
    } else {
        CALIBRATION_BASE
    };
    let scored = regime::residuals(
        &calibration
            .iter()
            .filter(|o| o.heuristic == drift_heuristic)
            .cloned()
            .collect::<Vec<_>>(),
    );
    let drift = Stage::EVERY
        .iter()
        .map(|&stage| regime::drift(&scored, stage, now))
        .filter(|d| d.n_recent > 0)
        .map(|d| DriftFacts {
            stage: d.stage.as_str().to_string(),
            heuristic: drift_heuristic.to_string(),
            n_recent: u64::try_from(d.n_recent).unwrap_or(u64::MAX),
            state: d.state(),
            adjusted,
        })
        .collect();
    let outcomes = OutcomeFacts {
        calibration_newest: calibration.iter().map(|o| o.as_of).max(),
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
        drift,
    };

    let backtest = BacktestFacts {
        enabled: eta.enabled && eta.nightly_folds_enabled,
        state: nightly_folds::read_state(root),
    };

    Facts {
        now,
        backtest,
        config: config_facts,
        data,
        fit: fit_facts,
        serving,
        outcomes,
    }
}

/// The reader App for `repo` on `host`, resolved read-only against `root`
/// (the sidecar and token files only): the same answer the daemon's
/// `forge_identity::read_credential` gives, without its registered root.
fn reader_in(
    root: &Path,
    repo: &str,
    host: Option<&str>,
) -> Option<crate::eta::fleet_fetch::Reader> {
    if host.is_some_and(|h| !h.eq_ignore_ascii_case("github.com")) {
        return None;
    }
    let roster = crate::forge_identity::cached(root);
    crate::forge_identity::read_credential_in(root, &roster, repo, std::time::SystemTime::now())
        .map(|(dir, app_id)| crate::eta::fleet_fetch::Reader { app_id, dir })
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

#[cfg(test)]
#[path = "doctor_facts_tests.rs"]
mod tests;
