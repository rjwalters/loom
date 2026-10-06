//! Version ordering — the post-merge version-ordering rule a delivery profile
//! depends on (#9790), small enough to compile in seconds on a qualification
//! runner and real enough that a wrong change fails its tests.

use std::cmp::Ordering;

/// A parsed `MAJOR.MINOR.PATCH` version, optionally `v`-prefixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parse `1.2.3` or `v1.2.3`. Anything else — missing parts, extra
    /// parts, non-digits, an empty string — is `None`, never a guess.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut it = s.split('.');
        let mut next = || -> Option<u64> {
            let p = it.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse().ok()
        };
        let v = Version {
            major: next()?,
            minor: next()?,
            patch: next()?,
        };
        if it.next().is_some() {
            return None;
        }
        Some(v)
    }
}

/// The newest parseable version in `tags`; unparseable tags are ignored, not
/// ordered lexically (`0.19.10` must beat `0.19.9`).
#[must_use]
pub fn newest<'a>(tags: impl IntoIterator<Item = &'a str>) -> Option<Version> {
    tags.into_iter().filter_map(Version::parse).max()
}

/// Compare two version strings; `None` if either does not parse.
#[must_use]
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    Some(Version::parse(a)?.cmp(&Version::parse(b)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_not_lexical_ordering() {
        assert_eq!(compare("0.19.10", "0.19.9"), Some(Ordering::Greater));
        assert_eq!(compare("v1.0.0", "0.99.99"), Some(Ordering::Greater));
        assert_eq!(compare("1.2.3", "v1.2.3"), Some(Ordering::Equal));
    }

    #[test]
    fn malformed_versions_never_parse() {
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.x.3",
            "1..3",
            "-1.2.3",
            "1.2.3-rc1",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn newest_ignores_unparseable_tags() {
        let tags = ["v0.19.9", "nightly", "v0.19.10", "0.2.0"];
        assert_eq!(
            newest(tags),
            Some(Version {
                major: 0,
                minor: 19,
                patch: 10
            })
        );
        assert_eq!(newest(["latest"]), None);
    }
}
