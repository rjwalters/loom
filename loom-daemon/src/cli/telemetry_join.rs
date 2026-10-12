//! `loom-daemon telemetry-join` — the thin CLI over
//! [`loom_daemon::telemetry_join`] (Issue #11528). One command brings a host
//! into SigNoz and proves its records land under the right identity: fetch
//! the per-host token from its declared store, write the exporter's 0600
//! `headers_file`, send one probe `host.export` record, and read it back
//! through the telemetry-reader requiring
//! `host.id == harness.ingest.principal == <host>`.
//!
//! Success prints one JSON line (`host`, `endpoint`, `headers_path`,
//! `elapsed_seconds`); every failure exits non-zero with its own reason and
//! never the token. The live round trip needs AWS SSM, the Access proxy and
//! network reach to SigNoz, so CI covers the engine only through fakes
//! (`telemetry_join_tests.rs`); a real run is recorded manually on the PR.

#[cfg(feature = "otlp")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "otlp")]
use std::time::Duration;

#[cfg(not(feature = "otlp"))]
use anyhow::Result;
#[cfg(feature = "otlp")]
use anyhow::{bail, Result};

#[cfg(feature = "otlp")]
use loom_daemon::observability::endpoint_policy;
#[cfg(feature = "otlp")]
use loom_daemon::signoz_read::{EndpointConfig, TelemetryReader};
#[cfg(feature = "otlp")]
use loom_daemon::telemetry_join::{
    self, JoinDeps, JoinPlan, OtlpProbeSender, ReaderProbe, DEFAULT_HEADERS_PATH, PROBE_QUERY,
};

#[derive(clap::Args)]
pub(crate) struct JoinArgs {
    /// The roster host id this machine must already resolve to
    /// (`fleet.hostId` in the machine-tier config, or `$LOOM_HOST_ID`;
    /// `loom-daemon host-id` prints it). Checked before anything is fetched,
    /// written or sent.
    #[arg(long, value_name = "ROSTER-ID")]
    host: String,

    /// OTLP base endpoint the probe is sent to.
    #[arg(long, value_name = "URL", default_value = loom_daemon::telemetry_join::DEFAULT_ENDPOINT)]
    endpoint: String,

    /// Credential source: `ssm://<parameter>` (AWS CLI, decrypted),
    /// `age-file:<path>` (`age -d`), or `file:<path>` (already-decrypted
    /// plaintext). Default: `ssm:///2am/cloudflare/otlp-host/<host>`.
    #[arg(long, value_name = "SPEC")]
    credential_source: Option<String>,

    /// Identity file for `age-file:` sources, passed to `age` as `-i`.
    #[arg(long, value_name = "PATH")]
    age_identity: Option<PathBuf>,

    /// The telemetry-reader's base URL (`http(s)://host[:port]`). Defaults to
    /// `TELEMETRY_READER_URL`, then `telemetry.signoz.endpoint`.
    #[arg(long, value_name = "URL")]
    reader_url: Option<String>,

    /// Read-back SQL override; must select the aliased columns `host_id` and
    /// `ingest_principal` and bind `{host:String}` and `{probe:String}`.
    /// Default queries `harness.ingest.principal` on the probe row.
    #[arg(long, value_name = "SQL")]
    probe_sql: Option<String>,

    /// How long the probe may take to appear in SigNoz, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = loom_daemon::telemetry_join::DEFAULT_TIMEOUT_SECS)]
    timeout: u64,

    /// Where the 0600 headers file is written (default
    /// `~/.config/loom/otlp-headers`). Replaced atomically; any existing
    /// looser-mode file is overwritten, not trusted.
    #[arg(long, value_name = "PATH")]
    headers_path: Option<PathBuf>,
}

