//! The I/O around the pure fit (#10245): read the fleet snapshots, build the
//! rows ([`super::rows`]), fit ([`super::coeffs::fit`]), write the coefficient
//! file and prune old ones. `loom-daemon eta fit` and the daemon's daily refit
//! (`observability::eta_fit`) both call this, so they cannot disagree about
//! what a fit is.
//!
//! # The daily refit
//!
//! [`due`] decides, from the clock, whether today's file exists and how fresh
//! the snapshots are, when the daemon should fit: at most once per UTC day,
//! at cutoff `T` = today 00:00Z, as soon as every snapshot has been refreshed
//! past `T`, or [`STALE_GRACE_HOURS`] after `T` with whatever is there (the
//! file's `window.data_through` then says how stale it was). Today's file,
//! whoever wrote it, makes the next check a no-op.
//!
//! # Retention
//!
//! A write to the default directory ([`super::fit_dir`]) prunes it to the
//! newest [`RETAIN_FILES`] `fit-*.json` files (by name, which is by cutoff),
//! so the registry's `load_latest` parses at most that many. A write to an
//! explicit `--out` path prunes nothing.

use super::coeffs::{self, CoefficientFile, FitMeta, FitWindow, Fitter};
use super::rows::{self, Assembled};
use super::FitStage;
use crate::eta::fleet::{self, FleetSnapshot};
use crate::eta::star::StarInputs;
use crate::eta::Provenance;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Coefficient files kept in the default directory.
pub const RETAIN_FILES: usize = 14;

/// How long after midnight UTC the daily refit waits for fresh snapshots
/// before fitting on stale ones.
pub const STALE_GRACE_HOURS: i64 = 6;

/// One stage's share of a fit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct StageReport {
    /// Training rows.
    pub rows: usize,
    /// Rows whose exit label is `true`.
    pub exits: usize,
    /// Rows whose merge label is an observed merge.
    pub merge_events: usize,
    /// The stage has a fitted exit hazard.
    pub hazard: bool,
    /// The stage is in the direct model.
    pub aft: bool,
}

/// What one fit did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FitReport {
    /// The file's content-derived id.
    pub id: String,
    /// The cutoff `T`.
    pub as_of: DateTime<Utc>,
    /// The data horizon `H` (`window.data_through`).
    pub data_through: DateTime<Utc>,
    /// Where the file was (or, on a dry run, would have been) written.
    pub path: PathBuf,
    /// Whether it was written.
    pub written: bool,
    /// Snapshots read.
    pub snapshots: usize,
    /// Per stage.
    pub stages: BTreeMap<FitStage, StageReport>,
    /// Dwells (path-statistics episodes).
    pub dwells: usize,
    /// Rows dropped for a missing queue feature.
    pub rows_dropped_missing: usize,
    /// Rows dropped for want of a flag timeline (snapshot predates #10245).
    pub rows_dropped_no_flags: usize,
    /// Rows whose PR-or-issue star is unknown (#10372): no raw event cache
    /// coverage at their cutoff.
    pub rows_star_unknown: usize,
    /// Old files removed by retention.
    pub pruned: usize,
}

/// This build, as the file's `fitter`.
#[must_use]
pub fn current_fitter() -> Fitter {
    let build = Provenance::current();
    Fitter {
        version: build.version,
        revision: build.revision,
    }
}

/// `now` truncated to 00:00Z: the daily cutoff.
#[must_use]
pub fn midnight(now: DateTime<Utc>) -> DateTime<Utc> {
    now.date_naive().and_time(NaiveTime::MIN).and_utc()
}

/// The fit of `snapshots` at cutoff `as_of`, and the rows it was fitted from.
/// Pure: the same snapshots in any order give the same file.
#[must_use]
pub fn fit_snapshots(
    snapshots: &[FleetSnapshot],
    as_of: DateTime<Utc>,
    fitter: &Fitter,
) -> (CoefficientFile, Assembled) {
    fit_snapshots_with_star(snapshots, as_of, fitter, None)
}

/// [`fit_snapshots`], also recording each row's PR-or-issue star (#10372).
/// The star is a non-model field: the file is the same either way.
#[must_use]
pub fn fit_snapshots_with_star(
    snapshots: &[FleetSnapshot],
    as_of: DateTime<Utc>,
    fitter: &Fitter,
    star: Option<&StarInputs>,
) -> (CoefficientFile, Assembled) {
    let assembled = rows::build_with_star(snapshots, as_of, star);
    let mut window = FitWindow::standard(as_of);
    window.data_through = Some(assembled.data_through);
    let meta = FitMeta {
        as_of,
        window,
        fitter: fitter.clone(),
    };
    let file = coeffs::fit(&meta, &assembled.rows, &assembled.dwells);
    (file, assembled)
}

