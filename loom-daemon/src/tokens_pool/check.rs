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
//! (`-5h-utilization`, `-7d-utilization`, `-7d-reset`, `-5h-reset`), so any
//! rename of the `anthropic-ratelimit-*` prefix segment still maps to our
//! internal fields.
//!
//! # Byte-compatible `.ranking`
//!
//! `.ranking` is pipe-delimited `name|status|5h_util|limit_reset` (the last two
//! fields optional, issues #4195 / #4874), one account per line, trailing
//! newline (empty report yields an empty string), ordered by
//! [`STATUS_RANK`] then `7d_reset` ascending with an absent-reset sentinel.
//! **Every** status is written — no status is filtered at write time (see the
//! curator correction on issue #4094: the read-side allowlist in
//! [`super::select`] depends on `rate_limited` entries being present so a
//! fully-rate-limited pool can still dispatch via the tier-1 fallback). The
//! file is written atomically (`<path>.tmp` + rename in the same directory).

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
/// DELIBERATELY still `loom-tokens/...` after epic #4081 Phase 4 (#4557)
/// retired the Python `loom-tokens` CLI. This is an outbound HTTP header value,
/// not operator-facing advice: it is part of the byte-compatibility contract
/// with `check.py`'s probe (identical request => identical rate-limit-header
/// response), and changing it would alter what Anthropic's API sees for no
/// benefit. Do not "fix" this to `loom-daemon`.
const USER_AGENT: &str = "loom-tokens/0.1 (claude-code-compatible)";
/// Default model used for the minimal probe request.
pub const DEFAULT_PROBE_MODEL: &str = "claude-haiku-4-5-20251001";
/// Default probe prompt (`max_tokens=1` regardless of prompt).
pub const DEFAULT_PROBE_PROMPT: &str = "hi";
const DEFAULT_TIMEOUT_SECONDS: f64 = 15.0;
/// 7d-utilization at or above which an account is `exhausted` (issue #3988).
pub const EXHAUSTED_THRESHOLD: f64 = 0.99;

const HEADER_SUFFIX_5H_UTIL: &str = "-5h-utilization";
const HEADER_SUFFIX_7D_UTIL: &str = "-7d-utilization";
const HEADER_SUFFIX_7D_RESET: &str = "-7d-reset";
/// The 5h counterpart of `-7d-reset`. Anthropic does not currently send it on
/// every response, so it is looked up opportunistically: when it is absent a
/// `rate_limited` account reports **no** reset rather than borrowing the 7d
/// one, which would claim a days-out return for an account that comes back
/// within the hour (issue #4874).
const HEADER_SUFFIX_5H_RESET: &str = "-5h-reset";
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
    /// When the account's **5h** window rolls over. Populated by the
    /// claude-monitor backend (`resets["5h"]`) and, when the API sends the
    /// header, by the native probe. It is the reset that matters for a
    /// `rate_limited` account — and for the 5h `usage_fraction` the dashboard
    /// charts (issue #4874). See [`limit_reset`].
    pub s5h_reset: Option<String>,
    pub error: Option<String>,
}

