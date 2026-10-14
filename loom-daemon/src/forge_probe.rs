//! `forge-probe` — the hosted-qualification runner, slice 1 (#9789, phase 1
//! of epic #9769).
//!
//! Executes the probe manifest (#9777's `forge-inventory probe-manifest`)
//! against a live forge and emits a sanitized receipt: per-case outcome,
//! server version, actor, run timestamp, expected/observed. Unexecuted
//! cases stay `unknown` — the #9789 acceptance rule that a missing answer
//! must never read as a pass, and the reason a slice-1 run on the full
//! matrix exits nonzero by design: the harness is honest about coverage
//! before it is complete.
//!
//! Safety model (slice 1):
//! - **Read-only by default.** Write cases refuse to run without
//!   `--live-write` (explicit opt-in, per #9789).
//! - **Disposable namespace.** Write cases only create resources whose
//!   title carries the run namespace (`loomp-…`); the disposable repo
//!   itself is created by the hosted-trial runbook's setup
//!   (docs/research/gitea-cloud-qualification-runbook.md), never here —
//!   creating and deleting repositories is a runbook action, not a probe.
//! - **Bounded.** Every HTTP call carries a timeout; no case retries.
//! - **Sanitized.** Receipts carry the actor *login* and version strings —
//!   never a token, header, or credential material.
//!
//! The transport is a trait: the live implementation shells `curl` with the
//! Authorization header written to stdin (never argv — #5982), and tests
//! script a fake. Cases are matched on the manifest's `test_id`
//! (`forge-probe::<group>::<name>`); a handler absent for slice 1 leaves
//! its row `unknown` with the reason recorded.

use std::time::Duration;

use anyhow::Result;

use crate::forge_contract::ForgeOutcome;

/// The receipt outcome vocabulary.
pub const OUTCOME_PASS: &str = "pass";
pub const OUTCOME_FAIL: &str = "fail";
pub const OUTCOME_UNSUPPORTED: &str = "unsupported";
pub const OUTCOME_UNKNOWN: &str = "unknown";

/// One executed (or explicitly unexecuted) probe case, as the receipt
/// records it and #9789's acceptance criteria require.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CaseResult {
    /// The manifest's `test_id` (`forge-probe::<group>::<name>`).
    pub test_id: String,
    /// The manifest operation this case exercises.
    pub operation: String,
    /// high | normal (risk-first ordering is the manifest's job).
    pub risk: String,
    /// required | optional.
    pub disposition: String,
    /// pass | fail | unsupported | unknown
    pub outcome: String,
    /// The server build the case ran against (from `/api/v1/version`).
    pub server_version: String,
    /// The actor login the write ran as, when known.
    pub actor: Option<String>,
    /// Unix seconds — the run's wall clock, UTC.
    pub at: u64,
    /// What the case expected, in the manifest's terms.
    pub expected: String,
    /// What actually happened (outcome-specific detail; no credentials).
    pub observed: String,
    /// Why a case did not run (slice boundary, transport fault, refusal).
    pub notes: Vec<String>,
}

impl CaseResult {
    /// A row the runner could not execute: honestly `unknown`, with the
    /// reason in `notes` (#9789: unexecuted stays unknown, never a pass).
    fn unknown(entry: &ProbeEntryView, why: &str, server_version: &str) -> Self {
        Self {
            test_id: entry
                .test_id
                .clone()
                .unwrap_or_else(|| format!("<no test_id> {}", entry.id)),
            operation: entry.id.clone(),
            risk: entry.risk.to_string(),
            disposition: entry.disposition.to_string(),
            outcome: OUTCOME_UNKNOWN.into(),
            server_version: server_version.to_string(),
            actor: None,
            at: now_secs(),
            expected: String::new(),
            observed: String::new(),
            notes: vec![why.to_string()],
        }
    }
}

/// A light view of one probe-manifest entry: the fields a receipt needs.
/// (`forge_inventory::probe::ProbeEntry` is the source; this keeps the
/// runner decoupled from that module's serde surface.)
#[derive(Debug, Clone)]
pub(crate) struct ProbeEntryView {
    pub id: String,
    pub test_id: Option<String>,
    pub risk: String,
    pub disposition: String,
    /// write cases refuse to run without the live-write opt-in.
    pub class: String,
}

/// HTTP transport: one request with an explicit token. Bounded by the
/// config's timeout; no retries (a probe records, it does not fight).
/// Public because `run` exposes it and the bin's CLI tree (a separate
/// crate) injects the fake — or the live impl — from outside the lib.
pub trait ProbeHttp {
    fn request(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), String>;
}

