//! Walk-forward coefficient files for a replay (#10524).
//!
//! A fitted heuristic (`land-2026-10-04-twin-otter-b`, the retired
//! twin-otter's evaluation with a pre-PR composition, or an offline IPCW
//! wrap over `-b`) reads its coefficients
//! from the registry it was built with, and refuses `no_model` without one.
//! `eta backtest` builds [`Registry::builtin`], which has none, so a replay
//! could never score a fitted heuristic on a PR-level stage.
//!
//! [`DatedFits`] holds one registry per coefficient file. A replayed case is
//! estimated by the registry built with the **newest file whose cutoff is
//! strictly before the case's `as_of`**: the rule
//! [`super::fit::load_latest`] applies when the live tracker builds its
//! registry, so a day's cases are scored by the model that day would have
//! served. A case older than every file gets the unfitted registry and
//! refuses `no_model`, as live serving would. With no file at all every
//! case gets the unfitted registry, so a replay is byte-identical to one
//! built from [`Registry::builtin`].
//!
//! The versioned files (`eta-fit/v2`, #10508; `eta-fit/v3`, #10521) are
//! dated the same way, each schema on its own: a case's registry holds the
//! newest file **of each schema** strictly before its `as_of`, so
//! `land-2026-10-06-keen-wren` and `land-2026-10-06-loop-kite` replay
//! walk-forward beside twin-otter-b for a paired backtest
//! ([`super::backtest_paired`]). With no v2 or v3 file the dated registries
//! are exactly the v1-only ones.
//!
//! Pure apart from [`DatedFits::load_dir`], which only reads the files.

use super::fit::{coeffs, v2, v3, CoefficientFile};
use super::history::StageSamples;
use super::{EstimateInput, Explanation, Heuristic, Kind, Registry, Tier};
use chrono::{DateTime, Utc};
use std::path::Path;
use std::sync::Arc;

/// One registry per coefficient file, plus the unfitted one.
pub struct DatedFits {
    /// `(cutoff, registry built with that file)`, cutoff ascending, one per
    /// cutoff (the later file name wins a tie, as in
    /// [`super::fit::load_latest`]).
    dated: Vec<(DateTime<Utc>, Registry)>,
    /// [`Registry::builtin`]: for a case before every cutoff.
    unfitted: Registry,
}

impl DatedFits {
    /// From `files` (`eta-fit/v1`), in any order. Pure.
    #[must_use]
    pub fn new(files: Vec<CoefficientFile>) -> Self {
        Self::with_schemas(files, Vec::new(), Vec::new())
    }

    /// From the `eta-fit/v1`, `eta-fit/v2` and `eta-fit/v3` files, each in
    /// any order. Pure. A registry is dated at every cutoff any schema has,
    /// and holds, per schema, the newest file whose cutoff is at or before
    /// it (`None` before that schema's first).
    #[must_use]
    pub fn with_schemas(
        v1: Vec<CoefficientFile>,
        v2: Vec<CoefficientFile>,
        v3: Vec<CoefficientFile>,
    ) -> Self {
        let (v1, v2, v3) = (by_cutoff(v1), by_cutoff(v2), by_cutoff(v3));
        let mut cutoffs: Vec<DateTime<Utc>> =
            v1.iter().chain(&v2).chain(&v3).map(|f| f.as_of).collect();
        cutoffs.sort();
        cutoffs.dedup();
        let at = |files: &[Arc<CoefficientFile>], cutoff: DateTime<Utc>| {
            let n = files.partition_point(|f| f.as_of <= cutoff);
            n.checked_sub(1).map(|i| Arc::clone(&files[i]))
        };
        let dated = cutoffs
            .into_iter()
            .map(|c| (c, Registry::with_all_fits(at(&v1, c), at(&v2, c), at(&v3, c))))
            .collect();
        DatedFits {
            dated,
            unfitted: Registry::builtin(),
        }
    }

    /// Every readable `eta-fit/v1` file directly in `dir`, and every
    /// readable `eta-fit/v2` / `eta-fit/v3` file in its `v2` / `v3`
    /// subdirectory (the layout `eta fit` writes, [`v2::fit_dir_v2`],
    /// [`v3::fit_dir_v3`]), each read in name order ([`coeffs::read`],
    /// [`v2::read_v2`], [`v3::read_v3`]; an unreadable or foreign file is
    /// skipped, and a missing subdirectory is no file).
    ///
    /// # Errors
    ///
    /// `dir` cannot be listed.
    pub fn load_dir(dir: &Path) -> std::io::Result<Self> {
        let v1 = json_files(dir)?;
        let sub = |name: &str| json_files(&dir.join(name)).unwrap_or_default();
        Ok(Self::with_schemas(
            v1.iter().filter_map(|p| coeffs::read(p)).collect(),
            sub("v2").iter().filter_map(|p| v2::read_v2(p)).collect(),
            sub("v3").iter().filter_map(|p| v3::read_v3(p)).collect(),
        ))
    }

    /// How many coefficient files (distinct cutoffs) there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.dated.len()
    }

    /// Whether there is none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dated.is_empty()
    }

    /// The cutoffs, ascending.
    #[must_use]
    pub fn cutoffs(&self) -> Vec<DateTime<Utc>> {
        self.dated.iter().map(|(c, _)| *c).collect()
    }

    /// The unfitted registry: the ids, tiers and kinds every dated one shares.
    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.unfitted
    }

    /// The registry an estimate at `as_of` is served from: the newest file
    /// strictly before `as_of`, else the unfitted one.
    #[must_use]
    pub fn at(&self, as_of: DateTime<Utc>) -> &Registry {
        let n = self.dated.partition_point(|(cutoff, _)| *cutoff < as_of);
        match n.checked_sub(1) {
            Some(i) => &self.dated[i].1,
            None => &self.unfitted,
        }
    }

    /// `id` replayed walk-forward over these files, or `None` for an id
    /// no registry has.
    #[must_use]
    pub fn heuristic(&self, id: &str) -> Option<WalkForward<'_>> {
        let h = self.unfitted.get(id)?;
        Some(WalkForward {
            fits: self,
            id: h.id(),
            kind: h.kind(),
            tier: h.tier(),
            models_hold: h.models_hold(),
        })
    }
}

/// `files` sorted by cutoff, one per cutoff: of two files with one cutoff,
/// the later one in `files` (the later name, for `load_dir`) is kept.
fn by_cutoff(mut files: Vec<CoefficientFile>) -> Vec<Arc<CoefficientFile>> {
    // Stable, so the later of a tie stays later.
    files.sort_by_key(|f| f.as_of);
    let mut out: Vec<Arc<CoefficientFile>> = Vec::new();
    for file in files {
        if out.last().is_some_and(|f| f.as_of == file.as_of) {
            out.pop();
        }
        out.push(Arc::new(file));
    }
    out
}

/// The `*.json` paths directly in `dir`, by name.
fn json_files(dir: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    Ok(paths)
}

/// One registered heuristic, each estimate served by
/// [`DatedFits::at`] its own `as_of`.
pub struct WalkForward<'a> {
    fits: &'a DatedFits,
    id: &'static str,
    kind: Kind,
    tier: Tier,
    models_hold: bool,
}

impl Heuristic for WalkForward<'_> {
    fn id(&self) -> &'static str {
        self.id
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn tier(&self) -> Tier {
        self.tier
    }

    fn models_hold(&self) -> bool {
        self.models_hold
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        self.fits
            .at(input.as_of)
            .get(self.id)
            .expect("every registry registers the same ids")
            .estimate(input, history)
    }
}