impl AccountResult {
    /// A result with only the identity fields set; every measurement is
    /// "unknown" until a probe (or the monitor backend) fills it in.
    #[must_use]
    pub fn new(name: impl Into<String>, status: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: status.into(),
            s5h_utilization: None,
            s7d_utilization: None,
            s7d_reset: None,
            s5h_reset: None,
            error: None,
        }
    }

    /// [`limit_reset`] for this result — the instant the window currently
    /// gating this account rolls over.
    #[must_use]
    pub fn limit_reset(&self) -> Option<&str> {
        limit_reset(&self.status, self.s5h_reset.as_deref(), self.s7d_reset.as_deref())
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
        obj.insert(
            "5h_reset".into(),
            self.s5h_reset
                .clone()
                .map_or(serde_json::Value::Null, serde_json::Value::String),
        );
        // The window that is actually gating this account right now — the one
        // `.ranking` carries and the dashboard counts down to (issue #4874).
        obj.insert(
            "limit_reset".into(),
            self.limit_reset()
                .map_or(serde_json::Value::Null, |r| serde_json::Value::String(r.to_string())),
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

/// Render `headers` as curl's `-H @-` stdin format: one `name: value` pair
/// per line, terminated with `\n`. Kept as a standalone, argv-free function
/// so tests can assert on its output directly without spawning `curl`.
fn header_lines(headers: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in headers {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push('\n');
    }
    out
}

/// Build the `curl` [`Command`] for one probe request (everything except
/// stdio wiring). Deliberately takes **no headers** — they are never placed
/// in argv (see [`CurlTransport::post`]'s header comment); they are written
/// to the child's stdin instead. Split out from `post` as a separately
/// testable seam so the regression test for issue #5982 can inspect argv
/// without spawning a process.
fn build_curl_command(url: &str, body: &str, timeout_secs: f64) -> Command {
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
        .arg("POST")
        .arg("-H")
        .arg("@-")
        .arg("--data-binary")
        .arg(body)
        .arg(url);
    cmd
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
        //
        // Request headers -- including the `authorization: Bearer <token>` /
        // `x-api-key` credential -- are NEVER placed on curl's command line.
        // On Linux, argv is world-readable via `/proc/<pid>/cmdline` (and
        // shows up in `ps`, `systemctl status` cgroup listings, and
        // potentially journald), so a bearer token passed as a `-H` argument
        // is exposed to every local process for the life of the probe (issue
        // #5982). Instead, `-H @-` tells curl to read the header set from
        // its stdin, one "name: value" pair per line.
        //
        // The request body carries no credential material (it is just the
        // fixed probe prompt/model), so it is passed as a normal
        // `--data-binary` argument -- `Command` execs curl directly with no
        // shell in between, so no shell-escaping is needed either way, and
        // this frees stdin for exclusive use by `-H @-` (curl does not
        // support reading two separate `@-` streams from the same stdin).
        let mut cmd = build_curl_command(url, body, timeout_secs);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| ProbeError::Request(format!("spawn curl: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(header_lines(headers).as_bytes());
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

/// Return `[(account_name, token), ...]` for bootstrapped accounts.
///
/// Reads every `*.token` file in `tokens_dir` (sorted by filename) and skips
/// entries listed in `.bad_tokens`, surfacing them with an **empty** token so
/// callers can still emit them as `status: blocked`. Mirrors
/// `check.discover_tokens`.
///
/// Bad-ness is decided by [`super::bad_tokens::blocking_entry_in_dir`] — the
/// same authoritative, cooldown-aware per-field parser `select.rs`'s
/// `is_bad` uses — not a naive whole-line-equality set (issue #6030). A
/// bare-name-only line (`"agent-bad\n"`, no timestamp/reason) still matches:
/// `blocking_entry_in_dir` treats a missing/unparseable timestamp as a
/// permanent (fail-closed) block. What it fixes is the realistic production
/// shape written by `mark_bad` / `claude-wrapper.sh`'s rotation helpers —
/// `"<ISO8601 ts> <name> <reason...>"` — which the old whole-line compare
/// never matched (the line is never equal to the bare account name), so a
/// genuinely bad-marked account with a real reason string was silently
/// probed with its live token instead of being reported `blocked`.
pub fn discover_tokens(tokens_dir: &Path) -> Vec<(String, String)> {
    if !tokens_dir.is_dir() {
        return Vec::new();
    }

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
        if super::bad_tokens::blocking_entry_in_dir(tokens_dir, &name).is_some() {
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
    s5h_reset: Option<String>,
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
        s5h_reset: epoch_to_iso(find_header_by_suffix(headers, HEADER_SUFFIX_5H_RESET).as_deref()),
        s5h_status: find_header_by_suffix(headers, HEADER_SUFFIX_5H_STATUS),
    }
}

/// The instant this account's **binding** limit window resets — i.e. the answer
/// to "when can I dispatch to it again?" (issue #4874).
///
/// The pool tracks two independent windows, and which one is holding an
/// account back depends on its status:
///
/// - `exhausted` is derived from **7d** utilization clearing
///   [`EXHAUSTED_THRESHOLD`], so the 5h window rolling over changes nothing —
///   the 7d reset is the release.
/// - `rate_limited` is a 429 whose 7d utilization is *below* the threshold, so
///   the **5h** window is what tripped and the 5h reset is the release. Naming
///   the 7d reset here would be actively misleading: on a live host a
///   `rate_limited` account had a 5h reset ~1.6h out and a 7d reset **six days**
///   out, and the dashboard would have counted down to the wrong one.
/// - Anything else (`available`, `blocked`, `error`, …) is not gated by a
///   window at all. Report the 5h reset, which is the rollover the reported
///   `5h_utilization` — the `usage_fraction` the dashboard charts and forecasts
///   against — is actually racing.
///
/// Returns `None` when the relevant instant is unknown. It is never
/// substituted with the other window's: an absent countdown reads as "unknown",
/// a wrong one reads as a fact.
///
/// Takes the three values rather than an [`AccountResult`] so the
/// claude-monitor writer — which has its own account struct — picks the window
/// through this same function instead of re-deriving the rule.
#[must_use]
pub fn limit_reset<'a>(
    status: &str,
    reset_5h: Option<&'a str>,
    reset_7d: Option<&'a str>,
) -> Option<&'a str> {
    match status {
        "exhausted" => reset_7d,
        _ => reset_5h,
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
    probe_account_with_blocking(name, token, model, probe_prompt, timeout_secs, transport, None)
}

/// [`probe_account`], additionally accepting the `.bad_tokens` entry (if any)
/// already known to be blocking this account (issue #6030).
///
/// A `.bad_tokens`-listed account is never actually probed (`discover_tokens`
/// hands it an empty token), so before this the operator-facing `error` field
/// on its `AccountResult` was the opaque `"bad_token_listed"` — giving no way
/// to tell an auth-dead account (needs `claude login` / a fresh OAuth token)
/// apart from a still-cooling-down exhaustion entry from `tokens check`'s
/// table/JSON output alone. Passing the [`super::bad_tokens::BlockingEntry`]
/// through lets the `error` field carry the real class + reason instead.
pub fn probe_account_with_blocking(
    name: &str,
    token: &str,
    model: &str,
    probe_prompt: &str,
    timeout_secs: f64,
    transport: &dyn ProbeTransport,
    blocking: Option<&super::bad_tokens::BlockingEntry>,
) -> AccountResult {
    if token.is_empty() {
        let mut r = AccountResult::new(name, "blocked");
        r.error = Some(match blocking {
            Some(entry) => format!("{}: {}", entry.class.label(), entry.reason),
            None => "bad_token_listed".to_string(),
        });
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
            s5h_reset: parsed.s5h_reset,
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
        s5h_reset: parsed.s5h_reset,
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

/// Format one `.ranking` line, emitting the optional 5h-utilization (issue
/// #4195) and binding-limit-reset (issue #4874) fields:
/// `name|status|5h_util|limit_reset`, truncated to the shortest form that
/// carries every known value — `name|status` when neither is known,
/// `name|status|util` when only the utilization is. A row that knows its reset
/// but not its utilization writes an **empty** third field
/// (`name|status||limit_reset`) so the reset stays in position 4; the reader
/// parses an empty utilization back to `None` rather than a fabricated `0.0`.
/// The float is fixed at 2 decimals so the probe and monitor writers stay
/// byte-identical.
///
/// `limit_reset` is whichever window is actually gating the account — see
/// [`limit_reset`], which both writers call to pick it. It is written verbatim
/// minus surrounding whitespace, and is **dropped** if it contains a `|` or `#`
/// — either would make the line unparseable (field split / comment strip), and
/// a silently mangled row is worse than an absent reset.
pub(crate) fn ranking_line(
    name: &str,
    status: &str,
    util_5h: Option<f64>,
    limit_reset: Option<&str>,
) -> String {
    let reset = limit_reset
        .map(str::trim)
        .filter(|r| !r.is_empty() && !r.contains('|') && !r.contains('#'));
    match (util_5h, reset) {
        (Some(u), Some(r)) => format!("{name}|{status}|{u:.2}|{r}"),
        (Some(u), None) => format!("{name}|{status}|{u:.2}"),
        (None, Some(r)) => format!("{name}|{status}||{r}"),
        (None, None) => format!("{name}|{status}"),
    }
}

/// Serialize a report to the selector's `name|status|5h_util|limit_reset`
/// format (trailing newline, empty report -> empty string). The 5h utilization
/// (issue #4195) and the binding-window reset instant (issue #4874) are
/// optional trailing fields, emitted when known.
#[must_use]
pub fn format_ranking_lines(report: &ProbeReport) -> String {
    if report.accounts.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for a in &report.accounts {
        out.push_str(&ranking_line(&a.name, &a.status, a.s5h_utilization, a.limit_reset()));
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
        // A bad-marked account is never actually probed (empty token) — look
        // up why it's blocked so the result carries the real class/reason
        // instead of the opaque "bad_token_listed" (#6030).
        let blocking = if token.is_empty() {
            super::bad_tokens::blocking_entry_in_dir(tokens_dir, name)
        } else {
            None
        };
        results.push(probe_account_with_blocking(
            name,
            token,
            opts.model,
            opts.probe_prompt,
            DEFAULT_TIMEOUT_SECONDS,
            transport,
            blocking.as_ref(),
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

/// Human-readable status table (best accounts first).
///
/// The reset column reports the **binding** window ([`limit_reset`]) and names
/// which one it is, e.g. `2026-08-02T03:00:00Z (7d)`. It was labelled
/// `7d resets` and fed from `s7d_reset` alone until issue #4874, which was
/// wrong twice over: on the claude-monitor backend it was never populated at
/// all (a permanent `-`), and on the probe backend a `rate_limited` account
/// showed its 7d reset — days away — when the 5h window was the one holding it.
#[must_use]
pub fn format_table(report: &ProbeReport) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Token pool ranking (probed at {})", report.ranked_at));
    lines.push("=".repeat(84));
    lines.push(format!(
        "{:<28} {:>9} {:>9} {:<13} {:<25}",
        "Account", "5h util", "7d util", "Status", "Resets at"
    ));
    lines.push("-".repeat(84));
    for a in &report.accounts {
        let s5 = a
            .s5h_utilization
            .map_or_else(|| "-".to_string(), |v| format!("{v:.2}"));
        let s7 = a
            .s7d_utilization
            .map_or_else(|| "-".to_string(), |v| format!("{v:.2}"));
        let window = if a.status == "exhausted" { "7d" } else { "5h" };
        let reset = a
            .limit_reset()
            .map_or_else(|| "-".to_string(), |r| format!("{r} ({window})"));
        let mut row = format!("{:<28} {:>9} {:>9} {:<13} {:<25}", a.name, s5, s7, a.status, reset);
        // Surface WHY a blocked/errored account is out of rotation (#6030) —
        // in particular, whether it needs `tokens unblock` (auth-dead,
        // permanent) or will clear itself (exhaustion, TTL) rather than just
        // "blocked" with no explanation.
        if let Some(err) = &a.error {
            row.push_str(&format!("  ({err})"));
        }
        lines.push(row);
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
    use std::collections::HashSet;
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
        let r = probe_with(resp(200, &[("anthropic-ratelimit-tokens-7d-utilization", "0.995")]));
        assert_eq!(r.status, "exhausted");
    }

    #[test]
    fn status_200_exhausted_at_threshold() {
        let r = probe_with(resp(200, &[("anthropic-ratelimit-tokens-7d-utilization", "0.99")]));
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
        let r = probe_with(resp(429, &[("anthropic-ratelimit-tokens-7d-utilization", "0.995")]));
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
    fn format_ranking_lines_emits_5h_util_third_field() {
        // The 5h utilization is the optional third field (issue #4195), fixed
        // at 2 decimals; an absent value keeps the legacy 2-field form.
        let mut loaded = AccountResult::new("a-1", "available");
        loaded.s5h_utilization = Some(0.42);
        let report = ProbeReport {
            ranked_at: "x".into(),
            accounts: vec![loaded, AccountResult::new("b-2", "available")],
        };
        assert_eq!(format_ranking_lines(&report), "a-1|available|0.42\nb-2|available\n");
    }

    #[test]
    fn format_ranking_lines_emits_the_binding_reset_fourth_field() {
        // Issue #4874: the probe already parses `anthropic-ratelimit-...-
        // 7d-reset`; it now survives into `.ranking` as the optional fourth
        // field instead of being discarded at the writer.
        let mut a = AccountResult::new("a-1", "exhausted");
        a.s5h_utilization = Some(0.0);
        a.s7d_reset = Some("2026-08-02T03:00:00Z".into());
        let report = ProbeReport {
            ranked_at: "x".into(),
            accounts: vec![a],
        };
        assert_eq!(format_ranking_lines(&report), "a-1|exhausted|0.00|2026-08-02T03:00:00Z\n");
    }

    #[test]
    fn limit_reset_picks_the_window_that_is_actually_gating() {
        // `exhausted` is derived from 7d utilization, so the 7d reset is the
        // release; `rate_limited` is the 5h window tripping with 7d still
        // healthy, so the 5h reset is. Reporting the 7d reset for a
        // rate_limited account would claim a days-out return for an account
        // that is back within the hour.
        let five = Some("2026-08-01T07:00:00Z");
        let seven = Some("2026-08-07T01:00:00Z");
        assert_eq!(limit_reset("exhausted", five, seven), seven);
        assert_eq!(limit_reset("rate_limited", five, seven), five);
        // Not gated by a window at all: report the rollover the 5h
        // `usage_fraction` is racing, which is what the dashboard charts.
        assert_eq!(limit_reset("available", five, seven), five);
        assert_eq!(limit_reset("blocked", five, seven), five);
    }

    #[test]
    fn limit_reset_never_substitutes_the_other_window() {
        // An unknown binding reset stays unknown. Falling back to the window
        // that *is* known would turn "I do not know" into a confident wrong
        // answer — the failure mode this whole field exists to avoid.
        assert_eq!(limit_reset("exhausted", Some("2026-08-01T07:00:00Z"), None), None);
        assert_eq!(limit_reset("rate_limited", None, Some("2026-08-07T01:00:00Z")), None);
    }

    #[test]
    fn format_ranking_lines_writes_the_5h_reset_for_a_rate_limited_account() {
        // The writer must go through `limit_reset`, not reach for `s7d_reset`.
        let mut a = AccountResult::new("b-2", "rate_limited");
        a.s5h_utilization = Some(1.0);
        a.s7d_reset = Some("2026-08-07T01:00:00Z".into());
        a.s5h_reset = Some("2026-08-01T07:00:00Z".into());
        let report = ProbeReport {
            ranked_at: "x".into(),
            accounts: vec![a],
        };
        assert_eq!(format_ranking_lines(&report), "b-2|rate_limited|1.00|2026-08-01T07:00:00Z\n");
    }

    #[test]
    fn probe_parses_the_5h_reset_header_when_the_api_sends_one() {
        // The API does not always send a 5h reset header. When it does, the
        // probe backend picks it up by the same suffix match as every other
        // rate-limit header; when it does not, the account simply has no 5h
        // reset (and a rate_limited row carries no countdown at all).
        let headers = vec![
            ("anthropic-ratelimit-unified-5h-utilization".to_string(), "1.0".to_string()),
            ("anthropic-ratelimit-unified-7d-utilization".to_string(), "0.6".to_string()),
            ("anthropic-ratelimit-unified-7d-reset".to_string(), "1786000000".to_string()),
            ("anthropic-ratelimit-unified-5h-reset".to_string(), "1785567600".to_string()),
        ];
        let parsed = parse_rate_limit_headers(&headers);
        assert_eq!(parsed.s5h_reset.as_deref(), Some("2026-08-01T07:00:00Z"));
        assert!(parsed.s7d_reset.is_some());

        let without = parse_rate_limit_headers(&headers[..3]);
        assert_eq!(without.s5h_reset, None, "an absent header is unknown, not borrowed from 7d");
    }

    #[test]
    fn table_names_which_window_the_reset_belongs_to() {
        // A bare instant in the reset column is ambiguous between the two
        // windows, and the operator's next action differs a lot depending on
        // which it is. The column says so explicitly.
        let mut exhausted = AccountResult::new("a-1", "exhausted");
        exhausted.s7d_reset = Some("2026-08-02T03:00:00Z".into());
        exhausted.s5h_reset = Some("2026-08-01T05:20:00Z".into());
        let mut limited = AccountResult::new("b-2", "rate_limited");
        limited.s7d_reset = Some("2026-08-07T01:00:00Z".into());
        limited.s5h_reset = Some("2026-08-01T07:00:00Z".into());
        let unknown = AccountResult::new("c-3", "error");
        let table = format_table(&ProbeReport {
            ranked_at: "x".into(),
            accounts: vec![exhausted, limited, unknown],
        });
        assert!(table.contains("Resets at"), "{table}");
        assert!(table.contains("2026-08-02T03:00:00Z (7d)"), "{table}");
        assert!(table.contains("2026-08-01T07:00:00Z (5h)"), "{table}");
        assert!(
            !table.contains("2026-08-07T01:00:00Z"),
            "the 7d reset of a 5h-limited account must not be shown: {table}"
        );
        let unknown_row = table.lines().find(|l| l.starts_with("c-3")).unwrap();
        assert!(
            unknown_row.trim_end().ends_with('-'),
            "unknown renders as a dash: {unknown_row:?}"
        );
    }

    #[test]
    fn ranking_line_reset_without_util_keeps_field_position() {
        // A row that knows its reset but not its utilization must write an
        // empty third field so the reset stays in position 4 — otherwise the
        // reader would read the reset as the utilization.
        assert_eq!(
            ranking_line("a-1", "exhausted", None, Some("2026-08-02T03:00:00Z")),
            "a-1|exhausted||2026-08-02T03:00:00Z"
        );
    }

    #[test]
    fn ranking_line_drops_reset_containing_delimiters() {
        // A `|` or `#` in the reset text would make the line unparseable
        // (field split / comment strip). Dropping the reset degrades to
        // "unknown"; writing it would silently mangle the whole row.
        assert_eq!(
            ranking_line("a-1", "exhausted", Some(0.5), Some("2026|08")),
            "a-1|exhausted|0.50"
        );
        assert_eq!(
            ranking_line("a-1", "exhausted", Some(0.5), Some("2026#08")),
            "a-1|exhausted|0.50"
        );
        // Whitespace-only is "unknown", not an empty trailing field.
        assert_eq!(ranking_line("a-1", "exhausted", Some(0.5), Some("   ")), "a-1|exhausted|0.50");
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

    /// #6030: a bad-marked account's `error` field names the real class and
    /// reason (e.g. `"auth: auth-dead: 401 Invalid bearer token"`) instead of
    /// the opaque `"bad_token_listed"` — so `tokens check`'s table/JSON output
    /// (the operator-visible list) can tell an auth-dead account apart from a
    /// still-cooling-down exhaustion entry without a separate `unblock`-dry-run.
    #[test]
    fn run_check_surfaces_the_blocking_reason_for_a_bad_account() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("agent-1.token"), "sk-ant-oat01-good").unwrap();
        fs::write(tmp.path().join("agent-auth.token"), "sk-ant-oat01-dead").unwrap();
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        fs::write(
            tmp.path().join(".bad_tokens"),
            format!("{ts} agent-auth auth-dead: 401 Invalid bearer token\n"),
        )
        .unwrap();

        let t = StubTransport::new(vec![resp(
            200,
            &[("anthropic-ratelimit-tokens-7d-utilization", "0.20")],
        )]);
        let opts = CheckOptions {
            source: Source::Probe,
            stagger: false,
            ..Default::default()
        };
        let report = run_check(tmp.path(), &opts, &t);
        let by_name: std::collections::HashMap<&str, &AccountResult> = report
            .accounts
            .iter()
            .map(|a| (a.name.as_str(), a))
            .collect();
        assert_eq!(by_name["agent-auth"].status, "blocked");
        assert_eq!(
            by_name["agent-auth"].error.as_deref(),
            Some("auth: auth-dead: 401 Invalid bearer token")
        );

        let table = format_table(&report);
        assert!(
            table.contains("auth: auth-dead: 401 Invalid bearer token"),
            "table does not surface the blocking reason: {table}"
        );
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

    // ---- curl argv never carries credential material (issue #5982) ----

    #[test]
    fn header_lines_formats_one_header_per_line() {
        let headers = vec![
            ("authorization".to_string(), "Bearer sk-ant-oat01-x".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        assert_eq!(
            header_lines(&headers),
            "authorization: Bearer sk-ant-oat01-x\ncontent-type: application/json\n"
        );
    }

    #[test]
    fn header_lines_empty_headers_is_empty_string() {
        assert_eq!(header_lines(&[]), "");
    }

    #[test]
    fn curl_command_argv_never_contains_the_bearer_token() {
        // Regression test for issue #5982: `loom-daemon tokens check` shelled
        // out to curl with the OAuth bearer token as a `-H "authorization:
        // Bearer <token>"` *argument*, which is world-readable for the life
        // of the process via /proc/<pid>/cmdline, `ps`, `systemctl status`
        // cgroup listings, and potentially journald. Headers -- including
        // the bearer token -- must be delivered exclusively via curl's
        // stdin (`-H @-`), never as command-line arguments.
        let token = "sk-ant-oat01-super-secret-value-should-never-leak-into-argv";
        let headers = build_headers(token);
        // Sanity check: this test would be a false negative if the token
        // never actually reached a header in the first place.
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "authorization" && v.contains(token)),
            "test setup did not produce a bearer header"
        );

        let cmd = build_curl_command(ANTHROPIC_MESSAGES_URL, "{}", 15.0);
        let argv: Vec<String> = std::iter::once(cmd.get_program().to_string_lossy().into_owned())
            .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
            .collect();
        let joined = argv.join(" ");

        assert!(!joined.contains(token), "bearer token leaked into curl argv: {joined}");
        assert!(
            !joined.contains("Bearer"),
            "Authorization header leaked into curl argv: {joined}"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("authorization:")),
            "headers must never be passed as literal curl -H arguments: {argv:?}"
        );
        // The only `-H` argument allowed is `-H @-`, which tells curl to
        // read the header set from stdin instead of argv.
        let h_positions: Vec<usize> = argv
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-H")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(h_positions.len(), 1, "expected exactly one -H flag: {argv:?}");
        assert_eq!(
            argv.get(h_positions[0] + 1).map(String::as_str),
            Some("@-"),
            "expected `-H @-` (read headers from stdin), not a literal header argument: {argv:?}"
        );
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
