//! The optional `dependabotMaxSemver` guard (#4765).
//!
//! Dependabot's `from X to Y` phrasing is stable across the title
//! (`Bump serde from 1.0.1 to 1.0.2`) and the body (`Bumps [serde](…) from
//! 1.0.1 to 1.0.2.`), including inside a **grouped** PR, whose title carries
//! no versions at all (`Bump the cargo group across 1 directory with 3
//! updates`) but whose body lists every member bump in that same shape. So one
//! extractor over "title then body" covers both, and a grouped PR qualifies
//! only if *every* bump it lists does — which falls out of taking the worst
//! level found rather than the first.
//!
//! When the guard is `all` (the default) none of this runs: CI-green is the
//! gate, and a ceiling nobody asked for would just re-create the starvation
//! this feature exists to end.

use std::sync::LazyLock;

use regex::Regex;

use super::config::MaxSemver;

/// How big a bump is, ordered: `Patch < Minor < Major`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BumpLevel {
    Patch,
    Minor,
    Major,
}

impl BumpLevel {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::Minor => "minor",
            Self::Major => "major",
        }
    }

    /// Does `max` admit a bump of this size?
    #[must_use]
    pub fn admitted_by(self, max: MaxSemver) -> bool {
        match max {
            MaxSemver::All => true,
            MaxSemver::Minor => self <= Self::Minor,
            MaxSemver::Patch => self == Self::Patch,
        }
    }
}

/// One `from X to Y` pair found in a title or body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bump {
    pub from: String,
    pub to: String,
    /// `None` when the pair is not two parseable numeric versions.
    pub level: Option<BumpLevel>,
}

static FROM_TO: LazyLock<Regex> = LazyLock::new(|| {
    // Deliberately loose on the version token and strict on the connective:
    // Dependabot always writes the literal " from <a> to <b>".
    Regex::new(r"(?i)\bfrom\s+v?([0-9][^\s]*)\s+to\s+v?([0-9][^\s]*)")
        .expect("static regex is valid")
});

/// Every `from X to Y` pair in `text`, in order of appearance.
#[must_use]
pub fn extract_bumps(text: &str) -> Vec<Bump> {
    FROM_TO
        .captures_iter(text)
        .map(|c| {
            let from = trim_trailing_punct(&c[1]);
            let to = trim_trailing_punct(&c[2]);
            let level = classify_pair(&from, &to);
            Bump { from, to, level }
        })
        .collect()
}

/// `1.2.3.` / `1.2.3)` / `1.2.3,` — Dependabot bodies end the sentence right
/// after the version.
fn trim_trailing_punct(s: &str) -> String {
    s.trim_end_matches([')', ']', '.', ',', ';', ':', '`', '"', '\''])
        .to_string()
}

/// Numeric `major.minor.patch` prefix, ignoring any pre-release/build suffix.
/// `None` when the leading component is not a number.
fn numeric_parts(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut it = core.split('.');
    let major = it.next()?.parse::<u64>().ok()?;
    let minor = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let patch = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// Classify a version pair.
///
/// **`0.y.z` is treated under caret-compatibility rules**, as Cargo and npm
/// both do: with a zero major, a change to `y` is a breaking change, so it is
/// reported as `Major`, and a change to `z` as `Minor`. The alternative —
/// reading `0.4.0 → 0.5.0` as a "minor" bump — would let the `minor` ceiling
/// admit exactly the breaking upgrades an operator set a ceiling to exclude.
#[must_use]
pub fn classify_pair(from: &str, to: &str) -> Option<BumpLevel> {
    let (fmaj, fmin, fpat) = numeric_parts(from)?;
    let (tmaj, tmin, tpat) = numeric_parts(to)?;

    if fmaj != tmaj {
        return Some(BumpLevel::Major);
    }
    if fmaj == 0 {
        if fmin != tmin {
            return Some(BumpLevel::Major);
        }
        if fpat != tpat {
            return Some(BumpLevel::Minor);
        }
        return Some(BumpLevel::Patch);
    }
    if fmin != tmin {
        return Some(BumpLevel::Minor);
    }
    Some(BumpLevel::Patch)
}

/// The verdict of the guard for one PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemverVerdict {
    /// The guard is `all`, so nothing was evaluated.
    NotEvaluated,
    /// Every extracted bump is at or below the ceiling. Carries the worst one.
    Within(BumpLevel),
    /// At least one bump exceeds the ceiling. Carries the worst one.
    Exceeded(BumpLevel),
    /// The guard is set but no `from X to Y` pair could be parsed at all, so
    /// the ceiling cannot be shown to hold. Fails closed.
    Unparseable,
}

/// Apply the ceiling to `title` then `body`.
#[must_use]
pub fn evaluate(max: MaxSemver, title: &str, body: &str) -> SemverVerdict {
    if max == MaxSemver::All {
        return SemverVerdict::NotEvaluated;
    }
    let mut bumps = extract_bumps(title);
    bumps.extend(extract_bumps(body));

    // A pair we could not classify is as disqualifying as one over the
    // ceiling: "we could not tell" must never read as "it is fine".
    if bumps.is_empty() || bumps.iter().any(|b| b.level.is_none()) {
        return SemverVerdict::Unparseable;
    }
    let worst = bumps
        .iter()
        .filter_map(|b| b.level)
        .max()
        .expect("non-empty, all levels present");
    if worst.admitted_by(max) {
        SemverVerdict::Within(worst)
    } else {
        SemverVerdict::Exceeded(worst)
    }
}

#[cfg(test)]
mod tests;
