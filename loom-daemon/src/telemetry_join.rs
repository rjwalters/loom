//! `loom-daemon telemetry-join` — one command that brings a host into the
//! fleet's SigNoz telemetry and proves its records land under the right
//! identity (Issue #11528).
//!
//! The command provisions exactly the path #11525 (option a) and #11526
//! (option a) settled on: direct OTLP from every daemon, one per-host Access
//! token fetched from the host's declared credential store, written to the
//! exporter's owner-only `headers_file`, and a live round trip — send one
//! probe `host.export` record, then read it back through the telemetry-reader
//! and require `host.id == harness.ingest.principal == <host>`. Any step that
//! fails exits non-zero with its own reason: a wrong token (naming the HTTP
//! status), an identity mismatch (before any I/O), or a probe that never
//! arrives.
//!
//! # Order matters
//!
//! The identity check runs first — before the credential fetch, the headers
//! write, or any network I/O — so a `--host` that does not name *this* host
//! can never provision anything, not even a wrong file. Everything after it
//! is behind the [`CredentialSource`], [`ProbeSender`] and [`ProbeReader`]
//! seams, so tests drive every exit path with fakes and the live paths are
//! only exercised by a real run (recorded manually; CI never touches AWS, the
//! Access proxy or SigNoz).
//!
//! # Secrets
//!
//! The token is fetched into memory, written only into the 0600 headers file,
//! and never logged, echoed, passed on a command line, or carried in an error.
//! [`JoinError`]'s `Display` is safe to print: it names files, statuses and
//! reasons, never credential material. The reader carries no credential by
//! design (#11357).

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::host_identity::{self, HostIdSource, ResolvedHostId};
use crate::telemetry::kinds::host_export::HostExportRecord;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

/// The fleet's OTLP endpoint (#11525, option a).
pub const DEFAULT_ENDPOINT: &str = "https://otlp-cf.2amlogic.com";

/// Parent path of the per-host SSM parameter; the parameter for host `h` is
/// `ssm://{DEFAULT_SSM_PARAM}/{h}` (#11525).
pub const DEFAULT_SSM_PARAM: &str = "/2am/cloudflare/otlp-host";

/// Where the exporter's `headers_file` lives when `--headers-path` is not
/// given, relative to the home directory.
pub const DEFAULT_HEADERS_PATH: &str = ".config/loom/otlp-headers";

/// Default whole-run deadline: the issue's acceptance window for the probe
/// to become visible in SigNoz.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// How long to wait between read-back polls.
pub const PROBE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Attribute key the ingest edge stamps with the authenticated principal
/// (#11528 scope note: the store-side location is not defined in this
/// repository; override the whole query with `--probe-sql` when the schema
/// differs).
pub const INGEST_PRINCIPAL_ATTR: &str = "harness.ingest.principal";

/// The read-back query. Binds `{host:String}` (the roster id) and
/// `{probe:String}` (a `LIKE` needle on the probe record's body — its exact
/// `captured_at`), and selects exactly two aliased columns, `host_id` and
/// `ingest_principal`, so the row parser does not care how they are
/// expressed. Schema conventions follow
/// `defaults/observability/signoz/host-export-queries.sql`.
pub const PROBE_QUERY: &str = "SELECT resources_string['host.id'] AS host_id, \
     attributes_string['harness.ingest.principal'] AS ingest_principal \
     FROM signoz_logs.distributed_logs_v2 \
     WHERE attributes_string['loom.kind'] = 'host.export' \
       AND resources_string['host.id'] = {host:String} \
       AND body LIKE {probe:String} \
     LIMIT 5";

