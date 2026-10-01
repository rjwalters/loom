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
//! (`forge-probe::<group>::<name>`); a handler absent for this slice leaves
//! its row `unknown` with the reason recorded.
//!
//! Slice scope: the coordination profile is fully implemented (the runner
//! plus every coordination row — #9789 slice 2); every other profile stays
//! honestly unknown until its own slice (#9790 owns the CI/delivery ones).

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
            // Gitea's binder needs the JSON content type — with curl's
            // default form content type the body parses as an empty form
            // and every write answers 422 "[Title]: Required" (the first
            // thing the live probe run caught, 2026-10-01).
            cmd.arg("--data-binary").arg(body);
            cmd.arg("-H").arg("Content-Type: application/json");
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn curl: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(format!("Authorization: token {token}\n").as_bytes());
        }
        let output = child
            .wait_with_output()
            .map_err(|e| format!("curl wait: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "curl exit {}: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
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

/// Build the receipt: every required-profile row from the probe manifest,
/// in the manifest's risk-first order, executed where a handler exists and
/// `unknown` where it does not.
pub fn run(cfg: &RunnerConfig, http: &dyn ProbeHttp) -> Result<Vec<CaseResult>> {
    let inv = crate::forge_inventory::load_embedded()?;
    let manifest = crate::forge_inventory::probe::build(&inv, &[]);
    let mut results = Vec::new();

    // The server version stamps every receipt row; a failed version probe
    // marks the whole run Unknown rather than inventing a version.
    let server_version = match http.request("GET", "version", &cfg.writer_token, None) {
        Ok((200, body)) => serde_json::from_str::<serde_json::Value>(body.trim())
            .ok()
            .and_then(|v| v.get("version").and_then(|s| s.as_str()).map(String::from))
            .unwrap_or_default(),
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
                why: format!("version probe transport fault: {e}"),
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
            // The runner executes the coordination profile; other profiles
            // stay unexecuted in the receipt so coverage stays honest.
            results.push(CaseResult::unknown(
                &view,
                "profile outside the coordination slice",
                &server_version,
            ));
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
        "forge-probe::coordination::comment-list" => {
            comment_list_ordered(test_id, view, cfg, http, server_version)
        }
        "forge-probe::coordination::issue-list" => {
            issue_list_paged(test_id, view, cfg, http, server_version)
        }
        "forge-probe::coordination::issue-search" => {
            issue_search_case(test_id, view, cfg, http, server_version)
        }
        "forge-probe::coordination::issue-edit-labels" => {
            issue_edit_labels_case(test_id, view, cfg, http, server_version)
        }
        "forge-probe::coordination::issue-edit-body" => {
            issue_edit_body_case(test_id, view, cfg, http, server_version)
        }
        "forge-probe::coordination::issue-close-with-reason" => {
            issue_close_with_reason_case(test_id, view, cfg, http, server_version)
        }
        "forge-probe::coordination::label-sync-catalogue" => {
            label_sync_catalogue_case(test_id, view, cfg, http, server_version)
        }
        "forge-probe::coordination::comment-edit-delete" => {
            comment_edit_delete_case(test_id, view, cfg, http, server_version)
        }
        "forge-probe::coordination::timeline-read" => {
            timeline_read_case(test_id, view, cfg, http, server_version)
        }
        "forge-probe::coordination::epic-sub-issues" => {
            epic_sub_issues_case(test_id, view, cfg, http, server_version)
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
            notes: vec!["case handler not implemented — later slice of the #9789 matrix".into()],
        }),
    }
}

/// The disposable issue title every write case uses: namespace-prefixed so
/// cleanup can find (only) this run's resources.
fn issue_title(cfg: &RunnerConfig, case: &str) -> String {
    format!("loomp-{}: {case}", cfg.run_ns)
}

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
        "body": "disposable probe resource — safe to delete (run receipt carries the number)"
    })
    .to_string();
    let (code, resp) = http
        .request("POST", &format!("repos/{}/issues", cfg.repo), &cfg.writer_token, Some(&body))
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    let created = serde_json::from_str::<serde_json::Value>(resp.trim()).ok();
    let number = created
        .as_ref()
        .and_then(|v| v.get("number").and_then(|n| n.as_u64()));
    let base = |outcome: &str, observed: String| CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: outcome.into(),
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: format!("HTTP 2xx with issue.number (disposable: {title})"),
        observed,
        notes: number
            .map(|n| vec![format!("disposable issue number: {n}")])
            .unwrap_or_default(),
    };
    match (code, number) {
        (200..=299, Some(n)) => {
            Ok(base(OUTCOME_PASS, format!("issue #{n} created in {}", cfg.repo)))
        }
        (403 | 404, _) => Err(ForgeOutcome::InsufficientPermission {
            operation: view.id.clone(),
            principal: crate::forge_contract::CredentialRef("env:GITEA_QUAL_WRITER_TOKEN".into()),
        }),
        _ => Ok(base(OUTCOME_FAIL, format!("HTTP {code}: {}", truncate(&resp)))),
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
            why: e,
        })?;
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("comment POST answered {code}: {}", truncate(&resp)),
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
            why: e,
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
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: if seen && (200..=299).contains(&rcode) {
            OUTCOME_PASS.into()
        } else {
            OUTCOME_FAIL.into()
        },
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: format!("comment read-back contains {marker:?}"),
        observed: if seen {
            "read-after-write confirmed".into()
        } else {
            "marker not found on read-back".into()
        },
        notes: vec![format!("disposable issue number: {number}")],
    })
}

