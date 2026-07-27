//! Account health probe + ranking for the agent token pool.
//!
//! Native Rust port of `loom_tools.tokens.check` (issue #4094, epic #4081
//! "eliminate Python from Loom", Phase 1). Probes each bootstrapped OAuth
//! account with a minimal Anthropic `POST /v1/messages` request and parses
//! rate-limit headers to derive session (5h) and weekly (7d) utilization plus
//! the next 7d reset time, then writes `.loom/tokens/.ranking` atomically.
//!
//! # HTTP mechanism: curl, not a crate
//!
//! `loom-daemon` carries **zero** HTTP-client crates, and PR #4092 (issue
//! #4082) set the house precedent of avoiding new dependencies for the token
//! pool (it hand-rolled [`super::rng`] rather than take `rand`). Following that
//! precedent — and matching `token_ranking_refresh.rs`, which already shells
//! out via `Command::new` — the probe transport shells to **`curl`**. The
//! probe surface is exercised in tests through the [`ProbeTransport`] trait so
//! no test ever spawns `curl` or touches the network.
//!
//! # Header resilience
//!
//! Rate-limit headers are matched by **suffix**, case-insensitively
//! (`-5h-utilization`, `-7d-utilization`, `-7d-reset`), so any rename of the
//! `anthropic-ratelimit-*` prefix segment still maps to our internal fields.
//!
//! # Byte-compatible `.ranking`
//!
//! `.ranking` is pipe-delimited `name|status`, one account per line, trailing
//! newline (empty report yields an empty string), ordered by
//! [`STATUS_RANK`] then `7d_reset` ascending with an absent-reset sentinel.
//! **Every** status is written — no status is filtered at write time (see the
//! curator correction on issue #4094: the read-side allowlist in
//! [`super::select`] depends on `rate_limited` entries being present so a
//! fully-rate-limited pool can still dispatch via the tier-1 fallback). The
//! file is written atomically (`<path>.tmp` + rename in the same directory).

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ---------------------------------------------------------------------------
// Constants (mirror check.py)
// ---------------------------------------------------------------------------

/// Anthropic messages endpoint the probe posts to.
pub const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";
const USER_AGENT: &str = "loom-tokens/0.1 (claude-code-compatible)";
/// Default model used for the minimal probe request.
pub const DEFAULT_PROBE_MODEL: &str = "claude-haiku-4-5-20251001";
/// Default probe prompt (`max_tokens=1` regardless of prompt).
pub const DEFAULT_PROBE_PROMPT: &str = "hi";
const DEFAULT_TIMEOUT_SECONDS: f64 = 15.0;
/// 7d-utilization at or above which an account is `exhausted` (issue #3988).
pub const EXHAUSTED_THRESHOLD: f64 = 0.95;

const HEADER_SUFFIX_5H_UTIL: &str = "-5h-utilization";
const HEADER_SUFFIX_7D_UTIL: &str = "-7d-utilization";
const HEADER_SUFFIX_7D_RESET: &str = "-7d-reset";
const HEADER_SUFFIX_5H_STATUS: &str = "-5h-status";

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// Per-account probe result. Mirrors `check.AccountResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountResult {
    pub name: String,
    /// `available` | `exhausted` | `rate_limited` | `blocked` | `error` | `skipped`.
    pub status: String,
    pub s5h_utilization: Option<f64>,
    pub s7d_utilization: Option<f64>,
    pub s7d_reset: Option<String>,
    pub error: Option<String>,
}

impl AccountResult {
    fn new(name: impl Into<String>, status: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: status.into(),
            s5h_utilization: None,
            s7d_utilization: None,
            s7d_reset: None,
            error: None,
        }
    }

    /// JSON shape matching `check.AccountResult.to_dict`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("name".into(), serde_json::Value::String(self.name.clone()));
        obj.insert("status".into(), serde_json::Value::String(self.status.clone()));
        obj.insert("5h_utilization".into(), float_or_null(self.s5h_utilization));
        obj.insert("7d_utilization".into(), float_or_null(self.s7d_utilization));
        obj.insert(
            "7d_reset".into(),
            self.s7d_reset
                .clone()
                .map_or(serde_json::Value::Null, serde_json::Value::String),
        );
        if let Some(err) = &self.error {
            obj.insert("error".into(), serde_json::Value::String(err.clone()));
        }
        serde_json::Value::Object(obj)
    }
}

fn float_or_null(v: Option<f64>) -> serde_json::Value {
    match v.and_then(serde_json::Number::from_f64) {
        Some(n) => serde_json::Value::Number(n),
        None => serde_json::Value::Null,
    }
}