/// Bounded runtime for the `aws`/`age` credential subprocesses.
const CREDENTIAL_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Why the join failed. Every variant is a distinct, printable reason; none
/// ever carries credential material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinError {
    /// `--host` does not name this host. Raised before any fetch, write or
    /// send.
    IdentityMismatch {
        requested: String,
        resolved: String,
        source: &'static str,
    },
    /// This host has no resolvable identity at all (`unknown-host`).
    IdentityUnknown,
    /// The endpoint is not a valid OTLP base or is a reserved placeholder.
    BadEndpoint(String),
    /// The credential could not be fetched from its store.
    Credential(String),
    /// The headers file could not be written, or does not round-trip through
    /// the exporter's parser.
    HeadersWrite(String),
    /// This binary was built without the `otlp` Cargo feature.
    NoOtlpFeature,
    /// The receiver refused the probe's authorization — `401`/`403` only
    /// (see [`send_failure`]), naming the status.
    ProbeRejected { status: u16 },
    /// The probe could not be sent: a transport failure, or a non-auth HTTP
    /// status (429, 5xx, …) that says nothing about the credential.
    ProbeTransport(String),
    /// No row matching the probe appeared within the deadline.
    ProbeTimeout { seconds: u64 },
    /// The probe row arrived, but its identity is not this host's.
    ReadbackMismatch {
        host_id: Option<String>,
        principal: Option<String>,
    },
    /// The telemetry-reader could not be queried.
    ReaderUnavailable(String),
    /// The probe record could not be built (serialization failed).
    ProbeBuild(String),
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentityMismatch {
                requested,
                resolved,
                source,
            } => write!(
                f,
                "identity mismatch: --host {requested:?} but this host resolves to \
                 {resolved:?} (source: {source}); refusing to provision another host's \
                 credentials — set fleet.hostId or LOOM_HOST_ID first"
            ),
            Self::IdentityUnknown => write!(
                f,
                "this host has no resolvable identity (unknown-host); set fleet.hostId \
                 or LOOM_HOST_ID before joining"
            ),
            Self::BadEndpoint(detail) => write!(f, "invalid --endpoint: {detail}"),
            Self::Credential(detail) => write!(f, "credential fetch failed: {detail}"),
            Self::HeadersWrite(detail) => write!(f, "headers file not provisioned: {detail}"),
            Self::NoOtlpFeature => write!(f, "this binary was built without the otlp feature"),
            Self::ProbeRejected { status } => write!(
                f,
                "probe rejected: OTLP HTTP {status} (credential refused — check the \
                 token in the host's credential store)"
            ),
            Self::ProbeTransport(detail) => write!(f, "probe send failed: {detail}"),
            Self::ProbeTimeout { seconds } => write!(
                f,
                "probe did not arrive in SigNoz within {seconds}s (identity, ingest \
                 path or retention may be wrong; check the token's host binding)"
            ),
            Self::ReadbackMismatch { host_id, principal } => write!(
                f,
                "probe row identity mismatch: host.id = {host_id:?}, \
                 {INGEST_PRINCIPAL_ATTR} = {principal:?} (expected both to be this host)"
            ),
            Self::ReaderUnavailable(detail) => write!(f, "telemetry-reader unavailable: {detail}"),
            Self::ProbeBuild(detail) => write!(f, "probe record could not be built: {detail}"),
        }
    }
}

impl std::error::Error for JoinError {}

/// A credential is fetchable from its declared store.
pub trait CredentialSource: fmt::Debug {
    /// The bearer token for this host's OTLP endpoint. Never logged.
    ///
    /// # Errors
    ///
    /// [`JoinError::Credential`] when the store cannot be reached or the
    /// value is empty.
    fn token(&self) -> Result<String, JoinError>;
}

/// `ssm://<parameter>`: shell out to the AWS CLI (`aws ssm get-parameter
/// --with-decryption`). No SDK dependency exists in this workspace, and the
/// token never appears in a command line — only the parameter *name* does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsmSource {
    /// The SSM parameter's full name.
    pub parameter: String,
}

impl CredentialSource for SsmSource {
    fn token(&self) -> Result<String, JoinError> {
        let mut cmd = std::process::Command::new("aws");
        cmd.args([
            "ssm",
            "get-parameter",
            "--name",
            &self.parameter,
            "--with-decryption",
            "--query",
            "Parameter.Value",
            "--output",
            "text",
        ]);
        let output =
            run_credential_command(cmd, &format!("aws ssm get-parameter {}", self.parameter))?;
        finish_credential(output, "aws ssm get-parameter")
    }
}

/// One completed credential subprocess: stdout / first stderr line, and
/// whether it exited zero. A deadline hit is a failure naming the tool.
struct CommandOutput {
    ok: bool,
    stdout: String,
    stderr_head: String,
}

/// Run a credential fetch under [`CREDENTIAL_COMMAND_TIMEOUT`], mapping every
/// failure (spawn, collect, deadline) to [`JoinError::Credential`].
fn run_credential_command(
    cmd: std::process::Command,
    tool: &str,
) -> Result<CommandOutput, JoinError> {
    match crate::proc_exec::run_bounded(cmd, CREDENTIAL_COMMAND_TIMEOUT) {
        Ok(crate::proc_exec::Completion::Exited(output)) => Ok(CommandOutput {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr_head: String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
        }),
        Ok(crate::proc_exec::Completion::TimedOut { .. }) => {
            Err(JoinError::Credential(format!("{tool} timed out")))
        }
        Err(e) => Err(JoinError::Credential(format!("{tool}: {e}"))),
    }
}

