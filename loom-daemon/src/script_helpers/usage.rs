//! Claude API usage checking via the Anthropic OAuth API — the native port of
//! `loom_tools.common.usage` (#4275), behind `check-usage.sh`.
//!
//! Reads the Claude Code OAuth token from the macOS Keychain and queries
//! `GET https://api.anthropic.com/api/oauth/usage` for current session and
//! weekly utilization. Results are cached to `.loom/usage-cache.json` so
//! snapshot/rate-limit checks do not hammer the API.
//!
//! Every failure mode (missing keychain entry, expired token, network trouble)
//! degrades to an `{"error": "..."}` object rather than raising.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Map, Value};

const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const USAGE_API_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const ANTHROPIC_BETA: &str = "oauth-2025-04-20";
const USER_AGENT: &str = "claude-code/2.0.32";

const DEFAULT_CACHE_TTL_SECS: i64 = 60;

/// Read the Claude Code OAuth access token from the macOS Keychain.
///
/// Returns `None` when the credential cannot be read or parsed. The wrapper
/// format `{"claudeAiOauth": {...}}` is unwrapped, and both `accessToken` and
/// `access_token` key spellings are accepted.
fn read_keychain_token() -> Option<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let mut data: Value = serde_json::from_str(&raw).ok()?;
    if let Some(inner) = data.get("claudeAiOauth").cloned() {
        data = inner;
    }
    data.get("accessToken")
        .or_else(|| data.get("access_token"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Call the Anthropic OAuth usage API via `curl`. `None` on any failure.
fn call_usage_api(token: &str) -> Option<Value> {
    let output = Command::new("curl")
        .args([
            "-s",
            "-f",
            "-H",
            &format!("Authorization: Bearer {token}"),
            "-H",
            &format!("anthropic-beta: {ANTHROPIC_BETA}"),
            "-H",
            &format!("User-Agent: {USER_AGENT}"),
            USAGE_API_URL,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// Map the Anthropic usage API response to the backward-compatible shape
/// `check-usage.sh` consumers expect.
#[must_use]
pub fn transform_api_response(api_data: &Value) -> Value {
    let five = api_data.get("five_hour");
    let seven = api_data.get("seven_day");

    let util = |src: Option<&Value>| -> Value {
        src.and_then(|v| v.get("utilization"))
            .and_then(Value::as_f64)
            .map_or(Value::Null, |u| json!(((u * 10.0).round() / 10.0)))
    };
    let resets = |src: Option<&Value>| -> Value {
        src.and_then(|v| v.get("resets_at"))
            .cloned()
            .unwrap_or(Value::Null)
    };

    let mut out = Map::new();
    out.insert("session_percent".into(), util(five));
    out.insert("session_reset".into(), resets(five));
    out.insert("weekly_all_percent".into(), util(seven));
    out.insert("weekly_reset".into(), resets(seven));
    out.insert(
        "timestamp".into(),
        json!(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    );
    out.insert("data_age_seconds".into(), json!(0));
    Value::Object(out)
}

/// The cache TTL: `LOOM_USAGE_CACHE_TTL` if set and parseable, else 60s.
fn cache_ttl_secs() -> i64 {
    std::env::var("LOOM_USAGE_CACHE_TTL")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_CACHE_TTL_SECS)
}

/// Return the fresh cached payload from `cache_path`, if it is younger than
/// `ttl_seconds` and carries no `error` key. `data_age_seconds` is refreshed.
#[must_use]
pub fn fresh_cached(
    cache_path: &Path,
    ttl_seconds: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Value> {
    let cached = super::read_json_file(cache_path)?;
    let obj = cached.as_object()?;
    if obj.contains_key("error") {
        return None;
    }
    let ts = obj.get("timestamp")?.as_str()?;
    let parsed = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ")
        .map(|d| d.and_utc())
        .or_else(|_| {
            chrono::DateTime::parse_from_rfc3339(ts).map(|d| d.with_timezone(&chrono::Utc))
        })
        .ok()?;
    let age = now.signed_duration_since(parsed).num_seconds();
    if age >= ttl_seconds {
        return None;
    }
    let mut out = obj.clone();
    out.insert("data_age_seconds".into(), json!(age.max(0)));
    Some(Value::Object(out))
}

/// Return current Claude API usage data for `repo_root`.
///
/// Checks the `.loom/usage-cache.json` file cache first; on a miss, reads the
/// Keychain token and queries the API. Any failure returns
/// `{"error": "<reason>"}` (`not_in_repo` / `no_keychain_token` /
/// `api_call_failed`).
#[must_use]
pub fn get_usage(repo_root: &Path) -> Value {
    let cache_path = repo_root.join(".loom").join("usage-cache.json");
    let ttl = cache_ttl_secs();

    if let Some(cached) = fresh_cached(&cache_path, ttl, chrono::Utc::now()) {
        return cached;
    }

    let Some(token) = read_keychain_token() else {
        return json!({"error": "no_keychain_token"});
    };
    let Some(api_data) = call_usage_api(&token) else {
        return json!({"error": "api_call_failed"});
    };

    let result = transform_api_response(&api_data);
    // Non-fatal — the data is still valid even if the cache write fails.
    let _ = super::write_json_file(&cache_path, &result);
    result
}

/// Human-readable usage status (`check-usage.sh --status`).
#[must_use]
pub fn format_usage_status(data: &Value) -> String {
    if let Some(err) = data.get("error") {
        let text = err.as_str().map_or_else(|| err.to_string(), str::to_string);
        return format!("ERROR: {text}");
    }

    let mut lines: Vec<String> = Vec::new();
    let ts = data
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    lines.push(format!("Claude Usage Status (as of {ts})"));
    lines.push("=".repeat(40));
    lines.push(String::new());

    let session_pct = data.get("session_percent").and_then(Value::as_f64);
    lines.push(match session_pct {
        Some(p) => format!("Session:     {}% used", fmt_pct(p)),
        None => "Session:     N/A".to_string(),
    });
    if let Some(reset) = data.get("session_reset").and_then(Value::as_str) {
        lines.push(format!("  Resets:    {reset}"));
    }
    lines.push(String::new());

    let weekly_pct = data.get("weekly_all_percent").and_then(Value::as_f64);
    lines.push(match weekly_pct {
        Some(p) => format!("Weekly:      {}% used", fmt_pct(p)),
        None => "Weekly:      N/A".to_string(),
    });
    if let Some(reset) = data.get("weekly_reset").and_then(Value::as_str) {
        lines.push(format!("  Resets:    {reset}"));
    }
    lines.push(String::new());

    if let Some(p) = session_pct {
        if p >= 97.0 {
            lines.push("RECOMMENDATION: Pause operations until session resets".to_string());
        } else if p >= 80.0 {
            lines.push("WARNING: Approaching session limit".to_string());
        } else {
            lines.push("Session usage is healthy".to_string());
        }
    }

    lines.join("\n")
}

/// Render a percentage the way Python's `repr(float)` does for these values —
/// `42.0` stays `42.0`, an integral value keeps its `.0`.
fn fmt_pct(p: f64) -> String {
    if (p.fract()).abs() < f64::EPSILON {
        format!("{p:.1}")
    } else {
        format!("{p}")
    }
}

/// CLI entry point (`loom-daemon usage [--status]`, behind `check-usage.sh`).
///
/// Exit codes: `0` when data was returned, `1` when the payload carries an
/// `error` key (including "not in a repo") — the historical contract.
#[must_use]
pub fn run(status_mode: bool, cwd: &Path) -> i32 {
    let Some(repo_root) = super::find_repo_root(cwd) else {
        eprintln!("{{\"error\": \"not_in_repo\"}}");
        return 1;
    };
    let data = get_usage(&repo_root);
    if status_mode {
        println!("{}", format_usage_status(&data));
    } else {
        println!("{}", serde_json::to_string(&data).unwrap_or_else(|_| "{}".into()));
    }
    i32::from(data.get("error").is_some())
}

/// Advisory: the API probe is bounded by `curl`'s own behavior. Exposed so a
/// future caller can reason about the worst-case wall time.
#[must_use]
pub const fn api_timeout_hint() -> Duration {
    Duration::from_secs(15)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn transform_maps_api_shape_and_rounds() {
        let api = json!({
            "five_hour": {"utilization": 42.04, "resets_at": "2026-01-23T15:00:00Z"},
            "seven_day": {"utilization": 15.06, "resets_at": "2026-01-27T00:00:00Z"}
        });
        let out = transform_api_response(&api);
        assert_eq!(out["session_percent"], json!(42.0));
        assert_eq!(out["session_reset"], json!("2026-01-23T15:00:00Z"));
        assert_eq!(out["weekly_all_percent"], json!(15.1));
        assert_eq!(out["weekly_reset"], json!("2026-01-27T00:00:00Z"));
        assert_eq!(out["data_age_seconds"], json!(0));
        assert!(out["timestamp"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn transform_tolerates_missing_blocks() {
        let out = transform_api_response(&json!({}));
        assert_eq!(out["session_percent"], Value::Null);
        assert_eq!(out["weekly_reset"], Value::Null);
    }

    #[test]
    fn fresh_cache_is_returned_with_updated_age() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("usage-cache.json");
        let now = chrono::Utc::now();
        let ts = (now - chrono::Duration::seconds(10))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        std::fs::write(
            &cache,
            json!({"session_percent": 5.0, "timestamp": ts, "data_age_seconds": 0}).to_string(),
        )
        .unwrap();
        let got = fresh_cached(&cache, 60, now).unwrap();
        assert_eq!(got["session_percent"], json!(5.0));
        assert_eq!(got["data_age_seconds"], json!(10));
    }

    #[test]
    fn stale_cache_is_a_miss() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("usage-cache.json");
        let now = chrono::Utc::now();
        let ts = (now - chrono::Duration::seconds(600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        std::fs::write(&cache, json!({"timestamp": ts}).to_string()).unwrap();
        assert!(fresh_cached(&cache, 60, now).is_none());
    }

    #[test]
    fn cached_error_payload_is_never_served() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("usage-cache.json");
        let now = chrono::Utc::now();
        std::fs::write(
            &cache,
            json!({
                "error": "api_call_failed",
                "timestamp": now.format("%Y-%m-%dT%H:%M:%SZ").to_string()
            })
            .to_string(),
        )
        .unwrap();
        assert!(fresh_cached(&cache, 60, now).is_none());
    }

    #[test]
    fn missing_or_unparseable_cache_is_a_miss() {
        let dir = tempdir().unwrap();
        assert!(fresh_cached(&dir.path().join("nope.json"), 60, chrono::Utc::now()).is_none());
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert!(fresh_cached(&bad, 60, chrono::Utc::now()).is_none());
        let no_ts = dir.path().join("no-ts.json");
        std::fs::write(&no_ts, "{}").unwrap();
        assert!(fresh_cached(&no_ts, 60, chrono::Utc::now()).is_none());
    }

    #[test]
    fn status_text_reports_error_payloads() {
        let out = format_usage_status(&json!({"error": "no_keychain_token"}));
        assert_eq!(out, "ERROR: no_keychain_token");
    }

    #[test]
    fn status_text_thresholds() {
        let base = |pct: f64| {
            json!({
                "session_percent": pct,
                "session_reset": "2026-01-23T15:00:00Z",
                "weekly_all_percent": 3.0,
                "weekly_reset": "2026-01-27T00:00:00Z",
                "timestamp": "2026-01-23T12:34:56Z"
            })
        };
        assert!(format_usage_status(&base(10.0)).contains("Session usage is healthy"));
        assert!(format_usage_status(&base(85.0)).contains("WARNING: Approaching session limit"));
        assert!(format_usage_status(&base(99.0)).contains("RECOMMENDATION: Pause operations"));
        assert!(format_usage_status(&base(42.0)).contains("Session:     42.0% used"));
        assert!(format_usage_status(&base(42.0)).contains("  Resets:    2026-01-23T15:00:00Z"));
    }

    #[test]
    fn status_text_handles_null_percentages() {
        let out = format_usage_status(&json!({"timestamp": "2026-01-23T12:34:56Z"}));
        assert!(out.contains("Session:     N/A"));
        assert!(out.contains("Weekly:      N/A"));
    }

    /// The exit-1-outside-a-repo path is the documented `check-usage.sh`
    /// contract and must not shell out to the Keychain at all.
    #[test]
    fn run_outside_a_repo_exits_1() {
        let dir = tempdir().unwrap();
        // A tempdir with no .git/.loom ancestor is the "not in a repo" case on
        // CI; if the temp root happens to sit inside one, skip rather than
        // assert a false negative.
        if super::super::find_repo_root(dir.path()).is_some() {
            return;
        }
        assert_eq!(run(false, dir.path()), 1);
    }
}
