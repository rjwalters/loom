//! The repo's short-hash convention: SHA-256, first 16 hex characters.
//!
//! Shared by [`crate::dep_classify::fingerprint`] (blocker fingerprints) and
//! [`crate::dep_recheck`] (conclusion hashes). Both are **persisted
//! identifiers** — they are written into live issue comments and compared on a
//! later pass — so the derivation is reproduced exactly rather than modernised.
//!
//! # A portability landmine, faithfully NOT reproduced
//!
//! Both shell originals compute their digest through a `_sha256` helper that
//! falls back `sha256sum` → `shasum -a 256` → **`cksum`**. That last branch is
//! not a SHA at all: on a host with neither tool every identifier silently
//! becomes a 32-bit CRC in a different format, so markers written there never
//! match markers written anywhere else.
//!
//! This always computes SHA-256. That is a deliberate divergence on hosts
//! missing both tools, and it is the right one — on such a host the shell's own
//! output was already incompatible with every other host's. Every machine in
//! practice has one of the two, so no live identifier changes.

use sha2::{Digest, Sha256};

/// Hex characters kept. `awk '{print substr($1, 1, 16)}'`.
const SHORT_LEN: usize = 16;

/// SHA-256 of `text`, truncated to the first 16 hex characters.
#[must_use]
pub fn short_sha16(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(SHORT_LEN)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_sixteen_hex_characters() {
        let h = short_sha16("anything");
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn it_is_the_real_sha256_prefix_not_a_checksum() {
        // Pins the value, not just the shape: a `cksum` fallback (or any other
        // digest) would produce something else, and every persisted marker on
        // every other host would stop matching.
        assert_eq!(short_sha16(""), "e3b0c44298fc1c14");
        assert_eq!(short_sha16("abc"), "ba7816bf8f01cfea");
    }

    #[test]
    fn distinct_inputs_differ() {
        assert_ne!(short_sha16("a"), short_sha16("b"));
    }
}
