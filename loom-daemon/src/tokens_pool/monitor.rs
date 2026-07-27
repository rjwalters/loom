//! Optional, auto-detected claude-monitor integration for token selection.
//!
//! Native Rust port of `loom_tools.tokens.monitor` (issue #4094, epic #4081).
//! A companion tool, **claude-monitor**, maintains richer per-account
//! utilization data than Loom's own probe. When present, Loom consumes
//! `~/.claude-monitor/ranking.json` to produce its spawn-time `.ranking`
//! **without** a hard dependency: this module is pure file detection. When the
//! file is absent, malformed, stale, or `schema != 1`, callers fall back to
//! probing so behavior collapses to the probe path byte-for-byte.
//!
//! Two surfaces are never mixed: `ranking.json` (no secrets — utilization
//! only, consumed here) and `accounts.env` (secrets, consumed by bootstrap).
//!
//! Ordering policy stays Loom's: `(status_rank, util_7d, util_5h)` using the
//! [`super::check::status_rank`] vocabulary; the email join to Loom account
//! names goes through the `index.json` manifest. The monitor dir is overridable
//! via `LOOM_CLAUDE_MONITOR_DIR` so tests never touch a real `~/.claude-monitor`.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::check::{status_rank, AccountResult, ProbeReport};

const CLAUDE_MONITOR_DIR_VAR: &str = "LOOM_CLAUDE_MONITOR_DIR";
const DEFAULT_CLAUDE_MONITOR_DIR: &str = "~/.claude-monitor";
const RANKING_JSON_NAME: &str = "ranking.json";
const SUPPORTED_SCHEMA: i64 = 1;
/// Freshness window (10 min); a `ranking.json` older than this is stale.
const MONITOR_FRESH_SECONDS: i64 = 600;
/// Utilization sentinel for absent values so they sort after known (lower)
/// utilizations within a status bucket.
const UTIL_SENTINEL: f64 = 2.0;

/// One account resolved from `ranking.json`, joined to a Loom name.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorAccount {
    pub name: String,
    pub status: String,
    pub util_7d: Option<f64>,
    pub util_5h: Option<f64>,
}

/// Resolve the claude-monitor directory: `$LOOM_CLAUDE_MONITOR_DIR` (tilde
/// expanded), else `~/.claude-monitor`.
#[must_use]
pub fn claude_monitor_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var(CLAUDE_MONITOR_DIR_VAR) {
        if !override_dir.trim().is_empty() {
            return expand_tilde(&override_dir);
        }
    }
    expand_tilde(DEFAULT_CLAUDE_MONITOR_DIR)
}

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(raw)
}

/// Parse an ISO-8601 timestamp (accepting a trailing `Z`) to aware UTC.
fn parse_iso8601(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let text = raw?.trim();
    if text.is_empty() {
        return None;
    }
    let normalized = if let Some(stripped) = text.strip_suffix('Z') {
        format!("{stripped}+00:00")
    } else {
        text.to_string()
    };
    if let Ok(dt) = DateTime::parse_from_rfc3339(&normalized) {
        return Some(dt.with_timezone(&Utc));
    }
    // Naive datetime (no offset) -> assume UTC.
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S") {
        return Some(DateTime::from_naive_utc_and_offset(ndt, Utc));
    }
    // Date only -> midnight UTC.
    if let Ok(nd) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Some(DateTime::from_naive_utc_and_offset(nd.and_hms_opt(0, 0, 0)?, Utc));
    }
    None
}

/// A missing/unparseable timestamp is treated as **stale** (fail closed).
fn is_fresh(generated_at: Option<&str>, now: DateTime<Utc>) -> bool {
    match parse_iso8601(generated_at) {
        Some(dt) => {
            let age = (now - dt).num_seconds();
            (0..MONITOR_FRESH_SECONDS).contains(&age)
        }
        None => false,
    }
}

