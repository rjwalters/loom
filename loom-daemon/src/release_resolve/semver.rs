//! Version and commit parsing, and the 3-component compare (epic #7810, PR 5).

use regex::Regex;
use std::cmp::Ordering;
use std::sync::OnceLock;

fn version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[0-9]+\.[0-9]+\.[0-9]+").expect("static version pattern"))
}

fn commit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"commit ([0-9a-f]+)").expect("static commit pattern"))
}

/// The first `N.N.N` in `text`, or `None`.
///
/// Used on both a release tag (`v0.19.24`) and `loom-daemon --version` output,
/// which is why it scans rather than parsing a fixed shape.
#[must_use]
pub fn extract_version(text: &str) -> Option<String> {
    version_re().find(text).map(|m| m.as_str().to_string())
}

/// The first `commit <hex>` in `text`, or `None`.
#[must_use]
pub fn extract_commit(text: &str) -> Option<String> {
    commit_re()
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Compare up to three dot-separated numeric components.
///
/// Non-numeric suffixes are stripped defensively and missing components default
/// to `0`, so `0.19` compares equal to `0.19.0` and `0.19.24-dirty` compares as
/// `0.19.24`. That is the shell's `semver_compare`, and it is deliberately
/// lenient: a version string the daemon cannot parse must not silently sort as
/// newer and trigger a roll.
#[must_use]
pub fn compare(a: &str, b: &str) -> Ordering {
    let parts = |v: &str| -> [u64; 3] {
        let mut out = [0u64; 3];
        for (i, seg) in v.split('.').take(3).enumerate() {
            let digits: String = seg.chars().take_while(char::is_ascii_digit).collect();
            out[i] = digits.parse().unwrap_or(0);
        }
        out
    };
    parts(a).cmp(&parts(b))
}

#[cfg(test)]
mod tests;