/// Aggregate probe report across all accounts. Mirrors `check.ProbeReport`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeReport {
    pub ranked_at: String,
    pub accounts: Vec<AccountResult>,
}

impl ProbeReport {
    /// JSON shape matching `check.ProbeReport.to_dict`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ranked_at": self.ranked_at,
            "accounts": self.accounts.iter().map(AccountResult::to_json).collect::<Vec<_>>(),
        })
    }
}

// ---------------------------------------------------------------------------
// HTTP transport abstraction (curl in production, stub in tests)
// ---------------------------------------------------------------------------

/// A successful HTTP response (status + headers; the body is discarded — the
/// probe only reads status and rate-limit headers).
#[derive(Debug, Clone)]
pub struct ProbeResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

/// A transport-level failure (never a 4xx/5xx — those are `ProbeResponse`s).
#[derive(Debug, Clone)]
pub enum ProbeError {
    /// Request exceeded the timeout (curl exit 28). Mirrors `requests.Timeout`.
    Timeout,
    /// DNS/connect failure (curl exit 6/7). Mirrors `requests.ConnectionError`.
    Connection(String),
    /// Any other transport failure. Mirrors a generic `requests.RequestException`.
    Request(String),
}

/// Injectable HTTP transport so the probe is testable without the network.
pub trait ProbeTransport {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
        timeout_secs: f64,
    ) -> Result<ProbeResponse, ProbeError>;
}

/// Production transport: shells to `curl`.
pub struct CurlTransport;

impl ProbeTransport for CurlTransport {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
        timeout_secs: f64,
    ) -> Result<ProbeResponse, ProbeError> {
        // `-D -` dumps the response headers to stdout; `-o /dev/null` discards
        // the body; `-w` appends a sentinel line carrying the numeric status
        // code so we never have to parse the (HTTP/1 vs HTTP/2) status line.
        // The request body is streamed on stdin (`--data-binary @-`) so no
        // shell escaping of the JSON is required.
        let mut cmd = Command::new("curl");
        cmd.arg("--silent")
            .arg("--show-error")
            .arg("--max-time")
            .arg(format!("{timeout_secs}"))
            .arg("-o")
            .arg(if cfg!(windows) { "NUL" } else { "/dev/null" })
            .arg("-D")
            .arg("-")
            .arg("-w")
            .arg("\nLOOM_HTTP_CODE:%{http_code}")
            .arg("-X")
            .arg("POST");
        for (name, value) in headers {
            cmd.arg("-H").arg(format!("{name}: {value}"));
        }
        cmd.arg("--data-binary").arg("@-").arg(url);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| ProbeError::Request(format!("spawn curl: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body.as_bytes());
        }
        let output = child
            .wait_with_output()
            .map_err(|e| ProbeError::Request(format!("curl wait: {e}")))?;

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(match code {
                28 => ProbeError::Timeout,
                6 | 7 => ProbeError::Connection(if stderr.is_empty() {
                    format!("curl exit {code}")
                } else {
                    stderr
                }),
                _ => ProbeError::Request(if stderr.is_empty() {
                    format!("curl exit {code}")
                } else {
                    stderr
                }),
            });
        }

        parse_curl_output(&String::from_utf8_lossy(&output.stdout))
            .ok_or_else(|| ProbeError::Request("could not parse curl output".into()))
    }
}

/// Parse the combined `-D -` header dump + `-w LOOM_HTTP_CODE:` trailer.
fn parse_curl_output(raw: &str) -> Option<ProbeResponse> {
    let mut status: Option<u16> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("LOOM_HTTP_CODE:") {
            status = rest.trim().parse::<u16>().ok();
            continue;
        }
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with("HTTP/") {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    status.map(|status| ProbeResponse { status, headers })
}

// ---------------------------------------------------------------------------
// Token discovery
// ---------------------------------------------------------------------------

/// Read `.bad_tokens` into a set of account names (one per line, `#` comments
/// and blank lines skipped).
fn read_bad_names(tokens_dir: &Path) -> HashSet<String> {
    let mut bad = HashSet::new();
    let bad_file = tokens_dir.join(".bad_tokens");
    if let Ok(text) = std::fs::read_to_string(&bad_file) {
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            bad.insert(line.to_string());
        }
    }
    bad
}

