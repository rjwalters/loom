//! The model and effort a sweep's telemetry reports (Issue #11370).
//!
//! `SweepInfo.model` / `SweepInfo.effort` stay the literal launch arguments
//! (they drive `--model` / `--effort` and survive a restart in the claim lock).
//! Telemetry reports the *resolved* facts instead, so a fit sees one bucket per
//! model and an effort whenever the runtime has one:
//!
//! - model: an alias (`sonnet`) becomes its full id; an empty or missing value
//!   is an absent fact (`model_source=unknown`), never `''`.
//! - effort: the explicit request, else the ambient `LOOM_EFFORT` the child
//!   inherits (`spawn-claude.sh` applies it), else the `claude` CLI's own
//!   session default ([`CLAUDE_DEFAULT_EFFORT`]); absent for a runtime with no
//!   effort setting.

/// Alias to full model id. The single table: update it when a tier rolls.
/// (`activity/resource_usage.rs` prices by its own, coarser family key.)
pub const MODEL_ALIAS_IDS: &[(&str, &str)] = &[
    ("sonnet", "claude-sonnet-5-5"),
    ("opus", "claude-opus-5-5"),
    ("haiku", "claude-haiku-5-5"),
];

/// The effort the `claude` CLI runs a session at when neither `--effort` nor
/// `LOOM_EFFORT` is given. The daemon cannot read the CLI's own default, so
/// this shipped constant is the documented source (`telemetry-schema.md`);
/// keep it in step with the CLI.
pub const CLAUDE_DEFAULT_EFFORT: &str = "medium";

/// `telemetry::effort_source` value for an effort the dispatch named.
pub const EFFORT_EXPLICIT: &str = "explicit";
/// `telemetry::effort_source` value for a config, environment or runtime default.
pub const EFFORT_DEFAULT: &str = "default";

/// The full model id for `alias_or_id`: an alias maps through
/// [`MODEL_ALIAS_IDS`], a full id passes through, and a trailing `@effort`
/// suffix is dropped. `None` for an empty or whitespace-only value.
#[must_use]
pub fn normalize_model_id(alias_or_id: &str) -> Option<String> {
    let base = alias_or_id.split('@').next().unwrap_or("").trim();
    if base.is_empty() {
        return None;
    }
    let lower = base.to_ascii_lowercase();
    Some(
        MODEL_ALIAS_IDS
            .iter()
            .find(|(alias, _)| *alias == lower)
            .map_or_else(|| base.to_string(), |(_, id)| (*id).to_string()),
    )
}

/// The effort a sweep on `runtime` runs at and where it came from, given the
/// dispatch's explicit `effort` and the ambient `LOOM_EFFORT`. `None` when the
/// runtime has no effort setting the daemon can name.
#[must_use]
pub fn resolve_effort(
    explicit: Option<&str>,
    ambient: Option<&str>,
    runtime: Option<&str>,
) -> Option<(String, &'static str)> {
    let named = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    if let Some(e) = named(explicit) {
        return Some((e, EFFORT_EXPLICIT));
    }
    if let Some(e) = named(ambient) {
        return Some((e, EFFORT_DEFAULT));
    }
    (runtime == Some("claude")).then(|| (CLAUDE_DEFAULT_EFFORT.to_string(), EFFORT_DEFAULT))
}

/// [`resolve_effort`] reading the daemon's own `LOOM_EFFORT`, which the
/// children it spawns inherit.
#[must_use]
pub fn resolve_effort_from_env(
    explicit: Option<&str>,
    runtime: Option<&str>,
) -> Option<(String, &'static str)> {
    resolve_effort(explicit, std::env::var("LOOM_EFFORT").ok().as_deref(), runtime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_become_full_ids_and_full_ids_pass_through() {
        assert_eq!(normalize_model_id("sonnet").as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(normalize_model_id("Opus@high").as_deref(), Some("claude-opus-5-5"));
        assert_eq!(normalize_model_id("claude-haiku-5-5").as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(normalize_model_id("gpt-5").as_deref(), Some("gpt-5"));
    }

    #[test]
    fn empty_model_is_absent() {
        assert_eq!(normalize_model_id(""), None);
        assert_eq!(normalize_model_id("  "), None);
        assert_eq!(normalize_model_id("@high"), None);
    }

    #[test]
    fn effort_precedence_and_source() {
        assert_eq!(
            resolve_effort(Some("high"), Some("low"), Some("claude")),
            Some(("high".into(), "explicit"))
        );
        assert_eq!(
            resolve_effort(Some(""), Some("low"), Some("claude")),
            Some(("low".into(), "default"))
        );
        assert_eq!(
            resolve_effort(None, None, Some("claude")),
            Some((CLAUDE_DEFAULT_EFFORT.into(), "default"))
        );
        assert_eq!(resolve_effort(None, None, Some("codex")), None);
        assert_eq!(resolve_effort(None, None, None), None);
    }
}