/// The resolved, validated configuration.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// e.g. `https://gitea.example.com` (the runbook's
    /// `GITEA_QUAL_INSTANCE_URL`).
    pub origin: String,
    /// The disposable repo write cases create resources in — created by the
    /// hosted-trial runbook's setup, never by the runner.
    pub repo: String,
    pub writer_token: String,
    pub readonly_token: Option<String>,
    /// The run namespace stamped into every disposable resource title.
    pub run_ns: String,
    /// Explicit live-write opt-in (#9789): without it, write cases refuse.
    pub live_write: bool,
    pub timeout: Duration,
    /// Run only these test_id substrings; empty = every manifest row.
    pub only: Vec<String>,
}

/// The live transport: `curl` with the Authorization header on stdin
/// (#5982 — never argv), bounded `--max-time`.
pub struct LiveHttp {
    pub origin: String,
    pub timeout: Duration,
}

impl ProbeHttp for LiveHttp {
    fn request(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        // curl's `--max-time 0` disables the limit entirely; refuse it
        // before spawning curl so no call can be unbounded.
        if self.timeout.is_zero() {
            return Err("timeout must be positive (0 would disable the per-call bound)".into());
        }
        let url = format!(
            "{}/api/v1/{}",
            self.origin.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        let mut cmd = Command::new("curl");
        cmd.arg("-q")
            .arg("--globoff")
            .arg("--silent")
            .arg("--show-error")
            .arg("--max-time")
            .arg(self.timeout.as_secs_f64().to_string())
            .arg("-o")
            .arg("-")
            .arg("-w")
            .arg("\n%{http_code}")
            .arg("-X")
            .arg(method)
            .arg("-H")
            .arg("@-")
            .arg(&url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(body) = body {
            cmd.arg("--data-binary").arg(body);
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn curl: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let mut headers = format!("Authorization: token {token}\n");
            if body.is_some() {
                // curl's --data-binary defaults to form-urlencoded, which
                // Gitea's binder routes to the form decoder, not JSON.
                headers.push_str("Content-Type: application/json\n");
            }
            let _ = stdin.write_all(headers.as_bytes());
        }
        let output = child
            .wait_with_output()
            .map_err(|e| format!("curl wait: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "curl exit {}: {}",
                output.status.code().unwrap_or(-1),
                redact_secrets(String::from_utf8_lossy(&output.stderr).trim(), &[token])
            ));
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        let (body, code) = raw
            .rsplit_once('\n')
            .map(|(b, c)| (b.to_string(), c.trim().to_string()))
            .unwrap_or_else(|| (raw.to_string(), String::new()));
        let code: u16 = code
            .parse()
            .map_err(|_| format!("unparseable status line from curl: {code:?}"))?;
        Ok((code, body))
    }
}

/// One path-safe name component: nonempty, `[A-Za-z0-9._-]` only, and not a
/// dot segment. The allowlist excludes `/`, `\`, `%`, `?`, `#`, `@`, `:`
/// and whitespace, so a validated value can neither traverse (curl
/// normalizes `..`) nor smuggle URL delimiters or encoded traversal.
fn is_safe_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Validate the repo and run namespace at the public runner boundary, before
/// any request — a malformed value must never reach an authenticated write
/// URL (it could redirect the write to a different repository).
fn validate_config(cfg: &RunnerConfig) -> Result<()> {
    let mut parts = cfg.repo.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None)
            if is_safe_component(owner) && is_safe_component(name) => {}
        _ => anyhow::bail!(
            "repo must be exactly <owner>/<repo> (letters, digits, '.', '_', '-'; no traversal or URL delimiters)"
        ),
    }
    if !is_safe_component(&cfg.run_ns) {
        anyhow::bail!("run namespace must be nonempty (letters, digits, '.', '_', '-')");
    }
    Ok(())
}