/// A row the runner refused to execute: honestly `unknown`, with the
/// safety reason recorded (#9789: unexecuted stays unknown, never a pass).
fn refused_row(
    view: &ProbeEntryView,
    test_id: &str,
    server_version: &str,
    what: &str,
) -> CaseResult {
    CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: OUTCOME_UNKNOWN.into(),
        server_version: server_version.to_string(),
        actor: None,
        at: now_secs(),
        expected: format!("a recorded {what}"),
        observed: "refused: live-write not opted in (--live-write)".into(),
        notes: vec!["safety: read-only by default (#9789)".into()],
    }
}

/// Fixture: one disposable issue carrying `marker` in its body. Write setup
/// — the caller gates it on `live_write` exactly like a write case.
fn fixture_issue(
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    case: &str,
    marker: &str,
) -> std::result::Result<u64, ForgeOutcome> {
    let title = issue_title(cfg, case);
    let body = serde_json::json!({
        "title": title,
        "body": format!("disposable probe resource — safe to delete\n\n{marker}")
    })
    .to_string();
    let (code, resp) = http
        .request("POST", &format!("repos/{}/issues", cfg.repo), &cfg.writer_token, Some(&body))
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    let number = serde_json::from_str::<serde_json::Value>(resp.trim())
        .ok()
        .and_then(|v| v.get("number").and_then(|n| n.as_u64()));
    match (code, number) {
        (200..=299, Some(n)) => Ok(n),
        (403 | 404, _) => Err(ForgeOutcome::InsufficientPermission {
            operation: view.id.clone(),
            principal: crate::forge_contract::CredentialRef("env:GITEA_QUAL_WRITER_TOKEN".into()),
        }),
        _ => Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("fixture issue-create answered {code}: {}", truncate(&resp)),
        }),
    }
}

/// Read a JSON object field from a response body.
fn json_obj(body: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(body.trim()).ok()
}

/// A page-walk over a paginated list endpoint: fetch `page` (1-based,
/// `limit` per page) until an empty page or `max_pages`, and report the
/// walked rows. A transport fault mid-walk is `None` — the caller records
/// partial pagination, never a pass on a short read.
fn walk_pages(
    view: &ProbeEntryView,
    http: &dyn ProbeHttp,
    token: &str,
    base_path: &str,
    limit: usize,
    max_pages: usize,
) -> std::result::Result<Vec<serde_json::Value>, ForgeOutcome> {
    let mut rows = Vec::new();
    for page in 1..=max_pages {
        // base_path may already carry a query string (state/type filters)
        let sep = if base_path.contains('?') { '&' } else { '?' };
        let path = format!("{base_path}{sep}page={page}&limit={limit}");
        let (code, body) = http.request("GET", &path, token, None).map_err(|e| {
            ForgeOutcome::Unknown {
                operation: view.id.clone(),
                why: format!(
                    "page {page} transport fault — partial pagination after {} row(s) over {} page(s), the list is unusable for decisions: {e}",
                    rows.len(),
                    page - 1
                ),
            }
        })?;
        if !(200..=299).contains(&code) {
            return Err(ForgeOutcome::Unknown {
                operation: view.id.clone(),
                why: format!("list page {page} answered {code}"),
            });
        }
        let arr = json_obj(&body)
            .and_then(|v| {
                if v.is_array() {
                    v.as_array().cloned()
                } else {
                    v.get("data").and_then(|d| d.as_array().cloned())
                }
            })
            .ok_or_else(|| ForgeOutcome::Unknown {
                operation: view.id.clone(),
                why: format!("list page {page} is not a JSON array: {}", truncate(&body)),
            })?;
        let done = arr.is_empty();
        rows.extend(arr);
        if done {
            return Ok(rows);
        }
    }
    Err(ForgeOutcome::Unknown {
        operation: view.id.clone(),
        why: format!("list pagination did not terminate within {max_pages} pages"),
    })
}

// -- slice-2 coordination handlers -------------------------------------------

/// `comment-list` (HIGH): the lease arbiter. Two comments on a fixture
/// issue must read back in creation ORDER, each carrying a forge-assigned
/// `updated_at`, over COMPLETE pagination (a truncated first-page read can
/// miss the earliest lease record — #9777's unknown).
fn comment_list_ordered(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    if !cfg.live_write {
        return Ok(refused_row(
            view,
            test_id,
            server_version,
            "comment order + updated_at evidence",
        ));
    }
    let base = format!("repos/{}/issues", cfg.repo);
    let n = fixture_issue(view, cfg, http, "comment-list", "comment-list fixture")?;
    // Deterministic per run_ns: the fake scripts them; the run namespaces
    // them across runs.
    let marker1 = format!("probe-c1-{}", cfg.run_ns);
    let marker2 = format!("probe-c2-{}", cfg.run_ns);
    for m in [&marker1, &marker2] {
        let (code, resp) = http
            .request(
                "POST",
                &format!("{base}/{n}/comments"),
                &cfg.writer_token,
                Some(&serde_json::json!({"body": m}).to_string()),
            )
            .map_err(|e| ForgeOutcome::Unknown {
                operation: view.id.clone(),
                why: e,
            })?;
        if !(200..=299).contains(&code) {
            return Err(ForgeOutcome::Unknown {
                operation: view.id.clone(),
                why: format!("comment POST answered {code}: {}", truncate(&resp)),
            });
        }
    }
    let rows = walk_pages(view, http, &cfg.writer_token, &format!("{base}/{n}/comments"), 1, 5)?;
    let bodies: Vec<String> = rows
        .iter()
        .filter_map(|c| c.get("body").and_then(|b| b.as_str()).map(String::from))
        .collect();
    let with_updated = rows
        .iter()
        .filter(|c| c.get("updated_at").and_then(|u| u.as_str()).is_some())
        .count();
    let ids: Vec<String> = rows
        .iter()
        .filter_map(|c| c.get("id").map(|i| i.to_string()))
        .collect();
    let order_ok = bodies.len() >= 2
        && bodies
            .iter()
            .rev()
            .take(2)
            .rev()
            .eq([marker1.as_str(), marker2.as_str()]);
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: if order_ok && with_updated == rows.len() {
            OUTCOME_PASS.into()
        } else {
            OUTCOME_FAIL.into()
        },
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: format!(
            "comments in creation order [{marker1}, {marker2}] with updated_at on every row, complete pagination"
        ),
        observed: format!(
            "{} row(s) walked; order {} ; updated_at on {with_updated}/{}",
            rows.len(),
            if order_ok { "preserved" } else { "BROKEN" },
            rows.len()
        ),
        notes: vec![
            format!("disposable issue number: {n}"),
            format!("comment ids: [{}]", ids.join(", ")),
        ],
    })
}

