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
pub(crate) trait ProbeHttp {
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
pub(crate) struct LiveHttp {
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
            cmd.arg("--data-binary").arg(body);
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
    let mut server_version = String::new();

    // The server version stamps every receipt row; a failed version probe
    // marks the whole run Unknown rather than inventing a version.
    match http.request("GET", "version", &cfg.writer_token, None) {
        Ok((200, body)) => {
            server_version = serde_json::from_str::<serde_json::Value>(body.trim())
                .ok()
                .and_then(|v| v.get("version").and_then(|s| s.as_str()).map(String::from))
                .unwrap_or_default();
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
                why: format!("version probe transport fault: {e}"),
            };
            return Err(anyhow::anyhow!("{why}"));
        }
    }

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
        "forge-probe::coordination::issue-comment" => {
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
    let (code, resp) = http.request(
        "POST",
        &format!("repos/{}/issues", cfg.repo),
        &cfg.writer_token,
        Some(&body),
    )
    .map_err(|e| ForgeOutcome::Unknown { operation: view.id.clone(), why: e })?;
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
        notes: number.map(|n| vec![format!("disposable issue number: {n}")]).unwrap_or_default(),
    };
    match (code, number) {
        (200..=299, Some(n)) => Ok(base(
            OUTCOME_PASS,
            format!("issue #{n} created in {}", cfg.repo),
        )),
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
    let (code, resp) = http.request(
        "POST",
        &format!("repos/{}/issues/{number}/comments", cfg.repo),
        &cfg.writer_token,
        Some(&serde_json::json!({"body": marker}).to_string()),
    )
    .map_err(|e| ForgeOutcome::Unknown { operation: view.id.clone(), why: e })?;
    if !(200..=299).contains(&code) {
        return Err(ForgeOutcome::Unknown {
            operation: view.id.clone(),
            why: format!("comment POST answered {code}: {}", truncate(&resp)),
        });
    }
    let (rcode, rresp) = http.request(
        "GET",
        &format!("repos/{}/issues/{number}/comments", cfg.repo),
        &cfg.writer_token,
        None,
    )
    .map_err(|e| ForgeOutcome::Unknown { operation: view.id.clone(), why: e })?;
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

    /// Scripted fake: ordered (path-part → queue) routes. A request answers
    /// from the route whose path-part appears *latest* in the URL —
    /// `…/issues/7/comments` comes from `comments`, never `issues` — pops
    /// the next scripted answer, and repeats the last one once the queue
    /// runs dry. Deterministic, offline, no jq/curl subprocess — the shim
    /// lessons from #9880 apply.
    struct FakeHttp {
        routes: Vec<(&'static str, std::cell::RefCell<Queue>)>,
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
        fn push(&mut self, path_part: &'static str, responses: Vec<Result<(u16, String), String>>) {
            self.routes.push((
                path_part,
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
            let best = self
                .routes
                .iter()
                .filter_map(|(part, q)| path.rfind(part).map(|pos| (pos, part.len(), q)))
                .max_by_key(|(pos, len, _)| (*pos, *len))
                .map(|(_, _, q)| q);
            let Some(q) = best else {
                return Err(format!("fake: no scripted response for {path}"));
            };
            let mut q = q.borrow_mut();
            if !q.script.is_empty() {
                let answer = q.script.remove(0);
                q.last = Some(answer.clone());
                return Ok(answer);
            }
            // Repeat the last answer for calls beyond the script.
            q.last
                .clone()
                .ok_or_else(|| format!("fake: scripted queue exhausted for {path}"))
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
    fn unimplemented_rows_stay_unknown_and_the_verdict_is_nonzero() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        let results = run(&cfg(false), &http).unwrap();
        assert!(!results.is_empty());
        let (_, unknown, failed, unsupported) = verdict(&results);
        assert!(unknown > 0, "slice 1 cannot have executed the whole matrix");
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
        let write_unknown = results.iter().filter(|r| {
            r.outcome == OUTCOME_UNKNOWN
                && r.notes.iter().any(|n| n.contains("live-write not opted in"))
        });
        assert!(write_unknown.count() > 0, "write rows refuse without opt-in");
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
            "issues",
            vec![Ok((201, r#"{"number": 12, "title": "x"}"#.into()))],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["issue-create".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results.iter().find(|r| r.test_id.contains("issue-create")).unwrap();
        assert_eq!(row.outcome, OUTCOME_PASS);
        assert!(row.notes.iter().any(|n| n.contains("12")));
        assert!(row.observed.contains("#12"));
    }

    #[test]
    fn a_403_is_insufficient_permission_never_does_not_exist() {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push("issues", vec![Ok((403, r#"{"message":"denied"}"#.into()))]);
        let mut cfg = cfg(true);
        cfg.only = vec!["issue-create".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results.iter().find(|r| r.test_id.contains("issue-create")).unwrap();
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
        http.push(
            "issues",
            vec![
                Ok((201, r#"{"number": 7}"#.into())),
                Ok((201, r#"{"number": 7, "body": "probe-1"}"#.into())),
            ],
        );
        // comments GET: marker absent -> FAIL
        http.push(
            "comments",
            vec![Ok((200, r#"[{"body": "something else"}]"#.into()))],
        );
        let mut cfg = cfg(true);
        cfg.only = vec!["issue-comment".into()];
        let results = run(&cfg, &http).unwrap();
        let row = results.iter().find(|r| r.test_id.contains("issue-comment")).unwrap();
        assert_eq!(row.outcome, OUTCOME_FAIL, "absent marker is a fail");
    }

    #[test]
    fn disposable_titles_carry_the_run_namespace() {
        let cfg = cfg(true);
        assert!(issue_title(&cfg, "issue-create").starts_with("loomp-loomp-testrun:"));
        assert!(issue_title(&cfg, "issue-create").contains("issue-create"));
    }
}
