//! OTLP-export coverage guard for registered record kinds (Issue #12117).
//!
//! [`super::kind_producer`] proves a kind has a *producer*; this proves it has
//! an *export path*. Every registered `loom.kind` must EITHER route over OTLP
//! (its registry row declares an `otlp:` class other than `NotExported` AND
//! `observability/otlp/{mapping*,traces}` has a non-test arm for its
//! `TelemetryRecord` variant), OR be listed in [`NOT_EXPORTED_KINDS`] with a
//! reason and declare `otlp: NotExported`. A kind that is registered but quietly
//! never exported is the zero-rows-in-SigNoz failure this closes.
//!
//! Runtime export health stays with [`super::kind_silence`]; this is the
//! static, build-time half.

/// Registered kinds that deliberately do **not** export over OTLP, each with
/// the reason. Empty today: every registered kind has an OTLP mapping. A kind
/// whose row declares `otlp: NotExported` must be added here, in a reviewed line.
pub const NOT_EXPORTED_KINDS: &[(&str, &str)] = &[];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::kinds::{TelemetryKindOtlp, TELEMETRY_KINDS};
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn is_declared_not_exported(kind: &str) -> bool {
        NOT_EXPORTED_KINDS.iter().any(|(k, _)| *k == kind)
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) != Some("tests") {
                    rust_files(&path, out);
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !(name == "tests.rs" || name.ends_with("_tests.rs")) {
                    out.push(path);
                }
            }
        }
    }

    /// Non-test source of the OTLP mapping layer: `mapping.rs`, `traces.rs`, and
    /// every file under `mapping/`, each cut at its first `#[cfg(test)]` so a mapping
    /// reference that exists only inside a test module does not count.
    fn mapping_source() -> Vec<String> {
        let mapping = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/observability/otlp");
        // `traces.rs` maps `trace.span` (the only `otlp: Spans` kind).
        let mut files = vec![mapping.join("mapping.rs"), mapping.join("traces.rs")];
        rust_files(&mapping.join("mapping"), &mut files);
        files
            .iter()
            .map(|p| {
                let body = std::fs::read_to_string(p)
                    .unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
                match body.find("#[cfg(test)]") {
                    Some(i) => body[..i].to_string(),
                    None => body,
                }
            })
            .collect()
    }

    /// `true` when `body` mentions `TelemetryRecord::<variant>` as a whole
    /// identifier (so `CiJob` does not match `CiJobLog`).
    fn references_variant(body: &str, variant: &str) -> bool {
        let needle = format!("TelemetryRecord::{variant}");
        body.match_indices(&needle).any(|(i, _)| {
            body[i + needle.len()..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
        })
    }

    #[test]
    fn every_registered_kind_is_exported_or_declared_not_exported() {
        let undeclared: Vec<&str> = TELEMETRY_KINDS
            .iter()
            .filter(|m| {
                m.otlp == TelemetryKindOtlp::NotExported && !is_declared_not_exported(m.kind)
            })
            .map(|m| m.kind)
            .collect();
        assert!(
            undeclared.is_empty(),
            "kind(s) declare `otlp: NotExported` but are not on \
             kind_export::NOT_EXPORTED_KINDS: {undeclared:?}. Add a (kind, reason) row, \
             or give the row an OTLP class and a mapping (#12117)."
        );
    }

    #[test]
    fn every_exported_kind_has_an_otlp_mapping_arm() {
        let sources = mapping_source();
        let unmapped: Vec<&str> = TELEMETRY_KINDS
            .iter()
            .filter(|m| m.otlp != TelemetryKindOtlp::NotExported)
            .filter(|m| !sources.iter().any(|s| references_variant(s, m.variant)))
            .map(|m| m.kind)
            .collect();
        assert!(
            unmapped.is_empty(),
            "kind(s) declare an OTLP class but observability/otlp/mapping* has no \
             non-test `TelemetryRecord::<Variant>` arm for them, so they never \
             export: {unmapped:?}. Add the mapping, or declare the kind on \
             kind_export::NOT_EXPORTED_KINDS with `otlp: NotExported` (#12117)."
        );
    }

    #[test]
    fn not_exported_allowlist_is_honest() {
        let mut seen = BTreeSet::new();
        for (kind, reason) in NOT_EXPORTED_KINDS {
            let meta = TELEMETRY_KINDS
                .iter()
                .find(|m| m.kind == *kind)
                .unwrap_or_else(|| panic!("NOT_EXPORTED_KINDS names an unregistered kind: {kind}"));
            assert!(seen.insert(*kind), "NOT_EXPORTED_KINDS lists {kind} twice");
            assert!(!reason.trim().is_empty(), "NOT_EXPORTED_KINDS entry {kind} has no reason");
            assert_eq!(
                meta.otlp,
                TelemetryKindOtlp::NotExported,
                "{kind} is on NOT_EXPORTED_KINDS but its row exports over OTLP — remove it"
            );
        }
    }

    #[test]
    fn the_variant_scan_is_not_vacuous() {
        let sources = mapping_source();
        assert!(sources
            .iter()
            .any(|s| references_variant(s, "SweepStarted")));
        // Whole-identifier matching: a prefix must not count.
        assert!(!references_variant("TelemetryRecord::CiJobLog(r)", "CiJob"));
        assert!(references_variant("TelemetryRecord::CiJob(_)", "CiJob"));
    }

    /// The inventory in `defaults/docs/observability.md` names every kind.
    #[test]
    fn observability_doc_inventories_every_kind() {
        let doc = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/docs/observability.md");
        let body =
            std::fs::read_to_string(&doc).unwrap_or_else(|e| panic!("read {}: {e}", doc.display()));
        let missing: Vec<&str> = TELEMETRY_KINDS
            .iter()
            .map(|m| m.kind)
            .filter(|k| !body.contains(&format!("| `{k}` |")))
            .collect();
        assert!(
            missing.is_empty(),
            "defaults/docs/observability.md \"Telemetry kind inventory\" has no row for: \
             {missing:?} (#12117)"
        );
    }
}
