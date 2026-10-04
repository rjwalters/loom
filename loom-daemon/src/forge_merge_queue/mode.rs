//! The per-repo merge-mode setting (`champion.mergeMode`, #10255).
//!
//! One key, two values, strict parse:
//!
//! | Value | Meaning |
//! |---|---|
//! | `direct` (default) | Today's behavior: `merge-pr.sh` merges an approved PR itself. |
//! | `queue` | Hand the approved head to the forge's merge queue (dormant until #9978's later phases). |
//!
//! Resolution follows Loom's **env > config > default** chain: the
//! [`MERGE_MODE_ENV`] variable, then the tier-merged effective config
//! ([`crate::config_resolver::resolve_effective_config`]), then
//! [`MergeMode::Direct`]. Unlike the soft-fail knobs elsewhere in
//! `config_resolver`, an **unknown or wrongly-typed value is an error**, never
//! a silent fallback to `direct`: an operator who wrote `"queued"` meant to
//! opt in, and reading that as "direct" would hide the typo; reading it as
//! "queue" would opt in on a guess. Either way the caller refuses and names
//! the offending value and where it came from.

use std::fmt;
use std::path::Path;

use serde_json::Value;

use crate::config_resolver::{get_path, resolve_effective_config};

/// Dotted config key. The only spelling — #9978 floated two; this is the one
/// documented in `defaults/docs/hyperparameters.md`.
pub const MERGE_MODE_KEY: &str = "champion.mergeMode";

/// Env override (one-off operator override, highest precedence). An empty or
/// whitespace-only value is treated as unset.
pub const MERGE_MODE_ENV: &str = "LOOM_MERGE_MODE";

/// How an approved PR reaches the base branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergeMode {
    /// `merge-pr.sh` merges directly. The default.
    #[default]
    Direct,
    /// Enqueue the approved head on the forge merge queue.
    Queue,
}

impl MergeMode {
    /// Strict parse: exactly `direct` or `queue` (surrounding whitespace is
    /// ignored; case is not).
    ///
    /// # Errors
    ///
    /// Any other spelling.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "direct" => Ok(MergeMode::Direct),
            "queue" => Ok(MergeMode::Queue),
            other => Err(other.to_string()),
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MergeMode::Direct => "direct",
            MergeMode::Queue => "queue",
        }
    }
}

/// Which link of the precedence chain supplied the mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeModeSource {
    Env,
    Config,
    Default,
}

impl MergeModeSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MergeModeSource::Env => "env",
            MergeModeSource::Config => "config",
            MergeModeSource::Default => "default",
        }
    }
}

/// A resolved mode and its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedMergeMode {
    pub mode: MergeMode,
    pub source: MergeModeSource,
}

/// An unknown or wrongly-typed `champion.mergeMode`. Carries the offending
/// value (a config value, never a credential) and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeModeError {
    pub source: MergeModeSource,
    pub value: String,
}

impl fmt::Display for MergeModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let origin = match self.source {
            MergeModeSource::Env => format!("${MERGE_MODE_ENV}"),
            _ => format!("config key `{MERGE_MODE_KEY}`"),
        };
        write!(
            f,
            "invalid merge mode {} from {origin}: expected \"direct\" or \"queue\". \
             Nothing falls back to a default on an invalid value — fix or remove it.",
            self.value
        )
    }
}

impl std::error::Error for MergeModeError {}

/// The pure resolution: `env` is the raw [`MERGE_MODE_ENV`] value (if set),
/// `config` the already-merged effective config tree.
///
/// # Errors
///
/// [`MergeModeError`] when the winning tier holds an unknown value, or when
/// the config value is not a string. A JSON `null` counts as unset.
pub fn resolve_merge_mode_from(
    env: Option<&str>,
    config: &Value,
) -> Result<ResolvedMergeMode, MergeModeError> {
    if let Some(raw) = env.filter(|v| !v.trim().is_empty()) {
        return MergeMode::parse(raw)
            .map(|mode| ResolvedMergeMode {
                mode,
                source: MergeModeSource::Env,
            })
            .map_err(|value| MergeModeError {
                source: MergeModeSource::Env,
                value: format!("{value:?}"),
            });
    }
    match get_path(config, MERGE_MODE_KEY) {
        None | Some(Value::Null) => Ok(ResolvedMergeMode {
            mode: MergeMode::Direct,
            source: MergeModeSource::Default,
        }),
        Some(Value::String(s)) => MergeMode::parse(s)
            .map(|mode| ResolvedMergeMode {
                mode,
                source: MergeModeSource::Config,
            })
            .map_err(|value| MergeModeError {
                source: MergeModeSource::Config,
                value: format!("{value:?}"),
            }),
        Some(other) => Err(MergeModeError {
            source: MergeModeSource::Config,
            value: other.to_string(),
        }),
    }
}

/// Resolve `champion.mergeMode` for `repo_root` (env > config > default).
///
/// # Errors
///
/// See [`resolve_merge_mode_from`].
pub fn resolve_merge_mode(repo_root: &Path) -> Result<ResolvedMergeMode, MergeModeError> {
    let env = std::env::var(MERGE_MODE_ENV).ok();
    resolve_merge_mode_from(env.as_deref(), &resolve_effective_config(repo_root))
}
