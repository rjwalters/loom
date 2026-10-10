//! The SigNoz read client: keyset-paged `JSONEachRow` reads from the
//! telemetry store's ClickHouse (#10196 R6).
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
//! - [`ClickhouseHttp`]: ClickHouse's HTTP interface, bound query parameters,
//!   the credential read from an owner-only file at call time and never
//!   logged ([`SqlPages`] pairs it with one query).
//! - [`FileRows`]: the rows of an export an operator ran with
//!   `clickhouse-client`, served under the same filters, order and keyset —
//!   and the recorded-fixture reader tests use.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

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
    /// The backend did not answer (connection, timeout, missing credential).
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

/// ClickHouse's HTTP interface: `POST` a query, bind the parameters as
/// `param_*`, authenticate with `X-ClickHouse-User` / `X-ClickHouse-Key`.
///
/// The password is read from `credential_file` on every request and dropped
/// immediately; it never reaches a log line, an error string or a file. A
/// credential file readable by group or others is refused (unix), per the
/// owner-only rule in `credential-storage.md`.
#[derive(Debug, Clone)]
pub struct ClickhouseHttp {
    /// `http(s)://host:port`.
    pub endpoint: String,
    /// ClickHouse user.
    pub user: Option<String>,
    /// Owner-only password file.
    pub credential_file: Option<PathBuf>,
    /// Per-request timeout.
    pub timeout: std::time::Duration,
}

impl ClickhouseHttp {
    fn password(&self) -> Result<Option<String>, ReadError> {
        let Some(path) = &self.credential_file else {
            return Ok(None);
        };
        let unavailable = |why: &str| {
            ReadError::Unavailable(format!("credential file {}: {why}", path.display()))
        };
        let meta = std::fs::metadata(path).map_err(|_| unavailable("unreadable"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(unavailable("readable by group or others; chmod 600 it"));
            }
        }
        let _ = meta;
        let secret = std::fs::read_to_string(path).map_err(|_| unavailable("unreadable"))?;
        let secret = secret.trim().to_string();
        if secret.is_empty() {
            return Err(unavailable("empty"));
        }
        Ok(Some(secret))
    }

    /// The request URL: the endpoint plus every parameter in `params`, each
    /// bound as `param_<name>`.
    ///
    /// # Errors
    ///
    /// The endpoint is not an `http(s)` URL.
    pub fn url_with(&self, params: &[(String, String)]) -> Result<reqwest::Url, ReadError> {
        let mut url = reqwest::Url::parse(&self.endpoint)
            .map_err(|e| ReadError::Unavailable(format!("invalid endpoint: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ReadError::Unavailable("endpoint is not http(s)".to_string()));
        }
        {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in params {
                pairs.append_pair(&format!("param_{name}"), value);
            }
        }
        Ok(url)
    }

    /// [`Self::url_with`] `query`'s [`PageQuery::params`].
    ///
    /// # Errors
    ///
    /// The endpoint is not an `http(s)` URL.
    pub fn url(&self, query: &PageQuery) -> Result<reqwest::Url, ReadError> {
        self.url_with(&query.params())
    }

    /// `POST` `sql` with `query`'s parameters bound; the response body.
    ///
    /// # Errors
    ///
    /// The backend is unreachable or answered with an error status.
    pub fn post(&self, sql: &str, query: &PageQuery) -> Result<String, ReadError> {
        self.post_with(sql, &query.params())
    }

    /// `POST` `sql` with `params` bound; the response body.
    ///
    /// # Errors
    ///
    /// The backend is unreachable or answered with an error status.
    pub fn post_with(&self, sql: &str, params: &[(String, String)]) -> Result<String, ReadError> {
        let url = self.url_with(params)?;
        let password = self.password()?;
        let user = self.user.clone();
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
                        let client = reqwest::Client::builder()
                            .timeout(timeout)
                            .build()
                            .map_err(|e| ReadError::Unavailable(format!("client: {e}")))?;
                        let mut request = client.post(url).body(sql);
                        if let Some(user) = user {
                            request = request.header("X-ClickHouse-User", user);
                        }
                        if let Some(password) = password {
                            request = request.header("X-ClickHouse-Key", password);
                        }
                        let response = request.send().await.map_err(|e| {
                            ReadError::Unavailable(format!("request failed: {}", e.without_url()))
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

/// Where the telemetry store's ClickHouse is, from configuration.
///
/// The neutral key is `telemetry.signoz.{endpoint,user,credentialFile}`. For
/// one release the old `autonomous.eta.fleetRefresh.signoz.*` key is read as a
/// per-field fallback (`legacy` names the fields that came from it, so the
/// caller can warn once), letting #11098 Stage 3 delete `autonomous.eta.*`
/// without breaking replay. `credentialFile` is a **path** to an owner-only
/// file, never the secret.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointConfig {
    /// `http(s)://host:port`.
    pub endpoint: Option<String>,
    /// ClickHouse user.
    pub user: Option<String>,
    /// Owner-only password file.
    pub credential_file: Option<PathBuf>,
    /// The fields read from the deprecated `autonomous.eta` key.
    pub legacy: Vec<&'static str>,
}

/// The neutral configuration key.
pub const ENDPOINT_CONFIG_KEY: &str = "telemetry.signoz";
/// The deprecated key read as a fallback for one release.
pub const LEGACY_ENDPOINT_CONFIG_KEY: &str = "autonomous.eta.fleetRefresh.signoz";

impl EndpointConfig {
    /// Resolve from an effective config document.
    #[must_use]
    pub fn from_config(config: &Value) -> Self {
        let block = |path: &str| {
            path.split('.')
                .try_fold(config, |node, key| node.get(key))
                .cloned()
        };
        let neutral = block(ENDPOINT_CONFIG_KEY);
        let legacy = block(LEGACY_ENDPOINT_CONFIG_KEY);
        let read = |b: &Option<Value>, key: &str| {
            b.as_ref()
                .and_then(|b| b.get(key))
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let mut out = EndpointConfig::default();
        let mut pick = |key: &'static str| {
            read(&neutral, key).or_else(|| {
                let old = read(&legacy, key);
                if old.is_some() {
                    out.legacy.push(key);
                }
                old
            })
        };
        let endpoint = pick("endpoint");
        let user = pick("user");
        let credential_file = pick("credentialFile").map(PathBuf::from);
        out.endpoint = endpoint;
        out.user = user;
        out.credential_file = credential_file;
        out
    }

    /// Resolve from `repo_root`'s effective Loom configuration.
    #[must_use]
    pub fn read(repo_root: &Path) -> Self {
        Self::from_config(&crate::config_resolver::resolve_effective_config(repo_root))
    }
}

/// One query over [`ClickhouseHttp`]: every page `POST`s `sql` with the
/// page's parameters bound.
#[derive(Debug, Clone)]
pub struct SqlPages {
    /// The transport.
    pub http: ClickhouseHttp,
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