/// Build the receipt: every required-profile row from the probe manifest,
/// in the manifest's risk-first order, executed where a handler exists and
/// `unknown` where it does not.
pub fn run(cfg: &RunnerConfig, http: &dyn ProbeHttp) -> Result<Vec<CaseResult>> {
    // curl's `--max-time 0` means "no limit"; the runner's bounded-call
    // contract forbids that, so refuse before any request is made.
    if cfg.timeout.is_zero() {
        anyhow::bail!("timeout must be positive (0 would disable the per-call bound)");
    }
    validate_config(cfg)?;
    let inv = crate::forge_inventory::load_embedded()?;
    let manifest = crate::forge_inventory::probe::build(&inv, &[]);
    let mut results = Vec::new();

    // The server version stamps every receipt row; a failed version probe
    // marks the whole run Unknown rather than inventing a version.
    let server_version = match http.request("GET", "version", &cfg.writer_token, None) {
        Ok((200, body)) => {
            match serde_json::from_str::<serde_json::Value>(body.trim())
                .ok()
                .and_then(|v| v.get("version").and_then(|s| s.as_str()).map(String::from))
                .filter(|v| !v.trim().is_empty() && is_clean(v, &secrets_of(cfg)))
            {
                Some(v) => v,
                // No observed version = no version-qualified evidence: stop
                // before any case (and any write) runs.
                None => {
                    let why = ForgeOutcome::Unknown {
                        operation: "server.version".into(),
                        why: format!(
                            "GET version answered 200 without a usable version ({})",
                            body_shape(&body)
                        ),
                    };
                    return Err(anyhow::anyhow!("{why}"));
                }
            }
        }
        Ok((code, body)) => {
            let _ = body;
            let why = ForgeOutcome::Unknown {
                operation: "server.version".into(),
                why: format!("GET version answered {code}"),
            };
            return Err(anyhow::anyhow!("{why}"));
        }
        Err(e) => {
            let why = ForgeOutcome::Unknown {
                operation: "server.version".into(),
                why: format!("version probe transport fault: {}", scrub(cfg, &e)),
            };
            return Err(anyhow::anyhow!("{why}"));
        }
    };

    for entry in &manifest.entries {
        let view = ProbeEntryView {
            id: entry.id.clone(),
            test_id: entry.test_id.clone(),
            risk: entry.risk.to_string(),
            disposition: entry.disposition.to_string(),
            class: entry.class.to_string(),
        };
        if entry.profile != "required-coordination" {
            // Slice 1 runs the coordination profile only; other profiles
            // stay unexecuted in the receipt so coverage stays honest.
            results.push(CaseResult::unknown(&view, "profile not in slice 1", &server_version));
            continue;
        }
        if !cfg.only.is_empty()
            && !cfg
                .only
                .iter()
                .any(|id| entry.test_id.as_deref().is_some_and(|t| t.contains(id)))
        {
            results.push(CaseResult::unknown(&view, "deselected by --case", &server_version));
            continue;
        }
        let Some(test_id) = &entry.test_id else {
            results.push(CaseResult::unknown(
                &view,
                "manifest row carries no test_id",
                &server_version,
            ));
            continue;
        };
        let outcome = execute_case(&view, test_id, cfg, http, &server_version);
        results.push(match outcome {
            Ok(r) => r,
            Err(o) => CaseResult {
                test_id: test_id.clone(),
                operation: entry.id.clone(),
                risk: entry.risk.to_string(),
                disposition: entry.disposition.to_string(),
                outcome: OUTCOME_UNKNOWN.into(),
                server_version: server_version.clone(),
                actor: None,
                at: now_secs(),
                expected: "a definitive answer".into(),
                observed: o.to_string(),
                notes: vec!["transport/protocol fault — recorded, not retried".into()],
            },
        });
    }
    Ok(results)
}

/// Dispatch on the manifest's test_id. Slice 1 implements the coordination
/// group's seed cases; every other row stays honestly unknown.
fn execute_case(
    view: &ProbeEntryView,
    test_id: &str,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    let is_write = view.class == "write";
    if is_write && !cfg.live_write {
        return Ok(CaseResult {
            test_id: test_id.to_string(),
            operation: view.id.clone(),
            risk: view.risk.clone(),
            disposition: view.disposition.clone(),
            outcome: OUTCOME_UNKNOWN.into(),
            server_version: server_version.to_string(),
            actor: None,
            at: now_secs(),
            expected: "a live write".into(),
            observed: "refused: live-write not opted in (--live-write)".into(),
            notes: vec!["safety: read-only by default (#9789)".into()],
        });
    }
    match test_id {
        "forge-probe::coordination::issue-create" => {
            issue_create(test_id, view, cfg, http, server_version)
        }
        "forge-probe::coordination::comment-create" => {
            issue_comment_readback(test_id, view, cfg, http, server_version)
        }
        other => Ok(CaseResult {
            test_id: other.to_string(),
            operation: view.id.clone(),
            risk: view.risk.clone(),
            disposition: view.disposition.clone(),
            outcome: OUTCOME_UNKNOWN.into(),
            server_version: server_version.to_string(),
            actor: None,
            at: now_secs(),
            expected: "a recorded pass/fail/unsupported".into(),
            observed: String::new(),
            notes: vec!["case handler not implemented — slice 2 (#9789 matrix)".into()],
        }),
    }
}

