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
//! The "sha256 → first 16 hex characters" tail is the repo-wide short-hash
//! convention, owned by [`crate::short_hash`] — including the `cksum`
//! portability landmine the shell original carries and this port deliberately
//! does not reproduce. Only the sort/dedupe/join step above is specific to
//! this module.

use crate::short_hash::short_sha16;
use std::collections::BTreeSet;

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
    short_sha16(&joined)
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
    format!("fact-{}", short_sha16(&format!("{escalation}\n{commit_sha}")))
}

#[cfg(test)]
mod tests;
