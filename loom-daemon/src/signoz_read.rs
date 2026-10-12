//! The SigNoz read client: keyset-paged `JSONEachRow` reads from the
//! telemetry store, through the telemetry-reader (#10196 R6, #11357).
//!
//! This is transport only. It knows nothing about what a query selects or
//! what a row means: callers bring their own SQL and their own row parser.
//! It moved here from `eta/fleet_signoz_refresh.rs` so it survives the ETA
//! subsystem's removal (#11098); nothing in this module may depend on the
//! `eta` module.
//!
//! # The paging contract
//!
//! Every query a [`SignozRead`] serves pages by the keyset
//! `(knowable_time_ns, record_id)` — the [`RowCursor`] — oldest first, and
//! returns `JSONEachRow`. The bound parameters are [`PageQuery::params`]:
//! each caller filter by its own name, plus `since_ns`, `until_ns`,
//! `after_ns`, `after_id` and `limit`.
//!
//! # Two transports
//!
//! - [`TelemetryReader`]: the telemetry-reader's `POST /v1/query`, the SQL as
//!   the body and bound `param_*` URL parameters, no credential
//!   ([`SqlPages`] pairs it with one query).
//! - [`FileRows`]: the rows of an export an operator ran with
//!   `clickhouse-client`, served under the same filters, order and keyset —
//!   and the recorded-fixture reader tests use.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::Path;

/// The paging position of one row: `(knowable_time_ns, record_id)` exactly
/// as the query orders by, so a continuation resumes after it even when the
/// row itself is rejected by its caller.
pub type RowCursor = (i64, String);

/// One page request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageQuery {
    /// Exact-match column filters, compared case-insensitively. Each is bound
    /// as a query parameter of the same name, and [`FileRows`] applies it to
    /// the row's column of the same name.
    pub filters: Vec<(String, String)>,
    /// Oldest knowable-at, inclusive.
    pub since: DateTime<Utc>,
    /// Newest knowable-at, inclusive.
    pub until: DateTime<Utc>,
    /// Resume strictly after this cursor; `None` on the first page.
    pub after: Option<RowCursor>,
    /// Rows per page.
    pub limit: u32,
}

impl PageQuery {
    /// The value of filter `name`, if the query has one.
    #[must_use]
    pub fn filter(&self, name: &str) -> Option<&str> {
        self.filters
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The bound query parameters: every filter, then the window, keyset and
    /// page size.
    #[must_use]
    pub fn params(&self) -> Vec<(String, String)> {
        let (after_ns, after_id) = self.after.clone().unwrap_or((0, String::new()));
        let ns = |at: DateTime<Utc>| at.timestamp_nanos_opt().unwrap_or(0).max(0).to_string();
        let mut params = self.filters.clone();
        params.extend([
            ("since_ns".to_string(), ns(self.since)),
            ("until_ns".to_string(), ns(self.until)),
            ("after_ns".to_string(), after_ns.max(0).to_string()),
            ("after_id".to_string(), after_id),
            ("limit".to_string(), self.limit.to_string()),
        ]);
        params
    }
}

/// Why a page could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The backend did not answer (connection, timeout, bad endpoint).
    Unavailable(String),
    /// It answered with an error status.
    Refused(String),
}

/// The fetch seam: one page of a query's `JSONEachRow` output.
pub trait SignozRead {
    /// Read one page.
    ///
    /// # Errors
    ///
    /// The page could not be read.
    fn page(&mut self, query: &PageQuery) -> Result<String, ReadError>;
}

/// Rows from a `JSONEachRow` export, served page by page under the query's
/// filters, the `[since, until]` window on `knowable_time_ns`, and the
/// `(knowable_time_ns, record_id)` keyset.
#[derive(Debug, Clone)]
pub struct FileRows {
    rows: Vec<(RowCursor, Value, String)>,
}

impl FileRows {
    /// Parse `text`. Rows no query would return in any order (no
    /// `knowable_time_ns` / `record_id`) are a malformed export.
    ///
    /// # Errors
    ///
    /// A line is not a JSON object or lacks the paging columns.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut rows = Vec::new();
        for (n, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value =
                serde_json::from_str(line).map_err(|e| format!("line {}: not JSON: {e}", n + 1))?;
            let ns = match value.get("knowable_time_ns") {
                Some(Value::Number(v)) => v.as_i64(),
                Some(Value::String(s)) => s.trim().parse().ok(),
                _ => None,
            }
            .ok_or_else(|| format!("line {}: no integer knowable_time_ns", n + 1))?;
            let id = match value.get("record_id") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) => String::new(),
                _ => return Err(format!("line {}: no record_id column", n + 1)),
            };
            rows.push(((ns, id), value, line.to_string()));
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(FileRows { rows })
    }

    /// [`Self::parse`] the file at `path`.
    ///
    /// # Errors
    ///
    /// Unreadable, or malformed as for [`Self::parse`].
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        Self::parse(&text)
    }
}

impl SignozRead for FileRows {
    fn page(&mut self, query: &PageQuery) -> Result<String, ReadError> {
        let since = query.since.timestamp_nanos_opt().unwrap_or(0);
        let until = query.until.timestamp_nanos_opt().unwrap_or(i64::MAX);
        let lines: Vec<&str> = self
            .rows
            .iter()
            .filter(|(cursor, value, _)| {
                query.filters.iter().all(|(name, want)| {
                    value
                        .get(name)
                        .and_then(Value::as_str)
                        .is_some_and(|have| have.eq_ignore_ascii_case(want))
                }) && cursor.0 >= since
                    && cursor.0 <= until
                    && query.after.as_ref().is_none_or(|after| cursor > after)
            })
            .take(query.limit as usize)
            .map(|(_, _, line)| line.as_str())
            .collect();
        Ok(lines.join("\n"))
    }
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

    /// [`Self::url_with`] `query`'s [`PageQuery::params`].
    ///
    /// # Errors
    ///
    /// As for [`Self::url_with`].
    pub fn url(&self, query: &PageQuery) -> Result<reqwest::Url, ReadError> {
        self.url_with(&query.params())
    }

    /// `POST` `sql` with `query`'s parameters bound; the response body.
    ///
    /// # Errors
    ///
    /// The reader is unreachable or answered with an error status.
    pub fn post(&self, sql: &str, query: &PageQuery) -> Result<String, ReadError> {
        self.post_with(sql, &query.params())
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

/// One query over [`TelemetryReader`]: every page `POST`s `sql` with the
/// page's parameters bound.
#[derive(Debug, Clone)]
pub struct SqlPages {
    /// The transport.
    pub http: TelemetryReader,
    /// The keyset-paged query (see the module's paging contract).
    pub sql: &'static str,
}

impl SignozRead for SqlPages {
    fn page(&mut self, query: &PageQuery) -> Result<String, ReadError> {
        self.http.post(self.sql, query)
    }
}

#[cfg(test)]
#[path = "signoz_read_tests.rs"]
mod tests;
