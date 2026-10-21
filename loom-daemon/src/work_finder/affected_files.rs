//! Same-tick file-overlap gate for daemon dispatch (issue #9781).
//!
//! Two same-repo issues that edit the same files and are built in parallel
//! cost a Doctor rebase cycle as soon as the first PR merges (#4161). This
//! mirrors `/loom:sweep`'s overlap-aware wave partitioning: a candidate whose
//! Curator `## Affected Files` set intersects the surface already occupied in
//! its repo this tick is deferred to a later tick.
//!
//! * **Surface**: backtick-quoted paths under an `Affected Files` heading (a
//!   span with a `/`, or a root file with a known extension).
//! * **Unknown surface** (no section, "To be determined", no paths) is
//!   excluded entirely: it neither blocks nor occupies.
//! * **Scheduling signal only**: no label, hold, or stacking edge (#3729).
//! * **Occupied surface** of a repo = candidates admitted earlier this tick,
//!   plus `loom:building` / in-flight items the existing listing already
//!   carries a body for. Nothing is fetched: an in-flight issue absent from
//!   the listing is NOT covered (v1); the reactive Doctor rebase backstops it.
//! * No priority exempts a candidate (#11508): priority changes order only.
//!   A verified red-main fix (#9244) is never deferred, but still occupies:
//!   the in-flight work it would wait behind cannot merge while `main` is red.

use std::collections::{BTreeSet, HashMap};

use super::PriorityCandidate;

pub mod check;

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

/// Extensions a slash-less span must end in to count as a root-level file
/// (`CLAUDE.md`, `Cargo.toml`). Without this, `WorkItem.body`, `Type.ALL` or
/// `v0.19.1` would read as paths and over-defer (#9781 review).
const ROOT_FILE_EXTS: &[&str] = &[
    "rs", "md", "sh", "ts", "tsx", "js", "jsx", "mjs", "cjs", "json", "yml", "yaml", "toml", "py",
    "txt", "lock", "html", "css", "sql", "go", "rb", "cfg", "ini", "xml", "svg",
];

/// Extensionless root files a slash-less span may name exactly (`Dockerfile`,
/// `VERSION`). Matched case-sensitively and whole, so an identifier such as
/// `version` or `Makefiles` still reads as not path-shaped.
const ROOT_FILE_NAMES: &[&str] = &[
    "Dockerfile",
    "Containerfile",
    "Makefile",
    "GNUmakefile",
    "Justfile",
    "Procfile",
    "Gemfile",
    "Rakefile",
    "Brewfile",
    "Vagrantfile",
    "VERSION",
    "LICENSE",
    "CODEOWNERS",
    ".gitignore",
    ".gitattributes",
    ".dockerignore",
    ".editorconfig",
];

/// `span` as a repo-relative file path, or `None` when it is not path-shaped
/// (an identifier, a code snippet, a bare word). A trailing `:line` is dropped.
/// A span with a `/` is a path; one without must end in a known file
/// extension ([`ROOT_FILE_EXTS`]) or be a known extensionless root file
/// ([`ROOT_FILE_NAMES`]).
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
        !stem.is_empty() && ROOT_FILE_EXTS.contains(&ext.to_ascii_lowercase().as_str())
    });
    (base.contains('/') || has_ext || ROOT_FILE_NAMES.contains(&base)).then(|| base.to_string())
}

/// The paths present in both surfaces, sorted.
#[must_use]
pub fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> Vec<String> {
    a.intersection(b).cloned().collect()
}

/// Per-tick overlap state: each listed candidate's surface and, per
/// workspace, the surface already occupied. The multi-workspace tick holds
/// one per tick.
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
    /// its surface is unknown, it is a verified red-main fix (#9244), or
    /// nothing intersects. No priority exempts a candidate (#11508).
    #[must_use]
    pub fn overlapping(&self, cand: &PriorityCandidate) -> Vec<String> {
        if cand.main_red_fix {
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

    /// Record a listed item's `## Affected Files` surface; `in_flight` marks
    /// an item this dispatcher already holds, and a listed `loom:building`
    /// item occupies its surface regardless. Reads only the listing's own
    /// body and labels.
    pub fn note_surface(&mut self, idx: usize, item: &super::WorkItem, in_flight: bool) {
        let occupying = in_flight || item.labels.iter().any(|l| l == super::BUILDING_LABEL);
        self.note(idx, item.number, item.body.as_deref(), occupying);
    }

    /// Defer `cand` when it shares files with its repo's occupied surface,
    /// resolving its queue row as `deferred_file_overlap`. `true` means
    /// the caller skips it. A scheduling signal only: no label, hold, or
    /// stacking edge.
    pub fn defer(&self, cand: &PriorityCandidate, report: &mut super::TickReport) -> bool {
        let shared = self.overlapping(cand);
        if shared.is_empty() {
            return false;
        }
        let detail =
            super::ready_queue::short_detail(&format!("file overlap: {}", shared.join(", ")));
        super::ready_queue::resolve(
            &mut report.queue,
            cand,
            crate::types::QueueDisposition::DeferredFileOverlap,
            Some(detail),
        );
        true
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