/// Shared tail of a credential fetch: non-empty stdout on exit zero, a
/// reason that names the tool and its first stderr line — never the value.
fn finish_credential(output: CommandOutput, tool: &str) -> Result<String, JoinError> {
    if !output.ok {
        return Err(JoinError::Credential(format!("{tool} failed: {}", output.stderr_head)));
    }
    let token = output.stdout;
    if token.is_empty() {
        return Err(JoinError::Credential(format!("{tool} returned an empty value")));
    }
    Ok(token)
}

/// `age-file:<path>`: decrypt with `age -d` (optionally `-i <identity>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgeFileSource {
    /// The encrypted file's path.
    pub path: PathBuf,
    /// Identity file passed as `-i`, when the passphrase is held in one.
    pub identity: Option<PathBuf>,
}

impl CredentialSource for AgeFileSource {
    fn token(&self) -> Result<String, JoinError> {
        let mut cmd = std::process::Command::new("age");
        cmd.arg("--decrypt");
        if let Some(identity) = &self.identity {
            cmd.args(["-i", &identity.display().to_string()]);
        }
        cmd.arg(&self.path);
        let output = run_credential_command(cmd, &format!("age -d {}", self.path.display()))?;
        finish_credential(output, "age -d")
    }
}

/// `file:<path>`: an already-decrypted plaintext file (e.g. a
/// chezmoi-rendered secret), read directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSource {
    /// The file's path.
    pub path: PathBuf,
}

impl CredentialSource for FileSource {
    fn token(&self) -> Result<String, JoinError> {
        let token = std::fs::read_to_string(&self.path)
            .map_err(|e| JoinError::Credential(format!("read {}: {e}", self.path.display())))?
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(JoinError::Credential(format!("{} is empty", self.path.display())));
        }
        Ok(token)
    }
}

/// A parsed `--credential-source` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSpec {
    /// `ssm://<parameter>`.
    Ssm { parameter: String },
    /// `age-file:<path>`.
    AgeFile { path: PathBuf },
    /// `file:<path>`.
    File { path: PathBuf },
}

/// The default spec for `host`: `ssm://{DEFAULT_SSM_PARAM}/{host}`.
#[must_use]
pub fn default_credential_spec(host: &str) -> String {
    format!("ssm://{DEFAULT_SSM_PARAM}/{host}")
}

/// Parse a `--credential-source` value.
///
/// # Errors
///
/// [`JoinError::Credential`] naming the accepted forms when the spec does not
/// match one.
pub fn parse_credential_spec(spec: &str) -> Result<CredentialSpec, JoinError> {
    if let Some(parameter) = spec.strip_prefix("ssm://") {
        let parameter = parameter.trim();
        if parameter.is_empty() {
            return Err(JoinError::Credential(
                "ssm:// needs a parameter name (ssm:///path/to/parameter)".to_string(),
            ));
        }
        return Ok(CredentialSpec::Ssm {
            parameter: parameter.to_string(),
        });
    }
    if let Some(path) = spec.strip_prefix("age-file:") {
        let path = path.trim();
        if path.is_empty() {
            return Err(JoinError::Credential(
                "age-file: needs a path (age-file:/path/to/encrypted)".to_string(),
            ));
        }
        return Ok(CredentialSpec::AgeFile {
            path: PathBuf::from(path),
        });
    }
    if let Some(path) = spec.strip_prefix("file:") {
        let path = path.trim();
        if path.is_empty() {
            return Err(JoinError::Credential(
                "file: needs a path (file:/path/to/plaintext)".to_string(),
            ));
        }
        return Ok(CredentialSpec::File {
            path: PathBuf::from(path),
        });
    }
    Err(JoinError::Credential(format!(
        "unknown credential source {spec:?}; use ssm://<parameter>, age-file:<path> or \
         file:<path>"
    )))
}

