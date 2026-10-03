//! Pure overlap/feature math (#9785 step 2's feature list): set metrics,
//! line-interval overlap with anchor semantics, symbol overlap, intent-aware
//! breakdowns, and the hub-weighted ablation. Every function here is total,
//! deterministic, and side-effect free so frozen evidence replays to
//! identical numbers.

use super::artifact::{Intent, LineInterval, RetrievedFile};
use super::outcome::FileChanges;
use super::patch::{FilePatch, Span};
use std::collections::{BTreeMap, BTreeSet};

/// Jaccard of two sets; `None` when either side is unknown (empty-but-known
/// is a real empty set, not unknown).
pub fn jaccard(a: Option<&BTreeSet<String>>, b: Option<&BTreeSet<String>>) -> Option<f64> {
    let (a, b) = (a?, b?);
    if a.is_empty() && b.is_empty() {
        return Some(0.0);
    }
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    Some(inter as f64 / union as f64)
}

/// Containment of `a` in `b`: |a ∩ b| / |a|.
pub fn containment(a: Option<&BTreeSet<String>>, b: Option<&BTreeSet<String>>) -> Option<f64> {
    let (a, b) = (a?, b?);
    if a.is_empty() {
        return Some(0.0);
    }
    Some(a.intersection(b).count() as f64 / a.len() as f64)
}

/// Hub-weighted Jaccard with frozen per-path weights (default 1.0).
pub fn hub_weighted_jaccard(
    a: Option<&BTreeSet<String>>,
    b: Option<&BTreeSet<String>>,
    weights: &BTreeMap<String, f64>,
) -> Option<f64> {
    let (a, b) = (a?, b?);
    let w = |f: &String| weights.get(f).copied().unwrap_or(1.0).max(0.0);
    let inter: f64 = a.intersection(b).map(&w).sum();
    let union: f64 = a.union(b).map(&w).sum();
    if union == 0.0 {
        return Some(0.0);
    }
    Some(inter / union)
}

/// Overlap length of two spans under anchor semantics: two anchors collide
/// only at the same line; an anchor inside an interval collides; intervals
/// intersect numerically.
pub fn span_overlap_len(a: &Span, b: &Span) -> u32 {
    match (a.anchor, b.anchor) {
        (true, true) => u32::from(a.start == b.start),
        (true, false) => u32::from(b.contains(a.start)),
        (false, true) => u32::from(a.contains(b.start)),
        (false, false) => {
            let lo = a.start.max(b.start);
            let hi = a.end.min(b.end);
            hi.saturating_sub(lo.saturating_sub(1))
        }
    }
}