/// Return `[(email_lower, name)]` from the `index.json` manifest, preserving
/// manifest order (Python dict insertion order). Missing/malformed -> empty.
fn load_index_email_map(tokens_dir: &Path) -> Vec<(String, String)> {
    let index_path = tokens_dir.join("index.json");
    let raw = match std::fs::read_to_string(&index_path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let data: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(accounts) = data.get("accounts").and_then(|a| a.as_array()) {
        for entry in accounts {
            let email = entry.get("email").and_then(|e| e.as_str());
            let name = entry.get("name").and_then(|n| n.as_str());
            if let (Some(email), Some(name)) = (email, name) {
                if email.is_empty() || name.is_empty() {
                    continue;
                }
                let key = email.trim().to_ascii_lowercase();
                // Dict semantics: update value in place, keep first position.
                if let Some(existing) = out.iter_mut().find(|(k, _)| *k == key) {
                    existing.1 = name.to_string();
                } else {
                    out.push((key, name.to_string()));
                }
            }
        }
    }
    out
}

/// Read + validate `ranking.json`; `None` when absent, unreadable, not valid
/// JSON, not an object, or an unsupported `schema`.
fn load_ranking_json(monitor_dir: &Path) -> Option<serde_json::Value> {
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

fn coerce_float(value: Option<&serde_json::Value>) -> Option<f64> {
    match value {
        Some(serde_json::Value::Bool(_)) | None => None,
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(serde_json::Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn order_key(a: &MonitorAccount) -> (i32, f64, f64) {
    (
        status_rank(&a.status),
        a.util_7d.unwrap_or(UTIL_SENTINEL),
        a.util_5h.unwrap_or(UTIL_SENTINEL),
    )
}

/// Translate a fresh `ranking.json` into ordered [`MonitorAccount`]s, or `None`
/// when no usable, fresh data exists (caller degrades to probe under `auto`,
/// or emits nothing under `monitor`). Mirrors `monitor.build_monitor_accounts`.
///
/// `now` is injectable for tests; production passes `None` (uses `Utc::now`).
#[must_use]
pub fn build_monitor_accounts(
    tokens_dir: &Path,
    monitor_dir: Option<&Path>,
    now: Option<DateTime<Utc>>,
) -> Option<Vec<MonitorAccount>> {
    let owned_dir;
    let monitor_dir = match monitor_dir {
        Some(d) => d,
        None => {
            owned_dir = claude_monitor_dir();
            &owned_dir
        }
    };

    let data = load_ranking_json(monitor_dir)?;
    let now = now.unwrap_or_else(Utc::now);
    if !is_fresh(data.get("generated_at").and_then(|g| g.as_str()), now) {
        return None;
    }

    let email_to_name = load_index_email_map(tokens_dir);
    if email_to_name.is_empty() {
        return None;
    }
    let lookup = |email_lower: &str| -> Option<&str> {
        email_to_name
            .iter()
            .find(|(k, _)| k == email_lower)
            .map(|(_, name)| name.as_str())
    };

    let mut accounts: Vec<MonitorAccount> = Vec::new();
    let mut matched: Vec<String> = Vec::new();

    if let Some(entries) = data.get("accounts").and_then(|a| a.as_array()) {
        for entry in entries {
            let email = match entry.get("email").and_then(|e| e.as_str()) {
                Some(e) if !e.is_empty() => e,
                _ => continue,
            };
            let name = match lookup(&email.trim().to_ascii_lowercase()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let status = entry
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let (util_7d, util_5h) = match entry.get("utilization") {
                Some(serde_json::Value::Object(util)) => {
                    (coerce_float(util.get("7d")), coerce_float(util.get("5h")))
                }
                _ => (None, None),
            };
            matched.push(name.clone());
            accounts.push(MonitorAccount {
                name,
                status,
                util_7d,
                util_5h,
            });
        }
    }

    // Represent manifest accounts the monitor did not mention (unknown status,
    // sorts last) so the selector still sees them.
    for (_, name) in &email_to_name {
        if !matched.contains(name) {
            matched.push(name.clone());
            accounts.push(MonitorAccount {
                name: name.clone(),
                status: String::new(),
                util_7d: None,
                util_5h: None,
            });
        }
    }

    if accounts.is_empty() {
        return None;
    }

    accounts.sort_by(|a, b| {
        let (ar, a7, a5) = order_key(a);
        let (br, b7, b5) = order_key(b);
        ar.cmp(&br).then(a7.total_cmp(&b7)).then(a5.total_cmp(&b5))
    });
    Some(accounts)
}

/// Serialize ordered accounts to the selector's `name|status` format (trailing
/// newline; empty -> empty string). Mirrors `monitor.format_ranking_lines`.
#[must_use]
pub fn format_ranking_lines(accounts: &[MonitorAccount]) -> String {
    if accounts.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for a in accounts {
        out.push_str(&a.name);
        out.push('|');
        out.push_str(&a.status);
        out.push('\n');
    }
    out
}

/// Atomic write of arbitrary ranking text (`<path>.tmp` + rename in the same
/// directory). Shared by the probe and monitor writers.
pub fn write_ranking_text(text: &str, ranking_path: &Path) -> std::io::Result<()> {
    if let Some(parent) = ranking_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = ranking_path.with_file_name(format!(
        "{}.tmp",
        ranking_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(".ranking")
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
    }
    std::fs::rename(&tmp, ranking_path)
}

/// Write the monitor-sourced `.ranking` (pipe format) atomically. Mirrors
/// `monitor.write_monitor_ranking_atomic`.
pub fn write_monitor_ranking_atomic(
    accounts: &[MonitorAccount],
    ranking_path: &Path,
) -> std::io::Result<()> {
    write_ranking_text(&format_ranking_lines(accounts), ranking_path)
}

/// Build a [`ProbeReport`] from claude-monitor's `ranking.json`, or `None` when
/// no fresh data is available. When `write_ranking` is set and data exists,
/// emits `.ranking` in the selector's pipe format (raw statuses). Mirrors
/// `check._run_monitor_check`.
pub fn run_monitor_check(
    tokens_dir: &Path,
    write_ranking: bool,
    ranked_at_fn: impl Fn() -> String,
) -> Option<ProbeReport> {
    let accounts = build_monitor_accounts(tokens_dir, None, None)?;

    // The in-memory report uses "available" for an empty status; the WRITER
    // uses the raw status (empty stays empty) — matches check._run_monitor_check.
    let results: Vec<AccountResult> = accounts
        .iter()
        .map(|a| AccountResult {
            name: a.name.clone(),
            status: if a.status.is_empty() {
                "available".to_string()
            } else {
                a.status.clone()
            },
            s5h_utilization: a.util_5h,
            s7d_utilization: a.util_7d,
            s7d_reset: None,
            error: None,
        })
        .collect();

    if write_ranking {
        let ranking_path = tokens_dir.join(".ranking");
        if let Err(e) = write_monitor_ranking_atomic(&accounts, &ranking_path) {
            eprintln!("WARNING failed to write {}: {e}", ranking_path.display());
        }
    }

    Some(ProbeReport {
        ranked_at: ranked_at_fn(),
        accounts: results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fresh_now() -> DateTime<Utc> {
        Utc::now()
    }

    fn write_index(tokens_dir: &Path, pairs: &[(&str, &str)]) {
        let accounts: Vec<serde_json::Value> = pairs
            .iter()
            .map(|(name, email)| serde_json::json!({"name": name, "email": email}))
            .collect();
        fs::create_dir_all(tokens_dir).unwrap();
        fs::write(
            tokens_dir.join("index.json"),
            serde_json::json!({"version": 2, "accounts": accounts}).to_string(),
        )
        .unwrap();
    }

    fn write_ranking_json(monitor_dir: &Path, generated_at: &str, accounts: serde_json::Value) {
        fs::create_dir_all(monitor_dir).unwrap();
        fs::write(
            monitor_dir.join("ranking.json"),
            serde_json::json!({
                "schema": 1,
                "generated_at": generated_at,
                "accounts": accounts,
            })
            .to_string(),
        )
        .unwrap();
    }

    fn iso(dt: DateTime<Utc>) -> String {
        dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    #[test]
    fn freshness_boundary() {
        let now = Utc::now();
        // 599s old -> fresh.
        assert!(is_fresh(Some(&iso(now - chrono::Duration::seconds(599))), now));
        // 601s old -> stale.
        assert!(!is_fresh(Some(&iso(now - chrono::Duration::seconds(601))), now));
        // exactly 600 -> stale (half-open window).
        assert!(!is_fresh(Some(&iso(now - chrono::Duration::seconds(600))), now));
        // undated -> stale.
        assert!(!is_fresh(None, now));
        // future -> stale (negative age).
        assert!(!is_fresh(Some(&iso(now + chrono::Duration::seconds(60))), now));
    }

    #[test]
    fn build_accounts_orders_by_status_then_util() {
        let tmp = tempfile::tempdir().unwrap();
        let tokens_dir = tmp.path().join("tokens");
        let monitor_dir = tmp.path().join("monitor");
        write_index(
            &tokens_dir,
            &[
                ("acct-a", "a@example.com"),
                ("acct-b", "b@example.com"),
                ("acct-c", "c@example.com"),
            ],
        );
        let now = fresh_now();
        write_ranking_json(
            &monitor_dir,
            &iso(now),
            serde_json::json!([
                {"email": "b@example.com", "status": "available", "utilization": {"7d": 0.80, "5h": 0.10}},
                {"email": "a@example.com", "status": "available", "utilization": {"7d": 0.20, "5h": 0.10}},
                {"email": "c@example.com", "status": "rate_limited", "utilization": {"7d": 0.10, "5h": 0.90}},
            ]),
        );

        let accounts = build_monitor_accounts(&tokens_dir, Some(&monitor_dir), Some(now)).unwrap();
        let ordered: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
        // available (a: 0.20) < available (b: 0.80) < rate_limited (c).
        assert_eq!(ordered, ["acct-a", "acct-b", "acct-c"]);
    }

    #[test]
    fn build_accounts_appends_unmentioned_manifest_accounts_last() {
        let tmp = tempfile::tempdir().unwrap();
        let tokens_dir = tmp.path().join("tokens");
        let monitor_dir = tmp.path().join("monitor");
        write_index(&tokens_dir, &[("acct-a", "a@example.com"), ("acct-z", "z@example.com")]);
        let now = fresh_now();
        write_ranking_json(
            &monitor_dir,
            &iso(now),
            serde_json::json!([
                {"email": "a@example.com", "status": "available", "utilization": {"7d": 0.50}},
            ]),
        );
        let accounts = build_monitor_accounts(&tokens_dir, Some(&monitor_dir), Some(now)).unwrap();
        // acct-z (unmentioned, empty status -> rank 99) sorts last.
        assert_eq!(accounts.last().unwrap().name, "acct-z");
        assert_eq!(accounts.last().unwrap().status, "");
    }

    #[test]
    fn stale_ranking_json_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let tokens_dir = tmp.path().join("tokens");
        let monitor_dir = tmp.path().join("monitor");
        write_index(&tokens_dir, &[("acct-a", "a@example.com")]);
        let now = Utc::now();
        write_ranking_json(
            &monitor_dir,
            &iso(now - chrono::Duration::seconds(3600)),
            serde_json::json!([{"email": "a@example.com", "status": "available"}]),
        );
        assert!(build_monitor_accounts(&tokens_dir, Some(&monitor_dir), Some(now)).is_none());
    }

    #[test]
    fn unsupported_schema_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let tokens_dir = tmp.path().join("tokens");
        let monitor_dir = tmp.path().join("monitor");
        write_index(&tokens_dir, &[("acct-a", "a@example.com")]);
        fs::create_dir_all(&monitor_dir).unwrap();
        fs::write(
            monitor_dir.join("ranking.json"),
            serde_json::json!({"schema": 2, "generated_at": iso(Utc::now())}).to_string(),
        )
        .unwrap();
        assert!(build_monitor_accounts(&tokens_dir, Some(&monitor_dir), None).is_none());
    }

    #[test]
    fn missing_index_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let tokens_dir = tmp.path().join("tokens");
        let monitor_dir = tmp.path().join("monitor");
        fs::create_dir_all(&tokens_dir).unwrap();
        let now = fresh_now();
        write_ranking_json(
            &monitor_dir,
            &iso(now),
            serde_json::json!([{"email": "a@example.com", "status": "available"}]),
        );
        assert!(build_monitor_accounts(&tokens_dir, Some(&monitor_dir), Some(now)).is_none());
    }

    #[test]
    fn format_ranking_lines_manifest_only_has_empty_status() {
        let accounts = vec![MonitorAccount {
            name: "acct-z".into(),
            status: String::new(),
            util_7d: None,
            util_5h: None,
        }];
        assert_eq!(format_ranking_lines(&accounts), "acct-z|\n");
    }

    #[test]
    fn run_monitor_check_writes_raw_status_but_report_defaults_available() {
        let tmp = tempfile::tempdir().unwrap();
        let tokens_dir = tmp.path().join("tokens");
        let monitor_dir = tmp.path().join("monitor");
        write_index(&tokens_dir, &[("acct-a", "a@example.com"), ("acct-z", "z@example.com")]);
        let now = fresh_now();
        write_ranking_json(
            &monitor_dir,
            &iso(now),
            serde_json::json!([
                {"email": "a@example.com", "status": "available", "utilization": {"7d": 0.50}},
            ]),
        );
        // Point the default monitor dir at our fixture via env.
        std::env::set_var(CLAUDE_MONITOR_DIR_VAR, &monitor_dir);
        let report = run_monitor_check(&tokens_dir, true, || "2026-01-01T00:00:00Z".to_string());
        std::env::remove_var(CLAUDE_MONITOR_DIR_VAR);

        let report = report.unwrap();
        // In-memory report: acct-z's empty status becomes "available".
        let z = report.accounts.iter().find(|a| a.name == "acct-z").unwrap();
        assert_eq!(z.status, "available");

        // Written .ranking: acct-z keeps its RAW empty status.
        let ranking = fs::read_to_string(tokens_dir.join(".ranking")).unwrap();
        assert!(ranking.contains("acct-z|\n"));
        assert!(ranking.contains("acct-a|available\n"));
    }

    #[test]
    fn coerce_float_variants() {
        assert_eq!(coerce_float(Some(&serde_json::json!(0.5))), Some(0.5));
        assert_eq!(coerce_float(Some(&serde_json::json!("0.25"))), Some(0.25));
        assert_eq!(coerce_float(Some(&serde_json::json!(true))), None);
        assert_eq!(coerce_float(Some(&serde_json::json!("bad"))), None);
        assert_eq!(coerce_float(None), None);
    }
}
