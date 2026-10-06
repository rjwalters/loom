//! The ETA tracker's coefficient-file hot reload (#10243): when a pass loads
//! a fit with a new id, the registry is rebuilt around it. Both the
//! `eta-fit/v1` file and the `eta-fit/v2` file (#10508) are tracked.

use std::path::Path;
use std::sync::Arc;

use crate::eta::fit::{self, CoefficientFile};
use crate::eta::Registry;

/// The registry to swap in after a pass loaded `loaded` (the `eta-fit/v1`
/// and `eta-fit/v2` files, #10508) while the live registry was built with
/// the files `registered` names (#10243), or `None` to keep it. Pure, and
/// keyed on the ids alone: the same files again swap nothing; a new id (the
/// daily refit), or a file appearing or disappearing, rebuilds the whole
/// registry, which is the same as rebuilding the fitted heuristics.
pub(super) fn swap_fit(
    registered: (Option<&str>, Option<&str>),
    loaded: (Option<CoefficientFile>, Option<CoefficientFile>),
) -> Option<Registry> {
    let id = |f: &Option<CoefficientFile>| f.as_ref().map(|f| f.id.clone());
    if registered.0.map(str::to_string) == id(&loaded.0)
        && registered.1.map(str::to_string) == id(&loaded.1)
    {
        return None;
    }
    Some(Registry::with_fits(loaded.0.map(Arc::new), loaded.1.map(Arc::new)))
}

/// Log a change of `eta-fit/v2` file (#10508), at info: until the first
/// v2 refit, `land-2026-10-06-keen-wren` refuses its PR stages `no_model`.
pub(super) fn log_fit_v2(old: Option<&str>, new: Option<&CoefficientFile>) {
    if old == new.map(|f| f.id.as_str()) {
        return;
    }
    match new {
        Some(file) => log::info!(
            "eta: eta-fit/v2 file {} (as_of {}) loaded for land-2026-10-06-keen-wren, replacing {}",
            file.id,
            file.as_of.to_rfc3339(),
            old.unwrap_or("none")
        ),
        None => log::info!(
            "eta: no eta-fit/v2 file (replacing {}); land-2026-10-06-keen-wren refuses its PR \
             stages no_model until a v2 fit is written",
            old.unwrap_or("none")
        ),
    }
}

/// Log a change of coefficient file: the new id and cutoff, or one warning
/// naming the directory when there is none.
pub(super) fn log_fit(old: Option<&str>, new: Option<&CoefficientFile>, workspace_root: &Path) {
    match new {
        Some(file) => log::info!(
            "eta: coefficient file {} (as_of {}) loaded for the fitted heuristics, replacing {}",
            file.id,
            file.as_of.to_rfc3339(),
            old.unwrap_or("none")
        ),
        None => log::warn!(
            "eta: no coefficient file under {} (replacing {}); \
             land-2026-10-04-twin-otter refuses no_model until a fit is written",
            fit::fit_dir(workspace_root).display(),
            old.unwrap_or("none")
        ),
    }
}