/// Build the live source for a parsed spec.
///
/// # Errors
///
/// [`JoinError::NoOtlpFeature`] is never raised here; a bad spec is
/// [`JoinError::Credential`].
pub fn credential_source<'a>(
    spec: &'a CredentialSpec,
    age_identity: Option<&'a Path>,
) -> Box<dyn CredentialSource + 'a> {
    match spec {
        CredentialSpec::Ssm { parameter } => Box::new(SsmSource {
            parameter: parameter.clone(),
        }),
        CredentialSpec::AgeFile { path } => Box::new(AgeFileSource {
            path: path.clone(),
            identity: age_identity.map(Path::to_path_buf),
        }),
        CredentialSpec::File { path } => Box::new(FileSource { path: path.clone() }),
    }
}

/// What one probe send decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAck {
    /// The receiver accepted the batch.
    Accepted,
}

/// Map one failed probe send to its terminal [`JoinError`]. Only `401`/`403`
/// — the receiver refusing *this* authorization — are
/// [`JoinError::ProbeRejected`] ("credential refused"). Any other HTTP status
/// (429 throttling, 5xx, 4xx shape errors) is reported as what it is, a
/// server/transport failure, so an outage is never blamed on a good token.
/// `status` is the receiver's HTTP status when it answered at all; `detail`
/// is the transport's own (token-free) wording for the no-status case.
#[must_use]
pub fn send_failure(status: Option<u16>, detail: &str) -> JoinError {
    match status {
        Some(status @ (401 | 403)) => JoinError::ProbeRejected { status },
        Some(status) => JoinError::ProbeTransport(format!(
            "OTLP HTTP {status} from the receiver (a server/transport failure, not a \
             credential refusal — retry later or check the endpoint)"
        )),
        None => JoinError::ProbeTransport(detail.to_string()),
    }
}

/// The live probe transport: the same `OtlpExporter` the daemon's exporter
/// task uses. Construction does no I/O — the exporter (which loads the
/// headers file) is built inside [`ProbeSender::send_probe`], and [`join`]
/// only calls that after it has fetched the token and written the file. So a
/// clean host (no file yet) can join, and a re-run never sends with a stale
/// token left by a previous run (#11628 review). The ingest-key slot stays
/// empty on purpose: the file's `Authorization` replaces the Bearer header,
/// so no key exists anywhere in this command.
#[cfg(feature = "otlp")]
#[derive(Debug, Clone)]
pub struct OtlpProbeSender {
    endpoint: String,
    headers_file: PathBuf,
}

#[cfg(feature = "otlp")]
impl OtlpProbeSender {
    /// A sender for `endpoint` that will read `headers_file` at send time.
    #[must_use]
    pub fn new(endpoint: impl Into<String>, headers_file: impl Into<PathBuf>) -> Self {
        Self {
            endpoint: endpoint.into(),
            headers_file: headers_file.into(),
        }
    }
}

#[cfg(feature = "otlp")]
impl ProbeSender for OtlpProbeSender {
    async fn send_probe(&self, envelope: &TelemetryEnvelope) -> Result<ProbeAck, JoinError> {
        use crate::observability::exporter::Exporter as _;
        use crate::observability::otlp::{rejected_status, OtlpExporter};
        let headers_file = self.headers_file.display().to_string();
        let exporter = OtlpExporter::with_headers_file(
            self.endpoint.clone(),
            String::new(),
            Some(&headers_file),
        )
        .map_err(|e| JoinError::HeadersWrite(format!("cannot load at send time: {e}")))?;
        let outcome = exporter
            .emit_batch_outcome(std::slice::from_ref(envelope))
            .await;
        match outcome.error {
            Some(error) => Err(send_failure(rejected_status(&error), &error.to_string())),
            None => Ok(ProbeAck::Accepted),
        }
    }
}

/// Sends the probe record over the provisioned OTLP path. Not `Debug` as a
/// trait bound: a sender may hold credential material.
pub trait ProbeSender {
    /// Send one envelope. A credential refusal (`401`/`403`) is
    /// [`JoinError::ProbeRejected`] (naming the status); any other send
    /// failure is [`JoinError::ProbeTransport`] — see [`send_failure`].
    ///
    /// # Errors
    ///
    /// As above — the two terminal send outcomes.
    fn send_probe(
        &self,
        envelope: &TelemetryEnvelope,
    ) -> impl std::future::Future<Output = Result<ProbeAck, JoinError>> + Send;
}

/// One read-back row: the two aliased columns of [`PROBE_QUERY`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRow {
    /// The row's `host.id` resource attribute.
    pub host_id: Option<String>,
    /// The row's ingest-principal attribute.
    pub ingest_principal: Option<String>,
}

