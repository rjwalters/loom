//! Blocker fingerprints — the Rust port of `classify-dependency-block.sh`'s
//! `_fingerprint` (epic #7810, PR 3).
//!
//! A fingerprint identifies *which set of blockers* a defer or un-escalation
//! decision was made about. Champion writes it into the marker comment so a
//! later pass can tell "the same blockers, still open" from "a different set,
//! re-evaluate" without re-deriving anything.
//!
//! It is therefore a **persisted identifier**: fingerprints already written to
//! live issue comments must keep matching, so the derivation is reproduced
//! exactly rather than modernised.
//!
//! # The derivation
//!
//! ```text
//! printf '%s\n' $nodes | sort -u | tr '\n' ' '   # split, sort, dedupe, join
//!   → strip the single trailing space
//!   → sha256
//!   → first 16 hex characters
//! ```
//!
//! The unquoted `$nodes` is deliberate word splitting (the shell carries a
//! `shellcheck disable=SC2086` for it), so any run of whitespace separates
//! nodes.
//!
//! # A portability landmine, faithfully NOT reproduced
//!
//! The shell's `_sha256` falls back through `sha256sum` → `shasum -a 256` →
//! **`cksum`**. That last branch is not a SHA at all: on a host with neither
//! tool, every fingerprint silently becomes a 32-bit CRC in a different format,
//! so markers written there never match markers written anywhere else.
//!
//! This port always computes SHA-256. That is a deliberate *divergence* from
//! the shell on hosts missing both tools — and it is the right one, because on
//! such a host the shell's own output was already incompatible with every other
//! host's. Every machine in practice has one of the two (both are present on
//! the development and CI images), so no live fingerprint changes.

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// Number of leading hex characters kept. `awk '{print substr($1, 1, 16)}'`.
const FINGERPRINT_LEN: usize = 16;

/// The fingerprint for a whitespace-separated set of blocker nodes.
///
/// Nodes are split on any whitespace, sorted, deduplicated, and joined with a
/// single space before hashing — so the same set in a different order, or with
/// repeats, yields the same fingerprint. That is the property the marker
/// comment depends on.
#[must_use]
pub fn fingerprint(nodes: &str) -> String {
    let unique: BTreeSet<&str> = nodes.split_whitespace().collect();
    let joined = unique.into_iter().collect::<Vec<_>>().join(" ");
    let digest = Sha256::digest(joined.as_bytes());
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    hex.chars().take(FINGERPRINT_LEN).collect()
}

/// The fingerprint for a fact-based un-escalation, which keys on the escalation
/// text plus the commit that resolved it.
///
/// `printf '%s\n%s' "$escalation" "$COMMIT_SHA"` — a literal newline between
/// the two, and no trailing newline. The `fact-` prefix is applied by the
/// caller in the shell; it is applied here so the whole identifier has one
/// owner.
#[must_use]
pub fn fact_fingerprint(escalation: &str, commit_sha: &str) -> String {
    let digest = Sha256::digest(format!("{escalation}\n{commit_sha}").as_bytes());
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    format!("fact-{}", hex.chars().take(FINGERPRINT_LEN).collect::<String>())
}

#[cfg(test)]
mod tests;
