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

/// Resource attribute key the ingest edge stamps with the authenticated principal
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
     resources_string['harness.ingest.principal'] AS ingest_principal \
     FROM signoz_logs.distributed_logs_v2 \
     WHERE attributes_string['loom.kind'] = 'host.export' \
       AND resources_string['host.id'] = {host:String} \
       AND body LIKE {probe:String} \
     LIMIT 5";

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

#[path = "telemetry_join_credentials.rs"]
mod credentials;
pub use credentials::*;

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
/// only calls that after it has fetched the credential and written the file.
/// A clean host can join, and a re-run uses the newly validated file. The
/// ingest-key slot is `None`: authentication comes exclusively from the file
/// (Access headers by default, or explicit legacy Bearer authorization).
#[derive(Debug, Clone)]
pub struct OtlpProbeSender {
    endpoint: String,
    headers_file: PathBuf,
}

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

impl ProbeSender for OtlpProbeSender {
    async fn send_probe(&self, envelope: &TelemetryEnvelope) -> Result<ProbeAck, JoinError> {
        use crate::observability::exporter::Exporter as _;
        use crate::observability::otlp::{rejected_status, OtlpExporter};
        let headers_file = self.headers_file.display().to_string();
        let exporter =
            OtlpExporter::with_headers_file(self.endpoint.clone(), None, Some(&headers_file))
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

    ensure_external_path(&plan.headers_path)?;

    // (b) Credential material, from the host's declared store.
    let credential = deps.credential.credential()?;

    // (c) The owner-only headers file, verified through the exporter's own
    // parser before anything is sent.
    write_credential_headers(&plan.headers_path, &credential.headers()?)?;

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

/// Credentials may not be provisioned inside any repository/worktree, including
/// through a symlinked parent. Check before creating missing directories.
fn ensure_external_path(path: &Path) -> Result<(), JoinError> {
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(JoinError::HeadersWrite(
            "credential paths may not contain parent-directory traversal".into(),
        ));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| JoinError::HeadersWrite("cannot resolve credential path".into()))?
            .join(path)
    };
    let existing = absolute
        .ancestors()
        .find(|p| p.exists())
        .ok_or_else(|| JoinError::HeadersWrite("cannot resolve credential parent".into()))?;
    let resolved = existing
        .canonicalize()
        .map_err(|_| JoinError::HeadersWrite("cannot resolve credential parent".into()))?;
    if resolved.ancestors().any(|p| p.join(".git").exists()) {
        return Err(JoinError::HeadersWrite(
            "credential path must be outside every repository/worktree".into(),
        ));
    }
    Ok(())
}

/// Write the headers file atomically with mode 0600 and prove the exporter's
/// parser accepts it. A pre-existing looser-mode file is replaced, never
/// trusted or mutated in place: the new inode carries 0600 from creation.
pub fn write_headers_file(path: &Path, token: &str) -> Result<(), JoinError> {
    write_credential_headers(path, &Credential::Bearer(token.to_owned()).headers()?)
}

fn write_credential_headers(path: &Path, text: &str) -> Result<(), JoinError> {
    ensure_external_path(path)?;
    let refuse = |detail: String| JoinError::HeadersWrite(detail);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(parent)
        .map_err(|e| refuse(format!("cannot create credential parent: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::metadata(parent)
            .map_err(|_| refuse("cannot stat credential parent".into()))?;
        // SAFETY: geteuid only reads the caller's effective user id.
        if meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o077 != 0 {
            return Err(refuse(
                "credential parent must belong to the invoking user and be private (0700)".into(),
            ));
        }
    }

    let mut temp = tempfile::Builder::new()
        .prefix(".telemetry-join")
        .rand_bytes(8)
        .tempfile_in(parent)
        .map_err(|e| refuse(format!("cannot create a temporary file: {e}")))?;
    use std::io::Write as _;
    temp.write_all(text.as_bytes())
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|e| refuse(format!("cannot write the headers file: {e}")))?;
    // NamedTempFile is already owner-only (0600 on unix); persist() renames
    // atomically, replacing whatever was at the path.
    // Validate before replacement: an invalid credential must leave the old
    // file intact. The parser's errors never contain header values.
    let parsed = crate::observability::request_headers::RequestHeaders::load(temp.path())
        .map_err(|e| refuse(e.to_string()))?;
    let names: Vec<_> = parsed.names().collect();
    if names.contains(&"cf-access-client-id") != names.contains(&"cf-access-client-secret") {
        return Err(refuse("Access credentials require both client id and client secret".into()));
    }
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid only reads the caller's effective user id.
            if meta.uid() != unsafe { libc::geteuid() } {
                return Err(refuse("destination must belong to the invoking user".into()));
            }
        }
        if !meta.is_file() {
            return Err(refuse(
                "destination must be a regular file, not a symlink or directory".into(),
            ));
        }
    }
    temp.persist(path)
        .map_err(|e| refuse(format!("cannot install the headers file: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "telemetry_join_tests.rs"]
mod tests;
