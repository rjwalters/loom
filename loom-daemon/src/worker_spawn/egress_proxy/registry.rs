//! The live launch records a request is authorized against (issue #8674).
//!
//! One record per contained launch: a per-launch **placeholder** (the only
//! credential-shaped string the container ever sees), the real credential it
//! stands for, and the single upstream origin that credential may be sent to.
//!
//! Three properties this file is responsible for, each of which the tests in
//! `super::tests` assert directly:
//!
//! 1. **The placeholder is worthless by itself.** It is looked up in this
//!    registry; a value that is not a live record's placeholder is refused,
//!    never forwarded with *some* credential.
//! 2. **Closing invalidates immediately.** [`Registry::close_all`] runs the
//!    moment the container exits, so a placeholder that escaped the box is
//!    dead before anything off-host could replay it.
//! 3. **There is exactly one reachable upstream per record.** The upstream is
//!    a property of the *record*, never of the request, so no request can
//!    steer the substituted credential anywhere else. [`Upstream::pinned`]
//!    additionally refuses a request that merely *names* another host, so an
//!    attempt shows up as a logged 403 rather than as silent success against
//!    the pinned host.
//!
//! A record's credential is not immutable: host-side account rotation
//! ([`super::rotation`], #8818) swaps it in place behind the SAME placeholder.
//! The per-record `account`, `generation` and `evidence` fields exist for that,
//! and none of them is ever set from anything the container sent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How a provider expects the real credential to be presented upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeaderStyle {
    /// `Authorization: Bearer <credential>` — Claude Code's
    /// `ANTHROPIC_AUTH_TOKEN` shape, and most OpenAI-compatible endpoints.
    #[default]
    AuthorizationBearer,
    /// `x-api-key: <credential>` — Anthropic's native API-key header.
    XApiKey,
}

impl HeaderStyle {
    /// `(header name, header value)` carrying `credential`.
    #[must_use]
    pub fn render(self, credential: &str) -> (&'static str, String) {
        match self {
            Self::AuthorizationBearer => ("authorization", format!("Bearer {credential}")),
            Self::XApiKey => ("x-api-key", credential.to_string()),
        }
    }
}

/// Header names an inbound request may use to present its placeholder, and
/// which are therefore **always stripped** before forwarding: whatever the
/// container sent must never reach the upstream, even when it was not the
/// value this proxy matched on.
pub const CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
];

/// The one origin a record's credential may be sent to.
///
/// Parsed from a profile's `credentialProxy.upstream`. Deliberately hand-parsed
/// rather than pulling in a URL crate: the accepted grammar is exactly
/// `scheme://host[:port][/path]`, and *everything* else — userinfo, a query, a
/// fragment — is rejected rather than normalised away, because each of those is
/// a way to smuggle a different destination past a lenient parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    scheme: String,
    host: String,
    port: Option<u16>,
    /// Path prefix every proxied request is mounted under; `""` or `/v1`.
    base_path: String,
}

impl Upstream {
    /// Parse and validate. The error message never echoes the input beyond the
    /// structural problem, so a credential pasted into `upstream` by mistake
    /// cannot reach a log through this path.
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let raw = raw.trim();
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or("upstream must be an absolute URL")?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "https" && scheme != "http" {
            return Err("upstream scheme must be http or https");
        }
        if rest.contains('@') {
            return Err("upstream must not carry userinfo");
        }
        if rest.contains('?') || rest.contains('#') {
            return Err("upstream must not carry a query or fragment");
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (host, port) = split_authority(authority)?;
        let base_path = path.trim_end_matches('/').to_string();
        Ok(Self {
            scheme,
            host,
            port,
            base_path,
        })
    }

