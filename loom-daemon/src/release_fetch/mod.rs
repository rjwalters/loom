//! Artifact fetch + verification (epic #7810, PR 6a).
//!
//! The sequel to [`crate::release_resolve`]: that module answers *which*
//! artifact is latest; this one downloads it and gates it on three checks --
//! checksum, signature, and (#8837) GLIBC compatibility -- before anything is
//! allowed to provision or restart from it.
//!
//! Ported from `loom-daemon-update.sh`'s `fetch_and_verify_artifact` and its
//! seven helpers (Epic #4990 Phase 3 / #5020, keyless signing #5054). The
//! shell entry point now delegates here exactly as `--resolve-json` delegates
//! to [`crate::release_resolve`] (#7977) — see `cli/release_fetch.rs` for the
//! stdout/stderr contract that delegation relies on.
//!
//! # The two invariants a port must not blur (read `signature` first)
//!
//! 1. **The checksum is unconditional.** [`crate::release_resolve`] already
//!    guarantees a resolved release publishes both the binary and its
//!    `.sha256` sibling, so there is always something to verify against — a
//!    missing digest, an unreadable binary, or a mismatch are all a hard
//!    failure, never a soft skip.
//! 2. **A signature has three honest outcomes, not two.** *Could not check*
//!    (no tooling, no signature published, an underivable identity) must
//!    never collapse into either *passed* (accepts a tampered artifact) or
//!    *failed* (bricks every host with no `cosign`/no distributed key — #5054
//!    deliberately ships neither). [`signature::Outcome`] keeps the three
//!    apart on purpose.
//!
//! # Scratch directories are trapped, not leaked
//!
//! [`fetch::ScratchDir`] is an RAII guard: a scratch dir is removed on any
//! early return (a failed download, a failed checksum, a failed signature)
//! and handed to the caller intact only on the one path that needs it kept
//! alive past this process's own exit — [`fetch::VerifiedArtifact::tmp_dir`],
//! which the shell wrapper folds into its own `_LOOM_FETCH_TMPDIRS` EXIT trap
//! so the on-disk lifetime story is unchanged end to end.

pub mod checksum;
pub mod cosign;
pub mod evidence;
pub mod fetch;
pub mod glibc;
pub mod signature;
pub mod source;

/// RAII temp dir for the release_fetch tests (#9896): removed on drop, so it
/// is cleaned even when a test panics. Derefs to `PathBuf`.
#[cfg(test)]
pub(crate) struct TestDir(std::path::PathBuf);

#[cfg(test)]
impl TestDir {
    pub(crate) fn new(label: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("loom-daemon-{label}-test-"))
            .tempdir()
            .unwrap()
            .keep();
        Self(dir)
    }
}

#[cfg(test)]
impl std::ops::Deref for TestDir {
    type Target = std::path::PathBuf;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
impl AsRef<std::path::Path> for TestDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub use evidence::{fetch_and_verify_with_evidence, SignatureEvidence};
pub use fetch::{
    fetch_and_verify, fetch_and_verify_with_policy, FetchInputs, FetchOutcome, SignaturePolicy,
    VerifiedArtifact,
};
