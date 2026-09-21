//! Explicit one-shot fixture replay for testing installed OTLP artifacts.
#[cfg(feature = "otlp")]
use anyhow::Context;
use anyhow::Result;
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct TelemetryExportArgs {
    /// JSONL TelemetryEnvelope fixture (maximum 1 MiB, 1000 envelopes).
    #[arg(long)]
    input: PathBuf,
    /// Collector OTLP/HTTP base URL. No signal suffix.
    #[arg(long)]
    endpoint: String,
    /// File containing the Collector ingress Bearer credential.
    #[arg(long)]
    key_file: PathBuf,
}
impl TelemetryExportArgs {
    #[cfg(not(feature = "otlp"))]
    pub(crate) fn run(self) -> Result<()> {
        anyhow::bail!("this binary was built without the otlp feature")
    }

    #[cfg(feature = "otlp")]
    pub(crate) fn run(self) -> Result<()> {
        use loom_daemon::observability::{endpoint_policy, exporter::Exporter, otlp::OtlpExporter};
        use std::io::Read;
        anyhow::ensure!(
            endpoint_policy::reserved_placeholder_host(&self.endpoint).is_none(),
            "placeholder telemetry endpoint refused"
        );
        let url = reqwest::Url::parse(&self.endpoint).context("invalid collector URL")?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "collector URL must be HTTP(S), without credentials, query or fragment"
        );
        let mut input = Vec::new();
        std::fs::File::open(self.input)?
            .take(1_048_577)
            .read_to_end(&mut input)?;
        anyhow::ensure!(input.len() <= 1_048_576, "fixture exceeds 1 MiB");
        let text = std::str::from_utf8(&input)?;
        let envelopes: Vec<loom_daemon::telemetry::TelemetryEnvelope> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        anyhow::ensure!(
            !envelopes.is_empty() && envelopes.len() <= 1000,
            "fixture must contain 1..=1000 envelopes"
        );
        let key =
            std::fs::read_to_string(self.key_file).context("cannot read collector key file")?;
        anyhow::ensure!(!key.trim().is_empty(), "collector key file is empty");
        let exporter = OtlpExporter::new(self.endpoint, key.trim().to_string())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let outcome = runtime.block_on(exporter.emit_batch_outcome(&envelopes));
        println!(
            "{}",
            serde_json::json!({"acknowledged_envelopes":outcome.acknowledged,"exported_envelopes":outcome.exported,"signals":outcome.signals})
        );
        if let Some(error) = outcome.error {
            return Err(error.into());
        }
        anyhow::ensure!(
            outcome.acknowledged == envelopes.len(),
            "fixture was not fully acknowledged"
        );
        Ok(())
    }
}