/// The disposable issue title every write case uses: namespace-prefixed so
/// cleanup can find (only) this run's resources.
fn issue_title(cfg: &RunnerConfig, case: &str) -> String {
    format!("{}: {case}", cfg.run_ns)
}

const DISPOSABLE_BODY: &str =
    "disposable probe resource — safe to delete (run receipt carries the number)";

fn issue_create(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    let title = issue_title(cfg, "issue-create");
    let body = serde_json::json!({
        "title": title,
        "body": DISPOSABLE_BODY
    })
    .to_string();
    let (code, resp) = http
        .request("POST", &format!("repos/{}/issues", cfg.repo), &cfg.writer_token, Some(&body))
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: scrub(cfg, &e),
        })?;
    let created = serde_json::from_str::<serde_json::Value>(resp.trim()).ok();
    let number = created
        .as_ref()
        .and_then(|v| v.get("number").and_then(|n| n.as_u64()));
    let base = |outcome: &str, observed: String| {
        let (outcome, observed, actor) =
            with_actor(http, &cfg.writer_token, &secrets_of(cfg), outcome, observed);
        CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome,
        server_version: server_version.to_string(),
        actor,
        at: now_secs(),
        expected: format!("HTTP 2xx with issue.number, read back with matching title and body (disposable: {title})"),
        observed,
        notes: number
            .map(|n| vec![format!("disposable issue number: {n}")])
            .unwrap_or_default(),
    }
    };
    match (code, number) {
        (200..=299, Some(n)) => {
            // A 2xx acknowledgement alone proves nothing: read the issue back
            // and require the number, namespaced title and submitted body.
            let (rcode, rresp) = http
                .request("GET", &format!("repos/{}/issues/{n}", cfg.repo), &cfg.writer_token, None)
                .map_err(|e| ForgeOutcome::Unknown {
                    operation: view.id.clone(),
                    why: scrub(cfg, &e),
                })?;
            let read = serde_json::from_str::<serde_json::Value>(rresp.trim()).ok();
            let matches = (200..=299).contains(&rcode)
                && read.as_ref().is_some_and(|v| {
                    v.get("number").and_then(|x| x.as_u64()) == Some(n)
                        && v.get("title").and_then(|x| x.as_str()) == Some(title.as_str())
                        && v.get("body").and_then(|x| x.as_str()) == Some(DISPOSABLE_BODY)
                });
            if matches {
                Ok(base(OUTCOME_PASS, format!("issue #{n} created in {} and read back", cfg.repo)))
            } else {
                Ok(base(
                    OUTCOME_FAIL,
                    format!("issue #{n} read-back mismatch (HTTP {rcode}; {})", body_shape(&rresp)),
                ))
            }
        }
        (403 | 404, _) => Err(ForgeOutcome::InsufficientPermission {
            operation: view.id.clone(),
            principal: crate::forge_contract::CredentialRef("env:GITEA_QUAL_WRITER_TOKEN".into()),
        }),
        _ => Ok(base(OUTCOME_FAIL, format!("HTTP {code} ({})", body_shape(&resp)))),
    }
}

fn issue_comment_readback(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    // Create the disposable issue, comment on it, read the comment back:
    // a write is only proven when the read sees it (read-after-write).
    let create = issue_create(test_id, view, cfg, http, server_version)?;
    // Ownership of the disposable issue is proven only by a passing create
    // (number + title + body read back). Anything else must not be written to.
    if create.outcome != OUTCOME_PASS {
        return Ok(create);
    }
    let number: u64 = create
        .notes
        .iter()
        .filter_map(|n| n.rsplit_once(": ").map(|(_, v)| v.trim().parse().ok()))
        .flatten()
        .next()
        .ok_or_else(|| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: "issue-create did not yield a number to comment on".into(),
        })?;
    let marker = format!("probe-{}", now_secs());
    let (code, resp) = http
        .request(
            "POST",
            &format!("repos/{}/issues/{number}/comments", cfg.repo),
            &cfg.writer_token,
            Some(&serde_json::json!({"body": marker}).to_string()),
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: scrub(cfg, &e),
        })?;
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("comment POST answered {code} ({})", body_shape(&resp)),
        });
    }
    let (rcode, rresp) = http
        .request(
            "GET",
            &format!("repos/{}/issues/{number}/comments", cfg.repo),
            &cfg.writer_token,
            None,
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: scrub(cfg, &e),
        })?;
    let seen = serde_json::from_str::<serde_json::Value>(rresp.trim())
        .ok()
        .and_then(|v| {
            v.as_array().map(|a| {
                a.iter()
                    .any(|c| c.get("body").and_then(|b| b.as_str()) == Some(marker.as_str()))
            })
        })
        .unwrap_or(false);
    let (outcome, observed, actor) = with_actor(
        http,
        &cfg.writer_token,
        &secrets_of(cfg),
        if seen && (200..=299).contains(&rcode) {
            OUTCOME_PASS
        } else {
            OUTCOME_FAIL
        },
        if seen {
            "read-after-write confirmed".into()
        } else {
            "marker not found on read-back".into()
        },
    );
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome,
        server_version: server_version.to_string(),
        actor,
        at: now_secs(),
        expected: format!("comment read-back contains {marker:?}"),
        observed,
        notes: vec![format!("disposable issue number: {number}")],
    })
}

