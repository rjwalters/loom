//! Build provenance embedded as the `loom` object of telemetry records.
//!
//! It was first defined for ETA records (#9289) and borrowed from there by
//! `pass.summary` / `pass.verdict`, `auto_update.tick`,
//! `token_ranking.refresh`, the release-fetch evidence and the stale-blocked
//! release telemetry. It moved here (#11098, Stage 2) so those non-ETA
//! records no longer depend on the ETA subsystem. The serde shape is
//! unchanged: the wire contract in `telemetry-schema.md` depends on it.

use serde::{Deserialize, Serialize};

/// Which Loom build computed a record (operator requirement on #9289).
/// Sourced from [`crate::telemetry::trace::provenance::daemon`], the same
/// source every span's `loom.daemon.*` attributes come from.
///
/// A build whose revision or tree state is `unknown` (a tarball build) still
/// emits, so no data is lost, but with `complete: false`; accuracy queries
/// exclude incomplete rows, because a result that cannot be pinned to a
/// commit cannot be attributed to a heuristic's code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Loom version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// Full 40-hex git SHA, or `unknown` for a tarball build.
    pub revision: String,
    /// `clean`, `dirty` or `unknown`.
    pub tree_state: String,
    /// `revision` is a full 40-hex SHA and `tree_state` is `clean` or
    /// `dirty`: the build is pinned. Always [`Provenance::completeness`] of
    /// the other two fields.
    pub complete: bool,
}

/// Whether `revision` is a full 40-hex lowercase git SHA.
fn is_full_sha(revision: &str) -> bool {
    revision.len() == 40
        && revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Provenance {
    /// The running binary.
    #[must_use]
    pub fn current() -> Self {
        let build = crate::telemetry::trace::provenance::daemon();
        Provenance {
            version: build.version.to_string(),
            revision: build.revision.to_string(),
            tree_state: build.tree_state.to_string(),
            complete: Self::completeness(build.revision, build.tree_state),
        }
    }

    /// Whether a build with `revision` and `tree_state` is fully pinned.
    #[must_use]
    pub fn completeness(revision: &str, tree_state: &str) -> bool {
        is_full_sha(revision) && matches!(tree_state, "clean" | "dirty")
    }

    /// Whether every field is well formed: a non-empty version, a full
    /// 40-hex revision or the build system's literal `unknown`, a known tree
    /// state, and a `complete` flag that matches them. A record whose
    /// provenance fails this is never emitted. An `unknown` revision or tree
    /// state is well formed (and emitted), but not [`Self::complete`].
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let revision_ok = self.revision == "unknown" || is_full_sha(&self.revision);
        !self.version.trim().is_empty()
            && revision_ok
            && matches!(self.tree_state.as_str(), "clean" | "dirty" | "unknown")
            && self.complete == Self::completeness(&self.revision, &self.tree_state)
    }
}

#[cfg(test)]
mod tests {
    use super::Provenance;

    /// The wire shape is part of the exported-facts contract (#11098): the
    /// move out of `eta` must keep it byte-identical, in field order too.
    #[test]
    fn wire_shape_is_unchanged_by_the_move() {
        let p = Provenance {
            version: "0.19.959".to_string(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
            tree_state: "clean".to_string(),
            complete: true,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(
            json,
            r#"{"version":"0.19.959","revision":"0123456789abcdef0123456789abcdef01234567","tree_state":"clean","complete":true}"#
        );
        let back: Provenance = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        assert!(p.is_valid());
    }
}