/// `issue-list`: complete pagination over a multi-page list — every page
/// walked until empty (bounded); a transport fault mid-walk records partial
/// pagination and stays unknown (the page-two-failure rule).
fn issue_list_paged(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    if !cfg.live_write {
        return Ok(refused_row(view, test_id, server_version, "multi-page list evidence"));
    }
    let n1 = fixture_issue(view, cfg, http, "issue-list", "issue-list fixture one")?;
    let n2 = fixture_issue(view, cfg, http, "issue-list", "issue-list fixture two")?;
    let rows = walk_pages(
        view,
        http,
        &cfg.writer_token,
        &format!("repos/{}/issues?state=all&type=issues", cfg.repo),
        1,
        5,
    )?;
    let numbers: Vec<u64> = rows
        .iter()
        .filter_map(|i| i.get("number").and_then(|v| v.as_u64()))
        .collect();
    let found = numbers.contains(&n1) && numbers.contains(&n2);
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: if found {
            OUTCOME_PASS.into()
        } else {
            OUTCOME_FAIL.into()
        },
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: "complete pagination: a walked multi-page list contains every created issue"
            .into(),
        observed: format!(
            "{} row(s) walked; fixtures #{n1},#{n2} {}",
            rows.len(),
            if found { "present" } else { "MISSING" }
        ),
        notes: vec![format!("disposable issue numbers: {n1}, {n2}")],
    })
}

/// `issue-search`: a marker-bearing fixture is found in the first page of
/// the search endpoint (first-page-sufficient per the manifest).
fn issue_search_case(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    if !cfg.live_write {
        return Ok(refused_row(view, test_id, server_version, "search-hit evidence"));
    }
    // Deterministic per run_ns: the fake scripts the exact search URL.
    let marker = format!("loomp-search-{}", cfg.run_ns);
    let n = fixture_issue(view, cfg, http, "issue-search", &marker)?;
    let (code, body) = http
        .request(
            "GET",
            &format!("repos/issues/search?q={marker}&type=issues"),
            &cfg.writer_token,
            None,
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    let found = json_obj(&body)
        .map(|v| {
            let arr = v
                .as_array()
                .cloned()
                .or_else(|| v.get("data").and_then(|d| d.as_array().cloned()))
                .unwrap_or_default();
            arr.iter()
                .any(|i| i.get("number").and_then(|x| x.as_u64()) == Some(n))
        })
        .unwrap_or(false);
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: if (200..=299).contains(&code) && found {
            OUTCOME_PASS.into()
        } else {
            OUTCOME_FAIL.into()
        },
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: format!("search finds the marker fixture #{n} in the first page"),
        observed: format!("search answered {code}; hit: {found}"),
        notes: vec![format!("disposable issue number: {n}")],
    })
}

/// `issue-edit-labels` (HIGH): a colon+Unicode-bearing label must survive
/// add + readback byte-equal — the name-encoding unknown (#9777).
fn issue_edit_labels_case(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    if !cfg.live_write {
        return Ok(refused_row(view, test_id, server_version, "label round-trip evidence"));
    }
    let base = format!("repos/{}", cfg.repo);
    let n = fixture_issue(view, cfg, http, "issue-edit-labels", "labels fixture")?;
    let name = format!("loomp-{}: labels ✓", cfg.run_ns);
    let (code, resp) = http
        .request(
            "POST",
            &format!("{base}/labels"),
            &cfg.writer_token,
            Some(&serde_json::json!({"name": name, "color": "#00aabb"}).to_string()),
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    let label_id = json_obj(&resp).and_then(|v| v.get("id").and_then(|i| i.as_u64()));
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("label create answered {code}: {}", truncate(&resp)),
        });
    }
    let Some(label_id) = label_id else {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("label create carried no id: {}", truncate(&resp)),
        });
    };
    let (code, resp) = http
        .request(
            "POST",
            &format!("{base}/issues/{n}/labels"),
            &cfg.writer_token,
            Some(&serde_json::json!({"labels": [label_id]}).to_string()),
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("label add answered {code}: {}", truncate(&resp)),
        });
    }
    let (_, body) = http
        .request("GET", &format!("{base}/issues/{n}"), &cfg.writer_token, None)
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    let readback = json_obj(&body)
        .and_then(|v| {
            v.get("labels")
                .and_then(|l| l.as_array())
                .map(|a| a.to_owned())
        })
        .unwrap_or_default();
    let names: Vec<String> = readback
        .iter()
        .filter_map(|l| l.get("name").and_then(|x| x.as_str()).map(String::from))
        .collect();
    let exact = names.iter().any(|x| x == &name);
    // best-effort cleanup of the disposable label
    let (ccode, _) = http
        .request("DELETE", &format!("{base}/labels/{label_id}"), &cfg.writer_token, None)
        .unwrap_or((0, String::new()));
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: if exact { OUTCOME_PASS.into() } else { OUTCOME_FAIL.into() },
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: format!("label {name:?} survives add + readback byte-equal"),
        observed: format!(
            "readback labels {names:?}; exact {}",
            if exact { "match" } else { "MISMATCH" }
        ),
        notes: vec![
            format!("disposable issue number: {n}"),
            format!("label id {label_id}; cleanup DELETE answered {ccode}"),
            "competing-claims ordering (concurrent add/remove from two hosts) needs a two-client run — not probeable single-client".into(),
        ],
    })
}

