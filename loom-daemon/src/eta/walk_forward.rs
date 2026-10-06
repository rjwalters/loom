//! Walk-forward coefficient files for a replay (#10524).
//!
//! A fitted heuristic (`land-2026-10-04-twin-otter`, its pre-PR composition
//! `-b`, and `land-2026-10-06-quick-tern` over `-b`) reads its coefficients
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
//! Pure apart from [`DatedFits::load_dir`], which only reads the files.

use super::fit::{coeffs, CoefficientFile};
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
    /// From `files`, in any order. Pure.
    #[must_use]
    pub fn new(files: Vec<CoefficientFile>) -> Self {
        let mut files = files;
        // Stable: of two files with one cutoff, the later one in `files`
        // (the later name, for `load_dir`) is kept.
        files.sort_by_key(|f| f.as_of);
        let mut dated: Vec<(DateTime<Utc>, Registry)> = Vec::new();
        for file in files {
            let cutoff = file.as_of;
            if dated.last().is_some_and(|(c, _)| *c == cutoff) {
                dated.pop();
            }
            dated.push((cutoff, Registry::with_fit(Some(Arc::new(file)))));
        }
        DatedFits {
            dated,
            unfitted: Registry::builtin(),
        }
    }

    /// Every readable `eta-fit/v1` file directly in `dir`, read in name
    /// order ([`coeffs::read`]; an unreadable or foreign file is skipped).
    ///
    /// # Errors
    ///
    /// `dir` cannot be listed.
    pub fn load_dir(dir: &Path) -> std::io::Result<Self> {
        let mut paths: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        paths.sort();
        Ok(Self::new(paths.iter().filter_map(|p| coeffs::read(p)).collect()))
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