    /// `host` or `host:port` — the form a `Host:` header carries.
    #[must_use]
    pub fn authority(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Explicit port, when the URL carried one.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// The absolute URL a request for `target` (an origin-form path, query
    /// included) is forwarded to. `target` is used verbatim: this proxy never
    /// rewrites a path, so it cannot be tricked into rewriting one *into* a
    /// different origin.
    #[must_use]
    pub fn url_for(&self, target: &str) -> String {
        let target = if target.starts_with('/') {
            target.to_string()
        } else {
            format!("/{target}")
        };
        format!("{}://{}{}{target}", self.scheme, self.authority(), self.base_path)
    }

    /// Is `requested` (an authority a request named for itself, via an
    /// absolute-form request target or its `Host:` header) allowed?
    ///
    /// Two cases are legitimate and nothing else is: the client addressed
    /// **this proxy** (the ordinary case — the harness's base URL points here),
    /// or it addressed the pinned upstream explicitly. A third host is an
    /// attempt to use this listener as an open relay.
    #[must_use]
    pub fn pinned(&self, requested: &str) -> bool {
        let requested = requested.trim().to_ascii_lowercase();
        let host = strip_port(&requested);
        host == self.host || is_proxy_local(host)
    }
}

/// `host:port` / `[v6]:port` / `host` -> `host`.
fn strip_port(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => authority,
    }
}

/// Names that can only mean "the listener the container was pointed at".
fn is_proxy_local(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "host.docker.internal")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
        // The docker bridge gateway address the container reaches the host at
        // on Linux; it is whatever `docker network inspect` reported, so it is
        // matched structurally (a private address) rather than by literal.
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_private())
}

fn split_authority(authority: &str) -> Result<(String, Option<u16>), &'static str> {
    if authority.is_empty() {
        return Err("upstream has no host");
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or("upstream has an unterminated IPv6 host")?;
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(
                p.parse::<u16>()
                    .map_err(|_| "upstream port is not a number")?,
            ),
            None if tail.is_empty() => None,
            None => return Err("upstream authority is malformed"),
        };
        return Ok((host.to_ascii_lowercase(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Ok((
            host.to_ascii_lowercase(),
            Some(
                port.parse::<u16>()
                    .map_err(|_| "upstream port is not a number")?,
            ),
        )),
        None => Ok((authority.to_ascii_lowercase(), None)),
    }
}

/// One launch's per-request usage tally (issue #8699): request/response byte
/// counts and the provider's own rate-limit/usage response headers.
///
/// Holds nothing secret by construction: [`super::server`] only ever feeds
/// this an **allowlisted** set of header names
/// (`anthropic-ratelimit-*`, `x-ratelimit-*`, `retry-after`), never anything
/// from [`CREDENTIAL_HEADERS`], so `#[derive(Debug)]` here is safe — unlike
/// [`Record`], which still hand-writes its `Debug` because it also carries the
/// credential.
#[derive(Clone, Debug, Default)]
pub struct Usage {
    /// Total bytes of every request body forwarded upstream for this launch.
    pub request_bytes: u64,
    /// Total bytes of every response body streamed back for this launch.
    pub response_bytes: u64,
    /// The most recently observed rate-limit/usage response headers —
    /// replaced (not accumulated) on every response, so this always reflects
    /// the provider's current window rather than a stale first reading.
    pub headers: Vec<(String, String)>,
}

/// A live launch's credential substitution. `Debug` redacts the secret and
/// there is deliberately no `Serialize`, mirroring
/// [`super::super::credential::Resolved`].
#[derive(Clone)]
pub struct Record {
    /// Non-secret correlation id, safe to log. Never derived from the
    /// placeholder.
    pub launch_id: String,
    /// Pool provider namespace or profile name — attribution only.
    pub provider: String,
    pub upstream: Upstream,
    pub header: HeaderStyle,
    pub(super) credential: String,
    pub(super) open: bool,
    /// Pool account name the credential belongs to (non-secret, host-side
    /// only). `None` disables rotation for this record (#8818).
    pub(super) account: Option<String>,
    /// Bumped on every in-place credential swap, so an upstream status that
    /// answered a request made with the OLD credential is never counted as
    /// evidence against the new one.
    pub(super) generation: u32,
    /// Swaps performed so far for this launch.
    pub(super) rotations: u32,
    /// What the upstream said about the CURRENT credential.
    pub(super) evidence: Evidence,
    /// This launch's pool account name, and the workspace root its pool lives
    /// under — set only when [`super::credential::Source::Pool`] selected the
    /// credential (#8699). `None` for an env-sourced credential (a one-off
    /// export, #8363) or a non-pool-capable caller such as `exec::build`'s
    /// Claude path (#8697): the proxy bad-marks nothing in either case,
    /// mirroring `ingest::LaunchRecord::is_pool_selected`'s guard.
    pool_account: Option<String>,
    workspace_root: Option<Arc<PathBuf>>,
    /// Model class a mark is scoped to (#8424 item 3), normalized the same
    /// way `ingest::ingest_launch_log` does — so a proxy-side mark and a later
    /// exit-code-driven mark land on the very same `(account, class)` pair and
    /// the latter's no-downgrade check (see `api_keys_pool::ingest`) finds
    /// it instead of writing a second one.
    model_class: Option<String>,
    /// The longest cooldown (seconds; `0` = never marked) this launch has
    /// bad-marked at the proxy. Shared across every clone of this record (one
    /// per request), so a repeat 429 on the same launch — however many
    /// requests race it — bad-marks at most once per *strength*, and only
    /// ever escalates (60s rate-limited → 6h exhausted), never re-marks at
    /// the same or a weaker level (#8699 AC2).
    bad_marked: Arc<AtomicU64>,
    /// Shared with every clone of this record so a per-request tally lands in
    /// the one instance the launch owns.
    usage: Arc<Mutex<Usage>>,
}

/// Upstream refusals the proxy itself observed for a record's current
/// credential. A rotation that bad-marks an account needs the matching kind
/// (see [`super::rotation`]): the container's word alone is not enough.
///
/// Deliberately narrow, because the container chooses the method, path, body
/// and non-credential headers of every request it sends through the proxy, so
/// it can make the upstream refuse a HEALTHY credential on purpose (a bad path
/// or body is a 400/404; a stripped header may be a 401/403). Only statuses
/// that such a request cannot cheaply fake count, and even then evidence is a
/// pre-gate, not proof: a permanent mark additionally needs the host's own
/// re-probe (see [`super::rotation`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Evidence {
    /// The upstream answered 401 (403 is not counted: it is a scope/permission
    /// answer a request can provoke, not "this credential is dead").
    pub(super) auth_failure: bool,
    /// The upstream answered 429 (other 4xx are request-shape answers the
    /// container controls, not exhaustion).
    pub(super) rate_limited: bool,
}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let usage = self.usage.lock().unwrap_or_else(|e| e.into_inner()).clone();
        f.debug_struct("Record")
            .field("launch_id", &self.launch_id)
            .field("provider", &self.provider)
            .field("upstream", &self.upstream)
            .field("header", &self.header)
            .field("credential", &"<redacted>")
            .field("open", &self.open)
            .field("pool_account", &self.pool_account)
            .field("model_class", &self.model_class)
            .field("bad_marked", &(self.bad_marked.load(Ordering::Relaxed) > 0))
            .field("usage", &usage)
            .field("account", &self.account)
            .field("generation", &self.generation)
            .field("rotations", &self.rotations)
            .finish()
    }
}