/// `issue-edit-body`: machine markers must survive a body edit byte-equal —
/// literal-body fidelity (#9777's unknown: provider-side rewriting breaks
/// park/premise records).
fn issue_edit_body_case(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    if !cfg.live_write {
        return Ok(refused_row(view, test_id, server_version, "literal-body evidence"));
    }
    let base = format!("repos/{}", cfg.repo);
    let n = fixture_issue(view, cfg, http, "issue-edit-body", "body fixture")?;
    let body_text =
        "<!-- loom:park reason=\"probe\" -->\n\nunicode ✓ 'single \"double' <tags &>".to_string();
    let (code, resp) = http
        .request(
            "PATCH",
            &format!("{base}/issues/{n}"),
            &cfg.writer_token,
            Some(&serde_json::json!({"body": body_text}).to_string()),
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("body edit answered {code}: {}", truncate(&resp)),
        });
    }
    let (_, body) = http
        .request("GET", &format!("{base}/issues/{n}"), &cfg.writer_token, None)
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    let readback = json_obj(&body)
        .and_then(|v| v.get("body").and_then(|b| b.as_str()).map(String::from))
        .unwrap_or_default();
    let equal = readback == body_text;
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: if equal {
            OUTCOME_PASS.into()
        } else {
            OUTCOME_FAIL.into()
        },
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: "edited body readback byte-equal (markers, unicode, quotes, tags intact)".into(),
        observed: if equal {
            "byte-equal".into()
        } else {
            format!("REWRITTEN: {readback:?}")
        },
        notes: vec![format!("disposable issue number: {n}")],
    })
}

/// `issue-close-with-reason`: close as not-planned, read the reason back,
/// reopen — Loom's role-autonomy rules depend on the distinction being
/// durable and readable.
fn issue_close_with_reason_case(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    if !cfg.live_write {
        return Ok(refused_row(view, test_id, server_version, "close-reason evidence"));
    }
    let base = format!("repos/{}", cfg.repo);
    let n = fixture_issue(view, cfg, http, "issue-close-with-reason", "close fixture")?;
    let (code, resp) = http
        .request(
            "PATCH",
            &format!("{base}/issues/{n}"),
            &cfg.writer_token,
            Some(
                &serde_json::json!({"state": "closed", "state_reason": "not_planned"}).to_string(),
            ),
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("close answered {code}: {}", truncate(&resp)),
        });
    }
    let (_, body) = http
        .request("GET", &format!("{base}/issues/{n}"), &cfg.writer_token, None)
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    let obj = json_obj(&body);
    let state = obj
        .as_ref()
        .and_then(|v| v.get("state").and_then(|s| s.as_str()))
        .unwrap_or("");
    let reason = obj
        .as_ref()
        .and_then(|v| v.get("state_reason"))
        .and_then(|r| {
            if r.is_null() {
                None
            } else {
                r.as_str().map(String::from)
            }
        });
    let closed = state == "closed";
    let reason_ok = reason.as_deref() == Some("not_planned");
    // reopen to leave the fixture reusable/closable by cleanup
    let (_, _) = http
        .request(
            "PATCH",
            &format!("{base}/issues/{n}"),
            &cfg.writer_token,
            Some(&serde_json::json!({"state": "open"}).to_string()),
        )
        .unwrap_or((0, String::new()));
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: if closed && reason_ok {
            OUTCOME_PASS.into()
        } else {
            OUTCOME_FAIL.into()
        },
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: "close reads back state=closed with a durable state_reason=not_planned".into(),
        observed: format!(
            "state={state}; state_reason={}",
            reason.as_deref().unwrap_or("(absent/null)")
        ),
        notes: vec![format!("disposable issue number: {n}")],
    })
}

