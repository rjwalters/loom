//! Unconditional artifact checksum verification (epic #7810, PR 6a).
//!
//! Port of `loom-daemon-update.sh`'s `verify_artifact_checksum`. Unconditional
//! by design: [`crate::release_resolve`] only resolves a release that
//! publishes BOTH the binary and its `.sha256` sibling, so there is always
//! something to verify against — a missing expected digest, an unreadable
//! binary, or a mismatch are all `false`, never a soft-fallback condition.
//!
//! The shell original had a THIRD failure mode this port cannot reach: no
//! `shasum`/`sha256sum` on `PATH`. [`crate::release_resolve::host::sha256_file`]
//! computes the digest in-process via the `sha2` crate, so "no checksum tool
//! available" simply does not exist here — a strict reduction in the ways
//! this can fail, not a behavior this had to preserve.

use crate::release_resolve::host::sha256_file;
use std::path::Path;

/// Verify `bin_path`'s sha256 against the first whitespace-delimited field of
/// `sha_path` — the `shasum -a 256` / `sha256sum` line format the release's
/// `.sha256` asset uses (`awk 'NR==1{print $1}'` in the shell original).
#[must_use]
pub fn verify(bin_path: &Path, sha_path: &Path) -> bool {
    let Some(expected) = read_expected(sha_path) else {
        return false;
    };
    let Some(actual) = sha256_file(bin_path) else {
        return false;
    };
    expected == actual
}

fn read_expected(sha_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(sha_path).ok()?;
    let field = text.lines().next()?.split_whitespace().next()?;
    (!field.is_empty()).then(|| field.to_string())
}

#[cfg(test)]
mod tests;