impl Record {
    #[must_use]
    pub fn new(
        launch_id: impl Into<String>,
        provider: impl Into<String>,
        upstream: Upstream,
        header: HeaderStyle,
        credential: impl Into<String>,
    ) -> Self {
        Self {
            launch_id: launch_id.into(),
            provider: provider.into(),
            upstream,
            header,
            credential: credential.into(),
            open: true,
            pool_account: None,
            workspace_root: None,
            model_class: None,
            bad_marked: Arc::new(AtomicU64::new(0)),
            usage: Arc::new(Mutex::new(Usage::default())),
            account: None,
            generation: 0,
            rotations: 0,
            evidence: Evidence::default(),
        }
    }

    /// Attach the pool attribution the proxy needs to bad-mark this launch's
    /// account (#8699). Builder method, kept separate from [`Self::new`] so
    /// every existing caller — and every non-pool launch — is unaffected.
    #[must_use]
    pub fn with_pool_account(
        mut self,
        workspace_root: PathBuf,
        account: impl Into<String>,
        model_class: Option<String>,
    ) -> Self {
        self.pool_account = Some(account.into());
        self.workspace_root = Some(Arc::new(workspace_root));
        self.model_class = model_class;
        self
    }

    /// `(header, value)` to send upstream in place of whatever the container
    /// presented.
    #[must_use]
    pub fn upstream_header(&self) -> (&'static str, String) {
        self.header.render(&self.credential)
    }

