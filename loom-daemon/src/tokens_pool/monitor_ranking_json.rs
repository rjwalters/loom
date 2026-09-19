//! Pure `ranking.json` entry-parsing helpers, split out of
//! [`super::monitor`] (issue #8297) so that module has headroom for the
//! per-class ingest work without crossing its frozen
//! `.loom/docs/file-size-policy.md` line budget. No behavior changed by this
//! split — every item here is used exactly as it was when defined in
//! `monitor.rs`, re-exported back into that module's namespace by a single
//! `use` there.

use std::path::Path;

use super::monitor::parse_iso8601;

const RANKING_JSON_NAME: &str = "ranking.json";
const SUPPORTED_SCHEMA: i64 = 1;

/// Read + validate `ranking.json`; `None` when absent, unreadable, not valid
/// JSON, not an object, or an unsupported `schema`.
pub(super) fn load_ranking_json(monitor_dir: &Path) -> Option<serde_json::Value> {
    let ranking_path = monitor_dir.join(RANKING_JSON_NAME);
    let raw = std::fs::read_to_string(&ranking_path).ok()?;
    let data: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if !data.is_object() {
        return None;
    }
    if data.get("schema").and_then(serde_json::Value::as_i64) != Some(SUPPORTED_SCHEMA) {
        return None;
    }
    Some(data)
}

pub(super) fn coerce_float(value: Option<&serde_json::Value>) -> Option<f64> {
    match value {
        Some(serde_json::Value::Bool(_)) | None => None,
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(serde_json::Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Normalize a monitor-reported reset instant to the canonical
/// `%Y-%m-%dT%H:%M:%SZ` text the `.ranking` writer emits (issue #4874).
/// Anything unparseable as a timestamp yields `None` — an account with no
/// usable reset stays "unknown" rather than carrying junk downstream to the
/// dashboard's countdown.
pub(super) fn coerce_reset(value: Option<&serde_json::Value>) -> Option<String> {
    let raw = value?.as_str()?;
    parse_iso8601(Some(raw)).map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Resolve a `ranking.json` account entry's upstream id, if it carries one,
/// namespaced to match `index.json`'s own `monitor-pk:<id>` convention
/// (design D2, the only namespace `monitor_db.rs`'s import actually confirms
/// today — see its `provider_from_monitor_value` doc comment for the same
/// unconfirmed-upstream-schema caveat). claude-monitor's `ranking.json` is
/// not confirmed to carry an account-id field at all (unlike `index.json`,
/// which #5607 added one to); this is opportunistic — absent, unrecognized,
/// or malformed input all yield `None`, and the caller falls back cleanly to
/// the email map.
pub(super) fn ranking_row_upstream_id(entry: &serde_json::Value) -> Option<String> {
    let raw = entry.get("account_id")?;
    if let Some(s) = raw.as_str() {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(format!("monitor-pk:{trimmed}"));
    }
    if let Some(n) = raw.as_i64() {
        return Some(format!("monitor-pk:{n}"));
    }
    None
}

/// Whether a `ranking.json` account entry **self-identifies** as a
/// non-Claude provider (design D6b, issue #5608).
///
/// This is the mechanism that makes the §3 collision unrepresentable even
/// when two `ranking.json` rows genuinely share one email — one row tracking
/// the human's real Anthropic account, one tracking a different provider's
/// account under the same operator email. Email alone cannot disambiguate
/// that pair; a row that names its own provider can be dropped before the
/// join is even attempted, so it never reaches the severity-merge that used
/// to let it override the Anthropic row's status.
///
/// claude-monitor's `ranking.json` schema is not confirmed to carry a
/// `provider` field in this repo today (the confirmed shape is
/// email/status/utilization/resets only — see [`ranking_row_upstream_id`]'s
/// sibling caveat); this is forward-compatible defense-in-depth, not a load-
/// bearing assumption. A row with no `provider` field at all — the only
/// shape any fixture or live host in this repo currently exercises — is
/// permissive (`false`, fail-open), so behavior is unchanged until/unless a
/// future claude-monitor version starts emitting one.
pub(super) fn ranking_row_is_non_claude_provider(entry: &serde_json::Value) -> bool {
    match entry.get("provider").and_then(|v| v.as_str()) {
        Some(p) => {
            let p = p.trim();
            !p.is_empty()
                && !p.eq_ignore_ascii_case("anthropic")
                && !p.eq_ignore_ascii_case("claude")
        }
        None => false,
    }
}
