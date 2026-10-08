#![allow(clippy::unwrap_used, clippy::expect_used)]

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
    log: std::cell::RefCell<Vec<(String, String)>>,
    bodies: std::cell::RefCell<Vec<(String, Option<String>)>>,
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
            log: std::cell::RefCell::new(Vec::new()),
            bodies: std::cell::RefCell::new(Vec::new()),
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
        method: &str,
        path: &str,
        _token: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        self.bodies
            .borrow_mut()
            .push((format!("{method} {path}"), body.map(str::to_string)));
        self.calls.set(self.calls.get() + 1);
        self.log
            .borrow_mut()
            .push((method.to_string(), path.to_string()));
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
fn a_malformed_version_response_stops_the_run_before_any_case() {
    for body in ["not json", "{}", r#"{"version": 28}"#, r#"{"version": ""}"#] {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, body.into()))]);
        assert!(run(&cfg(true), &http).is_err(), "{body:?} must not qualify");
        assert_eq!(http.calls.get(), 1, "{body:?}: only the version probe may run");
    }
}

#[test]
fn issue_create_passes_on_2xx_with_a_number_and_records_the_disposable_number() {
    let mut http = FakeHttp::new();
    http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
    http.push(
        "issues",
        vec![
            Ok((201, r#"{"number": 12}"#.into())),
            Ok((200, issue_json(12, "loomp-testrun: issue-create", DISPOSABLE_BODY))),
        ],
    );
    http.push("user", vec![Ok((200, r#"{"login":"probe-writer"}"#.into()))]);
    let mut cfg = cfg(true);
    cfg.only = vec!["issue-create".into()];
    let results = run(&cfg, &http).unwrap();
    let row = results
        .iter()
        .find(|r| r.test_id.contains("issue-create"))
        .unwrap();
    assert_eq!(row.outcome, OUTCOME_PASS);
    assert_eq!(row.actor.as_deref(), Some("probe-writer"));
    assert!(row.notes.iter().any(|n| n.contains("12")));
    assert!(row.observed.contains("#12"));
}

fn issue_json(number: u64, title: &str, body: &str) -> String {
    serde_json::json!({"number": number, "title": title, "body": body}).to_string()
}

fn issue_create_row(readback: Result<(u16, String), String>) -> CaseResult {
    let mut http = FakeHttp::new();
    http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
    http.push("issues", vec![Ok((201, r#"{"number": 12}"#.into())), readback]);
    let mut cfg = cfg(true);
    cfg.only = vec!["issue-create".into()];
    run(&cfg, &http)
        .unwrap()
        .into_iter()
        .find(|r| r.test_id.contains("issue-create"))
        .unwrap()
}

#[test]
fn issue_create_fails_when_the_readback_is_missing_or_mismatched() {
    let ns_title = "loomp-testrun: issue-create";
    for readback in [
        Ok((404, r#"{"message":"not found"}"#.into())),
        Ok((200, "not json".into())),
        Ok((200, issue_json(13, ns_title, DISPOSABLE_BODY))),
        Ok((200, issue_json(12, "other title", DISPOSABLE_BODY))),
        Ok((200, issue_json(12, ns_title, "other body"))),
    ] {
        assert_eq!(issue_create_row(readback).outcome, OUTCOME_FAIL);
    }
}

#[test]
fn comment_create_never_writes_when_the_issue_create_readback_mismatches() {
    let mut http = FakeHttp::new();
    http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
    http.push(
        "issues",
        vec![
            Ok((201, r#"{"number": 12}"#.into())),
            Ok((200, issue_json(12, "someone else's issue", "not ours"))),
        ],
    );
    http.push("user", vec![Ok((200, r#"{"login":"probe-writer"}"#.into()))]);
    let mut cfg = cfg(true);
    cfg.only = vec!["comment-create".into()];
    let results = run(&cfg, &http).unwrap();
    let row = results
        .iter()
        .find(|r| r.test_id.contains("comment-create"))
        .unwrap();
    assert_eq!(row.outcome, OUTCOME_FAIL);
    assert!(
        !http
            .log
            .borrow()
            .iter()
            .any(|(m, p)| m == "POST" && p.ends_with("/comments")),
        "no comment POST may follow an unproven disposable issue"
    );
}

#[test]
fn a_passing_case_without_actor_evidence_fails_closed() {
    for user in [
        Err("connection reset".into()),
        Ok((401, r#"{"message":"bad token"}"#.into())),
        Ok((200, "not json".into())),
        Ok((200, "{}".into())),
    ] {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push(
            "issues",
            vec![
                Ok((201, r#"{"number": 12}"#.into())),
                Ok((200, issue_json(12, "loomp-testrun: issue-create", DISPOSABLE_BODY))),
            ],
        );
        http.push("user", vec![user]);
        let mut cfg = cfg(true);
        cfg.only = vec!["issue-create".into()];
        let row = run(&cfg, &http)
            .unwrap()
            .into_iter()
            .find(|r| r.test_id.contains("issue-create"))
            .unwrap();
        assert_eq!(row.outcome, OUTCOME_FAIL);
        assert!(row.actor.is_none());
        assert!(row.observed.contains("actor evidence unavailable"));
    }
}

#[test]
fn with_actor_keeps_a_non_pass_outcome_and_a_resolved_actor() {
    let mut http = FakeHttp::new();
    http.push("user", vec![Ok((200, r#"{"login":"w"}"#.into()))]);
    let (o, _, a) = with_actor(&http, "t", &[], OUTCOME_PASS, "ok".into());
    assert_eq!((o.as_str(), a.as_deref()), (OUTCOME_PASS, Some("w")));
    let down = FakeHttp::new();
    let (o, obs, a) = with_actor(&down, "t", &[], OUTCOME_FAIL, "bad".into());
    assert_eq!((o.as_str(), obs.as_str(), a), (OUTCOME_FAIL, "bad", None));
}

#[test]
fn issue_create_readback_transport_fault_is_unknown() {
    assert_eq!(issue_create_row(Err("connection reset".into())).outcome, OUTCOME_UNKNOWN);
}

#[test]
fn a_403_is_insufficient_permission_never_does_not_exist() {
    let mut http = FakeHttp::new();
    http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
    http.push("issues", vec![Ok((403, r#"{"message":"denied"}"#.into()))]);
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
    http.push(
        "issues",
        vec![
            Ok((201, r#"{"number": 7}"#.into())),
            Ok((200, issue_json(7, "loomp-testrun: issue-create", DISPOSABLE_BODY))),
        ],
    );
    // comments GET: marker absent -> FAIL
    http.push("comments", vec![Ok((200, r#"[{"body": "something else"}]"#.into()))]);
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
    assert!(issue_title(&cfg, "issue-create").starts_with("loomp-testrun:"));
    assert!(issue_title(&cfg, "issue-create").contains("issue-create"));
}

#[test]
fn issue_title_uses_run_ns_as_the_full_namespace_without_a_second_prefix() {
    let mut c = cfg(true);
    assert_eq!(issue_title(&c, "issue-create"), "loomp-testrun: issue-create");
    c.run_ns = "qual-custom".into();
    assert_eq!(issue_title(&c, "issue-create"), "qual-custom: issue-create");
}

#[test]
fn issue_create_post_payload_carries_the_exact_title() {
    let mut http = FakeHttp::new();
    http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
    http.push(
        "issues",
        vec![
            Ok((201, r#"{"number": 12}"#.into())),
            Ok((200, issue_json(12, "qual-custom: issue-create", DISPOSABLE_BODY))),
        ],
    );
    let mut c = cfg(true);
    c.run_ns = "qual-custom".into();
    c.only = vec!["issue-create".into()];
    run(&c, &http).unwrap();
    let bodies = http.bodies.borrow();
    let post = bodies
        .iter()
        .find(|(k, b)| k.starts_with("POST") && k.ends_with("/issues") && b.is_some())
        .expect("issue POST recorded");
    let payload: serde_json::Value = serde_json::from_str(post.1.as_deref().unwrap()).unwrap();
    assert_eq!(payload["title"], "qual-custom: issue-create");
}

#[test]
fn live_http_sends_json_bodies_with_a_json_content_type() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = conn.read(&mut chunk).unwrap();
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let want = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if body.len() >= want || n == 0 {
                    break;
                }
            } else if n == 0 {
                break;
            }
        }
        conn.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .unwrap();
        String::from_utf8_lossy(&buf).to_string()
    });
    let http = LiveHttp {
        origin: format!("http://127.0.0.1:{port}"),
        timeout: Duration::from_secs(5),
    };
    let payload = r#"{"title":"loomp-review: issue-create","body":"disposable"}"#;
    let (code, _) = http
        .request("POST", "repos/o/r/issues", "faketoken", Some(payload))
        .unwrap();
    assert_eq!(code, 201);
    let raw = server.join().unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let head = head.to_ascii_lowercase();
    assert!(head.contains("content-type: application/json"), "headers: {head}");
    let decoded: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(decoded["title"], "loomp-review: issue-create");
}

const FAKE_WRITER: &str = "FAKE-WRITER-TOKEN-do-not-use";
const FAKE_RO: &str = "FAKE-READONLY-TOKEN-do-not-use";

fn leaky_cfg() -> RunnerConfig {
    let mut c = cfg(true);
    c.writer_token = FAKE_WRITER.into();
    c.readonly_token = Some(FAKE_RO.into());
    c
}

fn assert_clean(text: &str) {
    assert!(!text.contains(FAKE_WRITER), "writer token leaked: {text}");
    assert!(!text.contains(FAKE_RO), "readonly token leaked: {text}");
}

#[test]
fn version_failures_do_not_echo_credentials() {
    let echoed = format!("bad token {FAKE_WRITER} / {FAKE_RO}");
    for answer in [
        Ok((200, echoed.clone())),
        Ok((401, echoed.clone())),
        Err(format!("curl exit 7: {echoed} https://u:{FAKE_WRITER}@h/x")),
    ] {
        let mut http = FakeHttp::new();
        http.push("version", vec![answer]);
        let err = run(&leaky_cfg(), &http).unwrap_err();
        assert_clean(&format!("{err} {err:?}"));
    }
}

#[test]
fn create_and_readback_failures_do_not_echo_credentials() {
    let echoed = format!("{{\"message\":\"{FAKE_WRITER} {FAKE_RO}\"}}");
    let mut http = FakeHttp::new();
    http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
    http.push("issues", vec![Ok((500, echoed.clone()))]);
    http.push("user", vec![Ok((200, r#"{"login":"bot"}"#.into()))]);
    let results = run(&leaky_cfg(), &http).unwrap();
    let receipt = serde_json::to_string(&results).unwrap();
    assert_clean(&receipt);
    assert!(receipt.contains("HTTP 500"), "status must survive: {receipt}");

    // Create succeeds, read-back returns an echoing mismatch.
    let mut http = FakeHttp::new();
    http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
    http.push(
        "issues",
        vec![
            Ok((201, r#"{"number":7}"#.into())),
            Ok((200, echoed.clone())),
        ],
    );
    http.push("user", vec![Ok((200, r#"{"login":"bot"}"#.into()))]);
    let results = run(&leaky_cfg(), &http).unwrap();
    let receipt = serde_json::to_string(&results).unwrap();
    assert_clean(&receipt);

    // Transport fault carrying the token in its message.
    let mut http = FakeHttp::new();
    http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
    http.push("issues", vec![Err(format!("curl exit 35: {FAKE_WRITER}"))]);
    let results = run(&leaky_cfg(), &http).unwrap();
    assert_clean(&serde_json::to_string(&results).unwrap());
}

#[test]
fn successful_version_and_login_do_not_echo_credentials() {
    // A 200 version carrying a token is unusable evidence: the run stops.
    for v in [FAKE_WRITER, FAKE_RO] {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, format!(r#"{{"version":"{v}"}}"#)))]);
        let err = run(&leaky_cfg(), &http).unwrap_err();
        assert_clean(&format!("{err} {err:?}"));
        assert!(err.to_string().contains("without a usable version"), "{err}");
    }

    // issue-create with a valid read-back: only the login varies.
    let issue_create_row = |login: &str| {
        let mut http = FakeHttp::new();
        http.push("version", vec![Ok((200, r#"{"version":"28.0.0"}"#.into()))]);
        http.push(
            "issues",
            vec![
                Ok((201, r#"{"number":7}"#.into())),
                Ok((200, issue_json(7, "loomp-testrun: issue-create", DISPOSABLE_BODY))),
            ],
        );
        http.push("user", vec![Ok((200, serde_json::json!({ "login": login }).to_string()))]);
        let mut c = leaky_cfg();
        c.only = vec!["issue-create".into()];
        let results = run(&c, &http).unwrap();
        let receipt = serde_json::to_string(&results).unwrap();
        assert_clean(&receipt);
        assert!(!receipt.contains("u:pw"), "{receipt}");
        let row = results
            .into_iter()
            .find(|r| r.test_id.contains("issue-create"))
            .unwrap();
        (row, receipt)
    };

    // Control: a clean login on a valid read-back is a PASS with an actor.
    let (row, receipt) = issue_create_row("probe-writer");
    assert_eq!(row.outcome, OUTCOME_PASS, "{receipt}");
    assert_eq!(row.actor.as_deref(), Some("probe-writer"));

    // A credential-bearing, empty or whitespace-only login is unusable
    // evidence: the otherwise-passing case fails closed with no actor.
    for l in [FAKE_WRITER, FAKE_RO, "https://u:pw@h", "", "   "] {
        let (row, receipt) = issue_create_row(l);
        assert_eq!(row.outcome, OUTCOME_FAIL, "login {l:?}: {receipt}");
        assert!(row.actor.is_none(), "login {l:?}: {receipt}");
        assert!(row.observed.contains("actor evidence unavailable"), "login {l:?}: {receipt}");
        assert!(row.observed.contains("issue #7 created"), "{receipt}");
    }
}

#[test]
fn redact_secrets_blanks_tokens_and_url_userinfo() {
    let out = redact_secrets("a SECRET b https://user:pw@host/p c", &["SECRET", ""]);
    assert!(!out.contains("SECRET") && !out.contains("user:pw"), "{out}");
    assert!(out.contains("https://[redacted]@host/p"), "{out}");
}

#[test]
fn zero_timeout_is_rejected_before_any_request() {
    let http = LiveHttp {
        origin: "http://127.0.0.1:1".into(),
        timeout: Duration::ZERO,
    };
    let err = http.request("GET", "version", "tok", None).unwrap_err();
    assert!(err.contains("timeout must be positive"), "{err}");

    let mut c = cfg(true);
    c.timeout = Duration::ZERO;
    let fake = FakeHttp::new();
    let err = run(&c, &fake).unwrap_err();
    assert_eq!(fake.calls.get(), 0, "no request may precede validation");
    assert!(err.to_string().contains("timeout must be positive"), "{err:#}");
}

#[test]
fn malformed_repo_or_namespace_is_rejected_before_any_request() {
    let bad_repos = [
        "qualification/loomp-test/../../production/live",
        "../production",
        "owner/..",
        "owner/%2e%2e",
        "owner/repo/extra",
        "owner",
        "",
        "/repo",
        "owner/",
        "owner/re?po",
        "owner/re#po",
        "ow ner/repo",
        "owner\\repo",
    ];
    for repo in bad_repos {
        let mut c = cfg(true);
        c.repo = repo.into();
        let fake = FakeHttp::new();
        let err = run(&c, &fake).unwrap_err();
        assert_eq!(fake.calls.get(), 0, "repo {repo:?}: no transport call may precede validation");
        assert!(err.to_string().contains("repo must be exactly"), "{repo:?}: {err:#}");
    }
    for ns in ["", ".", "..", "a/b", "a b"] {
        let mut c = cfg(true);
        c.run_ns = ns.into();
        let fake = FakeHttp::new();
        let err = run(&c, &fake).unwrap_err();
        assert_eq!(fake.calls.get(), 0, "ns {ns:?}: no transport call may precede validation");
        assert!(err.to_string().contains("run namespace"), "{ns:?}: {err:#}");
    }
}

#[test]
fn a_normal_owner_repo_control_passes_validation() {
    let mut c = cfg(true);
    c.repo = "qual-org.1/loomp_test-2".into();
    assert!(validate_config(&c).is_ok());
    assert!(validate_config(&cfg(false)).is_ok());
}
