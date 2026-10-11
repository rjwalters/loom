//! Record this host's alerts on loom-ui's Alerts page (#11295).
//!
//! loom-ui is the alert hub for every repo: each sender posts its alerts to
//! `POST {loom-ui}/api/alerts/external` (2AMLogic/loom-ui
//! `docs/alerts/external.md`), and once per notify window claude-bot picks one
//! for the banner and Matrix. Before this module most loom-daemon alerting
//! (fleet_alert, star escalations, the watchdog's outage issue, a dead
//! safehoused) never reached loom-ui at all.
//!
//! # Contract
//!
//! Body `{source, rule, key, title, severity, state, detail?, link?}`,
//! `Authorization: Bearer <host ingest key>`. The incident id is
//! `ext:<source>:<rule>:<key>`: posting `open` again is idempotent and
//! `resolved` closes it. `source` must equal the host id the key
//! authenticates (403 otherwise), so it is this host's id
//! ([`source_host`]) and the alert kind goes in `rule`.
//!
//! # Config: the ingest exporter's, nothing new
//!
//! The URL and key are the native `/ingest` exporter's
//! ([`crate::observability`]): `observability.enabled`, the endpoint with its
//! `/ingest` path replaced by [`ALERTS_PATH`], and the same ingest key file.
//! No ingest config means this module is off and every [`post`] is a no-op.
//! `LOOM_LOOM_UI_ALERTS=0` turns it off on a host that exports telemetry;
//! `LOOM_LOOM_UI_ALERTS_URL` overrides the derived URL.
//!
//! # Delivery
//!
//! [`post`] never blocks: it appends to a bounded FIFO [`outbox::Outbox`] that
//! one task drains in order, so an incident's `open` always goes out before
//! its `resolved`. A 2xx is delivered; any other 4xx except 429 is dropped and
//! logged (retrying a refused body cannot help); 429, 5xx and network errors
//! are retried with capped exponential backoff, a bounded number of times.
//! The watchdog, a separate one-shot process, uses [`post_blocking`] instead.

pub mod outbox;
pub mod sanitize;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use outbox::{Next, Outbox};

/// The path loom-ui serves external alerts on.
pub const ALERTS_PATH: &str = "/api/alerts/external";
/// `0`/`false`/`off` turns the output off.
pub const ENABLED_ENV: &str = "LOOM_LOOM_UI_ALERTS";
/// A full URL that replaces the one derived from `observability.endpoint`.
pub const URL_ENV: &str = "LOOM_LOOM_UI_ALERTS_URL";
/// Per-request timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// `warning` or `critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Critical,
}

impl Severity {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// `open` or `resolved`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Resolved,
}

impl State {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
        }
    }
}

/// One alert transition. Fields are raw; [`payload`] sanitizes them to the
/// contract's charsets and lengths, so callers never have to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub rule: String,
    pub key: String,
    pub title: String,
    pub severity: Severity,
    pub state: State,
    pub detail: Option<String>,
    pub link: Option<String>,
}

impl Alert {
    #[must_use]
    pub fn new(
        rule: impl Into<String>,
        key: impl Into<String>,
        title: impl Into<String>,
        severity: Severity,
        state: State,
    ) -> Self {
        Self {
            rule: rule.into(),
            key: key.into(),
            title: title.into(),
            severity,
            state,
            detail: None,
            link: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    #[must_use]
    pub fn with_link(mut self, link: impl Into<String>) -> Self {
        self.link = Some(link.into());
        self
    }
}

/// The JSON body for `alert` from `source`, every field sanitized. An empty
/// detail and an unusable link are omitted (loom-ui keeps the earlier one).
#[must_use]
pub fn payload(alert: &Alert, source: &str) -> serde_json::Value {
    let mut body = serde_json::json!({
        "source": sanitize::ident(source),
        "rule": sanitize::ident(&alert.rule),
        "key": sanitize::key(&alert.key),
        "title": sanitize::title(&alert.title, &alert.rule),
        "severity": alert.severity.as_str(),
        "state": alert.state.as_str(),
    });
    if let Some(detail) = alert.detail.as_deref().map(sanitize::detail) {
        if !detail.is_empty() {
            body["detail"] = serde_json::Value::String(detail);
        }
    }
    if let Some(link) = alert.link.as_deref().and_then(sanitize::link) {
        body["link"] = serde_json::Value::String(link);
    }
    body
}

/// This host's id as loom-ui knows it. The `/ingest` exporter compares the id
/// loom-ui echoes for the key against [`crate::sweep_registry::host_identity`];
/// when they disagree (#4830) the echoed one is what the key authenticates,
/// so it is the `source` loom-ui will accept.
#[must_use]
pub fn source_host() -> String {
    crate::observability::global_host_id_mismatch()
        .map(|m| m.ingest_host_id)
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(crate::sweep_registry::host_identity)
}

/// What one POST came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// 2xx.
    Delivered,
    /// A 4xx other than 429 (bad body, not allowlisted, source full): retrying
    /// the same body cannot succeed.
    Dropped(String),
    /// 429, 5xx, or no answer at all.
    Retry(String),
}

/// Classify an HTTP status per the contract.
#[must_use]
pub fn classify_status(status: u16) -> Delivery {
    match status {
        200..=299 => Delivery::Delivered,
        429 | 500..=599 => Delivery::Retry(format!("HTTP {status}")),
        _ => Delivery::Dropped(format!("HTTP {status}")),
    }
}

/// Where alerts go: the URL and the bearer key. `Debug` never prints the key.
#[derive(Clone)]
pub struct Target {
    pub url: String,
    bearer: String,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("url", &self.url)
            .field("bearer", &"<redacted>")
            .finish()
    }
}

