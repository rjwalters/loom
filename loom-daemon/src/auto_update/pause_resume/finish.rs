//! H5 always hands dispatch and restart recovery back (#10832).
//!
//! [`ResumeHost::finish`] lifts the recovery suppression and the dispatch hold
//! that startup placed for a live manifest. An H5 that left without calling it
//! would keep dispatch held and the paused claims out of every recovery pass
//! until the next restart. That is what happened when startup armed for a
//! manifest that H5's own load then found missing or unreadable (the file was
//! removed or damaged in between).
//!
//! [`FinishGuard`] makes it structural: every way out of
//! [`super::run_h5`] — the finished path, each early return, a panic —
//! finishes every manifest id the run is responsible for exactly once.

use std::sync::Arc;

use super::host::ResumeHost;

/// The note an H5 that ended without finishing its manifest leaves.
pub(super) const UNFINISHED_NOTE: &str =
    "pause-and-roll resume ended without finishing its pause manifest (it was missing or \
     unreadable when H5 loaded it, or H5 stopped early); dispatch is released and the paused \
     claims are left to ordinary restart recovery";

/// Calls [`ResumeHost::finish`] for every tracked manifest id on the first
/// [`FinishGuard::finish`], or on drop.
pub(super) struct FinishGuard {
    host: Arc<dyn ResumeHost>,
    ids: Vec<String>,
    done: bool,
}

impl FinishGuard {
    /// A guard for a run startup armed `armed` for (`None` when startup armed
    /// nothing, e.g. for a manifest it could not read).
    pub(super) fn new(host: Arc<dyn ResumeHost>, armed: Option<&str>) -> Self {
        Self {
            host,
            ids: armed.map(str::to_string).into_iter().collect(),
            done: false,
        }
    }

    /// Also finish `id` (the manifest H5 loaded, which may not be the one
    /// startup armed for).
    pub(super) fn track(&mut self, id: &str) {
        if !self.ids.iter().any(|i| i == id) {
            self.ids.push(id.to_string());
        }
    }

    /// Finish now, with `note`. Later calls, and the drop, do nothing.
    pub(super) fn finish(&mut self, note: &str) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        for id in &self.ids {
            self.host.finish(id, note);
        }
    }
}

impl Drop for FinishGuard {
    fn drop(&mut self) {
        if !self.done && !self.ids.is_empty() {
            log::warn!("pause_resume: {UNFINISHED_NOTE} (manifest(s) {:?})", self.ids);
        }
        self.finish(UNFINISHED_NOTE);
    }
}