/// `label-sync-catalogue`: create → edit → list (complete) → delete →
/// list, recording the numeric-id/name pairing the label layer must
/// translate (#9777: Gitea addresses labels by id, Loom by name).
fn label_sync_catalogue_case(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    if !cfg.live_write {
        return Ok(refused_row(view, test_id, server_version, "label catalogue evidence"));
    }
    let base = format!("repos/{}", cfg.repo);
    let name = format!("loomp-{}: catalogue", cfg.run_ns);
    let (code, resp) = http
        .request(
            "POST",
            &format!("{base}/labels"),
            &cfg.writer_token,
            Some(
                &serde_json::json!({"name": name, "color": "#00aabb", "description": "probe"})
                    .to_string(),
            ),
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    let label_id = json_obj(&resp).and_then(|v| v.get("id").and_then(|i| i.as_u64()));
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("label create answered {code}: {}", truncate(&resp)),
        });
    }
    let Some(label_id) = label_id else {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("label create carried no id: {}", truncate(&resp)),
        });
    };
    let (code, resp) = http
        .request(
            "PATCH",
            &format!("{base}/labels/{label_id}"),
            &cfg.writer_token,
            Some(
                &serde_json::json!({"color": "#00ccdd", "description": "probe-edited"}).to_string(),
            ),
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("label edit answered {code}: {}", truncate(&resp)),
        });
    }
    let rows = walk_pages(view, http, &cfg.writer_token, &format!("{base}/labels"), 50, 5)?;
    let hit = rows
        .iter()
        .find(|l| l.get("id").and_then(|i| i.as_u64()) == Some(label_id));
    let listed_ok = hit
        .map(|l| {
            l.get("color").and_then(|c| c.as_str()) == Some("#00ccdd")
                && l.get("description").and_then(|d| d.as_str()) == Some("probe-edited")
        })
        .unwrap_or(false);
    let (_ccode, _) = http
        .request("DELETE", &format!("{base}/labels/{label_id}"), &cfg.writer_token, None)
        .unwrap_or((0, String::new()));
    let rows_after = walk_pages(view, http, &cfg.writer_token, &format!("{base}/labels"), 50, 5)
        .unwrap_or_default();
    let gone = !rows_after
        .iter()
        .any(|l| l.get("id").and_then(|i| i.as_u64()) == Some(label_id));
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: if listed_ok && gone {
            OUTCOME_PASS.into()
        } else {
            OUTCOME_FAIL.into()
        },
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: "label create/edit/read/delete round-trips with the edit visible in the catalogue list".into(),
        observed: format!(
            "listed {} ; deleted {}",
            if listed_ok { "edited state" } else { "STALE edit" },
            if gone { "confirmed" } else { "STILL PRESENT" }
        ),
        notes: vec![format!("label id {label_id} (numeric); name {name:?} — id/name translation is the integration's job")],
    })
}

/// `comment-edit-delete`: edit a comment in place and delete it — the
/// lease-renewal path re-PATCHes a comment it located by marker, so the id
/// namespace and edit semantics are load-bearing.
fn comment_edit_delete_case(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    if !cfg.live_write {
        return Ok(refused_row(view, test_id, server_version, "comment edit/delete evidence"));
    }
    let base = format!("repos/{}/issues", cfg.repo);
    let n = fixture_issue(view, cfg, http, "comment-edit-delete", "comment edit fixture")?;
    let (code, resp) = http
        .request(
            "POST",
            &format!("{base}/{n}/comments"),
            &cfg.writer_token,
            Some(&serde_json::json!({"body": "original"}).to_string()),
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    let cid = json_obj(&resp).and_then(|v| v.get("id").and_then(|i| i.as_u64()));
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("comment POST answered {code}: {}", truncate(&resp)),
        });
    }
    let Some(cid) = cid else {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("comment POST carried no id: {}", truncate(&resp)),
        });
    };
    let (code, resp) = http
        .request(
            "PATCH",
            &format!("{base}/comments/{cid}"),
            &cfg.writer_token,
            Some(&serde_json::json!({"body": "edited"}).to_string()),
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("comment edit answered {code}: {}", truncate(&resp)),
        });
    }
    let rows = walk_pages(view, http, &cfg.writer_token, &format!("{base}/{n}/comments"), 50, 5)?;
    let hit = rows
        .iter()
        .find(|c| c.get("id").and_then(|i| i.as_u64()) == Some(cid));
    let edited = hit
        .map(|c| c.get("body").and_then(|b| b.as_str()) == Some("edited"))
        .unwrap_or(false);
    let (dcode, _) = http
        .request("DELETE", &format!("{base}/comments/{cid}"), &cfg.writer_token, None)
        .unwrap_or((0, String::new()));
    let rows_after =
        walk_pages(view, http, &cfg.writer_token, &format!("{base}/{n}/comments"), 50, 5)
            .unwrap_or_default();
    let gone = !rows_after
        .iter()
        .any(|c| c.get("id").and_then(|i| i.as_u64()) == Some(cid));
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: if edited && gone {
            OUTCOME_PASS.into()
        } else {
            OUTCOME_FAIL.into()
        },
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: "comment edits in place and deletes cleanly (id namespace: repo-global)".into(),
        observed: format!(
            "edit visible: {edited}; delete: HTTP {dcode}, {}",
            if gone {
                "confirmed gone"
            } else {
                "STILL PRESENT"
            }
        ),
        notes: vec![
            format!("disposable issue number: {n}"),
            format!("comment id {cid} ({}-scoped)", "repo"),
        ],
    })
}

/// `timeline-read`: a fixture issue's timeline must merge comments and
/// state events with their timestamps intact.
fn timeline_read_case(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    if !cfg.live_write {
        return Ok(refused_row(view, test_id, server_version, "timeline evidence"));
    }
    let base = format!("repos/{}/issues", cfg.repo);
    let n = fixture_issue(view, cfg, http, "timeline-read", "timeline fixture")?;
    let (code, resp) = http
        .request(
            "POST",
            &format!("{base}/{n}/comments"),
            &cfg.writer_token,
            Some(&serde_json::json!({"body": "timeline marker"}).to_string()),
        )
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("comment POST answered {code}: {}", truncate(&resp)),
        });
    }
    let (_, _) = http
        .request(
            "PATCH",
            &format!("{base}/{n}"),
            &cfg.writer_token,
            Some(&serde_json::json!({"state": "closed"}).to_string()),
        )
        .unwrap_or((0, String::new()));
    let rows = walk_pages(view, http, &cfg.writer_token, &format!("{base}/{n}/timeline"), 50, 5)?;
    let kinds: Vec<String> = rows
        .iter()
        .filter_map(|e| e.get("type").and_then(|t| t.as_str()).map(String::from))
        .collect();
    let has_comment = kinds.iter().any(|k| k == "comment");
    let has_close = kinds.iter().any(|k| k == "close");
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: if has_comment && has_close {
            OUTCOME_PASS.into()
        } else if rows.is_empty() {
            OUTCOME_UNSUPPORTED.into()
        } else {
            OUTCOME_FAIL.into()
        },
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: "timeline carries the comment and close events".into(),
        observed: format!("event types: [{}]", kinds.join(", ")),
        notes: vec![format!("disposable issue number: {n}")],
    })
}