    /// This launch's pool account name, when its credential was pool-selected.
    #[must_use]
    pub fn pool_account(&self) -> Option<&str> {
        self.pool_account.as_deref()
    }

    /// The workspace root whose pool [`Self::pool_account`] belongs to.
    #[must_use]
    pub fn workspace_root(&self) -> Option<&std::path::Path> {
        self.workspace_root.as_deref().map(PathBuf::as_path)
    }

    /// The model class a mark should be scoped to, when the launch named one.
    #[must_use]
    pub fn model_class(&self) -> Option<&str> {
        self.model_class.as_deref()
    }

    /// Atomically raises this launch's bad-mark level to `cooldown_secs`
    /// (`None` = unbounded), returning `true` only when that is strictly
    /// stronger than anything this launch has already marked. A repeat 429
    /// at the same or a weaker level — concurrent or sequential — sees
    /// `false` and does nothing, so a racing pair marks once (#8699 AC2);
    /// a later, stronger signal (a quota exhaustion after a bare 429) still
    /// escalates rather than being dropped.
    pub fn begin_bad_mark(&self, cooldown_secs: Option<u64>) -> bool {
        let level = cooldown_secs.unwrap_or(u64::MAX).max(1);
        self.bad_marked.fetch_max(level, Ordering::SeqCst) < level
    }

    /// Add one request/response pair's byte counts to this launch's running
    /// total, and replace the captured usage headers with the ones just
    /// observed.
    pub fn record_usage(
        &self,
        request_bytes: u64,
        response_bytes: u64,
        headers: Vec<(String, String)>,
    ) {
        let mut usage = self.usage.lock().unwrap_or_else(|e| e.into_inner());
        usage.request_bytes += request_bytes;
        usage.response_bytes += response_bytes;
        usage.headers = headers;
    }

    /// A snapshot of this launch's usage so far.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The one-line, secret-free usage summary written to the launch log when
    /// the launch ends (#8699 AC1's durable surface), keyed by `launch_id`:
    ///
    /// `# LOOM_EGRESS_USAGE launch=<id> provider=<p> account=<name|none>
    /// request_bytes=<n> response_bytes=<n> bad_marked=<bool> [<header>=<value>]...`
    ///
    /// Built only from fields that are non-secret by construction — never the
    /// credential, never the placeholder (which is the registry's map key and
    /// not reachable from a `Record` at all). Header values are provider
    /// telemetry from [`Usage::headers`]' allowlist, with whitespace and
    /// control characters replaced so a value can never split the line.
    #[must_use]
    pub fn usage_marker(&self) -> String {
        let usage = self.usage();
        let mut line = format!(
            "{USAGE_MARKER_PREFIX}launch={} provider={} account={} request_bytes={} \
             response_bytes={} bad_marked={}",
            marker_value(&self.launch_id),
            marker_value(&self.provider),
            self.pool_account
                .as_deref()
                .map_or_else(|| "none".to_string(), marker_value),
            usage.request_bytes,
            usage.response_bytes,
            self.bad_marked.load(Ordering::SeqCst) > 0,
        );
        for (name, value) in &usage.headers {
            line.push_str(&format!(" {}={}", marker_value(name), marker_value(value)));
        }
        line
    }
}

/// Prefix of the per-launch usage line [`Record::usage_marker`] renders.
/// `api_keys_pool::classify` drops lines carrying it before classifying a
/// retained launch log, so a captured header value (a `retry-after` next to a
/// rate-limit header name) can never itself read as an exhaustion signal.
pub const USAGE_MARKER_PREFIX: &str = "# LOOM_EGRESS_USAGE ";

/// `raw` with whitespace, control characters and `=` replaced by `_`, so one
/// value is always exactly one `key=value` token on one line.
fn marker_value(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_whitespace() || c.is_control() || c == '=' {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned.is_empty() {
        "_".to_string()
    } else {
        cleaned
    }
}

/// Why a request was not forwarded. Every variant is logged; none of them ever
/// carries the presented value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No placeholder-bearing header at all.
    MissingCredential,
    /// A value was presented that is not any live record's placeholder.
    UnknownPlaceholder,
    /// The record exists but its launch has ended.
    ClosedLaunch,
    /// The request named a host that is neither this proxy nor the pin.
    HostNotPinned,
    /// `CONNECT` and friends: tunnelling would make this an open relay.
    MethodNotAllowed,
}

