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
    /// When the account's 7d window resets (`accounts[].resets["7d"]` in
    /// `ranking.json`), normalized to `%Y-%m-%dT%H:%M:%SZ`. `None` when the
    /// monitor did not report one — never a fabricated instant (issue #4874).
    pub reset_7d: Option<String>,
    /// When the account's 5h window resets (`accounts[].resets["5h"]`), same
    /// normalization. This is the release instant for a `rate_limited`
    /// account, and the rollover the 5h utilization is racing for every other
    /// status — see [`super::check::limit_reset`].
    pub reset_5h: Option<String>,
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

/// Normalize a monitor-reported reset instant to the canonical
/// `%Y-%m-%dT%H:%M:%SZ` text the `.ranking` writer emits (issue #4874).
/// Anything unparseable as a timestamp yields `None` — an account with no
/// usable reset stays "unknown" rather than carrying junk downstream to the
/// dashboard's countdown.
fn coerce_reset(value: Option<&serde_json::Value>) -> Option<String> {
    let raw = value?.as_str()?;
    parse_iso8601(Some(raw)).map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
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
    // Index of `accounts` by Loom account name so a second `ranking.json` row
    // that resolves to a name already pushed by an earlier iteration of this
    // same loop updates that entry instead of appending a duplicate (issue
    // #4873 — two rows for one account inflated `capacity.total_accounts`).
    // This can happen when `load_index_email_map` legitimately carries two
    // different emails for one Loom account (e.g. a stale row left behind by
    // a re-auth/rotation) and `ranking.json` has a row for each. Merge rule,
    // chosen deliberately: **the more severe status wins** (`status_rank`,
    // higher = worse) — a scheduler must never treat an account as available
    // when another record for the same name says it is rate_limited or
    // exhausted; ties (equal severity) keep the earlier row.
    let mut index_by_name: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

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
            // `accounts[].resets` is the sibling of `utilization` that
            // claude-monitor already publishes (`{"5h": "...", "7d": "..."}`).
            // Both windows are read: which one is *binding* depends on the
            // status, and `check::limit_reset` makes that call at write time
            // (issue #4874).
            let (reset_7d, reset_5h) = match entry.get("resets") {
                Some(serde_json::Value::Object(resets)) => {
                    (coerce_reset(resets.get("7d")), coerce_reset(resets.get("5h")))
                }
                _ => (None, None),
            };
            let candidate = MonitorAccount {
                name: name.clone(),
                status,
                util_7d,
                util_5h,
                reset_7d,
                reset_5h,
            };
            if let Some(&idx) = index_by_name.get(&name) {
                if status_rank(&candidate.status) > status_rank(&accounts[idx].status) {
                    accounts[idx] = candidate;
                }
                // else: an earlier, equal-or-more-severe row already won —
                // drop this duplicate.
            } else {
                index_by_name.insert(name.clone(), accounts.len());
                matched.push(name);
                accounts.push(candidate);
            }
        }
    }

    // Represent manifest accounts the monitor did not mention (no usage rows
    // yet — e.g. freshly bootstrapped/never-used accounts) as `available` so
    // they are dispatchable: the table display, the written `.ranking` row,
    // and the daemon's healthy-count reader all agree on this single status
    // (issue #4645 — the empty-status writer bug that made fresh capacity
    // dispatch-invisible). Their unknown utilization sorts them after
    // known-utilization `available` accounts via `UTIL_SENTINEL`, but they
    // still rank ahead of `rate_limited`/`exhausted` accounts.
    //
    // `matched` is keyed by Loom account **name**, not email, so this is
    // already dedup-safe against `load_index_email_map` legitimately holding
    // two different emails for one name (issue #4873): once the first loop
    // above matches *either* email to a `ranking.json` row for that name, the
    // name is in `matched` and neither email's fallback entry here fires.
    for (_, name) in &email_to_name {
        if !matched.contains(name) {
            matched.push(name.clone());
            accounts.push(MonitorAccount {
                name: name.clone(),
                status: "available".to_string(),
                util_7d: None,
                util_5h: None,
                reset_7d: None,
                reset_5h: None,
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

/// Serialize ordered accounts to the selector's
/// `name|status|5h_util|limit_reset` format (trailing newline; empty -> empty
/// string). The 5h utilization (issue #4195) and the binding-window reset
/// (issue #4874) are optional trailing fields, emitted when known;
/// byte-identical with the probe writer (`check::ranking_line`), including the
/// [`super::check::limit_reset`] choice of *which* window to report.
#[must_use]
pub fn format_ranking_lines(accounts: &[MonitorAccount]) -> String {
    if accounts.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for a in accounts {
        out.push_str(&super::check::ranking_line(
            &a.name,
            &a.status,
            a.util_5h,
            super::check::limit_reset(&a.status, a.reset_5h.as_deref(), a.reset_7d.as_deref()),
        ));
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

    // Single source of truth: `build_monitor_accounts` already normalizes
    // monitor-unmentioned accounts to `available` (issue #4645), so the
    // in-memory report (used for the CLI table) and the `.ranking` writer
    // below both serialize the exact same status — they cannot disagree.
    let results: Vec<AccountResult> = accounts
        .iter()
        .map(|a| AccountResult {
            name: a.name.clone(),
            status: a.status.clone(),
            s5h_utilization: a.util_5h,
            s7d_utilization: a.util_7d,
            // Issue #4874: the monitor path used to hardcode `None` for both
            // resets, so the CLI's reset column was empty on every
            // monitor-sourced run even though `ranking.json` carries them.
            s7d_reset: a.reset_7d.clone(),
            s5h_reset: a.reset_5h.clone(),
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
        // acct-z (unmentioned) is normalized to "available" (issue #4645) but
        // its unknown utilization sorts it after acct-a's known 0.50 within
        // the same `available` rank bucket.
        assert_eq!(accounts.last().unwrap().name, "acct-z");
        assert_eq!(accounts.last().unwrap().status, "available");
    }

    #[test]
    fn build_accounts_dedupes_two_ranking_rows_for_one_name() {
        // Regression test for issue #4873: `index.json` carries two different
        // emails for the same Loom account (a stale row left behind by a
        // re-auth/rotation), and `ranking.json` has a row for each. Only one
        // `MonitorAccount` must survive for that name — and the merge picks
        // the more severe status (`exhausted` beats `available`).
        let tmp = tempfile::tempdir().unwrap();
        let tokens_dir = tmp.path().join("tokens");
        let monitor_dir = tmp.path().join("monitor");
        write_index(
            &tokens_dir,
            &[
                ("acct-a", "a-old@example.com"),
                ("acct-a", "a-new@example.com"),
                ("acct-b", "b@example.com"),
            ],
        );
        let now = fresh_now();
        write_ranking_json(
            &monitor_dir,
            &iso(now),
            serde_json::json!([
                {"email": "a-old@example.com", "status": "available", "utilization": {"7d": 0.10, "5h": 0.10}},
                {"email": "a-new@example.com", "status": "exhausted", "utilization": {"7d": 0.99, "5h": 0.99}},
                {"email": "b@example.com", "status": "available", "utilization": {"7d": 0.20, "5h": 0.20}},
            ]),
        );

        let accounts = build_monitor_accounts(&tokens_dir, Some(&monitor_dir), Some(now)).unwrap();
        let matches: Vec<&MonitorAccount> =
            accounts.iter().filter(|a| a.name == "acct-a").collect();
        assert_eq!(matches.len(), 1, "exactly one row must survive for a duplicated name");
        assert_eq!(matches[0].status, "exhausted", "the more severe status wins the merge");
        assert_eq!(accounts.len(), 2, "acct-a (deduped) + acct-b, never 3");
    }

    #[test]
    fn build_accounts_dedupes_ranking_row_plus_unmentioned_fallback() {
        // Regression test for issue #4873 (the second observed shape): one
        // email for a name is matched by a `ranking.json` row, while a
        // second, different email for the *same* name is absent from
        // `ranking.json` and would otherwise fall through to the
        // "unmentioned manifest account" path (issue #4645). The name-keyed
        // `matched` guard must suppress the fallback duplicate.
        let tmp = tempfile::tempdir().unwrap();
        let tokens_dir = tmp.path().join("tokens");
        let monitor_dir = tmp.path().join("monitor");
        write_index(
            &tokens_dir,
            &[
                ("acct-a", "a-matched@example.com"),
                ("acct-a", "a-unmentioned@example.com"),
                ("acct-z", "z@example.com"),
            ],
        );
        let now = fresh_now();
        write_ranking_json(
            &monitor_dir,
            &iso(now),
            serde_json::json!([
                {"email": "a-matched@example.com", "status": "rate_limited", "utilization": {"7d": 0.5, "5h": 0.5}},
            ]),
        );

        let accounts = build_monitor_accounts(&tokens_dir, Some(&monitor_dir), Some(now)).unwrap();
        let matches: Vec<&MonitorAccount> =
            accounts.iter().filter(|a| a.name == "acct-a").collect();
        assert_eq!(matches.len(), 1, "exactly one row must survive for acct-a");
        assert_eq!(
            matches[0].status, "rate_limited",
            "the matched ranking.json row wins, not the fallback"
        );
        // acct-z is genuinely absent from ranking.json -> still gets the
        // unmentioned-manifest fallback (issue #4645 preserved). It sorts
        // ahead of acct-a here (available < rate_limited in status_rank).
        assert_eq!(accounts.first().unwrap().name, "acct-z");
        assert_eq!(accounts.first().unwrap().status, "available");
        assert_eq!(accounts.len(), 2, "acct-a (deduped) + acct-z, never 3");
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
    fn format_ranking_lines_passes_through_whatever_status_it_is_given() {
        // `format_ranking_lines` is a pure formatter — it does not normalize an
        // empty status. `build_monitor_accounts` is the seam responsible for
        // never handing it one (issue #4645); this test documents the
        // formatter's own (unopinionated) behavior in isolation.
        let accounts = vec![MonitorAccount {
            name: "acct-z".into(),
            status: String::new(),
            util_7d: None,
            util_5h: None,
            reset_7d: None,
            reset_5h: None,
        }];
        assert_eq!(format_ranking_lines(&accounts), "acct-z|\n");
    }

    #[test]
    fn format_ranking_lines_emits_5h_util_third_field() {
        // The 5h utilization is the optional third field (issue #4195), fixed
        // at 2 decimals; an absent value keeps the legacy 2-field form.
        let accounts = vec![
            MonitorAccount {
                name: "a".into(),
                status: "available".into(),
                util_7d: Some(0.20),
                util_5h: Some(0.90),
                reset_7d: None,
                reset_5h: None,
            },
            MonitorAccount {
                name: "b".into(),
                status: "available".into(),
                util_7d: Some(0.10),
                util_5h: None,
                reset_7d: None,
                reset_5h: None,
            },
        ];
        assert_eq!(format_ranking_lines(&accounts), "a|available|0.90\nb|available\n");
    }

    #[test]
    fn format_ranking_lines_emits_the_binding_window_reset_fourth_field() {
        // Issue #4874: the monitor backend now carries a reset instant it reads
        // from `ranking.json` into the `.ranking` file's fourth field,
        // byte-identically with the probe writer — and it writes the reset of
        // whichever window is *binding*, not always the 7d one.
        let exhausted = vec![MonitorAccount {
            name: "a".into(),
            status: "exhausted".into(),
            util_7d: Some(1.0),
            util_5h: Some(0.0),
            reset_7d: Some("2026-08-02T03:00:00Z".into()),
            reset_5h: Some("2026-08-01T05:20:00Z".into()),
        }];
        assert_eq!(
            format_ranking_lines(&exhausted),
            "a|exhausted|0.00|2026-08-02T03:00:00Z\n",
            "an exhausted account is held by the 7d window"
        );

        // The live-host case that made "always write the 7d reset" wrong:
        // `r.j.walters@gmail.com` was `rate_limited` with a 5h reset ~1.6h out
        // and a 7d reset SIX DAYS out. Counting down to the 7d one would have
        // told the operator the fleet was stalled until Saturday.
        let rate_limited = vec![MonitorAccount {
            name: "b".into(),
            status: "rate_limited".into(),
            util_7d: Some(0.60),
            util_5h: Some(1.0),
            reset_7d: Some("2026-08-07T01:00:00Z".into()),
            reset_5h: Some("2026-08-01T07:00:00Z".into()),
        }];
        assert_eq!(
            format_ranking_lines(&rate_limited),
            "b|rate_limited|1.00|2026-08-01T07:00:00Z\n",
            "a rate_limited account is held by the 5h window, not the 7d one"
        );

        // A healthy account reports the rollover its 5h utilization — the
        // `usage_fraction` the dashboard charts — is actually racing.
        let available = vec![MonitorAccount {
            name: "c".into(),
            status: "available".into(),
            util_7d: Some(0.30),
            util_5h: Some(0.12),
            reset_7d: Some("2026-08-04T23:00:00Z".into()),
            reset_5h: Some("2026-08-01T06:50:00Z".into()),
        }];
        assert_eq!(format_ranking_lines(&available), "c|available|0.12|2026-08-01T06:50:00Z\n");
    }

    #[test]
    fn run_monitor_check_writes_available_for_unmentioned_manifest_account() {
        // Regression test for issue #4645: a manifest account the monitor DB
        // never mentions (e.g. freshly bootstrapped/never-used) must serialize
        // as a parseable `name|available` row — never a malformed `name|` row
        // — and the in-memory report (CLI table) must agree with what gets
        // written, since both come from the same `build_monitor_accounts`
        // output.
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
        // In-memory report (table display): acct-z is "available".
        let z = report.accounts.iter().find(|a| a.name == "acct-z").unwrap();
        assert_eq!(z.status, "available");

        // Written .ranking: acct-z is ALSO "available" — a parseable row, not
        // a `name|` malformed one. Table and writer agree (single source).
        let ranking = fs::read_to_string(tokens_dir.join(".ranking")).unwrap();
        assert!(!ranking.contains("acct-z|\n"), "malformed empty-status row must not be written");
        assert!(ranking.contains("acct-z|available\n"));
        assert!(ranking.contains("acct-a|available\n"));

        // Capacity's healthy-count reader must count the never-used account.
        let snap = crate::capacity::read_ranking_at(&tokens_dir).unwrap();
        assert_eq!(snap.total, 2);
        assert_eq!(snap.available, 2);
    }

    #[test]
    fn coerce_float_variants() {
        assert_eq!(coerce_float(Some(&serde_json::json!(0.5))), Some(0.5));
        assert_eq!(coerce_float(Some(&serde_json::json!("0.25"))), Some(0.25));
        assert_eq!(coerce_float(Some(&serde_json::json!(true))), None);
        assert_eq!(coerce_float(Some(&serde_json::json!("bad"))), None);
        assert_eq!(coerce_float(None), None);
    }

    #[test]
    fn coerce_reset_variants() {
        // Normalized to the canonical `.ranking` instant format; anything not
        // a parseable timestamp is "unknown", never carried through as junk.
        assert_eq!(
            coerce_reset(Some(&serde_json::json!("2026-08-02T03:00:00Z"))),
            Some("2026-08-02T03:00:00Z".to_string())
        );
        // An offset instant is normalized to UTC `Z`.
        assert_eq!(
            coerce_reset(Some(&serde_json::json!("2026-08-01T23:00:00-04:00"))),
            Some("2026-08-02T03:00:00Z".to_string())
        );
        assert_eq!(coerce_reset(Some(&serde_json::json!("not-a-date"))), None);
        assert_eq!(coerce_reset(Some(&serde_json::json!(""))), None);
        assert_eq!(coerce_reset(Some(&serde_json::json!(1_754_103_600))), None);
        assert_eq!(coerce_reset(None), None);
    }

    #[test]
    fn build_monitor_accounts_surfaces_both_resets_when_present() {
        // Fixture is a verbatim reduction of this host's real
        // `~/.claude-monitor/ranking.json` (issue #4874): `resets` is a sibling
        // of `utilization`, keyed by window, and both windows are reported.
        let tmp = tempfile::tempdir().unwrap();
        let tokens_dir = tmp.path().join("tokens");
        let monitor_dir = tmp.path().join("monitor");
        write_index(&tokens_dir, &[("acct-a", "a@example.com"), ("acct-b", "b@example.com")]);
        let now = fresh_now();
        write_ranking_json(
            &monitor_dir,
            &iso(now),
            serde_json::json!([
                {
                    "email": "a@example.com",
                    "status": "exhausted",
                    "utilization": {"5h": 0.0, "7d": 1.0},
                    "resets": {"5h": "2026-08-01T05:20:00Z", "7d": "2026-08-02T03:00:00Z"},
                },
                // No `resets` at all — must stay `None`, not inherit a sibling's.
                {"email": "b@example.com", "status": "available", "utilization": {"7d": 0.30}},
            ]),
        );
        let accounts =
            build_monitor_accounts(&tokens_dir, Some(&monitor_dir), Some(now)).expect("fresh");

        let a = accounts.iter().find(|a| a.name == "acct-a").unwrap();
        assert_eq!(
            a.reset_7d.as_deref(),
            Some("2026-08-02T03:00:00Z"),
            "the 7d reset must not pick up the 5h one"
        );
        assert_eq!(
            a.reset_5h.as_deref(),
            Some("2026-08-01T05:20:00Z"),
            "the 5h reset is read too — it is the release instant for a rate_limited account"
        );
        let b = accounts.iter().find(|a| a.name == "acct-b").unwrap();
        assert_eq!(b.reset_7d, None, "an account with no resets block stays unknown");
        assert_eq!(b.reset_5h, None);
    }

    #[test]
    fn run_monitor_check_reports_and_writes_the_binding_reset() {
        // End-to-end for the monitor backend (issue #4874): the CLI table
        // (`ProbeReport`) and the written `.ranking` must BOTH carry the reset,
        // and both must name the *binding* window. Before this,
        // `run_monitor_check` hardcoded `s7d_reset: None`, so the reset column
        // was empty on every monitor-sourced run even though `ranking.json` had
        // the instants all along.
        let tmp = tempfile::tempdir().unwrap();
        let tokens_dir = tmp.path().join("tokens");
        let monitor_dir = tmp.path().join("monitor");
        write_index(&tokens_dir, &[("acct-a", "a@example.com"), ("acct-b", "b@example.com")]);
        let now = fresh_now();
        write_ranking_json(
            &monitor_dir,
            &iso(now),
            serde_json::json!([
                {
                    "email": "a@example.com",
                    "status": "exhausted",
                    "utilization": {"5h": 0.0, "7d": 1.0},
                    "resets": {"5h": "2026-08-01T05:20:00Z", "7d": "2026-08-02T03:00:00Z"},
                },
                {
                    "email": "b@example.com",
                    "status": "rate_limited",
                    "utilization": {"5h": 1.0, "7d": 0.60},
                    "resets": {"5h": "2026-08-01T07:00:00Z", "7d": "2026-08-07T01:00:00Z"},
                },
            ]),
        );
        std::env::set_var(CLAUDE_MONITOR_DIR_VAR, &monitor_dir);
        let report = run_monitor_check(&tokens_dir, true, || "2026-01-01T00:00:00Z".to_string());
        std::env::remove_var(CLAUDE_MONITOR_DIR_VAR);

        let report = report.unwrap();
        let a = report.accounts.iter().find(|a| a.name == "acct-a").unwrap();
        assert_eq!(a.s7d_reset.as_deref(), Some("2026-08-02T03:00:00Z"));
        assert_eq!(a.limit_reset(), Some("2026-08-02T03:00:00Z"));
        let b = report.accounts.iter().find(|a| a.name == "acct-b").unwrap();
        assert_eq!(
            b.limit_reset(),
            Some("2026-08-01T07:00:00Z"),
            "rate_limited returns at the 5h boundary, not six days out at the 7d one"
        );

        let ranking = fs::read_to_string(tokens_dir.join(".ranking")).unwrap();
        assert!(
            ranking.contains("acct-a|exhausted|0.00|2026-08-02T03:00:00Z\n"),
            "unexpected .ranking body: {ranking:?}"
        );
        assert!(
            ranking.contains("acct-b|rate_limited|1.00|2026-08-01T07:00:00Z\n"),
            "unexpected .ranking body: {ranking:?}"
        );
    }
}