/// `epic-sub-issues`: GitHub has sub-issues; Gitea exposes issue
/// DEPENDENCIES — a different edge. A missing endpoint is honest
/// `unsupported` (the answer the GO/NO-GO needs), never a pass.
fn epic_sub_issues_case(
    test_id: &str,
    view: &ProbeEntryView,
    cfg: &RunnerConfig,
    http: &dyn ProbeHttp,
    server_version: &str,
) -> std::result::Result<CaseResult, ForgeOutcome> {
    if !cfg.live_write {
        return Ok(refused_row(view, test_id, server_version, "sub-issue evidence"));
    }
    let base = format!("repos/{}/issues", cfg.repo);
    let n = fixture_issue(view, cfg, http, "epic-sub-issues", "parent fixture")?;
    let (code, body) = http
        .request("GET", &format!("{base}/{n}/sub_issues"), &cfg.writer_token, None)
        .map_err(|e| ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: e,
        })?;
    Ok(CaseResult {
        test_id: test_id.to_string(),
        operation: view.id.clone(),
        risk: view.risk.clone(),
        disposition: view.disposition.clone(),
        outcome: match code {
            200..=299 => OUTCOME_PASS.into(),
            404 => OUTCOME_UNSUPPORTED.into(),
            _ => OUTCOME_FAIL.into(),
        },
        server_version: server_version.to_string(),
        actor: actor_of(http, &cfg.writer_token),
        at: now_secs(),
        expected: "a sub-issue/dependency edge readable through the API".into(),
        observed: match code {
            200..=299 => format!("endpoint exists; shape: {}", truncate(&body)),
            404 => "endpoint absent (404) — containment is not a native edge".into(),
            c => format!("answered {c}"),
        },
        notes: vec![
            format!("disposable issue number: {n}"),
            "dependency edges (block/blocked-by) are a different relationship from GitHub sub-issue containment".into(),
        ],
    })
}

