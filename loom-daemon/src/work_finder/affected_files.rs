//! Same-tick file-overlap gate for daemon dispatch (issue #9781).
//!
//! Two same-repo issues that edit the same files and are built in parallel
//! cost a Doctor rebase cycle as soon as the first PR merges (#4161). This
//! mirrors `/loom:sweep`'s overlap-aware wave partitioning: a candidate whose
//! Curator `## Affected Files` set intersects the surface already occupied in
//! its repo this tick is deferred to a later tick.
//!
//! * **Surface**: backtick-quoted paths under an `Affected Files` heading.
//! * **Unknown surface** (no section, "To be determined", no paths) is
//!   excluded entirely: it neither blocks nor occupies.
//! * **Scheduling signal only**: no label, hold, or stacking edge (#3729).
//! * **Occupied surface** of a repo = candidates admitted earlier this tick,
//!   plus `loom:building` / in-flight items the existing listing already
//!   carries a body for. Nothing is fetched: an in-flight issue absent from
//!   the listing is NOT covered (v1); the reactive Doctor rebase backstops it.
//! * Starred and red-main-fix candidates are never deferred, but still occupy.

use std::collections::{BTreeSet, HashMap};

use super::PriorityCandidate;

/// The set of file paths a Curator body names under `## Affected Files`, or
/// `None` when the surface is unknown (no section or no paths).
#[must_use]
pub fn affected_surface(body: &str) -> Option<BTreeSet<String>> {
    let mut in_section = false;
    let mut paths = BTreeSet::new();
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with('#') {
            let title = t.trim_start_matches('#').trim().to_ascii_lowercase();
            in_section = t.starts_with("##") && title.starts_with("affected files");
            continue;
        }
        if in_section {
            paths.extend(backticked(line).filter_map(|s| as_path(&s)));
        }
    }
    (!paths.is_empty()).then_some(paths)
}

/// Every backtick-quoted span on `line`.
fn backticked(line: &str) -> impl Iterator<Item = String> + '_ {
    line.split('`')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, s)| s.trim().to_string())
}

/// `span` as a repo-relative file path, or `None` when it is not path-shaped
/// (an identifier, a code snippet, a bare word). A trailing `:line` is dropped.
fn as_path(span: &str) -> Option<String> {
    let base = match span.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => span,
    };
    let base = base.trim_start_matches("./");
    if base.is_empty() || base.contains(char::is_whitespace) || base.contains("::") {
        return None;
    }
    let has_ext = base.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && !ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric())
    });
    (base.contains('/') || has_ext).then(|| base.to_string())
}

/// The paths present in both surfaces, sorted.
#[must_use]
pub fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> Vec<String> {
    a.intersection(b).cloned().collect()
}

/// Per-tick overlap state: each listed candidate's surface and, per
/// workspace, the surface already occupied. Lives inside
/// [`super::RepoCap`], whose admission hooks it rides on.
#[derive(Debug, Clone, Default)]
pub struct OverlapGate {
    surfaces: HashMap<(usize, u32), BTreeSet<String>>,
    occupied: HashMap<usize, BTreeSet<String>>,
}

impl OverlapGate {
    /// Record the surface of a listed item of workspace `idx`. An item that is
    /// already in flight / `loom:building` occupies its surface right away;
    /// any other is remembered for [`Self::overlapping`].
    pub fn note(&mut self, idx: usize, number: u32, body: Option<&str>, occupying: bool) {
        let Some(surface) = body.and_then(affected_surface) else {
            return;
        };
        if occupying {
            self.occupied.entry(idx).or_default().extend(surface);
        } else {
            self.surfaces.insert((idx, number), surface);
        }
    }

    /// The paths `cand` shares with its repo's occupied surface; empty when
    /// its surface is unknown, it is a lane candidate, or nothing intersects.
    #[must_use]
    pub fn overlapping(&self, cand: &PriorityCandidate) -> Vec<String> {
        if cand.operator_priority || cand.main_red_fix {
            return Vec::new();
        }
        match (
            self.surfaces.get(&(cand.workspace_idx, cand.number)),
            self.occupied.get(&cand.workspace_idx),
        ) {
            (Some(own), Some(taken)) => overlap(own, taken),
            _ => Vec::new(),
        }
    }

    /// `cand` was admitted: its surface now occupies its repo.
    pub fn occupy(&mut self, cand: &PriorityCandidate) {
        if let Some(surface) = self.surfaces.get(&(cand.workspace_idx, cand.number)) {
            self.occupied
                .entry(cand.workspace_idx)
                .or_default()
                .extend(surface.iter().cloned());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "affected_files_tests.rs"]
mod tests;