impl Refusal {
    /// HTTP status and reason phrase.
    #[must_use]
    pub fn status(self) -> (u16, &'static str) {
        match self {
            Self::MissingCredential | Self::UnknownPlaceholder | Self::ClosedLaunch => {
                (401, "Unauthorized")
            }
            Self::HostNotPinned => (403, "Forbidden"),
            Self::MethodNotAllowed => (405, "Method Not Allowed"),
        }
    }

    /// Stable machine-greppable token for the log line and the response body.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::MissingCredential => "missing_credential",
            Self::UnknownPlaceholder => "unknown_placeholder",
            Self::ClosedLaunch => "closed_launch",
            Self::HostNotPinned => "host_not_pinned",
            Self::MethodNotAllowed => "method_not_allowed",
        }
    }
}

/// The live set of launch records, shared between the dispatcher and every
/// connection task.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<String, Record>>>,
    /// Host-side account rotation (#8818). Unset — every native-harness
    /// launch, and any Claude launch with no known account — refuses every
    /// rotation request.
    pub(super) rotation: Arc<std::sync::OnceLock<super::rotation::Control>>,
    /// Serializes rotations so two concurrent requests cannot both bad-mark
    /// and both swap.
    pub(super) rotation_serial: Arc<Mutex<()>>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `placeholder` to `record`. The placeholder is the map KEY and is
    /// never logged or rendered anywhere.
    pub fn insert(&self, placeholder: &super::Placeholder, record: Record) {
        self.lock().insert(placeholder.as_str().to_string(), record);
    }

    /// Invalidate every record. Called the instant the contained launch exits;
    /// after this, every request is refused with [`Refusal::ClosedLaunch`]
    /// until the process itself goes away.
    pub fn close_all(&self) {
        for record in self.lock().values_mut() {
            record.open = false;
        }
    }

    /// [`Record::usage_marker`] for every launch this registry holds, sorted
    /// by `launch_id` so the output is deterministic. One launch per registry
    /// in practice (see `mod::arm`).
    #[must_use]
    pub fn usage_markers(&self) -> Vec<String> {
        let mut records: Vec<Record> = self.lock().values().cloned().collect();
        records.sort_by(|a, b| a.launch_id.cmp(&b.launch_id));
        records.iter().map(Record::usage_marker).collect()
    }

    /// Authorize one request.
    ///
    /// `presented` is every credential-shaped value the request carried, in
    /// the order they appeared. `requested_authority` is the host the request
    /// named for itself (absolute-form target, else `Host:`), when it named
    /// one at all.
    pub fn authorize(
        &self,
        method: &str,
        presented: &[String],
        requested_authority: Option<&str>,
    ) -> Result<Record, Refusal> {
        if method.eq_ignore_ascii_case("CONNECT") || method.eq_ignore_ascii_case("TRACE") {
            return Err(Refusal::MethodNotAllowed);
        }
        if presented.is_empty() {
            return Err(Refusal::MissingCredential);
        }
        let guard = self.lock();
        // Match on the FIRST presented value that names a record at all —
        // including a closed one, so an expired launch reports `closed_launch`
        // rather than the indistinguishable `unknown_placeholder`.
        let record = presented
            .iter()
            .find_map(|value| guard.get(value))
            .ok_or(Refusal::UnknownPlaceholder)?;
        if !record.open {
            return Err(Refusal::ClosedLaunch);
        }
        if let Some(authority) = requested_authority {
            if !record.upstream.pinned(authority) {
                return Err(Refusal::HostNotPinned);
            }
        }
        Ok(record.clone())
    }

    /// Record an upstream response status against the record that made the
    /// request — only if its credential has not been swapped since. Only 401
    /// and 429 are recorded (see [`Evidence`]).
    pub fn observe(&self, launch_id: &str, generation: u32, status: u16) {
        if !matches!(status, 401 | 429) {
            return;
        }
        for record in self.lock().values_mut() {
            if record.launch_id == launch_id && record.generation == generation {
                record.evidence.rate_limited |= status == 429;
                record.evidence.auth_failure |= status == 401;
            }
        }
    }

    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Record>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}
