//! Fakes-only coverage of [`super`] (Issue #11528): every exit path, the
//! headers-file contract, and the provisioning order (identity before any
//! fetch, write or send). The live SSM/age/OTLP/reader paths need real
//! credentials and network reach, so they are behind the
//! [`super::CredentialSource`] / [`super::ProbeSender`] / [`super::ProbeReader`]
//! seams and exercised manually on the PR, never here.

use super::*;
use serial_test::serial;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Fake credential sandboxes are private independently of the runner's umask.
fn private_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

/// Pins the identity inputs for the test's duration: `LOOM_HOST_ID` to
/// `host`, and the config-tier / persisted-file inputs cleared so `env` is
/// the winning source (the engine resolves through
/// [`host_identity::resolve`]), restoring the ambient values on drop.
struct HostGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl HostGuard {
    fn at(host: &str) -> Self {
        let keys = [
            host_identity::HOST_ID_ENV,
            host_identity::HOST_ID_FILE_ENV,
            crate::config_resolver::PRIVATE_DEFAULTS_ENV,
        ];
        let saved = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        std::env::set_var(host_identity::HOST_ID_ENV, host);
        for k in [
            host_identity::HOST_ID_FILE_ENV,
            crate::config_resolver::PRIVATE_DEFAULTS_ENV,
        ] {
            std::env::remove_var(k);
        }
        Self { saved }
    }
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

/// Records whether it was called at all — the mismatch AC asserts it never
/// is.
#[derive(Debug, Default)]
struct Recorder {
    called: AtomicBool,
}

impl Recorder {
    fn was_called(&self) -> bool {
        self.called.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
struct FakeCredential {
    recorder: Recorder,
    token: String,
}

impl CredentialSource for FakeCredential {
    fn credential(&self) -> Result<Credential, JoinError> {
        self.recorder.called.store(true, Ordering::SeqCst);
        Ok(Credential::Bearer(self.token.clone()))
    }
}

#[derive(Debug)]
struct FailingCredential(Recorder);

impl CredentialSource for FailingCredential {
    fn credential(&self) -> Result<Credential, JoinError> {
        self.0.called.store(true, Ordering::SeqCst);
        Err(JoinError::Credential("store down".into()))
    }
}

#[derive(Debug)]
struct FakeSender {
    recorder: Recorder,
    result: Option<JoinError>,
    /// The serialized record body of the last sent envelope, for needle
    /// assertions.
    sent_body: std::sync::Mutex<Option<String>>,
}

impl FakeSender {
    fn ok() -> Self {
        Self {
            recorder: Recorder::default(),
            result: None,
            sent_body: std::sync::Mutex::new(None),
        }
    }
}

impl ProbeSender for FakeSender {
    async fn send_probe(&self, envelope: &TelemetryEnvelope) -> Result<ProbeAck, JoinError> {
        self.recorder.called.store(true, Ordering::SeqCst);
        *self.sent_body.lock().unwrap() =
            Some(serde_json::to_string(&envelope.record).unwrap_or_default());
        match &self.result {
            Some(error) => Err(error.clone()),
            None => Ok(ProbeAck::Accepted),
        }
    }
}

/// Rows served per call; `Err` entries simulate reader outages. The bound
/// `host`/`probe` parameters of the last call are recorded for needle
/// assertions.
#[derive(Debug)]
struct FakeReader {
    calls: AtomicUsize,
    script: Vec<Result<Vec<ProbeRow>, JoinError>>,
    last_params: std::sync::Mutex<Vec<(String, String)>>,
}

impl FakeReader {
    fn always(rows: Vec<ProbeRow>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            script: vec![Ok(rows)],
            last_params: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl ProbeReader for FakeReader {
    fn read_probe(&mut self, params: &[(String, String)]) -> Result<Vec<ProbeRow>, JoinError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_params.lock().unwrap() = params.to_vec();
        let index = self.calls.load(Ordering::SeqCst) - 1;
        match self.script.get(index) {
            Some(Ok(rows)) => Ok(rows.clone()),
            Some(Err(e)) => Err(e.clone()),
            None => Ok(vec![]),
        }
    }
}

fn plan_at(dir: &std::path::Path, host: &str) -> JoinPlan {
    let mut plan = JoinPlan::new(host, "https://otlp.test", dir.join("otlp-headers"));
    plan.poll_interval = Duration::from_millis(1);
    plan
}

fn deps<'a, S: ProbeSender>(
    credential: &'a dyn CredentialSource,
    sender: &'a S,
    reader: &'a mut dyn ProbeReader,
) -> JoinDeps<'a, S> {
    JoinDeps {
        credential,
        sender,
        reader,
    }
}

const TOKEN: &str = "super-secret-token-abc";

/// AC: a `--host` differing from the resolved id exits non-zero before any
/// credential fetch, file write or network send.
#[tokio::test]
#[serial]
async fn identity_mismatch_provisions_nothing() {
    let _guard = HostGuard::at("this-host");
    let dir = private_tempdir();
    let plan = plan_at(dir.path(), "other-host");
    let credential = FakeCredential {
        recorder: Recorder::default(),
        token: TOKEN.to_string(),
    };
    let sender = FakeSender::ok();
    let mut reader = FakeReader::always(vec![]);
    let error = join(&plan, deps(&credential, &sender, &mut reader))
        .await
        .unwrap_err();
    match error {
        JoinError::IdentityMismatch {
            requested,
            resolved,
            source,
        } => {
            assert_eq!(requested, "other-host");
            assert_eq!(resolved, "this-host");
            assert_eq!(source, "env");
        }
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }
    assert!(!credential.recorder.was_called(), "credential was fetched");
    assert!(!sender.recorder.was_called(), "probe was sent");
    assert_eq!(reader.calls.load(Ordering::SeqCst), 0, "reader was queried");
    assert!(!plan.headers_path.exists(), "a file was written");
}

/// AC: a wrong token (fake sender returns 401/403) exits non-zero, the
/// message names the status, and the token never appears in it.
#[tokio::test]
#[serial]
async fn wrong_token_names_the_status_and_never_the_token() {
    let _guard = HostGuard::at("this-host");
    let dir = private_tempdir();
    let plan = plan_at(dir.path(), "this-host");
    let credential = FakeCredential {
        recorder: Recorder::default(),
        token: TOKEN.to_string(),
    };
    let sender = FakeSender {
        recorder: Recorder::default(),
        result: Some(JoinError::ProbeRejected { status: 403 }),
        sent_body: std::sync::Mutex::new(None),
    };
    let mut reader = FakeReader::always(vec![]);
    let error = join(&plan, deps(&credential, &sender, &mut reader))
        .await
        .unwrap_err();
    assert_eq!(error, JoinError::ProbeRejected { status: 403 });
    let rendered = error.to_string();
    assert!(rendered.contains("403"), "status not named: {rendered}");
    assert!(!rendered.contains(TOKEN), "token leaked: {rendered}");
    // The headers file was still provisioned — the token was valid *shape*;
    // the receiver refused it. Provisioning order is identity → fetch →
    // write → send.
    assert!(plan.headers_path.exists());
}

/// A credential-store failure is its own reason and stops before any send.
#[tokio::test]
#[serial]
async fn credential_failure_stops_before_send() {
    let _guard = HostGuard::at("this-host");
    let dir = private_tempdir();
    let plan = plan_at(dir.path(), "this-host");
    let credential = FailingCredential(Recorder::default());
    let sender = FakeSender::ok();
    let mut reader = FakeReader::always(vec![]);
    let error = join(&plan, deps(&credential, &sender, &mut reader))
        .await
        .unwrap_err();
    assert_eq!(error, JoinError::Credential("store down".into()));
    assert!(!sender.recorder.was_called(), "probe was sent anyway");
    assert!(!plan.headers_path.exists(), "a file was written anyway");
}

/// AC: a probe that never arrives within the timeout exits non-zero with its
/// own reason.
#[tokio::test]
#[serial]
async fn probe_timeout_is_distinct() {
    let _guard = HostGuard::at("this-host");
    let dir = private_tempdir();
    let mut plan = plan_at(dir.path(), "this-host");
    plan.timeout = Duration::from_secs(0);
    let credential = FakeCredential {
        recorder: Recorder::default(),
        token: TOKEN.to_string(),
    };
    let sender = FakeSender::ok();
    let mut reader = FakeReader::always(vec![]);
    let error = join(&plan, deps(&credential, &sender, &mut reader))
        .await
        .unwrap_err();
    assert_eq!(error, JoinError::ProbeTimeout { seconds: 0 }, "got: {error}");
}

/// A reader outage that outlasts the deadline is reported as the reader, not
/// as a silent probe.
#[tokio::test]
#[serial]
async fn reader_outage_is_distinct_from_probe_timeout() {
    let _guard = HostGuard::at("this-host");
    let dir = private_tempdir();
    let mut plan = plan_at(dir.path(), "this-host");
    plan.timeout = Duration::from_secs(0);
    let credential = FakeCredential {
        recorder: Recorder::default(),
        token: TOKEN.to_string(),
    };
    let sender = FakeSender::ok();
    let mut reader = FakeReader::always(vec![]);
    reader.script = vec![Err(JoinError::ReaderUnavailable("reader 503".into()))];
    let error = join(&plan, deps(&credential, &sender, &mut reader))
        .await
        .unwrap_err();
    assert_eq!(error, JoinError::ReaderUnavailable("reader 503".into()), "got: {error}");
}

/// A reader outage that clears before the deadline still joins.
#[tokio::test]
#[serial]
async fn reader_outage_then_success() {
    let _guard = HostGuard::at("this-host");
    let dir = private_tempdir();
    let mut plan = plan_at(dir.path(), "this-host");
    plan.timeout = Duration::from_secs(5);
    let credential = FakeCredential {
        recorder: Recorder::default(),
        token: TOKEN.to_string(),
    };
    let sender = FakeSender::ok();
    let mut reader = FakeReader::always(vec![]);
    reader.script = vec![
        Err(JoinError::ReaderUnavailable("reader 503".into())),
        Ok(vec![ProbeRow {
            host_id: Some("this-host".into()),
            ingest_principal: Some("this-host".into()),
        }]),
    ];
    let outcome = join(&plan, deps(&credential, &sender, &mut reader))
        .await
        .unwrap();
    assert_eq!(outcome.host, "this-host");
    assert_eq!(reader.calls.load(Ordering::SeqCst), 2);
}

/// AC: an identity mismatch in the read-back row exits non-zero with its own
/// reason.
#[tokio::test]
#[serial]
async fn readback_identity_mismatch_is_distinct() {
    let _guard = HostGuard::at("this-host");
    let dir = private_tempdir();
    let plan = plan_at(dir.path(), "this-host");
    let credential = FakeCredential {
        recorder: Recorder::default(),
        token: TOKEN.to_string(),
    };
    let sender = FakeSender::ok();
    let mut reader = FakeReader::always(vec![ProbeRow {
        host_id: Some("this-host".into()),
        ingest_principal: Some("someone-else".into()),
    }]);
    let error = join(&plan, deps(&credential, &sender, &mut reader))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        JoinError::ReadbackMismatch {
            host_id: Some("this-host".into()),
            principal: Some("someone-else".into()),
        },
        "got: {error}"
    );
    assert!(!error.to_string().contains(TOKEN));
}

/// AC: the success path (fakes) exits `Ok` — and the read-back needle is the
/// sent probe body's exact serialization, so no other record can match it.
#[tokio::test]
#[serial]
async fn success_round_trip_and_exact_needle() {
    let _guard = HostGuard::at("this-host");
    let dir = private_tempdir();
    let plan = plan_at(dir.path(), "this-host");
    let credential = FakeCredential {
        recorder: Recorder::default(),
        token: TOKEN.to_string(),
    };
    let sender = FakeSender::ok();
    let mut reader = FakeReader::always(vec![ProbeRow {
        host_id: Some("this-host".into()),
        ingest_principal: Some("this-host".into()),
    }]);
    let outcome = join(&plan, deps(&credential, &sender, &mut reader))
        .await
        .unwrap();
    assert_eq!(outcome.host, "this-host");
    assert_eq!(outcome.endpoint, "https://otlp.test");
    assert_eq!(outcome.headers_path, plan.headers_path);
    assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
    // The bound parameters: the roster id, and a needle equal to the body
    // the sender actually serialized.
    let params = reader.last_params.lock().unwrap().clone();
    let host_param = params
        .iter()
        .find(|(k, _)| k == "host")
        .map(|(_, v)| v.clone());
    assert_eq!(host_param.as_deref(), Some("this-host"));
    let needle = params
        .iter()
        .find(|(k, _)| k == "probe")
        .map(|(_, v)| v.clone())
        .unwrap();
    let body = sender.sent_body.lock().unwrap().clone().unwrap();
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    let captured = value["captured_at"].as_str().unwrap();
    assert_eq!(needle, format!("%{captured}%"));
}

/// AC: the headers file is written with mode 0600 and is accepted by the
/// exporter's own parser; a pre-existing looser-mode file is replaced, not
/// trusted.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn headers_file_round_trips_at_0600_and_replaces_loose_modes() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = HostGuard::at("this-host");
    let dir = private_tempdir();
    let path = dir.path().join("otlp-headers");
    std::fs::write(&path, "X-Stale: yes\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    write_headers_file(&path, TOKEN).unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "mode after replace: {mode:04o}");
    let headers = crate::observability::request_headers::RequestHeaders::load(&path)
        .expect("the exporter's parser must accept the file join wrote");
    assert!(headers.sets_authorization());
    assert_eq!(headers.names().collect::<Vec<_>>(), vec!["authorization"]);
    // The stale content is gone; the fresh token is the only secret present.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("X-Stale"));
    assert!(text.contains(TOKEN));

    // Parent directories are created when missing.
    let nested = dir.path().join("a/b/otlp-headers");
    write_headers_file(&nested, TOKEN).unwrap();
    assert!(nested.exists());
}

/// The probe needle matches the body actually serialized for the store.
#[test]
fn probe_needle_is_the_body_captured_at() {
    let record = HostExportRecord {
        captured_at: Utc::now(),
        host: "h".into(),
        exporters: vec![],
        fact_id_missing_total: 0,
    };
    let envelope = TelemetryEnvelope::new("h", TelemetryRecord::HostExport(record));
    let needle = probe_needle(&envelope).unwrap();
    let body = serde_json::to_string(&envelope.record).unwrap();
    let stripped = needle.trim_matches('%');
    assert!(body.contains(stripped), "needle {needle} not in body {body}");
    assert!(stripped.contains('T'), "an RFC 3339 instant, got {stripped}");
}

/// The credential spec grammar and its default.
#[test]
fn credential_specs_parse() {
    assert_eq!(
        parse_credential_spec("ssm:///2am/cloudflare/otlp-host/robb-pro").unwrap(),
        CredentialSpec::Ssm {
            parameter: "/2am/cloudflare/otlp-host/robb-pro".into()
        }
    );
    assert_eq!(
        parse_credential_spec("age-file:/secrets/token.age").unwrap(),
        CredentialSpec::AgeFile {
            path: "/secrets/token.age".into()
        }
    );
    assert_eq!(
        parse_credential_spec("file:/secrets/token.txt").unwrap(),
        CredentialSpec::File {
            path: "/secrets/token.txt".into()
        }
    );
    for bad in ["", "ssh://host", "ssm://", "age-file:", "file:", "token"] {
        assert!(parse_credential_spec(bad).is_err(), "{bad:?} must not parse");
    }
    assert_eq!(
        default_credential_spec("robb-pro"),
        "ssm-access:///2am/cloudflare/otlp-host/robb-pro"
    );
}

/// The `rejected_status` helper reads the transport's own failure wording.
#[test]
fn rejected_status_reads_transport_wording() {
    use crate::observability::exporter::ExportError;
    use crate::observability::otlp::rejected_status;
    let refused = ExportError::Transport(
        "OTLP HTTP 401 (credential refused; batch held until the credential is fixed)".into(),
    );
    assert_eq!(rejected_status(&refused), Some(401));
    assert_eq!(rejected_status(&ExportError::Transport("OTLP HTTP 502".into())), Some(502));
    assert_eq!(rejected_status(&ExportError::Transport("OTLP request timed out".into())), None);
    assert_eq!(
        rejected_status(&ExportError::Rejected {
            status: 400,
            body_snippet: String::new()
        }),
        None
    );
}

/// An empty credential value is a failure, not a silent empty header.
#[test]
fn empty_credential_value_is_refused() {
    let output = super::credentials::CommandOutput {
        ok: true,
        stdout: String::new(),
    };
    assert!(super::credentials::finish_credential(output, "aws ssm get-parameter").is_err());
}

/// A sender that, like the live [`OtlpProbeSender`], reads the headers file
/// only when the probe is sent — and snapshots what it found there.
#[derive(Debug)]
struct HeadersSnapshotSender {
    path: std::path::PathBuf,
    seen: std::sync::Mutex<Option<std::io::Result<String>>>,
}

impl ProbeSender for HeadersSnapshotSender {
    async fn send_probe(&self, _envelope: &TelemetryEnvelope) -> Result<ProbeAck, JoinError> {
        *self.seen.lock().unwrap() = Some(std::fs::read_to_string(&self.path));
        Ok(ProbeAck::Accepted)
    }
}

/// #11628 review (blocking): at send time the headers file exists and holds
/// the token `join()` just fetched — on a clean host (no file before the run)
/// and on a re-run over a previous run's stale token.
#[tokio::test]
#[serial]
async fn headers_file_holds_the_fresh_token_at_send_time() {
    let _guard = HostGuard::at("this-host");
    for stale in [
        None,
        Some("Authorization: Bearer stale-token-from-last-run\n"),
    ] {
        let dir = private_tempdir();
        let plan = plan_at(dir.path(), "this-host");
        if let Some(stale) = stale {
            std::fs::write(&plan.headers_path, stale).unwrap();
        } else {
            assert!(!plan.headers_path.exists());
        }
        let credential = FakeCredential {
            recorder: Recorder::default(),
            token: TOKEN.to_string(),
        };
        let sender = HeadersSnapshotSender {
            path: plan.headers_path.clone(),
            seen: std::sync::Mutex::new(None),
        };
        let mut reader = FakeReader::always(vec![ProbeRow {
            host_id: Some("this-host".into()),
            ingest_principal: Some("this-host".into()),
        }]);
        join(&plan, deps(&credential, &sender, &mut reader))
            .await
            .unwrap();
        let seen = sender.seen.lock().unwrap().take().expect("probe was sent");
        let text = seen.expect("the headers file exists at send time");
        assert_eq!(text, format!("Authorization: Bearer {TOKEN}\n"), "stale={stale:?}");
    }
}

/// #11628 review: only 401/403 are "credential refused"; every other status
/// is reported as a server/transport failure, never as a bad token.
#[test]
fn only_401_and_403_are_credential_refusals() {
    for status in [401, 403] {
        let error = send_failure(Some(status), "ignored");
        assert_eq!(error, JoinError::ProbeRejected { status });
        assert!(error.to_string().contains("credential refused"));
    }
    for status in [400_u16, 404, 407, 413, 429, 500, 502, 503, 504] {
        let error = send_failure(Some(status), "ignored");
        let rendered = error.to_string();
        assert!(matches!(error, JoinError::ProbeTransport(_)), "{status}: {error:?}");
        assert!(rendered.contains(&format!("OTLP HTTP {status}")), "{rendered}");
        assert!(rendered.contains("not a credential refusal"), "{rendered}");
    }
    assert_eq!(
        send_failure(None, "OTLP request timed out"),
        JoinError::ProbeTransport("OTLP request timed out".into())
    );
}

/// A one-status loopback OTLP receiver recording each request's
/// `Authorization` header, for driving the *live* [`OtlpProbeSender`].
type AccessHeaders = (Option<String>, Option<String>);
type RecordedAccess = std::sync::Arc<std::sync::Mutex<Vec<AccessHeaders>>>;

struct AuthSink {
    addr: std::net::SocketAddr,
    auth: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
    access: RecordedAccess,
    shutdown: std::sync::Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl AuthSink {
    fn answering(status: u16) -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let auth = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let access = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let access_seen = access.clone();
        let shutdown = std::sync::Arc::new(AtomicBool::new(false));
        let (seen, stop) = (auth.clone(), shutdown.clone());
        let handle = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).ok();
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    let Ok(n) = stream.read(&mut chunk) else {
                        break None;
                    };
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(pos + 4);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let header = |name: &str| {
                    head.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.trim()
                            .eq_ignore_ascii_case(name)
                            .then(|| value.trim().to_string())
                    })
                };
                let length: usize = header("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while buf.len() < header_end + length {
                    match stream.read(&mut chunk) {
                        Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                        _ => break,
                    }
                }
                // Record before answering, so a caller that saw the response
                // also sees the request (#7256).
                seen.lock().unwrap().push(header("authorization"));
                access_seen
                    .lock()
                    .unwrap()
                    .push((header("cf-access-client-id"), header("cf-access-client-secret")));
                let body = "{}";
                let response = format!(
                    "HTTP/1.1 {status} response\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            addr,
            auth,
            access,
            shutdown,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn auth_headers(&self) -> Vec<Option<String>> {
        self.auth.lock().unwrap().clone()
    }
}