impl Target {
    #[must_use]
    pub fn new(url: String, bearer: String) -> Self {
        Self { url, bearer }
    }
}

/// `{scheme}://{host}[:port]{prefix}/api/alerts/external` for an `/ingest`
/// endpoint: the endpoint's path with a trailing `/ingest` removed is the
/// loom-ui base. Query and fragment are dropped. `None` for a URL that does
/// not parse.
#[must_use]
pub fn alerts_url_from_ingest(endpoint: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(endpoint.trim()).ok()?;
    let path = url.path().trim_end_matches('/');
    let base = path.strip_suffix("/ingest").unwrap_or(path).to_string();
    url.set_path(&format!("{base}{ALERTS_PATH}"));
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

/// A URL the ingest key may be sent to: https, or http to a loopback host
/// (tests and an on-host proxy). No credentials, no placeholder domain.
fn acceptable_url(raw: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(raw).map_err(|e| format!("unparseable URL {raw}: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL carries credentials".to_string());
    }
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(format!("{raw} is not https"));
    }
    if let Some(host) = crate::observability::endpoint_policy::reserved_placeholder_host(raw) {
        return Err(format!("{raw} points at the reserved placeholder domain {host}"));
    }
    Ok(())
}

fn env_off() -> bool {
    std::env::var(ENABLED_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off")
    })
}

/// Resolve the target from `root`'s observability config. `Err` names why the
/// output is off (never the key).
///
/// # Errors
/// Off by env, observability off, no or unusable endpoint, or no readable key.
pub fn resolve_target(root: &Path) -> Result<Target, String> {
    if env_off() {
        return Err(format!("{ENABLED_ENV} turns it off"));
    }
    let config = crate::observability::read_config(root);
    if !crate::observability::resolve_enabled(&config) {
        return Err("observability is off (no ingest config)".to_string());
    }
    let url = match std::env::var(URL_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        Some(url) => url,
        None => {
            let endpoint = crate::observability::resolve_endpoint(&config)
                .ok_or_else(|| "observability.endpoint is not set".to_string())?;
            alerts_url_from_ingest(&endpoint)
                .ok_or_else(|| format!("cannot derive the alerts URL from {endpoint}"))?
        }
    };
    acceptable_url(&url)?;
    let key_file = crate::observability::resolve_ingest_key_file(&config)
        .ok_or_else(|| "no ingest key file".to_string())?;
    let bearer = std::fs::read_to_string(&key_file)
        .map_err(|e| format!("cannot read ingest key file {key_file}: {e}"))?
        .trim()
        .to_string();
    if bearer.is_empty() {
        return Err(format!("ingest key file {key_file} is empty"));
    }
    Ok(Target { url, bearer })
}

/// Something that can POST one alert body. The production one is
/// [`HttpTransport`]; tests use a fake.
pub trait Transport: Send + Sync + 'static {
    fn post(&self, body: &serde_json::Value) -> impl std::future::Future<Output = Delivery> + Send;
}

/// `reqwest` POST to the target.
pub struct HttpTransport {
    client: reqwest::Client,
    target: Target,
}

impl HttpTransport {
    /// # Errors
    /// The HTTP client cannot be built (no TLS backend).
    pub fn new(target: Target) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client, target })
    }
}

impl Transport for HttpTransport {
    async fn post(&self, body: &serde_json::Value) -> Delivery {
        let sent = self
            .client
            .post(&self.target.url)
            .bearer_auth(&self.target.bearer)
            .json(body)
            .send()
            .await;
        match sent {
            Ok(response) => {
                let status = response.status().as_u16();
                match classify_status(status) {
                    Delivery::Dropped(why) => {
                        // loom-ui's 4xx bodies name the field, never its value.
                        let text = response.text().await.unwrap_or_default();
                        let snippet: String = text.chars().take(300).collect();
                        Delivery::Dropped(format!("{why}: {snippet}"))
                    }
                    other => other,
                }
            }
            // `reqwest::Error`'s Display names the URL, never the headers.
            Err(e) => Delivery::Retry(format!("transport: {e}")),
        }
    }
}

/// The state [`post`] and the drain task share.
pub struct Shared {
    outbox: Mutex<Outbox>,
    notify: tokio::sync::Notify,
    /// Fixed source for tests; `None` resolves [`source_host`] per post.
    source: Option<String>,
}