/// Resolve the acting principal for a case. Each required operation must carry
/// actor evidence, so an otherwise-passing case whose actor cannot be resolved
/// fails closed rather than recording a PASS with no actor.
fn with_actor(
    http: &dyn ProbeHttp,
    token: &str,
    secrets: &[&str],
    outcome: &str,
    observed: String,
) -> (String, String, Option<String>) {
    let actor = actor_of(http, token, secrets);
    if outcome == OUTCOME_PASS && actor.is_none() {
        return (
            OUTCOME_FAIL.into(),
            format!(
                "{observed}; actor evidence unavailable (GET user failed or returned no login)"
            ),
            None,
        );
    }
    (outcome.into(), observed, actor)
}

/// The login comes from the server and lands in a published receipt, so one
/// that carries a run credential (or URL userinfo) is unusable evidence: it
/// resolves to `None` (a case then fails closed) rather than to a redacted
/// placeholder that could pass for a real actor.
fn actor_of(http: &dyn ProbeHttp, token: &str, secrets: &[&str]) -> Option<String> {
    let (code, body) = http.request("GET", "user", token, None).ok()?;
    if !(200..=299).contains(&code) {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(body.trim())
        .ok()
        .and_then(|v| v.get("login").and_then(|l| l.as_str()).map(String::from))
        .filter(|l| !l.trim().is_empty() && is_clean(l, secrets))
}

/// True when redaction would leave `s` unchanged, i.e. it embeds no credential.
fn is_clean(s: &str, secrets: &[&str]) -> bool {
    redact_secrets(s, secrets) == s
}

fn secrets_of(cfg: &RunnerConfig) -> [&str; 2] {
    [
        cfg.writer_token.as_str(),
        cfg.readonly_token.as_deref().unwrap_or(""),
    ]
}

/// Describe a response body without echoing it: a server may reflect a
/// request credential back, and receipts are published (#9789). Only the
/// size and whether it parsed as JSON survive.
fn body_shape(s: &str) -> String {
    let kind = if serde_json::from_str::<serde_json::Value>(s.trim()).is_ok() {
        "JSON"
    } else {
        "non-JSON"
    };
    format!("{kind} body, {} bytes, not echoed", s.len())
}

/// Replace every non-empty secret in `s` with `[redacted]` and blank any
/// `user:pass@` URL userinfo.
fn redact_secrets(s: &str, secrets: &[&str]) -> String {
    let mut out = s.to_string();
    for sec in secrets.iter().filter(|x| !x.is_empty()) {
        out = out.replace(sec, "[redacted]");
    }
    let mut res = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        res.push_str(head);
        let end = tail
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                res.push_str("[redacted]@");
                rest = &tail[at + 1..];
            }
            None => rest = tail,
        }
    }
    res.push_str(rest);
    res
}

/// Redact the run's writer/readonly tokens from a transport error.
fn scrub(cfg: &RunnerConfig, s: &str) -> String {
    redact_secrets(s, &secrets_of(cfg))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The verdict: exit nonzero when any required row is not a clean pass —
/// #9789's "required unknowns/unsupported produce a nonzero qualification
/// verdict" rule. A successful API response alone is insufficient. Returns
/// (ok, unknown_required, failed_required, unsupported_required).
pub fn verdict(results: &[CaseResult]) -> (bool, usize, usize, usize) {
    let mut unknown = 0;
    let mut failed = 0;
    let mut unsupported = 0;
    for r in results {
        if r.disposition != "required" {
            continue;
        }
        match r.outcome.as_str() {
            OUTCOME_PASS => {}
            OUTCOME_UNKNOWN => unknown += 1,
            OUTCOME_UNSUPPORTED => unsupported += 1,
            _ => failed += 1,
        }
    }
    (unknown == 0 && failed == 0 && unsupported == 0, unknown, failed, unsupported)
}

#[cfg(test)]
mod tests;