/// Return `[(account_name, token), ...]` for bootstrapped accounts.
///
/// Reads every `*.token` file in `tokens_dir` (sorted by filename) and skips
/// entries listed in `.bad_tokens`, surfacing them with an **empty** token so
/// callers can still emit them as `status: blocked`. Mirrors
/// `check.discover_tokens`.
pub fn discover_tokens(tokens_dir: &Path) -> Vec<(String, String)> {
    if !tokens_dir.is_dir() {
        return Vec::new();
    }
    let bad = read_bad_names(tokens_dir);

    let mut token_files: Vec<PathBuf> = match std::fs::read_dir(tokens_dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("token"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    token_files.sort();

    let mut tokens = Vec::new();
    for path in token_files {
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if bad.contains(&name) {
            tokens.push((name, String::new())); // known-bad: do not probe
            continue;
        }
        let token = match std::fs::read_to_string(&path) {
            Ok(t) => t.trim().to_string(),
            Err(_) => continue,
        };
        if !token.is_empty() {
            tokens.push((name, token));
        }
    }
    tokens
}

// ---------------------------------------------------------------------------
// Header parsing
// ---------------------------------------------------------------------------

fn find_header_by_suffix(headers: &[(String, String)], suffix: &str) -> Option<String> {
    let suffix_lower = suffix.to_ascii_lowercase();
    headers
        .iter()
        .find(|(name, _)| name.to_ascii_lowercase().ends_with(&suffix_lower))
        .map(|(_, value)| value.clone())
}

fn parse_float(raw: Option<&str>) -> Option<f64> {
    raw.and_then(|r| r.trim().parse::<f64>().ok())
}

/// Convert an integer-seconds reset timestamp to ISO-8601 UTC. An unparseable
/// value passes through verbatim (mirrors `check._epoch_to_iso`).
fn epoch_to_iso(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let ts = match raw.parse::<f64>() {
        Ok(t) if t.is_finite() => t,
        // Already ISO-8601, non-finite, or unparseable — pass through.
        _ => return Some(raw.to_string()),
    };
    match chrono::DateTime::<chrono::Utc>::from_timestamp(ts.floor() as i64, 0) {
        Some(dt) => Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        None => Some(raw.to_string()),
    }
}

/// Extracted rate-limit fields.
struct RateLimitFields {
    s5h_utilization: Option<f64>,
    s7d_utilization: Option<f64>,
    s7d_reset: Option<String>,
    #[allow(dead_code)]
    s5h_status: Option<String>,
}

fn parse_rate_limit_headers(headers: &[(String, String)]) -> RateLimitFields {
    RateLimitFields {
        s5h_utilization: parse_float(
            find_header_by_suffix(headers, HEADER_SUFFIX_5H_UTIL).as_deref(),
        ),
        s7d_utilization: parse_float(
            find_header_by_suffix(headers, HEADER_SUFFIX_7D_UTIL).as_deref(),
        ),
        s7d_reset: epoch_to_iso(find_header_by_suffix(headers, HEADER_SUFFIX_7D_RESET).as_deref()),
        s5h_status: find_header_by_suffix(headers, HEADER_SUFFIX_5H_STATUS),
    }
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

fn build_headers(token: &str) -> Vec<(String, String)> {
    let mut headers = vec![
        ("anthropic-version".to_string(), ANTHROPIC_VERSION.to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("user-agent".to_string(), USER_AGENT.to_string()),
    ];
    if token.starts_with("sk-ant-oat") {
        headers.push(("authorization".to_string(), format!("Bearer {token}")));
        headers.push(("anthropic-beta".to_string(), ANTHROPIC_OAUTH_BETA.to_string()));
    } else {
        headers.push(("x-api-key".to_string(), token.to_string()));
    }
    headers
}

/// Return `"exhausted"` when `s7d_util` clears the threshold, else `default`.
/// Applied on both the 2xx and 429 branches (issue #3988).
fn status_from_utilization(s7d_util: Option<f64>, default: &str) -> String {
    if s7d_util.is_some_and(|u| u >= EXHAUSTED_THRESHOLD) {
        "exhausted".to_string()
    } else {
        default.to_string()
    }
}

/// Probe a single account. Transport failures map to `status="error"`; they
/// never propagate as `Err`. Mirrors `check.probe_account`.
pub fn probe_account(
    name: &str,
    token: &str,
    model: &str,
    probe_prompt: &str,
    timeout_secs: f64,
    transport: &dyn ProbeTransport,
) -> AccountResult {
    if token.is_empty() {
        let mut r = AccountResult::new(name, "blocked");
        r.error = Some("bad_token_listed".to_string());
        return r;
    }

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": probe_prompt}],
    })
    .to_string();
    let headers = build_headers(token);

    let resp = match transport.post(ANTHROPIC_MESSAGES_URL, &headers, &body, timeout_secs) {
        Ok(r) => r,
        Err(ProbeError::Timeout) => {
            let mut r = AccountResult::new(name, "error");
            r.error = Some("timeout".to_string());
            return r;
        }
        Err(ProbeError::Connection(msg)) => {
            let mut r = AccountResult::new(name, "error");
            r.error = Some(format!("connection: {msg}"));
            return r;
        }
        Err(ProbeError::Request(msg)) => {
            let mut r = AccountResult::new(name, "error");
            r.error = Some(msg);
            return r;
        }
    };

    let code = resp.status;

    if code == 401 {
        let mut r = AccountResult::new(name, "blocked");
        r.error = Some("auth_401".to_string());
        return r;
    }

    if code == 429 {
        let parsed = parse_rate_limit_headers(&resp.headers);
        let status = status_from_utilization(parsed.s7d_utilization, "rate_limited");
        return AccountResult {
            name: name.to_string(),
            status,
            s5h_utilization: parsed.s5h_utilization,
            s7d_utilization: parsed.s7d_utilization,
            s7d_reset: parsed.s7d_reset,
            error: None,
        };
    }

    if code >= 400 {
        // 5xx and non-401/429 4xx — bad payload/upstream. Treat as error.
        let mut r = AccountResult::new(name, "error");
        r.error = Some(format!("http_{code}"));
        return r;
    }

    // 2xx — successful probe.
    let parsed = parse_rate_limit_headers(&resp.headers);
    let status = status_from_utilization(parsed.s7d_utilization, "available");
    AccountResult {
        name: name.to_string(),
        status,
        s5h_utilization: parsed.s5h_utilization,
        s7d_utilization: parsed.s7d_utilization,
        s7d_reset: parsed.s7d_reset,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Ranking + atomic write
// ---------------------------------------------------------------------------

/// Ranking rank for each status (lower sorts first). Mirrors `_STATUS_RANK`.
#[must_use]
pub fn status_rank(status: &str) -> i32 {
    match status {
        "available" => 0,
        "rate_limited" => 1,
        "exhausted" => 2,
        "blocked" => 3,
        "error" => 4,
        "skipped" => 5,
        _ => 99,
    }
}

/// Absent-reset sentinel so accounts without a 7d reset sort last in-bucket.
const RESET_SENTINEL: &str = "9999-12-31T23:59:59Z";

fn sort_key(a: &AccountResult) -> (i32, String) {
    let reset = a
        .s7d_reset
        .clone()
        .unwrap_or_else(|| RESET_SENTINEL.to_string());
    (status_rank(&a.status), reset)
}

/// Build a sorted report from probe results (stable sort). Mirrors
/// `check.build_report`.
#[must_use]
pub fn build_report(mut results: Vec<AccountResult>) -> ProbeReport {
    let ranked_at = now_iso();
    results.sort_by_key(sort_key);
    ProbeReport {
        ranked_at,
        accounts: results,
    }
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Serialize a report to the selector's `name|status` format (trailing newline,
/// empty report -> empty string). Mirrors `check.format_ranking_lines`.
#[must_use]
pub fn format_ranking_lines(report: &ProbeReport) -> String {
    if report.accounts.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for a in &report.accounts {
        out.push_str(&a.name);
        out.push('|');
        out.push_str(&a.status);
        out.push('\n');
    }
    out
}

/// Write `report` to `ranking_path` atomically (`<path>.tmp` + rename in the
/// same directory). Mirrors `check.write_ranking_atomic`.
pub fn write_ranking_atomic(report: &ProbeReport, ranking_path: &Path) -> std::io::Result<()> {
    super::monitor::write_ranking_text(&format_ranking_lines(report), ranking_path)
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// Where the ranking comes from (`--source`, #3697).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Auto,
    Monitor,
    Probe,
}

impl Source {
    /// Parse a `--source` / `$LOOM_RANKING_SOURCE` value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "monitor" => Some(Self::Monitor),
            "probe" => Some(Self::Probe),
            _ => None,
        }
    }
}

/// Resolve the ranking source: flag > `$LOOM_RANKING_SOURCE` > `auto`.
/// An invalid env value is ignored (treated as unset). Mirrors
/// `cli._resolve_ranking_source`.
#[must_use]
pub fn resolve_source(flag: Option<Source>) -> Source {
    if let Some(s) = flag {
        return s;
    }
    if let Ok(env) = std::env::var("LOOM_RANKING_SOURCE") {
        if let Some(s) = Source::parse(&env) {
            return s;
        }
        if !env.trim().is_empty() {
            eprintln!(
                "WARNING Ignoring invalid LOOM_RANKING_SOURCE={env:?}; \
                 expected one of auto, monitor, probe."
            );
        }
    }
    Source::Auto
}

/// Options for [`run_check`].
pub struct CheckOptions<'a> {
    pub source: Source,
    pub write_ranking: bool,
    pub probe_prompt: &'a str,
    pub model: &'a str,
    pub stagger: bool,
}

impl Default for CheckOptions<'_> {
    fn default() -> Self {
        Self {
            source: Source::Probe,
            write_ranking: false,
            probe_prompt: DEFAULT_PROBE_PROMPT,
            model: DEFAULT_PROBE_MODEL,
            stagger: true,
        }
    }
}

/// Probe all accounts (or consume claude-monitor data) and optionally write
/// `.ranking`. Mirrors `check.run_check`.
pub fn run_check(
    tokens_dir: &Path,
    opts: &CheckOptions,
    transport: &dyn ProbeTransport,
) -> ProbeReport {
    if matches!(opts.source, Source::Auto | Source::Monitor) {
        if let Some(report) =
            super::monitor::run_monitor_check(tokens_dir, opts.write_ranking, now_iso)
        {
            return report;
        }
        if opts.source == Source::Monitor {
            // monitor-only: no probe fallback. Empty report, leave `.ranking`.
            eprintln!(
                "WARNING check --source monitor: no fresh claude-monitor ranking.json; \
                 nothing to rank (not probing)."
            );
            return build_report(Vec::new());
        }
        // Auto: fall through to probing.
    }

    let pairs = discover_tokens(tokens_dir);
    if pairs.is_empty() {
        eprintln!("WARNING no tokens found in {}", tokens_dir.display());
    }

    let mut results = Vec::with_capacity(pairs.len());
    for (i, (name, token)) in pairs.iter().enumerate() {
        if i > 0 && opts.stagger && !token.is_empty() {
            let mut rng = super::rng::Rng::from_entropy();
            // 0.5..1.5s jitter (lean-genius pattern).
            let millis = 500 + (rng.next_u64() % 1000);
            std::thread::sleep(std::time::Duration::from_millis(millis));
        }
        results.push(probe_account(
            name,
            token,
            opts.model,
            opts.probe_prompt,
            DEFAULT_TIMEOUT_SECONDS,
            transport,
        ));
    }

    let report = build_report(results);

    if opts.write_ranking {
        let ranking_path = tokens_dir.join(".ranking");
        if let Err(e) = write_ranking_atomic(&report, &ranking_path) {
            eprintln!("WARNING failed to write {}: {e}", ranking_path.display());
        }
    }

    report
}

/// Human-readable status table (best accounts first). Mirrors
/// `check.format_table`.
#[must_use]
pub fn format_table(report: &ProbeReport) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Token pool ranking (probed at {})", report.ranked_at));
    lines.push("=".repeat(78));
    lines.push(format!(
        "{:<28} {:>9} {:>9} {:<13} {:<22}",
        "Account", "5h util", "7d util", "Status", "7d resets"
    ));
    lines.push("-".repeat(78));
    for a in &report.accounts {
        let s5 = a
            .s5h_utilization
            .map_or_else(|| "-".to_string(), |v| format!("{v:.2}"));
        let s7 = a
            .s7d_utilization
            .map_or_else(|| "-".to_string(), |v| format!("{v:.2}"));
        let reset = a.s7d_reset.clone().unwrap_or_else(|| "-".to_string());
        lines.push(format!("{:<28} {:>9} {:>9} {:<13} {:<22}", a.name, s5, s7, a.status, reset));
    }
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for a in &report.accounts {
        *counts.entry(a.status.clone()).or_insert(0) += 1;
    }
    let summary = counts
        .iter()
        .map(|(s, n)| format!("{n} {s}"))
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(String::new());
    lines.push(format!("Total {}: {summary}", report.accounts.len()));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;

    /// Stub transport driven by a queue of canned responses (or errors).
    struct StubTransport {
        responses: RefCell<Vec<Result<ProbeResponse, ProbeError>>>,
    }

    impl StubTransport {
        fn new(responses: Vec<Result<ProbeResponse, ProbeError>>) -> Self {
            Self {
                responses: RefCell::new(responses),
            }
        }
    }

    impl ProbeTransport for StubTransport {
        fn post(
            &self,
            _url: &str,
            _headers: &[(String, String)],
            _body: &str,
            _timeout: f64,
        ) -> Result<ProbeResponse, ProbeError> {
            self.responses
                .borrow_mut()
                .drain(..1)
                .next()
                .unwrap_or_else(|| Err(ProbeError::Request("stub exhausted".into())))
        }
    }

    fn resp(status: u16, headers: &[(&str, &str)]) -> Result<ProbeResponse, ProbeError> {
        Ok(ProbeResponse {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        })
    }

    fn good_headers(s7d: f64, s5h: f64) -> Vec<(String, String)> {
        vec![
            ("anthropic-ratelimit-tokens-5h-utilization".into(), format!("{s5h}")),
            ("anthropic-ratelimit-tokens-7d-utilization".into(), format!("{s7d}")),
            ("anthropic-ratelimit-tokens-7d-reset".into(), "1762070400".into()),
        ]
    }

    // ---- header parsing ------------------------------------------------

    #[test]
    fn suffix_match_canonical() {
        let headers = good_headers(0.10, 0.42);
        let parsed = parse_rate_limit_headers(&headers);
        assert!((parsed.s5h_utilization.unwrap() - 0.42).abs() < 1e-9);
        assert!((parsed.s7d_utilization.unwrap() - 0.10).abs() < 1e-9);
        assert_eq!(parsed.s7d_reset.as_deref(), Some("2025-11-02T08:00:00Z"));
    }

    #[test]
    fn suffix_match_after_prefix_rename() {
        let headers = vec![
            (
                "anthropic-ratelimit-input-tokens-7d-utilization".to_string(),
                "0.91".to_string(),
            ),
            (
                "anthropic-ratelimit-input-tokens-5h-utilization".to_string(),
                "0.55".to_string(),
            ),
        ];
        let parsed = parse_rate_limit_headers(&headers);
        assert!((parsed.s7d_utilization.unwrap() - 0.91).abs() < 1e-9);
        assert!((parsed.s5h_utilization.unwrap() - 0.55).abs() < 1e-9);
    }

    #[test]
    fn suffix_match_case_insensitive() {
        let headers =
            vec![("Anthropic-RateLimit-Tokens-5H-Utilization".to_string(), "0.30".to_string())];
        let parsed = parse_rate_limit_headers(&headers);
        assert!((parsed.s5h_utilization.unwrap() - 0.30).abs() < 1e-9);
    }

    #[test]
    fn missing_headers_return_none() {
        let parsed = parse_rate_limit_headers(&[("x-request-id".into(), "abc".into())]);
        assert!(parsed.s5h_utilization.is_none());
        assert!(parsed.s7d_utilization.is_none());
        assert!(parsed.s7d_reset.is_none());
    }

    #[test]
    fn unparseable_float_is_none() {
        assert!(parse_float(Some("not-a-number")).is_none());
    }

    #[test]
    fn epoch_conversions() {
        assert_eq!(epoch_to_iso(Some("0")).as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(epoch_to_iso(Some("1762070400")).as_deref(), Some("2025-11-02T08:00:00Z"));
        // ISO already / unparseable -> passthrough.
        assert_eq!(
            epoch_to_iso(Some("2026-05-09T00:00:00Z")).as_deref(),
            Some("2026-05-09T00:00:00Z")
        );
        assert!(epoch_to_iso(Some("")).is_none());
        assert!(epoch_to_iso(None).is_none());
    }

    // ---- auth header selection ----------------------------------------

    #[test]
    fn oauth_token_uses_bearer() {
        let h = build_headers("sk-ant-oat01-abc123");
        assert!(h
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer sk-ant-oat01-abc123"));
        assert!(h
            .iter()
            .any(|(k, v)| k == "anthropic-beta" && v == ANTHROPIC_OAUTH_BETA));
        assert!(!h.iter().any(|(k, _)| k == "x-api-key"));
    }

    #[test]
    fn api_key_uses_x_api_key() {
        let h = build_headers("sk-ant-api03-xyz");
        assert!(h
            .iter()
            .any(|(k, v)| k == "x-api-key" && v == "sk-ant-api03-xyz"));
        assert!(!h.iter().any(|(k, _)| k == "authorization"));
        assert!(!h.iter().any(|(k, _)| k == "anthropic-beta"));
    }

    // ---- status determination -----------------------------------------

    fn probe_with(response: Result<ProbeResponse, ProbeError>) -> AccountResult {
        let t = StubTransport::new(vec![response]);
        probe_account("agent-1", "sk-ant-oat01-x", DEFAULT_PROBE_MODEL, "hi", 15.0, &t)
    }

    #[test]
    fn status_200_available() {
        let r = probe_with(resp(
            200,
            &[
                ("anthropic-ratelimit-tokens-7d-utilization", "0.30"),
                ("anthropic-ratelimit-tokens-7d-reset", "1762070400"),
            ],
        ));
        assert_eq!(r.status, "available");
        assert!((r.s7d_utilization.unwrap() - 0.30).abs() < 1e-9);
        assert_eq!(r.s7d_reset.as_deref(), Some("2025-11-02T08:00:00Z"));
    }

    #[test]
    fn status_200_exhausted_above_threshold() {
        let r = probe_with(resp(200, &[("anthropic-ratelimit-tokens-7d-utilization", "0.97")]));
        assert_eq!(r.status, "exhausted");
    }

    #[test]
    fn status_200_exhausted_at_threshold() {
        let r = probe_with(resp(200, &[("anthropic-ratelimit-tokens-7d-utilization", "0.95")]));
        assert_eq!(r.status, "exhausted");
    }

    #[test]
    fn status_401_blocked() {
        let r = probe_with(resp(401, &[]));
        assert_eq!(r.status, "blocked");
        assert_eq!(r.error.as_deref(), Some("auth_401"));
    }

    #[test]
    fn status_429_rate_limited_below_threshold() {
        let r = probe_with(resp(429, &[("anthropic-ratelimit-tokens-7d-utilization", "0.20")]));
        assert_eq!(r.status, "rate_limited");
        assert!((r.s7d_utilization.unwrap() - 0.20).abs() < 1e-9);
    }

    #[test]
    fn status_429_promoted_to_exhausted_above_threshold() {
        let r = probe_with(resp(429, &[("anthropic-ratelimit-tokens-7d-utilization", "0.97")]));
        assert_eq!(r.status, "exhausted");
    }

    #[test]
    fn status_429_without_headers_stays_rate_limited() {
        let r = probe_with(resp(429, &[]));
        assert_eq!(r.status, "rate_limited");
        assert!(r.s7d_utilization.is_none());
    }

    #[test]
    fn status_500_error() {
        let r = probe_with(resp(503, &[]));
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("http_503"));
    }

    #[test]
    fn status_400_error() {
        let r = probe_with(resp(400, &[]));
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("http_400"));
    }

    #[test]
    fn status_timeout_error() {
        let r = probe_with(Err(ProbeError::Timeout));
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn status_connection_error() {
        let r = probe_with(Err(ProbeError::Connection("dns failure".into())));
        assert_eq!(r.status, "error");
        assert!(r.error.as_deref().unwrap().contains("connection"));
    }

    #[test]
    fn empty_token_blocked_not_probed() {
        let t = StubTransport::new(vec![]);
        let r = probe_account("agent-bad", "", DEFAULT_PROBE_MODEL, "hi", 15.0, &t);
        assert_eq!(r.status, "blocked");
        assert_eq!(r.error.as_deref(), Some("bad_token_listed"));
    }

    // ---- ordering + write ---------------------------------------------

    #[test]
    fn ranking_order_status_then_reset() {
        let results = vec![
            {
                let mut a = AccountResult::new("exhausted-1", "exhausted");
                a.s7d_reset = Some("2026-05-09T00:00:00Z".into());
                a
            },
            {
                let mut a = AccountResult::new("fresh", "available");
                a.s7d_reset = Some("2026-05-08T00:00:00Z".into());
                a
            },
            {
                let mut a = AccountResult::new("older", "available");
                a.s7d_reset = Some("2026-05-05T00:00:00Z".into());
                a
            },
            AccountResult::new("blocked-1", "blocked"),
            {
                let mut a = AccountResult::new("rl-1", "rate_limited");
                a.s7d_reset = Some("2026-05-07T00:00:00Z".into());
                a
            },
        ];
        let report = build_report(results);
        let ordered: Vec<&str> = report.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(ordered, ["older", "fresh", "rl-1", "exhausted-1", "blocked-1"]);
    }

    #[test]
    fn format_ranking_lines_pipe_format() {
        let report = ProbeReport {
            ranked_at: "2026-05-03T00:00:00Z".into(),
            accounts: vec![AccountResult::new("a-1", "available")],
        };
        assert_eq!(format_ranking_lines(&report), "a-1|available\n");
    }

    #[test]
    fn format_ranking_lines_empty_is_empty_string() {
        let report = ProbeReport {
            ranked_at: "x".into(),
            accounts: vec![],
        };
        assert_eq!(format_ranking_lines(&report), "");
    }

    #[test]
    fn atomic_write_leaves_no_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".ranking");
        let report = ProbeReport {
            ranked_at: "x".into(),
            accounts: vec![AccountResult::new("new", "available")],
        };
        fs::write(&target, "old contents").unwrap();
        write_ranking_atomic(&report, &target).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new|available\n");
        assert!(!target.with_extension("ranking.tmp").exists());
    }

    // ---- discover_tokens ----------------------------------------------

    #[test]
    fn discover_surfaces_bad_token_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("agent-1.token"), "sk-ant-oat01-aaa\n").unwrap();
        fs::write(tmp.path().join("agent-2.token"), "sk-ant-oat01-bbb\n").unwrap();
        fs::write(tmp.path().join(".bad_tokens"), "# comment\nagent-2\n\n").unwrap();
        let got = discover_tokens(tmp.path());
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["agent-1", "agent-2"]);
        assert_eq!(got.iter().find(|(n, _)| n == "agent-2").unwrap().1, "");
    }

    #[test]
    fn discover_skips_empty_token_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("agent-empty.token"), "\n").unwrap();
        fs::write(tmp.path().join("agent-real.token"), "sk-ant-oat01-x\n").unwrap();
        let got = discover_tokens(tmp.path());
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["agent-real"]);
    }

    #[test]
    fn discover_missing_dir_is_empty() {
        assert!(discover_tokens(&PathBuf::from("/nonexistent-xyz-123")).is_empty());
    }

    // ---- run_check ----------------------------------------------------

    #[test]
    fn run_check_writes_ranking_with_bad_account() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("agent-1.token"), "sk-ant-oat01-good").unwrap();
        fs::write(tmp.path().join("agent-bad.token"), "sk-ant-oat01-bad").unwrap();
        fs::write(tmp.path().join(".bad_tokens"), "agent-bad\n").unwrap();

        // Only agent-1 is probed (agent-bad has empty token -> blocked).
        let t = StubTransport::new(vec![resp(
            200,
            &[("anthropic-ratelimit-tokens-7d-utilization", "0.20")],
        )]);
        let opts = CheckOptions {
            source: Source::Probe,
            write_ranking: true,
            stagger: false,
            ..Default::default()
        };
        let report = run_check(tmp.path(), &opts, &t);
        let by_name: std::collections::HashMap<&str, &str> = report
            .accounts
            .iter()
            .map(|a| (a.name.as_str(), a.status.as_str()))
            .collect();
        assert_eq!(by_name["agent-1"], "available");
        assert_eq!(by_name["agent-bad"], "blocked");

        let ranking = fs::read_to_string(tmp.path().join(".ranking")).unwrap();
        let names: HashSet<&str> = ranking
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.split('|').next().unwrap())
            .collect();
        assert_eq!(names, HashSet::from(["agent-1", "agent-bad"]));
    }

    #[test]
    fn run_check_one_failure_does_not_kill_run() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a-good.token"), "sk-ant-oat01-good").unwrap();
        fs::write(tmp.path().join("z-bad.token"), "sk-ant-oat01-times-out").unwrap();

        // Probed in sorted-name order: a-good (ok), then z-bad (timeout).
        let t = StubTransport::new(vec![
            resp(200, &[("anthropic-ratelimit-tokens-7d-utilization", "0.10")]),
            Err(ProbeError::Timeout),
        ]);
        let opts = CheckOptions {
            source: Source::Probe,
            stagger: false,
            ..Default::default()
        };
        let report = run_check(tmp.path(), &opts, &t);
        let by_name: std::collections::HashMap<&str, &str> = report
            .accounts
            .iter()
            .map(|a| (a.name.as_str(), a.status.as_str()))
            .collect();
        assert_eq!(by_name["a-good"], "available");
        assert_eq!(by_name["z-bad"], "error");
    }

    #[test]
    fn run_check_empty_pool_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let t = StubTransport::new(vec![]);
        let opts = CheckOptions {
            source: Source::Probe,
            stagger: false,
            ..Default::default()
        };
        let report = run_check(tmp.path(), &opts, &t);
        assert!(report.accounts.is_empty());
    }

    // ---- curl output parsing ------------------------------------------

    #[test]
    fn parse_curl_output_extracts_status_and_headers() {
        let raw = "HTTP/2 200\r\n\
                   anthropic-ratelimit-tokens-7d-utilization: 0.42\r\n\
                   content-type: application/json\r\n\
                   \r\n\
                   \nLOOM_HTTP_CODE:200";
        let parsed = parse_curl_output(raw).unwrap();
        assert_eq!(parsed.status, 200);
        assert!(parsed
            .headers
            .iter()
            .any(|(k, v)| k == "anthropic-ratelimit-tokens-7d-utilization" && v == "0.42"));
    }

    #[test]
    fn parse_curl_output_handles_429() {
        let raw = "HTTP/1.1 429 Too Many Requests\nretry-after: 5\nLOOM_HTTP_CODE:429";
        let parsed = parse_curl_output(raw).unwrap();
        assert_eq!(parsed.status, 429);
    }

    // ---- source resolution --------------------------------------------

    #[test]
    fn source_parse_valid_and_invalid() {
        assert_eq!(Source::parse("auto"), Some(Source::Auto));
        assert_eq!(Source::parse(" MONITOR "), Some(Source::Monitor));
        assert_eq!(Source::parse("probe"), Some(Source::Probe));
        assert_eq!(Source::parse("bogus"), None);
    }
}