impl Shared {
    #[must_use]
    pub fn new(capacity: usize, source: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            outbox: Mutex::new(Outbox::new(capacity)),
            notify: tokio::sync::Notify::new(),
            source,
        })
    }

    /// Enqueue without blocking (a short mutex hold, no I/O).
    pub fn push(&self, alert: Alert) {
        let overflow = self
            .outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(alert, now());
        if let Some(lost) = overflow {
            log::warn!(
                "loom_ui_alerts: outbox full, dropped the oldest queued alert {} {} ({})",
                lost.rule,
                lost.key,
                lost.state.as_str()
            );
        }
        self.notify.notify_one();
    }

    /// Queued (not in flight) alerts.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

/// Drain `shared` through `transport` forever, in FIFO order.
pub async fn drain<T: Transport>(shared: Arc<Shared>, transport: T) {
    loop {
        let next = shared
            .outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_ready(now());
        match next {
            Next::Ready(entry) => {
                let source = shared.source.clone().unwrap_or_else(source_host);
                let delivery = transport.post(&payload(&entry.alert, &source)).await;
                match &delivery {
                    Delivery::Delivered => log::debug!(
                        "loom_ui_alerts: posted {} {} {}",
                        entry.alert.rule,
                        entry.alert.key,
                        entry.alert.state.as_str()
                    ),
                    Delivery::Dropped(why) => log::warn!(
                        "loom_ui_alerts: loom-ui refused {} {} {} — dropped: {why}",
                        entry.alert.rule,
                        entry.alert.key,
                        entry.alert.state.as_str()
                    ),
                    Delivery::Retry(why) => log::info!(
                        "loom_ui_alerts: {} {} {} not delivered (attempt {}), will retry: {why}",
                        entry.alert.rule,
                        entry.alert.key,
                        entry.alert.state.as_str(),
                        entry.attempts + 1
                    ),
                }
                let gave_up = shared
                    .outbox
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .settle(entry, &delivery, now());
                if let Some(lost) = gave_up {
                    log::warn!(
                        "loom_ui_alerts: gave up on {} {} {} after {} attempts",
                        lost.alert.rule,
                        lost.alert.key,
                        lost.alert.state.as_str(),
                        lost.attempts
                    );
                }
            }
            Next::Wait(wait) => {
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = shared.notify.notified() => {}
                }
            }
            Next::Empty => shared.notify.notified().await,
        }
    }
}

/// The outbox clock: tokio's, so a paused test runtime controls backoff; the
/// real clock everywhere else (also outside a runtime).
fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

static GLOBAL: OnceLock<Arc<Shared>> = OnceLock::new();

/// Start the output for the daemon at `root` on the current tokio runtime.
/// `None`, with one log line naming why, when there is no ingest config.
#[must_use]
pub fn start(root: &Path) -> Option<tokio::task::JoinHandle<()>> {
    let target = match resolve_target(root) {
        Ok(target) => target,
        Err(why) => {
            log::info!("loom_ui_alerts: off — {why}");
            return None;
        }
    };
    let url = target.url.clone();
    let transport = match HttpTransport::new(target) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("loom_ui_alerts: off — cannot build the HTTP client: {e}");
            return None;
        }
    };
    let shared = Shared::new(outbox::DEFAULT_CAPACITY, None);
    if GLOBAL.set(shared.clone()).is_err() {
        return None;
    }
    log::info!("loom_ui_alerts: recording alerts on loom-ui at {url} (#11295)");
    Some(tokio::spawn(drain(shared, transport)))
}

/// Whether [`start`] is running in this process.
#[must_use]
pub fn is_active() -> bool {
    GLOBAL.get().is_some()
}

/// Queue `alert` for loom-ui. Never blocks; a no-op when the output is off.
pub fn post(alert: Alert) {
    if let Some(shared) = GLOBAL.get() {
        shared.push(alert);
    }
}

/// POST `alert` once and wait at most `timeout` for the answer, from any
/// thread, inside a tokio runtime or not: the request runs on its own thread
/// and runtime. For the watchdog, a one-shot process that retries on its next
/// tick. A timeout is [`Delivery::Retry`].
#[must_use]
pub fn post_blocking(target: &Target, alert: &Alert, timeout: Duration) -> Delivery {
    let body = payload(alert, &source_host());
    let target = target.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("loom-ui-alert".to_string())
        .spawn(move || {
            let delivery = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => match HttpTransport::new(target) {
                    Ok(t) => rt.block_on(t.post(&body)),
                    Err(e) => Delivery::Retry(e),
                },
                Err(e) => Delivery::Retry(e.to_string()),
            };
            let _ = tx.send(delivery);
        });
    if let Err(e) = spawned {
        return Delivery::Retry(e.to_string());
    }
    rx.recv_timeout(timeout)
        .unwrap_or_else(|_| Delivery::Retry(format!("no answer within {}s", timeout.as_secs())))
}