/// Fit every snapshot under `root` at `as_of` and write the file to `out`, or
/// to `fit_dir(root)/fit-<T>.json` (then pruning that directory). With
/// `dry_run`, fit and report but write nothing.
///
/// # Errors
///
/// No snapshot exists under `root`, or the write failed.
pub fn fit_and_write(
    root: &Path,
    as_of: DateTime<Utc>,
    out: Option<&Path>,
    dry_run: bool,
    fitter: &Fitter,
) -> Result<FitReport> {
    let snapshots = fleet::load_all(root);
    fit_loaded(root, &snapshots, as_of, out, dry_run, fitter)
}

fn fit_loaded(
    root: &Path,
    snapshots: &[FleetSnapshot],
    as_of: DateTime<Utc>,
    out: Option<&Path>,
    dry_run: bool,
    fitter: &Fitter,
) -> Result<FitReport> {
    if snapshots.is_empty() {
        bail!(
            "no fleet snapshot under {}: run `loom-daemon eta fleet backfill --repo OWNER/NAME` \
             for each repo first",
            fleet::snapshot_dir(root).display()
        );
    }
    let repos: Vec<String> = snapshots.iter().map(|s| s.repo.clone()).collect();
    let star = StarInputs::load(root, &repos);
    let (file, assembled) = fit_snapshots_with_star(snapshots, as_of, fitter, Some(&star));
    let dir = coeffs::fit_dir(root);
    let path = out.map_or_else(|| dir.join(coeffs::path_for(as_of)), Path::to_path_buf);
    let mut pruned = 0;
    if !dry_run {
        coeffs::write(&path, &file).with_context(|| format!("writing {}", path.display()))?;
        if out.is_none() {
            pruned = prune_dir(&dir, RETAIN_FILES);
        }
    }

    let mut stages: BTreeMap<FitStage, StageReport> = FitStage::ALL
        .iter()
        .map(|s| (*s, StageReport::default()))
        .collect();
    for row in &assembled.rows {
        let stage = stages.entry(row.stage).or_default();
        stage.rows += 1;
        stage.exits += usize::from(row.exit == Some(true));
        stage.merge_events += usize::from(row.merge.merged);
    }
    for (stage, report) in &mut stages {
        report.hazard = file.hazard.contains_key(stage);
        report.aft = file.aft.as_ref().is_some_and(|a| a.stages.contains(stage));
    }
    Ok(FitReport {
        id: file.id.clone(),
        as_of,
        data_through: assembled.data_through,
        path,
        written: !dry_run,
        snapshots: snapshots.len(),
        stages,
        dwells: assembled.dwells.len(),
        rows_dropped_missing: assembled.stats.rows_dropped_missing,
        rows_dropped_no_flags: assembled.stats.rows_dropped_no_flags,
        rows_star_unknown: assembled.stats.rows_star_unknown,
        pruned,
    })
}

/// Remove all but the newest `keep` `fit-*.json` files in `dir` (by name).
/// Returns how many were removed; a file that cannot be removed is logged and
/// left, since the fit itself already succeeded.
pub fn prune_dir(dir: &Path, keep: usize) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("fit-") && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    let excess = files.len().saturating_sub(keep);
    let mut removed = 0;
    for path in files.iter().take(excess) {
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(e) => log::warn!("eta fit: could not prune {}: {e}", path.display()),
        }
    }
    removed
}

/// When the daily refit is due at `now`: `Some(T)` (today 00:00Z) iff today's
/// file does not exist, there is a snapshot, and either every snapshot is
/// as of `T` or later, or the [`STALE_GRACE_HOURS`] after `T` have passed.
#[must_use]
pub fn due(
    now: DateTime<Utc>,
    today_exists: bool,
    snapshot_as_ofs: &[DateTime<Utc>],
) -> Option<DateTime<Utc>> {
    let today = midnight(now);
    if today_exists {
        return None;
    }
    let oldest = snapshot_as_ofs.iter().min()?;
    (*oldest >= today || now >= today + Duration::hours(STALE_GRACE_HOURS)).then_some(today)
}

/// The daily refit: fit and write today's file when [`due`], else `None`.
#[must_use]
pub fn refit_if_due(root: &Path, now: DateTime<Utc>, fitter: &Fitter) -> Option<Result<FitReport>> {
    let today = midnight(now);
    let today_exists = coeffs::read(&coeffs::fit_dir(root).join(coeffs::path_for(today))).is_some();
    if today_exists {
        // Skip reading the snapshots at all.
        return None;
    }
    let snapshots = fleet::load_all(root);
    let as_ofs: Vec<DateTime<Utc>> = snapshots.iter().map(|s| s.as_of).collect();
    let at = due(now, today_exists, &as_ofs)?;
    Some(fit_loaded(root, &snapshots, at, None, false, fitter))
}
