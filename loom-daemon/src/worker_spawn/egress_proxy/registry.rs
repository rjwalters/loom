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

use std::collections::HashMap;
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
    credential: String,
    open: bool,
}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record")
            .field("launch_id", &self.launch_id)
            .field("provider", &self.provider)
            .field("upstream", &self.upstream)
            .field("header", &self.header)
            .field("credential", &"<redacted>")
            .field("open", &self.open)
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
        }
    }

    /// `(header, value)` to send upstream in place of whatever the container
    /// presented.
    #[must_use]
    pub fn upstream_header(&self) -> (&'static str, String) {
        self.header.render(&self.credential)
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

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Record>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}