fn actor_of(http: &dyn ProbeHttp, token: &str) -> Option<String> {
    let (code, body) = http.request("GET", "user", token, None).ok()?;
    if !(200..=299).contains(&code) {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(body.trim())
        .ok()
        .and_then(|v| v.get("login").and_then(|l| l.as_str()).map(String::from))
}

fn truncate(s: &str) -> String {
    let t = s.trim().replace('\n', " ");
    if t.chars().count() > 120 {
        format!("{}…", t.chars().take(120).collect::<String>())
    } else {
        t
    }
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Scripted fake: exact-path routes. A request answers from the route
    /// whose full `path` (manifest-relative, query string included) matches
    /// exactly — deterministic, no substring hazards between
    /// `/issues`, `/issues/7/labels` and `/labels` — pops the next scripted
    /// answer, and repeats the last one once the queue runs dry. Offline,
    /// no jq/curl subprocess — the shim lessons from #9880 apply.
    struct FakeHttp {
        routes: Vec<(String, std::cell::RefCell<Queue>)>,
        calls: std::cell::Cell<u32>,
    }

    struct Queue {
        script: Vec<Result<(u16, String), String>>,
        last: Option<Result<(u16, String), String>>,
    }

    impl FakeHttp {
        fn new() -> Self {
            Self {
                routes: Vec::new(),
                calls: std::cell::Cell::new(0),
            }
        }
        fn push(&mut self, path: &str, responses: Vec<Result<(u16, String), String>>) {
            self.routes.push((
                path.to_owned(),
                std::cell::RefCell::new(Queue {
                    script: responses,
                    last: None,
                }),
            ));
        }
    }

    impl ProbeHttp for FakeHttp {
        fn request(
            &self,
            _method: &str,
            path: &str,
            _token: &str,
            _body: Option<&str>,
        ) -> Result<(u16, String), String> {
            self.calls.set(self.calls.get() + 1);
            let Some(q) = self.routes.iter().find(|(p, _)| p == path).map(|(_, q)| q) else {
                return Err(format!("fake: no scripted response for {path}"));
            };
            let mut q = q.borrow_mut();
            if !q.script.is_empty() {
                let answer = q.script.remove(0);
                q.last = Some(answer.clone());
                return answer;
            }
            // Repeat the last answer for calls beyond the script.
            match q.last.clone() {
                Some(a) => a,
                None => Err(format!("fake: scripted queue exhausted for {path}")),
            }
        }
    }

    fn cfg(live_write: bool) -> RunnerConfig {
        RunnerConfig {
            origin: "https://gitea.example.com".into(),
            repo: "qual-org/loomp-test".into(),
            writer_token: "writer-tok".into(),
            readonly_token: None,
            run_ns: "loomp-testrun".into(),
            live_write,
            timeout: Duration::from_secs(5),
            only: Vec::new(),
        }
    }

    #[test]
    fn read_only_run_records_refusals_and_the_verdict_stays_nonzero() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        let results = run(&cfg(false), &http).unwrap();
        assert!(!results.is_empty());
        let (_, unknown, failed, unsupported) = verdict(&results);
        assert!(unknown > 0, "read-only mode cannot execute the whole matrix");
        assert_eq!(failed, 0);
        assert_eq!(unsupported, 0);
        let (ok, ..) = verdict(&results);
        assert!(!ok, "required unknowns must produce a nonzero verdict");
    }

    #[test]
    fn write_cases_refuse_without_the_live_write_opt_in() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        let results = run(&cfg(false), &http).unwrap();
        let write_refused = results
            .iter()
            .filter(|r| r.observed.contains("refused: live-write not opted in"))
            .count();
        assert!(write_refused > 0, "write rows refuse without opt-in");
        let exec = results.iter().filter(|r| r.outcome == OUTCOME_PASS).count();
        assert_eq!(exec, 0, "nothing executes write paths in read-only mode");
    }

    #[test]
    fn server_version_stamps_every_receipt_row() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        let results = run(&cfg(false), &http).unwrap();
        assert!(results.iter().all(|r| r.server_version == "28.0.0"));
    }

    #[test]
    fn an_unreachable_server_is_an_error_not_a_zero_exit() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Err("connection refused".into())]);
        assert!(run(&cfg(false), &http).is_err(), "no version = no run");
    }

    #[test]
    fn issue_create_passes_on_2xx_with_a_number_and_records_the_disposable_number() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/issues",
            vec![Ok((201, r#"{"number": 12, "title": "x"}"#.into()))],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["issue-create".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("issue-create"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_PASS);
        assert!(row.notes.iter().any(|n| n.contains("12")));
        assert!(row.observed.contains("#12"));
    }

    #[test]
    fn a_403_is_insufficient_permission_never_does_not_exist() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/issues",
            vec![Ok((403, r#"{"message":"denied"}"#.into()))],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["issue-create".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("issue-create"))
            .unwrap();
        // The transport outcome is Unknown in the receipt (the outcome enum
        // carried InsufficientPermission), and the verdict stays nonzero —
        // a permission problem is never a pass.
        assert_eq!(row.outcome, OUTCOME_UNKNOWN);
        let (ok, ..) = verdict(&results);
        assert!(!ok);
    }

    #[test]
    fn comment_readback_requires_the_marker_on_read() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push("repos/qual-org/loomp-test/issues", vec![Ok((201, r#"{"number": 7}"#.into()))]);
        // comments POST + GET: marker absent -> FAIL (the fake repeats the
        // last scripted answer once its queue runs dry)
        http.push(
            "repos/qual-org/loomp-test/issues/7/comments",
            vec![Ok((200, r#"[{"body": "something else"}]"#.into()))],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["comment-create".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("comment-create"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_FAIL, "absent marker is a fail");
    }

    #[test]
    fn disposable_titles_carry_the_run_namespace() {
        let cfg = cfg(true);
        assert!(issue_title(&cfg, "issue-create").starts_with("loomp-loomp-testrun:"));
        assert!(issue_title(&cfg, "issue-create").contains("issue-create"));
    }

    // -- slice 2: the coordination profile's remaining rows -----------------

    #[test]
    fn comment_list_preserves_order_updated_at_and_completes_pagination() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push("repos/qual-org/loomp-test/issues", vec![Ok((201, r#"{"number": 9}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/issues/9/comments",
            vec![
                Ok((200, r#"{"id": 101}"#.into())),
                Ok((200, r#"{"id": 102}"#.into())),
            ],
        );
        http.push(
            "repos/qual-org/loomp-test/issues/9/comments?page=1&limit=1",
            vec![Ok((
                200,
                r#"[{"id": 101, "body": "probe-c1-loomp-testrun", "updated_at": "2026-10-01T00:00:00Z"}]"#.into(),
            ))],
        );
        http.push(
            "repos/qual-org/loomp-test/issues/9/comments?page=2&limit=1",
            vec![Ok((
                200,
                r#"[{"id": 102, "body": "probe-c2-loomp-testrun", "updated_at": "2026-10-01T00:00:01Z"}]"#.into(),
            ))],
        );
        http.push(
            "repos/qual-org/loomp-test/issues/9/comments?page=3&limit=1",
            vec![Ok((200, "[]".into()))],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["comment-list".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("comment-list"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_PASS, "observed: {}", row.observed);
        assert!(row.notes.iter().any(|n| n.contains("101")));
    }

    #[test]
    fn a_page_two_transport_fault_is_partial_pagination_never_a_pass() {
        // The acceptance rule: a page-two failure must never read as a
        // complete list — the row stays unknown and the verdict is nonzero.
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/issues",
            vec![
                Ok((201, r#"{"number": 21}"#.into())),
                Ok((201, r#"{"number": 22}"#.into())),
            ],
        );
        http.push(
            "repos/qual-org/loomp-test/issues?state=all&type=issues&page=1&limit=1",
            vec![Ok((200, r#"[{"number": 21}]"#.into()))],
        );
        http.push(
            "repos/qual-org/loomp-test/issues?state=all&type=issues&page=2&limit=1",
            vec![Err("connection reset mid-page".into())],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["issue-list".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("issue-list"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_UNKNOWN, "observed: {}", row.observed);
        assert!(row.observed.contains("partial pagination"), "observed: {}", row.observed);
        let (ok, ..) = verdict(&results);
        assert!(!ok);
    }

    #[test]
    fn a_rewritten_body_is_a_fail_not_a_pass() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push("repos/qual-org/loomp-test/issues", vec![Ok((201, r#"{"number": 4}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/issues/4",
            vec![
                Ok((201, r#"{"number": 4}"#.into())), // PATCH accepted
                Ok((
                    200,
                    r#"{"number": 4, "body": "<!-- loom:park reason=\"probe\" -->\n\nunicode ✓ 'single \"double' &lt;tags &amp;&gt;"}"#.into(),
                )), // GET readback: HTML-escaped = rewritten
            ],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["issue-edit-body".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("issue-edit-body"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_FAIL, "observed: {}", row.observed);
    }

    #[test]
    fn a_close_without_a_readable_not_planned_reason_is_a_fail() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push("repos/qual-org/loomp-test/issues", vec![Ok((201, r#"{"number": 3}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/issues/3",
            vec![
                Ok((201, r#"{"number": 3}"#.into())), // PATCH close accepted
                Ok((200, r#"{"number": 3, "state": "closed"}"#.into())), // GET: no state_reason
                Ok((201, r#"{"number": 3}"#.into())), // PATCH reopen
            ],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["issue-close-with-reason".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("close-with-reason"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_FAIL, "observed: {}", row.observed);
        assert!(row.observed.contains("absent"), "observed: {}", row.observed);
    }

    #[test]
    fn a_missing_sub_issues_endpoint_is_unsupported_and_the_verdict_stays_nonzero() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push("repos/qual-org/loomp-test/issues", vec![Ok((201, r#"{"number": 5}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/issues/5/sub_issues",
            vec![Ok((404, r#"{"message":"Not Found"}"#.into()))],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["epic-sub-issues".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("epic-sub-issues"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_UNSUPPORTED);
        let (ok, ..) = verdict(&results);
        assert!(!ok, "required + unsupported must produce a nonzero verdict");
    }

    #[test]
    fn a_colon_and_unicode_label_name_survives_add_and_readback() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push("repos/qual-org/loomp-test/issues", vec![Ok((201, r#"{"number": 6}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/labels",
            vec![Ok((
                201,
                r#"{"id": 55, "name": "loomp-loomp-testrun: labels ✓"}"#.into(),
            ))],
        );
        http.push(
            "repos/qual-org/loomp-test/issues/6/labels",
            vec![Ok((200, r#"[{"id": 55}]"#.into()))],
        );
        http.push(
            "repos/qual-org/loomp-test/issues/6",
            vec![Ok((
                200,
                r#"{"number": 6, "labels": [{"id": 55, "name": "loomp-loomp-testrun: labels ✓"}]}"#
                    .into(),
            ))],
        );
        http.push("repos/qual-org/loomp-test/labels/55", vec![Ok((204, "".into()))]);
        let mut cfg = cfg(true);
        cfg.only = vec!["issue-edit-labels".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("issue-edit-labels"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_PASS, "observed: {}", row.observed);
    }

    #[test]
    fn comment_edit_and_delete_round_trip() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push("repos/qual-org/loomp-test/issues", vec![Ok((201, r#"{"number": 8}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/issues/8/comments",
            vec![Ok((201, r#"{"id": 77}"#.into()))],
        );
        http.push(
            "repos/qual-org/loomp-test/issues/comments/77",
            vec![Ok((200, r#"{"id": 77}"#.into()))],
        );
        http.push(
            "repos/qual-org/loomp-test/issues/8/comments?page=1&limit=50",
            vec![
                Ok((200, r#"[{"id": 77, "body": "edited"}]"#.into())),
                Ok((200, "[]".into())),
            ],
        );
        http.push(
            "repos/qual-org/loomp-test/issues/8/comments?page=2&limit=50",
            vec![Ok((200, "[]".into()))],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["comment-edit-delete".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("comment-edit-delete"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_PASS, "observed: {}", row.observed);
    }

    #[test]
    fn timeline_carries_comment_and_close_events() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push("repos/qual-org/loomp-test/issues", vec![Ok((201, r#"{"number": 6}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/issues/6/comments",
            vec![Ok((200, r#"{"id": 90}"#.into()))],
        );
        http.push("repos/qual-org/loomp-test/issues/6", vec![Ok((201, r#"{"number": 6}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/issues/6/timeline?page=1&limit=50",
            vec![Ok((
                200,
                r#"[{"type": "comment"}, {"type": "close"}]"#.into(),
            ))],
        );
        http.push(
            "repos/qual-org/loomp-test/issues/6/timeline?page=2&limit=50",
            vec![Ok((200, "[]".into()))],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["timeline-read".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("timeline-read"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_PASS, "observed: {}", row.observed);
    }

    #[test]
    fn search_finds_a_marker_fixture() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push("repos/qual-org/loomp-test/issues", vec![Ok((201, r#"{"number": 11}"#.into()))]);
        http.push(
            "repos/issues/search?q=loomp-search-loomp-testrun&type=issues",
            vec![Ok((
                200,
                r#"{"ok": true, "data": [{"number": 11}]}"#.into(),
            ))],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["issue-search".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("issue-search"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_PASS, "observed: {}", row.observed);
    }

    #[test]
    fn catalogue_round_trip_records_the_numeric_id() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/labels",
            vec![
                Ok((201, r#"{"id": 60, "name": "loomp-loomp-testrun: catalogue"}"#.into())),
                Ok((200, "[]".into())),
            ],
        );
        http.push("repos/qual-org/loomp-test/labels/60", vec![Ok((200, r#"{"id": 60}"#.into()))]);
        http.push(
            "repos/qual-org/loomp-test/labels?page=1&limit=50",
            vec![
                Ok((
                    200,
                    r##"[{"id": 60, "name": "loomp-loomp-testrun: catalogue", "color": "#00ccdd", "description": "probe-edited"}]"##.into(),
                )),
                Ok((200, "[]".into())),
            ],
        );
        http.push("repos/qual-org/loomp-test/labels?page=2&limit=50", vec![Ok((200, "[]".into()))]);
        let mut cfg = cfg(true);
        cfg.only = vec!["label-sync-catalogue".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results
            .iter()
            .find(|r| r.test_id.contains("label-sync-catalogue"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_PASS, "observed: {}", row.observed);
        assert!(row.notes.iter().any(|n| n.contains("60")));
    }
}
