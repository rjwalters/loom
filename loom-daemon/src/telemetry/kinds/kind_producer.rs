//! Producer-liveness guard for registered record kinds (Issue #12947).
//!
//! # Why this file exists
//!
//! On 2026-10-15, #11301 removed the `loom:blocked` release pass and the
//! `fleet.state` producer. Several kinds stayed **registered** in
//! [`crate::telemetry::kinds`] but silently lost their last producer —
//! `eta.stage_outcome` (a training-critical ETA fact), `pr.resolved` and
//! `fleet.state` itself went dark, emission dropped to zero, and **nothing
//! failed**: not the build, not a test, not CI. The silence watcher in
//! [`super::kind_silence`] is reactive, host-local, and marks exactly those
//! kinds exempt, so it never fired. The blackout surfaced days later in SigNoz.
//!
//! This module closes that gap with a compile-checked contract plus a
//! deterministic `#[test]`: every registered `loom.kind` must EITHER have a
//! reachable non-test producer in the daemon, OR be listed in
//! [`PRODUCER_LESS_KINDS`] with a reason/ticket. Removing a producer (as #11301
//! did) now forces the author to add the kind to that allowlist in a reviewed
//! line of code — turning a silent blackout into a visible, declared act.
//!
//! # Which variant of the guard this is, and why
//!
//! A truly sound "reachable producer" analysis would need to follow every call
//! path that constructs and emits a [`crate::telemetry::TelemetryRecord`] — not
//! feasible to compute in Rust at compile time without a parallel registry of
//! producer sites (the very duplication #8921 fought). So this implements the
//! tractable-but-real variant the issue calls for: a `#[test]` source-scans the
//! crate's **non-test** `.rs` files for a construction site of each kind's
//! `TelemetryRecord::<Variant>(…)`, excluding
//!
//! * the OTLP mapping layer (`observability/otlp/mapping`), which *consumes*
//!   every exportable kind by design and would otherwise mask a dead producer
//!   (this is exactly how #11301's casualties hid — their only surviving
//!   non-test references were the mapping match arms), and
//! * the registry declaration files themselves (`telemetry/kinds.rs`,
//!   `telemetry/mod.rs`, `telemetry/envelope.rs`).
//!
//! A kind with no surviving construction site outside those must be on
//! [`PRODUCER_LESS_KINDS`]. The scan reads only files under
//! `CARGO_MANIFEST_DIR` (a compile-time path), so it is deterministic and does
//! no network I/O.
//!
//! The scan cannot tell a construction (`emit_record(TelemetryRecord::X(r))`)
//! from a lingering match arm (`TelemetryRecord::X(r) => …`) in a non-test,
//! non-mapping consumer, so a kind that is only *read* there could pass without
//! a real producer. That residual gap is narrow — the #11301 pattern (producer
//! deleted, only the mapping/test references remain) is caught — and the
//! allowlist-honesty test keeps the declared set from drifting the other way.

/// Registered kinds that currently have **no** reachable producer in the
/// daemon, each with the reason/ticket that retired it. A kind here is exempt
/// from the producer-liveness guard; a kind that loses its producer without
/// being added here fails the [`tests`] guard.
///
/// Keep this list short and every entry justified: it is the explicit record of
/// a kind the daemon no longer emits. Removing the last producer of a kind is a
/// reviewed act *because* it must land as a row here.
pub const PRODUCER_LESS_KINDS: &[(&str, &str)] = &[
    // #11301 removed the `fleet.state` producer (the `loom:blocked` release
    // pass); the replacement reader lives in loom-ui ui#4380.
    (
        "fleet.state",
        "#11301: fleet.state producer retired; replacement reader loom-ui ui#4380",
    ),
    // Derived from `fleet.state` (#11126) and died with it in #11301.
    (
        "pr.resolved",
        "#11301: produced by fleet.state, which was retired; see loom-ui ui#4380",
    ),
    // Training-critical ETA fact, derived from `fleet.state`; the #11301
    // casualty this guard exists to prevent recurring.
    (
        "eta.stage_outcome",
        "#11301: produced by fleet.state, which was retired; see loom-ui ui#4380",
    ),
    // Produced by the `loom:blocked` release pass, also removed by #11301.
    (
        "pass.summary",
        "#11301: produced by the loom:blocked release pass, which was removed",
    ),
    // The daemon-side producer (collector presence derivation) is not present;
    // the OTLP export path and schema remain registered.
    (
        "operator.presence",
        "#11563: no daemon-side producer on main; schema/export path retained",
    ),
];