/// Reads the store for the probe row.
pub trait ProbeReader {
    /// Run the read-back query with `params` bound (`host`, `probe`), as
    /// `JSONEachRow`.
    ///
    /// # Errors
    ///
    /// [`JoinError::ReaderUnavailable`] when the reader cannot be queried.
    fn read_probe(&mut self, params: &[(String, String)]) -> Result<Vec<ProbeRow>, JoinError>;
}

/// The live reader: the telemetry-reader's `/v1/query` (#11357), no
/// credential.
#[derive(Debug, Clone)]
pub struct ReaderProbe {
    /// The resolved reader transport.
    pub reader: crate::signoz_read::TelemetryReader,
    /// The statement to run (the default, or `--probe-sql`).
    pub sql: String,
}

impl ProbeReader for ReaderProbe {
    fn read_probe(&mut self, params: &[(String, String)]) -> Result<Vec<ProbeRow>, JoinError> {
        let body = self
            .reader
            .post_with(&self.sql, params)
            .map_err(|e| JoinError::ReaderUnavailable(format!("{e:?}")))?;
        let mut rows = Vec::new();
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
                JoinError::ReaderUnavailable(format!("reader returned malformed row: {e}"))
            })?;
            let cell = |name: &str| {
                value
                    .get(name)
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            rows.push(ProbeRow {
                host_id: cell("host_id"),
                ingest_principal: cell("ingest_principal"),
            });
        }
        Ok(rows)
    }
}

/// The static shape of one join run: what does not change between attempts.
#[derive(Debug, Clone)]
pub struct JoinPlan {
    /// The roster id this host must already resolve to.
    pub host: String,
    /// The OTLP base endpoint.
    pub endpoint: String,
    /// Where the headers file is written.
    pub headers_path: PathBuf,
    /// How long the probe may take to appear.
    pub timeout: Duration,
    /// How long to wait between read-back polls.
    pub poll_interval: Duration,
    /// The read-back statement.
    pub probe_sql: String,
}

impl JoinPlan {
    /// A plan with the module defaults.
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        endpoint: impl Into<String>,
        headers_path: PathBuf,
    ) -> Self {
        Self {
            host: host.into(),
            endpoint: endpoint.into(),
            headers_path,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            poll_interval: PROBE_POLL_INTERVAL,
            probe_sql: PROBE_QUERY.to_string(),
        }
    }
}

/// The live collaborators, resolved once by the CLI. Generic over the
/// sender (its `send_probe` returns an anonymous future, so the trait is not
/// dyn compatible); the credential and reader seams are dyn.
pub struct JoinDeps<'a, S: ProbeSender> {
    /// The host's token source.
    pub credential: &'a dyn CredentialSource,
    /// The probe transport.
    pub sender: &'a S,
    /// The read-back transport.
    pub reader: &'a mut dyn ProbeReader,
}

/// What a successful join established, for the command's success line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinOutcome {
    /// The roster id the probe landed under.
    pub host: String,
    /// The endpoint the probe was sent to.
    pub endpoint: String,
    /// The headers file that was written.
    pub headers_path: PathBuf,
    /// Send-to-confirmed round-trip time.
    pub elapsed: Duration,
}