/// Merge one side's spans into disjoint evidence intervals: intervals are
/// merged; anchors become unit intervals (#9785 repair §A7).
fn evidence_intervals(spans: &[Span]) -> Vec<(u32, u32)> {
    let mut ivs: Vec<(u32, u32)> = spans
        .iter()
        .filter(|s| !s.anchor && s.end >= s.start)
        .map(|s| (s.start, s.end))
        .chain(
            spans
                .iter()
                .filter(|s| s.anchor)
                .map(|s| (s.start, s.start)),
        )
        .collect();
    ivs.sort();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (s, e) in ivs {
        match merged.last_mut() {
            Some(last) if s <= last.1.saturating_add(1) => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// Total overlap length of two span sets under anchor semantics, computed
/// as the line intersection of the two sides' **merged evidence**:
/// overlapping spans within one side never double-count, anchors count once
/// each, and the result can never exceed either side's union length (the
/// recall ≤ 1 invariant).
pub fn spans_overlap_len(a: &[Span], b: &[Span]) -> u32 {
    let ia = evidence_intervals(a);
    let ib = evidence_intervals(b);
    let mut total = 0u32;
    let (mut i, mut j) = (0usize, 0usize);
    while i < ia.len() && j < ib.len() {
        let (s1, e1) = ia[i];
        let (s2, e2) = ib[j];
        let lo = s1.max(s2);
        let hi = e1.min(e2);
        if hi >= lo {
            total += hi - lo + 1;
        }
        if e1 < e2 {
            i += 1;
        } else {
            j += 1;
        }
    }
    total
}

/// Union length of span sets, merging numerically-intersecting intervals
/// (anchors merge only with equal-position anchors).
pub fn spans_union_len(spans: &[Span]) -> u32 {
    let mut ivs: Vec<(u32, u32)> = spans
        .iter()
        .filter(|s| !s.anchor && s.end >= s.start)
        .map(|s| (s.start, s.end))
        .collect();
    ivs.sort();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (s, e) in ivs {
        match merged.last_mut() {
            Some(last) if s <= last.1.saturating_add(1) => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    let mut total: u32 = merged.iter().map(|(s, e)| e - s + 1).sum();
    // Anchors outside every merged interval, deduplicated by position.
    let mut anchors: BTreeSet<u32> = spans.iter().filter(|s| s.anchor).map(|s| s.start).collect();
    for (s, e) in &merged {
        anchors.retain(|a| !(a >= s && a <= e));
    }
    total += anchors.len() as u32;
    total
}

/// Suffix symbol match: `crate::foo::bar` matches `bar`; a *heuristic*
/// because historical qualified names may differ in module path while naming
/// the same item. The report labels it as such. Returns the number of
/// DISTINCT B-side symbols matched, so the Jaccard built on it can never
/// exceed 1.0 (a many-to-one suffix match counts once).
pub fn symbol_sets_intersect(a: &BTreeSet<String>, b: &BTreeSet<String>) -> usize {
    b.iter()
        .filter(|t| {
            a.contains(t.as_str())
                || a.iter()
                    .any(|s| s.rsplit("::").next() == t.rsplit("::").next() && !s.is_empty())
        })
        .count()
}

/// Very light changed-identifier extractor over a unified diff: identifiers
/// (length ≥ 4) on added/removed lines. Heuristic by construction — the
/// report labels it as such.
pub fn extract_identifiers(diff: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in diff.lines() {
        if !(line.starts_with('+') || line.starts_with('-'))
            || line.starts_with("+++")
            || line.starts_with("---")
        {
            continue;
        }
        for w in line[1..].split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if w.len() >= 4
                && w.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            {
                out.insert(w.to_string());
            }
        }
    }
    out
}

/// Predicted pair-overlap features from two retrieval results.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PredictedOverlap {
    /// File-set Jaccard over retrieval results (None = either missing).
    pub file_jaccard: Option<f64>,
    /// |A ∩ B| / |A| and |A ∩ B| / |B|.
    pub containment_a_in_b: Option<f64>,
    pub containment_b_in_a: Option<f64>,
    /// Predicted changed-line overlap fraction: shared interval length over
    /// union length across shared files (None = no comparable evidence).
    pub line_overlap_fraction: Option<f64>,
    /// Shared / total predicted interval evidence across shared files,
    /// deduplicated.
    pub interval_shared: Option<u32>,
    pub interval_union: Option<u32>,
    /// Symbol-set overlap (heuristic suffix matching). `None` = symbols
    /// missing on either side.
    pub symbol_jaccard: Option<f64>,
    /// Intent-stratified file Jaccards (#9784): edit∩edit, edit-vs-context,
    /// context∩context. `None` = that stratum empty on either side.
    pub edit_edit_file_jaccard: Option<f64>,
    pub edit_context_file_jaccard: Option<f64>,
    pub context_context_file_jaccard: Option<f64>,
    /// Hub-weighted file Jaccard ablation (frozen weights).
    pub hub_weighted_file_jaccard: Option<f64>,
    /// The strongest single collision, named and ranked: substantive when
    /// both sides mark the file Edit, else shared-context.
    pub strongest_collision: Option<StrongestCollision>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct StrongestCollision {
    pub path: String,
    /// `substantive-edit` (both sides Edit) or `shared-context`.
    pub kind: String,
    /// Evidence strength: min of the two sides' union interval length (0 for
    /// file-only evidence).
    pub strength: u32,
}

fn file_sets(files: &[RetrievedFile]) -> BTreeSet<String> {
    files.iter().map(|f| f.path.clone()).collect()
}

fn files_with_intent(files: &[RetrievedFile], want: Intent) -> BTreeSet<String> {
    files
        .iter()
        .filter(|f| f.intent == want)
        .map(|f| f.path.clone())
        .collect()
}

fn intervals_by_file(files: &[RetrievedFile]) -> BTreeMap<String, Vec<Span>> {
    let mut m: BTreeMap<String, Vec<Span>> = BTreeMap::new();
    for f in files {
        let spans: Vec<Span> = f
            .intervals
            .iter()
            .map(|LineInterval { start, end }| {
                if start == end {
                    // Single-line evidence with no width information is a
                    // point anchor; multi-line evidence is an interval.
                    Span::anchor(*start)
                } else {
                    Span::interval(*start, *end)
                }
            })
            .collect();
        m.entry(f.path.clone()).or_default().extend(spans);
    }
    m
}

pub fn predicted_overlap(
    a: &[RetrievedFile],
    b: &[RetrievedFile],
    hub_weights: &BTreeMap<String, f64>,
) -> PredictedOverlap {
    let sa = file_sets(a);
    let sb = file_sets(b);
    let file_jaccard = jaccard(Some(&sa), Some(&sb));
    let containment_a_in_b = containment(Some(&sa), Some(&sb));
    let containment_b_in_a = containment(Some(&sb), Some(&sa));
    let hub = hub_weighted_jaccard(Some(&sa), Some(&sb), hub_weights);

    // Line-level: restrict to shared files.
    let ia = intervals_by_file(a);
    let ib = intervals_by_file(b);
    let mut shared_len = 0u32;
    let mut union_len = 0u32;
    for path in sa.intersection(&sb) {
        let (Some(xa), Some(xb)) = (ia.get(path), ib.get(path)) else {
            continue;
        };
        shared_len += spans_overlap_len(xa, xb);
        // True union of both sides' predicted evidence (was max(a, b)).
        let both: Vec<Span> = xa.iter().chain(xb.iter()).copied().collect();
        union_len += spans_union_len(&both);
    }
    let has_evidence = shared_len > 0 || union_len > 0;
    let line_overlap_fraction = has_evidence.then(|| {
        if union_len == 0 {
            0.0
        } else {
            shared_len as f64 / union_len as f64
        }
    });
    let interval_shared = has_evidence.then_some(shared_len);
    let interval_union = has_evidence.then_some(union_len);

    // Symbols: union of per-file symbols per side.
    let syms = |files: &[RetrievedFile]| -> BTreeSet<String> {
        files
            .iter()
            .flat_map(|f| f.symbols.iter().cloned())
            .collect()
    };
    let (sya, syb) = (syms(a), syms(b));
    let symbol_jaccard = if sya.is_empty() || syb.is_empty() {
        None
    } else {
        let inter = symbol_sets_intersect(&sya, &syb) as f64;
        let uni = (sya.len() + syb.len()) as f64 - inter;
        Some(if uni == 0.0 { 0.0 } else { inter / uni })
    };

    let edit_edit = stratum_jaccard(a, b, Intent::Edit, Intent::Edit);
    let edit_ctx = stratum_jaccard(a, b, Intent::Edit, Intent::Context);
    let ctx_ctx = stratum_jaccard(a, b, Intent::Context, Intent::Context);

    // Strongest collision: prefer substantive edit∩edit, then strength.
    let mut strongest: Option<StrongestCollision> = None;
    for path in sa.intersection(&sb) {
        let ia_ = ia.get(path).map(|v| spans_union_len(v)).unwrap_or(0);
        let ib_ = ib.get(path).map(|v| spans_union_len(v)).unwrap_or(0);
        let strength = ia_.min(ib_);
        let a_edit = a
            .iter()
            .any(|f| &f.path == path && f.intent == Intent::Edit);
        let b_edit = b
            .iter()
            .any(|f| &f.path == path && f.intent == Intent::Edit);
        let kind = if a_edit && b_edit {
            "substantive-edit"
        } else {
            "shared-context"
        };
        let better = match &strongest {
            None => true,
            Some(s) => {
                (kind == "substantive-edit" && s.kind != "substantive-edit")
                    || (kind == s.kind && strength > s.strength)
            }
        };
        if better {
            strongest = Some(StrongestCollision {
                path: (*path).clone(),
                kind: kind.into(),
                strength,
            });
        }
    }

    PredictedOverlap {
        file_jaccard,
        containment_a_in_b,
        containment_b_in_a,
        line_overlap_fraction,
        interval_shared,
        interval_union,
        symbol_jaccard,
        edit_edit_file_jaccard: edit_edit,
        edit_context_file_jaccard: edit_ctx,
        context_context_file_jaccard: ctx_ctx,
        hub_weighted_file_jaccard: hub,
        strongest_collision: strongest,
    }
}

fn stratum_jaccard(
    a: &[RetrievedFile],
    b: &[RetrievedFile],
    ia: Intent,
    ib: Intent,
) -> Option<f64> {
    let fa = files_with_intent(a, ia);
    let fb = files_with_intent(b, ib);
    if fa.is_empty() || fb.is_empty() {
        return None;
    }
    let inter = fa.intersection(&fb).count() as f64;
    let uni = fa.union(&fb).count() as f64;
    Some(inter / uni)
}

/// Actual pair-overlap outcome (#9785 step 3's outcome table), measured from
/// the two PRs' own patches.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ActualOverlap {
    pub shared_changed_files: Vec<String>,
    pub file_jaccard: Option<f64>,
    pub containment_a_in_b: Option<f64>,
    pub containment_b_in_a: Option<f64>,
    /// Changed-line overlap measured on a common coordinate basis (shared /
    /// union). `None` = bases not comparable (per-commit-parent derivation on
    /// at least one side) — the issue's unknown-line-outcome rule.
    pub line_overlap_shared: Option<u32>,
    pub line_overlap_union: Option<u32>,
    pub line_overlap_fraction: Option<f64>,
    pub coordinate_basis: Option<String>,
    /// Heuristic actual changed-symbol overlap — identifiers extracted from
    /// changed diff lines. Labeled heuristic; `None` when either side is
    /// missing.
    pub changed_symbol_overlap: Option<f64>,
}

/// Measure actual overlap from the two sides' own patches plus their
/// derivation records (for coordinate-basis comparability).
pub fn actual_overlap(
    a: &[FilePatch],
    b: &[FilePatch],
    a_changes: &FileChanges,
    b_changes: &FileChanges,
) -> ActualOverlap {
    let set =
        |ps: &[FilePatch]| -> BTreeSet<String> { ps.iter().map(|p| p.path.clone()).collect() };
    let (sa, sb) = (set(a), set(b));
    let shared: Vec<String> = sa.intersection(&sb).cloned().collect();
    let comparable = a_changes.coordinate_basis == super::patch::CoordinateBasis::CommonSource
        && b_changes.coordinate_basis == super::patch::CoordinateBasis::CommonSource;
    let (line_shared, line_union, fraction, basis) = if comparable {
        let spans = |ps: &[FilePatch]| -> BTreeMap<String, Vec<Span>> {
            ps.iter()
                .map(|p| (p.path.clone(), p.intervals_new.clone()))
                .collect()
        };
        let (ma, mb) = (spans(a), spans(b));
        let mut sh = 0u32;
        let mut un = 0u32;
        for path in sa.intersection(&sb) {
            let (Some(xa), Some(xb)) = (ma.get(path), mb.get(path)) else {
                continue;
            };
            sh += spans_overlap_len(xa, xb);
            // True set union of both sides' evidence — the previous
            // max(a, b) undercounted the denominator whenever each side
            // changed different regions of the shared file (#9785 repair
            // §A7).
            let both: Vec<Span> = xa.iter().chain(xb.iter()).copied().collect();
            un += spans_union_len(&both);
        }
        let frac = Some(if un == 0 { 0.0 } else { sh as f64 / un as f64 });
        (Some(sh), Some(un), frac, Some("common_source".into()))
    } else {
        (None, None, None, None)
    };
    let symbol_overlap = match (&a_changes.changed_identifiers, &b_changes.changed_identifiers) {
        (Some(x), Some(y)) if !x.is_empty() && !y.is_empty() => {
            let inter = symbol_sets_intersect(x, y) as f64;
            let uni = (x.len() + y.len()) as f64 - inter;
            Some(if uni == 0.0 { 0.0 } else { inter / uni })
        }
        _ => None,
    };
    ActualOverlap {
        shared_changed_files: shared,
        file_jaccard: jaccard(Some(&sa), Some(&sb)),
        containment_a_in_b: containment(Some(&sa), Some(&sb)),
        containment_b_in_a: containment(Some(&sb), Some(&sa)),
        line_overlap_shared: line_shared,
        line_overlap_union: line_union,
        line_overlap_fraction: fraction,
        coordinate_basis: basis,
        changed_symbol_overlap: symbol_overlap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn jaccard_and_containment() {
        let a = set(&["1", "2", "3"]);
        let b = set(&["2", "3", "4"]);
        assert_eq!(jaccard(Some(&a), Some(&b)), Some(2.0 / 4.0));
        assert_eq!(containment(Some(&a), Some(&b)), Some(2.0 / 3.0));
        assert_eq!(containment(Some(&b), Some(&a)), Some(2.0 / 3.0));
        assert_eq!(jaccard(Some(&a), None), None);
        let e: BTreeSet<String> = BTreeSet::new();
        assert_eq!(jaccard(Some(&e), Some(&e)), Some(0.0));
    }

    #[test]
    fn hub_weights_shift_the_metric() {
        let a = set(&["hub.rs", "x.rs"]);
        let b = set(&["hub.rs", "y.rs"]);
        let mut w = BTreeMap::new();
        w.insert("hub.rs".to_string(), 10.0);
        assert_eq!(jaccard(Some(&a), Some(&b)), Some(1.0 / 3.0));
        assert_eq!(hub_weighted_jaccard(Some(&a), Some(&b), &w), Some(10.0 / 12.0));
    }

    #[test]
    fn anchor_and_interval_overlap_semantics() {
        assert_eq!(span_overlap_len(&Span::anchor(5), &Span::anchor(5)), 1);
        assert_eq!(span_overlap_len(&Span::anchor(5), &Span::anchor(6)), 0);
        assert_eq!(span_overlap_len(&Span::anchor(6), &Span::interval(5, 9)), 1);
        assert_eq!(span_overlap_len(&Span::interval(1, 5), &Span::interval(4, 9)), 2);
        assert_eq!(span_overlap_len(&Span::interval(1, 3), &Span::interval(5, 9)), 0);
    }

    #[test]
    fn union_merges_adjacent_intervals_and_counts_anchors_outside() {
        let spans = vec![Span::interval(1, 3), Span::interval(4, 9), Span::anchor(12)];
        assert_eq!(spans_union_len(&spans), 9 + 1);
    }

    #[test]
    fn duplicate_snippets_do_not_inflate() {
        let a = vec![Span::interval(1, 5), Span::interval(1, 5)];
        let b = vec![Span::interval(3, 9)];
        assert_eq!(spans_overlap_len(&a, &b), 3);
        assert_eq!(spans_union_len(&a), 5);
    }

    /// #9785 repair §A7: overlapping spans within ONE side must never
    /// double-count the intersection (the old pairwise sum reported 12 for
    /// the union's 10 lines).
    #[test]
    fn within_side_overlap_does_not_double_count() {
        let a = vec![Span::interval(1, 10), Span::interval(5, 6)];
        let b = vec![Span::interval(1, 10)];
        assert_eq!(spans_overlap_len(&a, &b), 10);
        // And the recall ≤ 1 invariant: shared length ≤ each side's union.
        assert!(spans_overlap_len(&a, &b) <= spans_union_len(&a));
        assert!(spans_overlap_len(&a, &b) <= spans_union_len(&b));
    }

    /// Duplicate anchors count once each, never multiply.
    #[test]
    fn duplicated_anchors_count_once() {
        let a = vec![Span::anchor(5), Span::anchor(5)];
        let b = vec![Span::anchor(5)];
        assert_eq!(spans_overlap_len(&a, &b), 1);
        assert_eq!(spans_overlap_len(&a, &b), spans_union_len(&a));
    }

    /// Two anchors colliding at the same line via each other's intervals is
    /// one line of overlap, not two.
    #[test]
    fn same_line_anchor_collision_is_one_line() {
        let a = vec![Span::anchor(6), Span::interval(1, 3)];
        let b = vec![Span::anchor(6), Span::interval(9, 12)];
        assert_eq!(spans_overlap_len(&a, &b), 1);
    }

    /// Recall ≤ 1 across a mix of anchors and intervals.
    #[test]
    fn recall_invariant_holds() {
        let a = vec![
            Span::interval(1, 4),
            Span::anchor(7),
            Span::interval(20, 24),
        ];
        let b = vec![
            Span::interval(3, 8),
            Span::anchor(7),
            Span::interval(22, 30),
        ];
        let shared = spans_overlap_len(&a, &b);
        let ua = spans_union_len(&a);
        let ub = spans_union_len(&b);
        assert!(shared <= ua && shared <= ub, "shared {shared} union {ua}/{ub}");
        // Shared = 2..4 (2 lines) + 7 (1: a's anchor absorbed into b's
        // 3..8 evidence — same line, counted once) + 22..24 (3) = 6.
        assert_eq!(shared, 6);
    }

    #[test]
    fn symbol_suffix_match() {
        let a = set(&["crate::foo::bar", "util::helper"]);
        let b = set(&["bar", "helper"]);
        assert_eq!(symbol_sets_intersect(&a, &b), 2);
    }

    #[test]
    fn identifier_extraction_skips_headers_and_short_words() {
        let diff = "--- a/src/f.rs\n+++ b/src/f.rs\n@@ -1 +1 @@\n-fn old_thing() {}\n+fn new_thing(helper) {}\n";
        let ids = extract_identifiers(diff);
        assert!(ids.contains("new_thing"));
        assert!(ids.contains("old_thing"));
        assert!(ids.contains("helper"));
        assert!(!ids.contains("fn"));
        assert!(!ids.contains("a/src/f.rs"));
    }
}