impl JoinArgs {
    /// Resolve the headers file's default against the home directory.
    #[cfg(feature = "otlp")]
    fn headers_path(&self) -> Result<PathBuf> {
        if let Some(path) = &self.headers_path {
            return Ok(path.clone());
        }
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("no home directory for the default headers path"))?;
        Ok(home.join(DEFAULT_HEADERS_PATH))
    }

    /// Resolve the reader transport from `--reader-url`, then
    /// `TELEMETRY_READER_URL`, then `telemetry.signoz.endpoint`.
    #[cfg(feature = "otlp")]
    fn reader(&self) -> Result<ReaderProbe> {
        let url = match &self.reader_url {
            Some(url) => Some(url.clone()),
            None => EndpointConfig::read(Path::new(".")).endpoint,
        };
        let endpoint = url.ok_or_else(|| {
            anyhow::anyhow!(
                "no telemetry-reader: pass --reader-url, set TELEMETRY_READER_URL, or set \
                 telemetry.signoz.endpoint"
            )
        })?;
        Ok(ReaderProbe {
            reader: TelemetryReader {
                endpoint,
                timeout: Duration::from_secs(10),
            },
            sql: self
                .probe_sql
                .clone()
                .unwrap_or_else(|| PROBE_QUERY.to_string()),
        })
    }

    /// The whole command lives behind the `otlp` feature, exactly like
    /// `telemetry-export`: a plain build fails with the family's standard
    /// message instead of reaching for a transport it does not have.
    #[cfg(not(feature = "otlp"))]
    pub(crate) async fn run(self) -> Result<()> {
        anyhow::bail!("this binary was built without the otlp feature")
    }

    #[cfg(feature = "otlp")]
    pub(crate) async fn run(self) -> Result<()> {
        // Endpoint policy first: never send headers to a placeholder.
        if let Some(host) = endpoint_policy::reserved_placeholder_host(&self.endpoint) {
            bail!("placeholder telemetry endpoint refused: {host}");
        }
        anyhow::ensure!(
            endpoint_policy::valid_otlp_endpoint(&self.endpoint),
            "OTLP endpoint must be HTTP(S), without credentials, query or fragment"
        );
        let spec = match &self.credential_source {
            Some(spec) => telemetry_join::parse_credential_spec(spec)?,
            None => telemetry_join::parse_credential_spec(
                &telemetry_join::default_credential_spec(&self.host),
            )?,
        };
        let credential = telemetry_join::credential_source(&spec, self.age_identity.as_deref());
        let headers_path = self.headers_path()?;
        // I/O-free: the exporter is built inside `send_probe`, after `join()`
        // has written the fresh headers file (a clean host has none yet).
        let sender = OtlpProbeSender::new(self.endpoint.clone(), headers_path.clone());
        let mut reader = self.reader()?;
        let plan = JoinPlan {
            host: self.host.clone(),
            endpoint: self.endpoint.clone(),
            headers_path,
            timeout: Duration::from_secs(self.timeout),
            poll_interval: telemetry_join::PROBE_POLL_INTERVAL,
            probe_sql: self
                .probe_sql
                .clone()
                .unwrap_or_else(|| PROBE_QUERY.to_string()),
        };
        let outcome = telemetry_join::join(
            &plan,
            JoinDeps {
                credential: credential.as_ref(),
                sender: &sender,
                reader: &mut reader,
            },
        )
        .await?;
        println!(
            "{}",
            serde_json::json!({
                "host": outcome.host,
                "endpoint": outcome.endpoint,
                "headers_path": outcome.headers_path.display().to_string(),
                "elapsed_seconds": outcome.elapsed.as_secs_f64(),
                "verified": "probe read back with host.id == harness.ingest.principal",
            })
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::whole_cli_parse::try_parse_cli;

    #[test]
    fn host_is_required_and_flags_parse() {
        assert!(try_parse_cli(&["loom-daemon", "telemetry-join"]).is_err());
        assert!(try_parse_cli(&[
            "loom-daemon",
            "telemetry-join",
            "--host",
            "robb-pro",
            "--credential-source",
            "age-file:/tmp/x.age",
            "--timeout",
            "5",
        ])
        .is_ok());
    }
}