/// Run the join: identity, credential, headers file, probe, read-back.
///
/// # Errors
///
/// Every [`JoinError`] variant, in the documented order — identity before
/// any I/O, then credential, headers write, probe send, read-back.
pub async fn join<S: ProbeSender + Sync>(
    plan: &JoinPlan,
    deps: JoinDeps<'_, S>,
) -> Result<JoinOutcome, JoinError> {
    // (a) Identity first: nothing is fetched, written or sent for another
    // host's name, and a host without an identity joins nothing.
    let resolved: ResolvedHostId = host_identity::resolve();
    if resolved.source == HostIdSource::Unknown {
        return Err(JoinError::IdentityUnknown);
    }
    if resolved.id != plan.host {
        return Err(JoinError::IdentityMismatch {
            requested: plan.host.clone(),
            resolved: resolved.id,
            source: resolved.source.as_str(),
        });
    }

    // (b) The token, from the host's declared store.
    let token = deps.credential.token()?;

    // (c) The owner-only headers file, verified through the exporter's own
    // parser before anything is sent.
    write_headers_file(&plan.headers_path, &token)?;

    // (d) One probe `host.export` record, sent over the OTLP path the file
    // provisions. The sender reads the headers file only now, after (c)
    // wrote the fresh token — never at construction (#11628 review).
    let record = HostExportRecord {
        captured_at: Utc::now(),
        host: plan.host.clone(),
        exporters: vec![],
        fact_id_missing_total: 0,
    };
    let envelope = TelemetryEnvelope::new(plan.host.clone(), TelemetryRecord::HostExport(record));
    let probe_needle = probe_needle(&envelope)?;
    let started = Instant::now();
    deps.sender.send_probe(&envelope).await?;

    // (e) Poll the reader until the row is visible with both identities.
    let params = vec![
        ("host".to_string(), plan.host.clone()),
        ("probe".to_string(), probe_needle),
    ];
    let deadline = started + plan.timeout;
    let mut last_reader_error: Option<String> = None;
    loop {
        match deps.reader.read_probe(&params) {
            Ok(rows) => {
                if let Some(matching) = rows
                    .iter()
                    .find(|row| row.host_id.as_deref() == Some(plan.host.as_str()))
                {
                    if matching.ingest_principal.as_deref() == Some(plan.host.as_str()) {
                        return Ok(JoinOutcome {
                            host: plan.host.clone(),
                            endpoint: plan.endpoint.clone(),
                            headers_path: plan.headers_path.clone(),
                            elapsed: started.elapsed(),
                        });
                    }
                    return Err(JoinError::ReadbackMismatch {
                        host_id: matching.host_id.clone(),
                        principal: matching.ingest_principal.clone(),
                    });
                }
                if let Some(wrong) = rows.first() {
                    // A row matched the probe body but not this host's id.
                    return Err(JoinError::ReadbackMismatch {
                        host_id: wrong.host_id.clone(),
                        principal: wrong.ingest_principal.clone(),
                    });
                }
            }
            Err(JoinError::ReaderUnavailable(detail)) => {
                last_reader_error = Some(detail);
            }
            Err(other) => return Err(other),
        }
        if Instant::now() >= deadline {
            return Err(last_reader_error.map_or_else(
                || JoinError::ProbeTimeout {
                    seconds: plan.timeout.as_secs(),
                },
                JoinError::ReaderUnavailable,
            ));
        }
        tokio::time::sleep(plan.poll_interval).await;
    }
}

/// The read-back needle: the probe body's exact `captured_at` literal, as a
/// `LIKE` pattern. The store's `body` column is the same serialization of the
/// same record the mapping performs, so the literal matches exactly and no
/// other record can.
fn probe_needle(envelope: &TelemetryEnvelope) -> Result<String, JoinError> {
    let body = serde_json::to_string(&envelope.record)
        .map_err(|e| JoinError::ProbeBuild(e.to_string()))?;
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| JoinError::ProbeBuild(e.to_string()))?;
    let captured = value
        .get("captured_at")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| JoinError::ProbeBuild("captured_at missing from the probe body".into()))?;
    Ok(format!("%{captured}%"))
}

/// Write the headers file atomically with mode 0600 and prove the exporter's
/// parser accepts it. A pre-existing looser-mode file is replaced, never
/// trusted or mutated in place: the new inode carries 0600 from creation.
pub fn write_headers_file(path: &Path, token: &str) -> Result<(), JoinError> {
    let refuse = |detail: String| JoinError::HeadersWrite(detail);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| refuse(format!("cannot create {}: {e}", parent.display())))?;
    }
    let text = format!("Authorization: Bearer {token}\n");
    let mut temp = tempfile::Builder::new()
        .prefix(".telemetry-join")
        .rand_bytes(8)
        .tempfile_in(path.parent().unwrap_or(Path::new(".")))
        .map_err(|e| refuse(format!("cannot create a temporary file: {e}")))?;
    use std::io::Write as _;
    temp.write_all(text.as_bytes())
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|e| refuse(format!("cannot write the headers file: {e}")))?;
    // NamedTempFile is already owner-only (0600 on unix); persist() renames
    // atomically, replacing whatever was at the path.
    temp.persist(path)
        .map_err(|e| refuse(format!("cannot install the headers file: {e}")))?;
    let headers = crate::observability::request_headers::RequestHeaders::load(path)
        .map_err(|e| refuse(e.to_string()))?;
    if !headers.sets_authorization() {
        return Err(refuse("the written file does not carry an Authorization header".to_string()));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "telemetry_join_tests.rs"]
mod tests;
