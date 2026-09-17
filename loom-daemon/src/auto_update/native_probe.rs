//! The native artifact-resolution probe (epic #7810, PR 5).
//!
//! Adapts [`crate::release_resolve`] to the shape [`super::ArtifactResolution`]
//! already reasons about. It lives here rather than in `auto_update.rs` because
//! that file is over `.loom/docs/file-size-policy.md`'s threshold and frozen.
//!
//! Before this, the tick asked `loom-daemon-update.sh --resolve-json`. #7609
//! chose that deliberately — "rather than reimplementing release resolution in
//! Rust" — and it was right while the alternative was a second implementation.
//! Now the daemon owns the only one, so the question is answered in-process and
//! a host with no usable script is no longer limited by whether one exists.

use super::{ArtifactInfo, ArtifactResolution};
use crate::release_resolve::{resolve, Inputs, Resolution};
use std::path::Path;

/// Resolve the latest artifact for this host.
///
/// The field mapping is total rather than defaulted: an `Option` that arrives
/// `None` stays `None`, because "could not determine" must not become a value
/// the verdict logic compares against.
#[must_use]
pub(super) fn native_resolution(root: &Path) -> ArtifactResolution {
    let inputs = Inputs {
        repo_root: root,
        target_override: std::env::var("LOOM_DAEMON_UPDATE_TARGET").ok(),
        repo_override: std::env::var("LOOM_DAEMON_UPDATE_GH_REPO").ok(),
        // The daemon's own binary: its roll decision is about what IT is
        // running. The shell answers the operator's question instead — the
        // binary its detected supervisor launches — and passes that in. See
        // `Inputs::installed_bin`.
        installed_bin: crate::daemon_bin_resolve::resolve_daemon_bin().ok(),
        fetch_disabled: artifact_fetch_disabled(),
    };
    match resolve(&inputs) {
        Resolution::Unresolved(reason) => ArtifactResolution::Unresolved(reason),
        Resolution::Resolved(r) => ArtifactResolution::Resolved(ArtifactInfo {
            tag: r.tag,
            version: r.version,
            published_at: r.published_at,
            asset_sha256: r.asset_sha256,
            target: Some(r.target),
            installed_version: r.installed_version,
            installed_sha256: r.installed_sha256,
        }),
    }
}

/// `--no-fetch` / `LOOM_DAEMON_UPDATE_FETCH=0`, the fleet-wide opt-out the
/// script honoured in this mode. An operator who turned the artifact path off
/// gets that as the reason, which is what keeps the tick falling back to
/// source rather than reporting a forge problem.
fn artifact_fetch_disabled() -> bool {
    std::env::var("LOOM_DAEMON_UPDATE_FETCH")
        .map(|v| matches!(v.trim(), "0" | "false" | "no" | "off"))
        .unwrap_or(false)
}