impl Drop for AuthSink {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// #11628 review (blocking), end to end through the production sender: the
/// sender is built *before* `join()` (as `run()` does), yet the request on
/// the wire carries the freshly fetched token — on a clean host with no
/// headers file, and on a re-run whose file still holds a stale token.
#[tokio::test]
#[serial]
async fn live_sender_sends_the_fresh_token_on_clean_host_and_rerun() {
    let _guard = HostGuard::at("this-host");
    for stale in [
        None,
        Some("Authorization: Bearer stale-token-from-last-run\n"),
    ] {
        let sink = AuthSink::answering(200);
        let dir = private_tempdir();
        let mut plan = plan_at(dir.path(), "this-host");
        plan.endpoint = sink.base_url();
        if let Some(stale) = stale {
            std::fs::write(&plan.headers_path, stale).unwrap();
        }
        // Constructed before the file exists / is refreshed — must not read it.
        let sender = OtlpProbeSender::new(plan.endpoint.clone(), plan.headers_path.clone());
        let credential = FakeCredential {
            recorder: Recorder::default(),
            token: TOKEN.to_string(),
        };
        let mut reader = FakeReader::always(vec![ProbeRow {
            host_id: Some("this-host".into()),
            ingest_principal: Some("this-host".into()),
        }]);
        join(&plan, deps(&credential, &sender, &mut reader))
            .await
            .unwrap_or_else(|e| panic!("stale={stale:?}: {e}"));
        assert_eq!(sink.auth_headers(), vec![Some(format!("Bearer {TOKEN}"))], "stale={stale:?}");
    }
}

/// #11628 review: through the live sender, 401/403 are `ProbeRejected`;
/// 429 and 5xx are reported as server/transport failures.
#[tokio::test]
#[serial]
async fn live_sender_classifies_receiver_statuses() {
    let _guard = HostGuard::at("this-host");
    for (status, rejected) in [
        (401, true),
        (403, true),
        (429, false),
        (500, false),
        (503, false),
    ] {
        let sink = AuthSink::answering(status);
        let dir = private_tempdir();
        let mut plan = plan_at(dir.path(), "this-host");
        plan.endpoint = sink.base_url();
        let sender = OtlpProbeSender::new(plan.endpoint.clone(), plan.headers_path.clone());
        let credential = FakeCredential {
            recorder: Recorder::default(),
            token: TOKEN.to_string(),
        };
        let mut reader = FakeReader::always(vec![]);
        let error = join(&plan, deps(&credential, &sender, &mut reader))
            .await
            .unwrap_err();
        let rendered = error.to_string();
        if rejected {
            assert_eq!(error, JoinError::ProbeRejected { status });
        } else {
            assert!(matches!(error, JoinError::ProbeTransport(_)), "{status}: {error:?}");
            assert!(!rendered.contains("credential refused"), "{rendered}");
        }
        assert!(rendered.contains(&status.to_string()), "{rendered}");
        assert!(!rendered.contains(TOKEN), "token leaked: {rendered}");
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0, "no read-back after a failed send");
    }
}

#[path = "telemetry_join_contract_tests.rs"]
mod contract;