/// Whether `kind` is on the [`PRODUCER_LESS_KINDS`] allowlist.
#[must_use]
pub fn is_declared_producer_less(kind: &str) -> bool {
    PRODUCER_LESS_KINDS.iter().any(|(k, _)| *k == kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::kinds::TELEMETRY_KINDS;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    /// `true` for a path the producer scan skips: a test file, the OTLP mapping
    /// layer (a by-design consumer of every exportable kind), or a registry
    /// declaration file.
    fn is_excluded(path: &Path) -> bool {
        let s = path.to_string_lossy().replace('\\', "/");
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with("_tests.rs")
            || name.ends_with("_test.rs")
            || name == "tests.rs"
            || s.contains("/tests/")
        {
            return true;
        }
        if s.contains("observability/otlp/mapping") {
            return true;
        }
        s.ends_with("telemetry/kinds.rs")
            || s.ends_with("telemetry/mod.rs")
            || s.ends_with("telemetry/envelope.rs")
    }

    /// Every `.rs` file under `dir`, recursively.
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    /// The set of wire-kind tags that have a construction site
    /// (`TelemetryRecord::<Variant>(`) in a non-excluded source file.
    fn producer_backed_kinds() -> BTreeSet<&'static str> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        let bodies: Vec<String> = files
            .iter()
            .filter(|p| !is_excluded(p))
            .map(|p| std::fs::read_to_string(p).unwrap_or_default())
            .collect();
        TELEMETRY_KINDS
            .iter()
            .filter(|meta| {
                let needle = format!("TelemetryRecord::{}(", meta.variant);
                bodies.iter().any(|b| b.contains(&needle))
            })
            .map(|meta| meta.kind)
            .collect()
    }

    /// The guard: every registered kind is producer-backed or declared
    /// producer-less. A kind that loses its last producer without being added
    /// to [`PRODUCER_LESS_KINDS`] fails here (the #11301 silent-blackout case).
    #[test]
    fn every_registered_kind_has_a_producer_or_is_declared_producer_less() {
        let backed = producer_backed_kinds();
        let orphans: Vec<&str> = TELEMETRY_KINDS
            .iter()
            .map(|m| m.kind)
            .filter(|k| !backed.contains(k) && !is_declared_producer_less(k))
            .collect();
        assert!(
            orphans.is_empty(),
            "registered kind(s) with no reachable producer and not on \
             PRODUCER_LESS_KINDS: {orphans:?}. If a producer was intentionally \
             removed, declare it by adding a `(kind, reason)` row to \
             kind_producer::PRODUCER_LESS_KINDS; otherwise the producer is gone \
             and emission has silently stopped (see #11301 / #12947)."
        );
    }

    /// The allowlist stays honest: no stale entry that in fact has a producer,
    /// no unregistered or duplicate kind, and every reason is non-empty. A
    /// re-added producer must be removed from the allowlist (the reverse of the
    /// #11301 drift).
    #[test]
    fn producer_less_allowlist_is_honest() {
        let backed = producer_backed_kinds();
        let mut seen = BTreeSet::new();
        for (kind, reason) in PRODUCER_LESS_KINDS {
            assert!(
                TELEMETRY_KINDS.iter().any(|m| m.kind == *kind),
                "PRODUCER_LESS_KINDS names an unregistered kind: {kind}"
            );
            assert!(seen.insert(*kind), "PRODUCER_LESS_KINDS lists {kind} more than once");
            assert!(
                !reason.trim().is_empty(),
                "PRODUCER_LESS_KINDS entry {kind} has an empty reason"
            );
            assert!(
                !backed.contains(kind),
                "{kind} is on PRODUCER_LESS_KINDS but now HAS a producer — \
                 remove it from the allowlist so the guard watches it again"
            );
        }
    }

    /// The scan finds producers at all (guards against an exclusion rule or a
    /// path bug silently classifying every kind as producer-less, which would
    /// make the guard vacuous).
    #[test]
    fn the_scan_detects_real_producers() {
        let backed = producer_backed_kinds();
        // `sweep.started` has a stable, long-lived producer.
        assert!(
            backed.contains("sweep.started"),
            "producer scan found no producer for sweep.started — the scan is \
             broken or over-excluding (backed set: {backed:?})"
        );
        assert!(
            backed.len() + PRODUCER_LESS_KINDS.len() >= TELEMETRY_KINDS.len(),
            "scan + allowlist do not cover the registry"
        );
    }
}
