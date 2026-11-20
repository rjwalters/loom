//! The SigNoz read client: reads from the telemetry store, through the
//! telemetry-reader (#10196 R6, #11357).
//!
//! This is transport only. It knows nothing about what a query selects or
//! what a row means: callers bring their own SQL, their bound parameters and
//! their own row parser. It moved here from `eta/fleet_signoz_refresh.rs` so
//! it survives the ETA subsystem's removal (#11098); nothing in this module
//! may depend on the `eta` module.

use serde_json::Value;
use std::path::Path;

/// Why a query could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The backend did not answer (connection, timeout, bad endpoint).
    Unavailable(String),
    /// It answered with an error status.
    Refused(String),
}

/// The telemetry-reader (an authenticated query broker): `POST <base>/v1/query`
/// with the statement as the body and each bound value as a `param_<name>`
/// URL parameter.
///
/// It carries **no credential**: the reader identifies the caller by its
/// network identity and runs the statement as that caller's read-only store
/// account, so no Loom component holds a ClickHouse password (#11357,
/// #11178). The base comes from `--endpoint`, `TELEMETRY_READER_URL` or
/// `telemetry.signoz.endpoint` ([`EndpointConfig`]); no address is built in.
#[derive(Debug, Clone)]
pub struct TelemetryReader {
    /// The reader's base, a bare `http(s)://host[:port]`.
    pub endpoint: String,
    /// Per-request timeout.
    pub timeout: std::time::Duration,
}

/// The reader's one query route, joined onto [`TelemetryReader::endpoint`].
pub const QUERY_PATH: &str = "/v1/query";

impl TelemetryReader {
    /// The request URL: the base's [`QUERY_PATH`] plus every parameter in
    /// `params`, each bound as `param_<name>` (the only query keys the reader
    /// accepts).
    ///
    /// # Errors
    ///
    /// The endpoint is not a bare `http(s)://host[:port]` base (a path, a
    /// query, a fragment or userinfo in it is refused, and never echoed).
    pub fn url_with(&self, params: &[(String, String)]) -> Result<reqwest::Url, ReadError> {
        let bad = |why: &str| ReadError::Unavailable(format!("invalid reader endpoint: {why}"));
        let mut url = reqwest::Url::parse(self.endpoint.trim()).map_err(|e| bad(&e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(bad("not http(s)"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(bad("carries userinfo; the reader takes no credential"));
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(bad(&format!(
                "pass the bare base http(s)://host[:port]; {QUERY_PATH} is added"
            )));
        }
        url.set_path(QUERY_PATH);
        {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in params {
                pairs.append_pair(&format!("param_{name}"), value);
            }
        }
        if params.is_empty() {
            url.set_query(None);
        }
        Ok(url)
    }

    /// `POST` `sql` with `params` bound; the response body.
    ///
    /// # Errors
    ///
    /// The reader is unreachable or answered with an error status.
    pub fn post_with(&self, sql: &str, params: &[(String, String)]) -> Result<String, ReadError> {
        let url = self.url_with(params)?;
        let timeout = self.timeout;
        let sql = sql.to_string();
        // A private current-thread runtime on its own thread: callers are
        // synchronous (a CLI, or a daemon task's `spawn_blocking`), and this
        // must not depend on — or block — whichever runtime they are in.
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| ReadError::Unavailable(format!("runtime: {e}")))?;
                    runtime.block_on(async move {
                        // No redirects: a redirect could carry the statement
                        // somewhere other than the configured reader.
                        let client = reqwest::Client::builder()
                            .timeout(timeout)
                            .redirect(reqwest::redirect::Policy::none())
                            .build()
                            .map_err(|e| ReadError::Unavailable(format!("client: {e}")))?;
                        let response = client
                            .post(url)
                            .header(reqwest::header::CONTENT_TYPE, "text/plain; charset=utf-8")
                            .body(sql)
                            .send()
                            .await
                            .map_err(|e| {
                                ReadError::Unavailable(format!(
                                    "request failed: {}",
                                    e.without_url()
                                ))
                            })?;
                        let status = response.status();
                        let body = response.text().await.map_err(|e| {
                            ReadError::Unavailable(format!("body: {}", e.without_url()))
                        })?;
                        if !status.is_success() {
                            let head: String = body.chars().take(200).collect();
                            return Err(ReadError::Refused(format!("HTTP {status}: {head}")));
                        }
                        Ok(body)
                    })
                })
                .join()
                .unwrap_or_else(|_| Err(ReadError::Unavailable("reader thread panicked".into())))
        })
    }
}

/// Where the telemetry-reader is: `TELEMETRY_READER_URL`, else
/// `telemetry.signoz.endpoint` (a `--endpoint` flag overrides both).
///
/// There is no credential to configure. Every other key under
/// `telemetry.signoz` (the old ClickHouse user and password-file settings)
/// and the retired `autonomous.eta.fleetRefresh.signoz` block are not read;
/// `ignored` names any that are still set, so the caller can say so once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointConfig {
    /// The reader's base, `http(s)://host[:port]`.
    pub endpoint: Option<String>,
    /// Retired keys still present in the configuration (dotted paths).
    pub ignored: Vec<String>,
}

/// The configuration key.
pub const ENDPOINT_CONFIG_KEY: &str = "telemetry.signoz";
/// The environment variable naming the reader's base (the reader clients'
/// convention); it wins over the configuration.
pub const ENDPOINT_ENV: &str = "TELEMETRY_READER_URL";
/// The retired ETA-era block, no longer read.
const RETIRED_CONFIG_KEY: &str = "autonomous.eta.fleetRefresh.signoz";

impl EndpointConfig {
    /// Resolve from an effective config document and the value of
    /// [`ENDPOINT_ENV`] (`None` or blank: unset).
    #[must_use]
    pub fn resolve(config: &Value, env: Option<&str>) -> Self {
        let block = |path: &str| {
            path.split('.')
                .try_fold(config, |node, key| node.get(key))
                .cloned()
        };
        let neutral = block(ENDPOINT_CONFIG_KEY);
        let configured = neutral
            .as_ref()
            .and_then(|b| b.get("endpoint"))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let env = env
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let mut ignored = Vec::new();
        if let Some(Value::Object(block)) = &neutral {
            ignored.extend(
                block
                    .keys()
                    .filter(|key| *key != "endpoint")
                    .map(|key| format!("{ENDPOINT_CONFIG_KEY}.{key}")),
            );
        }
        if block(RETIRED_CONFIG_KEY).is_some() {
            ignored.push(RETIRED_CONFIG_KEY.to_string());
        }
        EndpointConfig {
            endpoint: env.or(configured),
            ignored,
        }
    }

    /// Resolve from `repo_root`'s effective Loom configuration and the
    /// process environment.
    #[must_use]
    pub fn read(repo_root: &Path) -> Self {
        Self::resolve(
            &crate::config_resolver::resolve_effective_config(repo_root),
            std::env::var(ENDPOINT_ENV).ok().as_deref(),
        )
    }
}

#[cfg(test)]
#[path = "signoz_read_tests.rs"]
mod tests;
