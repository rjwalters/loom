//! The `champion` config block behind the trusted-bot dependency-PR class
//! (#4765).
//!
//! ```jsonc
//! {
//!   "champion": {
//!     "autoMergeDependabot": true,            // default: false — opt in
//!     "trustedBotAuthors": ["dependabot[bot]"],
//!     "dependabotMaxSemver": "minor"          // "patch" | "minor" | "all"
//!   }
//! }
//! ```
//!
//! **Default off.** With the block absent — the state of every repo until it
//! opts in — [`BotPrConfig::enabled`] is `false` and every caller behaves
//! exactly as it did before this feature existed.
//!
//! # Key spelling
//!
//! camelCase is canonical, matching every other block in `.loom/config.json`
//! (`buildGate`, `intervalSecs`, `maxConcurrent`). The snake_case spellings
//! issue #4765 wrote its proposal in (`auto_merge_dependabot`,
//! `trusted_bot_authors`, `dependabot_max_semver`) are accepted as aliases so
//! a config copy-pasted from the issue is not silently ignored — a silently
//! ignored *enable* flag is the one failure mode here that looks like the
//! feature is broken rather than like a typo.

use std::path::Path;

use serde_json::Value;

/// The author Dependabot posts as. The default trusted set is exactly this
/// one login: `renovate[bot]` and friends are opt-in per repo, never implied.
pub const DEFAULT_TRUSTED_BOT_AUTHOR: &str = "dependabot[bot]";

/// How large a version bump may be and still qualify for the waivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxSemver {
    /// Only `x.y.Z` bumps.
    Patch,
    /// `x.Y.z` and `x.y.Z` bumps.
    Minor,
    /// No semver ceiling at all — the default. CI-green remains the gate.
    All,
}

impl MaxSemver {
    /// `None` for an unrecognized spelling, so a typo'd value can fail loudly
    /// at the CLI boundary rather than silently widening the guard to `All`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "patch" => Some(Self::Patch),
            "minor" => Some(Self::Minor),
            "all" | "any" | "major" => Some(Self::All),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::Minor => "minor",
            Self::All => "all",
        }
    }
}

/// The resolved block. Constructed by [`from_value`] / [`resolve`]; never
/// parsed ad hoc at a call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotPrConfig {
    /// `champion.autoMergeDependabot`. Off unless a repo explicitly opts in.
    pub enabled: bool,
    /// `champion.trustedBotAuthors`. Compared with the PR's `author.login`
    /// **exactly** — never a prefix, substring, branch name or title match,
    /// all of which a human-pushed branch can forge.
    pub trusted_bot_authors: Vec<String>,
    /// `champion.dependabotMaxSemver`.
    pub max_semver: MaxSemver,
}

impl Default for BotPrConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trusted_bot_authors: vec![DEFAULT_TRUSTED_BOT_AUTHOR.to_string()],
            max_semver: MaxSemver::All,
        }
    }
}

impl BotPrConfig {
    /// Exact-match membership. Case-insensitive because forge logins are, but
    /// never fuzzy in any other dimension.
    #[must_use]
    pub fn trusts(&self, author: &str) -> bool {
        self.trusted_bot_authors
            .iter()
            .any(|a| a.eq_ignore_ascii_case(author.trim()))
    }
}

/// Read one key from `champion`, preferring the camelCase spelling and
/// falling back to the snake_case alias.
fn key<'a>(champion: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    champion
        .get(camel)
        .or_else(|| champion.get(snake))
        .filter(|v| !v.is_null())
}

/// Parse the block out of an already-resolved config document.
///
/// Soft-fail throughout, matching `read_work_finder_config` /
/// `read_build_gate_config`: a wrong-typed or missing field falls through to
/// its default rather than erroring. The one thing that is *not* soft is an
/// empty `trustedBotAuthors: []` — that is a deliberate "trust nobody" and is
/// honoured as written, because silently restoring the default there would
/// re-trust an author the operator just removed.
#[must_use]
pub fn from_value(config: &Value) -> BotPrConfig {
    let mut out = BotPrConfig::default();
    let Some(champion) = config.get("champion").filter(|v| v.is_object()) else {
        return out;
    };

    if let Some(v) =
        key(champion, "autoMergeDependabot", "auto_merge_dependabot").and_then(Value::as_bool)
    {
        out.enabled = v;
    }

    if let Some(arr) =
        key(champion, "trustedBotAuthors", "trusted_bot_authors").and_then(Value::as_array)
    {
        out.trusted_bot_authors = arr
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    if let Some(s) =
        key(champion, "dependabotMaxSemver", "dependabot_max_semver").and_then(Value::as_str)
    {
        if let Some(m) = MaxSemver::parse(s) {
            out.max_semver = m;
        } else {
            log::warn!(
                "champion.dependabotMaxSemver: unrecognized value {s:?} — \
                 falling back to \"all\" (no semver ceiling)"
            );
        }
    }

    out
}

/// Resolve the block for a checkout, through the full config tier chain
/// (private defaults → `.loom/config.json` → project → local).
#[must_use]
pub fn resolve(repo_root: &Path) -> BotPrConfig {
    from_value(&crate::config_resolver::resolve_effective_config(repo_root))
}

#[cfg(test)]
mod tests;
