//! The agent telemetry relay's session side (Issue #10964): which launched
//! sessions may export through the daemon, and the environment that points
//! them at it.
//!
//! An agent CLI the daemon launches can export its own OTLP telemetry. With
//! the relay on, the daemon runs a loopback OTLP/HTTP receiver
//! (`otlp::relay`, behind the `otlp` Cargo feature), points each session it
//! launches at that receiver, and — for every record received — binds the
//! identity itself, applies the session-output redaction, and forwards to the
//! configured `otlp` exporter. This module is the half that needs no OTLP
//! types: the opt-in, the session registry, and the child environment.
//!
//! # Scope: sessions the daemon launches, and nothing else
//!
//! The only way a process learns the receiver's address and a token is
//! [`Relay::prepare`] writing them onto the [`Command`] of a child the daemon
//! is about to spawn. Nothing is ever written to this process's own
//! environment, to a file, or to a config a second process could read, so:
//!
//! - an interactive session started outside Loom gets nothing from Loom and
//!   exports nothing through the daemon;
//! - a machine with no daemon running has no receiver at all;
//! - a daemon without the relay opted in binds no socket and
//!   [`prepare_sweep_child`] / [`prepare_role_child`] leave the child's
//!   environment exactly as they found it.
//!
//! # Off by default, twice
//!
//! The receiver starts only when **both** hold: an `otlp` exporter actually
//! started (the relay never adds, resolves or starts one), and
//! `observability.agentRelay.enabled` is `true` (**env > config > default
//! `false`**, [`ENABLED_ENV`]). The same switch is read again per launch
//! against the *launched session's* workspace, so one repository can opt out
//! of a relay its daemon runs.
//!
//! # Attribution without trusting the sender
//!
//! Each launch mints a random token that reaches the child only through its
//! `OTEL_EXPORTER_OTLP_HEADERS` variable. The receiver looks a request's
//! token up here to learn which session sent it; nothing in the request body
//! takes part in that decision. The registry stores only the token's SHA-256,
//! and no log line, error or exported record ever carries the token.
//!
//! A token stops working when its session ends ([`end_execution`] for a
//! sweep, dropping the [`SessionLease`] for a role tick) and, as a backstop
//! for an end the daemon never observed, [`MAX_SESSION_LIFETIME`] after it
//! was minted.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// `observability.agentRelay.enabled` env override.
pub const ENABLED_ENV: &str = "LOOM_OBSERVABILITY_AGENT_RELAY";

/// The `observability` sub-block this module reads.
pub const CONFIG_PATH: &str = "observability.agentRelay";

/// How long a token is honoured when the daemon never sees its session end
/// (a session adopted by nothing, a lifecycle call that never ran). Longer
/// than any sweep or role tick is allowed to run.
pub const MAX_SESSION_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// Sessions tracked at once. A launch past the ceiling is simply not wired
/// (and says so once), so the registry cannot grow without bound.
pub const MAX_SESSIONS: usize = 512;

/// How long an unresolvable workspace's forge slug is left alone before the
/// lookup is tried again.
const REPO_RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

/// Variables set on a wired Claude Code child. The token-bearing one is last
/// so [`RELAY_HEADERS_ENV`] stays its single definition.
pub const RELAY_SET_ENV: &[&str] = &[
    "CLAUDE_CODE_ENABLE_TELEMETRY",
    "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA",
    "OTEL_METRICS_EXPORTER",
    "OTEL_LOGS_EXPORTER",
    "OTEL_TRACES_EXPORTER",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    RELAY_HEADERS_ENV,
];

/// The variable that carries the session token to the child.
pub const RELAY_HEADERS_ENV: &str = "OTEL_EXPORTER_OTLP_HEADERS";

/// The OTLP encoding a wired child is told to use. Protobuf rather than JSON
/// because it is the exact encoding (no int64-as-string or enum-as-name
/// variants to tolerate); the receiver accepts both.
pub const RELAY_PROTOCOL: &str = "http/protobuf";

/// Variables removed from a wired child, so nothing inherited from the
/// daemon's own environment can redirect a signal or widen what is captured.
///
/// - The per-signal endpoint / protocol / headers variables take precedence
///   over the generic ones in Claude Code, so an inherited one would send that
///   signal — and the session token — somewhere other than the relay.
/// - Compression: the receiver reads identity-encoded bodies only.
/// - The content gates: the relay scrubs secret *shapes*; it is not a content
///   filter, so it never turns on prompt, response, tool-content or raw-body
///   capture, and removes an inherited switch that would.
///   (`OTEL_LOG_TOOL_DETAILS` stays owned by
///   `observability.claudeCodeTelemetry.logToolDetails`.)
pub const RELAY_CLEARED_ENV: &[&str] = &[
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
    "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
    "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
    "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
    "OTEL_EXPORTER_OTLP_COMPRESSION",
    "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
    "OTEL_EXPORTER_OTLP_METRICS_COMPRESSION",
    "OTEL_EXPORTER_OTLP_LOGS_COMPRESSION",
    "OTEL_EXPORTER_OTLP_CLIENT_KEY",
    "OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE",
    "OTEL_EXPORTER_OTLP_CERTIFICATE",
    "OTEL_LOG_USER_PROMPTS",
    "OTEL_LOG_ASSISTANT_RESPONSES",
    "OTEL_LOG_TOOL_CONTENT",
    "OTEL_LOG_RAW_API_BODIES",
    "OTEL_LOG_MANAGED_SETTINGS",
];

/// **env > config > default** (`false`), read against `root`'s resolved
/// config. The relay carries an agent's own telemetry, so it never starts
/// because something else was enabled.
#[must_use]
pub fn enabled(root: &Path) -> bool {
    let from_env = std::env::var(ENABLED_ENV)
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
    from_env
        .or_else(|| {
            let config = crate::config_resolver::resolve_effective_config(root);
            crate::config_resolver::get_path(&config, CONFIG_PATH)?
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false)
}

/// The agent CLIs the relay is wired for. Deliberately a closed set: a
/// harness is added here only once its exporter configuration is known and
/// tested, never guessed from a runtime name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    /// Claude Code, configured entirely through its documented
    /// `CLAUDE_CODE_ENABLE_TELEMETRY` / `OTEL_*` environment.
    ClaudeCode,
}

impl Harness {
    /// The harness an **admitted** runtime name denotes, or `None` when it is
    /// not wired. `None` in (no admission: the spawn script resolves the
    /// runtime itself) is `None` out — the daemon does not know which CLI
    /// will run, and `service.name` is never a guess.
    ///
    /// Codex configures OTLP export in its `config.toml` `[otel]` table, not
    /// through `OTEL_EXPORTER_OTLP_*`, and opencode documents no OTLP exporter
    /// setting; neither is wired here.
    #[must_use]
    pub fn from_admitted_runtime(runtime: Option<&str>) -> Option<Self> {
        match runtime?.trim() {
            "claude" => Some(Harness::ClaudeCode),
            _ => None,
        }
    }

    /// The Loom runtime name, exported as `loom.runtime`.
    #[must_use]
    pub fn runtime(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude",
        }
    }

    /// The `service.name` the daemon binds on everything this harness sends.
    #[must_use]
    pub fn service_name(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
        }
    }
}

/// What kind of launch a session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A dispatched `/loom:sweep` child.
    Sweep,
    /// A scheduled role-runner tick.
    Role,
}

impl SessionKind {
    /// The `loom.session.kind` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Sweep => "sweep",
            SessionKind::Role => "role",
        }
    }
}

/// Who a launched session is, as the daemon knew it at launch. This — never
/// anything the session later sends — is what its telemetry is filed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    pub harness: Harness,
    pub kind: SessionKind,
    /// The role a role tick ran as; `None` for a sweep (which runs every
    /// lifecycle role inside one session).
    pub role: Option<String>,
    /// The issue a sweep was dispatched for.
    pub issue: Option<u32>,
    /// The sweep id, or a role tick's execution id.
    pub sweep_id: Option<String>,
    /// The workspace the session was launched in; resolved to a forge slug
    /// off the launch path, never exported as a path.
    pub workspace_root: PathBuf,
}

/// [`SessionIdentity`] as the receiver stamps it: with the daemon's host id
/// and the workspace's forge slug, when one has been resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundIdentity {
    pub identity: SessionIdentity,
    pub host_id: String,
    /// `owner/name`. Absent until resolved — a directory basename is never
    /// substituted.
    pub repo: Option<String>,
}

impl BoundIdentity {
    /// A stable, secret-free label for one session, for drop accounting.
    #[must_use]
    pub fn label(&self) -> String {
        let id = &self.identity;
        match (&id.sweep_id, &id.role, id.issue) {
            (Some(sweep), _, _) => sweep.clone(),
            (None, Some(role), _) => format!("role-{role}"),
            (None, None, Some(issue)) => format!("issue-{issue}"),
            (None, None, None) => id.kind.as_str().to_string(),
        }
    }
}

type TokenDigest = [u8; 32];

fn digest(token: &str) -> TokenDigest {
    Sha256::digest(token.as_bytes()).into()
}

struct Session {
    identity: SessionIdentity,
    registered_at: Instant,
}

enum RepoState {
    Known(String),
    /// A lookup is running, or failed at this instant.
    Pending(Instant),
}

#[derive(Default)]
struct State {
    sessions: HashMap<TokenDigest, Session>,
    repos: HashMap<PathBuf, RepoState>,
    warned_full: bool,
}

/// One daemon's relay: the receiver's address plus the live-session registry.
///
/// Deliberately not `Debug`-derived with its contents: nothing here is a
/// token, but the type is the one place that could grow one.
pub struct Relay {
    addr: SocketAddr,
    host_id: String,
    /// The runtime repo-slug lookups are spawned on. `None` means slugs
    /// arrive only through [`Relay::note_repo`].
    runtime: Option<tokio::runtime::Handle>,
    lookup: RepoLookup,
    state: Mutex<State>,
}

/// Resolves a workspace root (its display string) to a forge `owner/name`.
pub type RepoLookup = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync,
>;

/// The daemon's own resolver: the collector's bounded `gh` lookup (or the
/// repo-facts record when that is on).
fn collector_lookup() -> RepoLookup {
    Arc::new(|root: String| {
        Box::pin(async move {
            let mut cache = HashMap::new();
            super::collector::resolve_repo_slug_cached(&mut cache, &root).await
        })
    })
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay")
            .field("addr", &self.addr)
            .field("sessions", &self.live_sessions())
            .finish_non_exhaustive()
    }
}

impl Relay {
    /// A relay whose receiver listens on `addr`. Refuses any non-loopback
    /// address: the token is the only thing between a peer and a session's
    /// attribution, so the receiver is never reachable off-host.
    pub fn new(
        addr: SocketAddr,
        host_id: impl Into<String>,
        runtime: Option<tokio::runtime::Handle>,
    ) -> Option<Arc<Self>> {
        Self::with_lookup(addr, host_id, runtime, collector_lookup())
    }

    /// [`Relay::new`] with the forge-slug resolver supplied.
    pub fn with_lookup(
        addr: SocketAddr,
        host_id: impl Into<String>,
        runtime: Option<tokio::runtime::Handle>,
        lookup: RepoLookup,
    ) -> Option<Arc<Self>> {
        if !addr.ip().is_loopback() {
            log::error!("agent-relay: refusing a non-loopback receiver address");
            return None;
        }
        Some(Arc::new(Relay {
            addr,
            host_id: host_id.into(),
            runtime,
            lookup,
            state: Mutex::new(State::default()),
        }))
    }

    /// The receiver's address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The OTLP base URL handed to a wired child.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Sessions currently registered (expired ones included until looked up).
    #[must_use]
    pub fn live_sessions(&self) -> usize {
        self.lock().sessions.len()
    }

    /// Record `root`'s forge slug.
    pub fn note_repo(&self, root: &Path, slug: impl Into<String>) {
        self.lock()
            .repos
            .insert(root.to_path_buf(), RepoState::Known(slug.into()));
    }

    /// Start resolving `root`'s forge slug unless it is known, in flight, or
    /// failed recently. Never blocks: the lookup shells out to `gh`.
    fn resolve_repo(self: &Arc<Self>, root: &Path) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        {
            let mut state = self.lock();
            match state.repos.get(root) {
                Some(RepoState::Known(_)) => return,
                Some(RepoState::Pending(since)) if since.elapsed() < REPO_RETRY_AFTER => return,
                _ => {}
            }
            state
                .repos
                .insert(root.to_path_buf(), RepoState::Pending(Instant::now()));
        }
        let relay = Arc::downgrade(self);
        let root = root.to_path_buf();
        let lookup = (self.lookup)(root.display().to_string());
        runtime.spawn(async move {
            let slug = lookup.await;
            if let (Some(relay), Some(slug)) = (relay.upgrade(), slug) {
                relay.note_repo(&root, slug);
            }
        });
    }

    /// Register `identity` and return its freshly minted token, or `None` at
    /// the session ceiling.
    fn register(self: &Arc<Self>, identity: SessionIdentity) -> Option<String> {
        let token = mint_token();
        let root = identity.workspace_root.clone();
        {
            let mut state = self.lock();
            state
                .sessions
                .retain(|_, session| session.registered_at.elapsed() < MAX_SESSION_LIFETIME);
            if state.sessions.len() >= MAX_SESSIONS {
                if !state.warned_full {
                    state.warned_full = true;
                    log::warn!(
                        "agent-relay: {MAX_SESSIONS} sessions registered (the maximum); further \
                         launches are not wired until one ends"
                    );
                }
                return None;
            }
            state.sessions.insert(
                digest(&token),
                Session {
                    identity,
                    registered_at: Instant::now(),
                },
            );
        }
        self.resolve_repo(&root);
        Some(token)
    }

    /// The session `token` was minted for, or `None` when it is unknown,
    /// ended or expired. The only input is the token: this is the whole of
    /// the receiver's attribution decision.
    #[must_use]
    pub fn authorize(self: &Arc<Self>, token: &str) -> Option<BoundIdentity> {
        let key = digest(token);
        let (identity, repo) = {
            let mut state = self.lock();
            let session = state.sessions.get(&key)?;
            if session.registered_at.elapsed() >= MAX_SESSION_LIFETIME {
                state.sessions.remove(&key);
                return None;
            }
            let identity = session.identity.clone();
            let repo = match state.repos.get(&identity.workspace_root) {
                Some(RepoState::Known(slug)) => Some(slug.clone()),
                _ => None,
            };
            (identity, repo)
        };
        if repo.is_none() {
            self.resolve_repo(&identity.workspace_root);
        }
        Some(BoundIdentity {
            identity,
            host_id: self.host_id.clone(),
            repo,
        })
    }

    /// End every session launched under `execution` (a sweep id). Returns how
    /// many were ended.
    pub fn end_execution(&self, execution: &str) -> usize {
        let mut state = self.lock();
        let before = state.sessions.len();
        state.sessions.retain(|_, session| {
            session.identity.kind != SessionKind::Sweep
                || session.identity.sweep_id.as_deref() != Some(execution)
        });
        before - state.sessions.len()
    }

    fn end_token(&self, key: &TokenDigest) {
        self.lock().sessions.remove(key);
    }

    /// Wire `command` to this relay as `identity`, when `root` opted in and
    /// the launch is one the receiver can be reached from. Returns the
    /// session's lease; on `None` the command is untouched.
    ///
    /// After this call `command` carries the session token in its
    /// environment, and [`Command`]'s `Debug` output prints environment
    /// edits: never format a prepared command into a log line or an error.
    pub fn prepare(
        self: &Arc<Self>,
        command: &mut Command,
        root: &Path,
        identity: SessionIdentity,
    ) -> Option<SessionLease> {
        if !enabled(root) {
            return None;
        }
        if containerized(command, root) {
            // A container's loopback is its own: the receiver is unreachable
            // from inside one, and it is never bound to anything wider.
            log::debug!(
                "agent-relay: containerized dispatch is not wired (loopback-only receiver)"
            );
            return None;
        }
        let token = self.register(identity)?;
        for name in RELAY_CLEARED_ENV {
            command.env_remove(name);
        }
        command
            .env("CLAUDE_CODE_ENABLE_TELEMETRY", "1")
            .env("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA", "1")
            .env("OTEL_METRICS_EXPORTER", "otlp")
            .env("OTEL_LOGS_EXPORTER", "otlp")
            .env("OTEL_TRACES_EXPORTER", "otlp")
            .env("OTEL_EXPORTER_OTLP_PROTOCOL", RELAY_PROTOCOL)
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", self.endpoint())
            .env(RELAY_HEADERS_ENV, format!("Authorization=Bearer {token}"));
        Some(SessionLease {
            relay: Arc::downgrade(self),
            key: digest(&token),
            detached: false,
        })
    }
}

/// 244 bits from the OS random source, as 64 hex characters. Two v4 UUIDs
/// rather than a new dependency: `uuid` already draws from `getrandom`.
fn mint_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

/// Whether this launch will run inside a container, by the same two inputs
/// `spawn-claude.sh` reads: `LOOM_SWEEP_CONTAINERIZED` (on the child's own
/// environment first, then the daemon's) over `runtimes.containment.enabled`.
fn containerized(command: &Command, root: &Path) -> bool {
    const ENV: &str = "LOOM_SWEEP_CONTAINERIZED";
    let truthy = |value: &str| matches!(value.trim(), "1" | "true" | "yes");
    let on_command = command
        .get_envs()
        .find(|(name, _)| *name == std::ffi::OsStr::new(ENV))
        .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()));
    let from_env = match on_command {
        Some(explicit) => explicit,
        None => std::env::var(ENV).ok(),
    };
    if let Some(value) = from_env.filter(|v| !v.trim().is_empty()) {
        return truthy(&value);
    }
    let config = crate::config_resolver::resolve_effective_config(root);
    match crate::config_resolver::get_path(&config, "runtimes.containment.enabled") {
        Some(serde_json::Value::Bool(on)) => *on,
        Some(serde_json::Value::String(value)) => truthy(value),
        _ => false,
    }
}

/// A wired session's registration. Dropping it ends the session — its token
/// stops being honoured — unless [`SessionLease::until_execution_ends`] handed
/// that to [`end_execution`].
#[must_use = "dropping the lease ends the session's relay access"]
pub struct SessionLease {
    relay: Weak<Relay>,
    key: TokenDigest,
    detached: bool,
}

impl SessionLease {
    /// Keep the session registered past this lease: it ends when
    /// [`end_execution`] is called with its sweep id, or at
    /// [`MAX_SESSION_LIFETIME`]. For a sweep child, whose exit is observed by
    /// the reaper long after the dispatch call that launched it returned.
    pub fn until_execution_ends(mut self) {
        self.detached = true;
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        if let Some(relay) = self.relay.upgrade() {
            relay.end_token(&self.key);
        }
    }
}

static GLOBAL: OnceLock<Arc<Relay>> = OnceLock::new();

/// Register the running receiver's relay as the process-global. First
/// registration wins (one receiver per daemon process).
pub fn register_global(relay: Arc<Relay>) {
    let _ = GLOBAL.set(relay);
}

/// The running receiver's relay, or `None` when none started.
#[must_use]
pub fn global() -> Option<&'static Arc<Relay>> {
    GLOBAL.get()
}

/// Wire a dispatched sweep child. A no-op — the command is untouched — unless
/// a receiver is running, `root` opted in, and `runtime` is an admitted,
/// wired harness. The session ends at [`end_execution`]`(sweep_id)`.
pub fn prepare_sweep_child(
    command: &mut Command,
    root: &Path,
    runtime: Option<&str>,
    issue: Option<u32>,
    sweep_id: &str,
) -> bool {
    prepare_sweep_child_on(global(), command, root, runtime, issue, sweep_id)
}

/// [`prepare_sweep_child`] against an explicit relay (`None`: no receiver).
fn prepare_sweep_child_on(
    relay: Option<&Arc<Relay>>,
    command: &mut Command,
    root: &Path,
    runtime: Option<&str>,
    issue: Option<u32>,
    sweep_id: &str,
) -> bool {
    let (Some(relay), Some(harness)) = (relay, Harness::from_admitted_runtime(runtime)) else {
        return false;
    };
    let identity = SessionIdentity {
        harness,
        kind: SessionKind::Sweep,
        role: None,
        issue,
        sweep_id: Some(sweep_id.to_string()),
        workspace_root: root.to_path_buf(),
    };
    match relay.prepare(command, root, identity) {
        Some(lease) => {
            lease.until_execution_ends();
            true
        }
        None => false,
    }
}

/// Wire a scheduled role tick's child. Same no-op conditions as
/// [`prepare_sweep_child`]; the session lives exactly as long as the returned
/// lease, which the (blocking) launch holds until its child has exited.
pub fn prepare_role_child(
    command: &mut Command,
    root: &Path,
    runtime: Option<&str>,
    role: &str,
    execution: Option<&str>,
) -> Option<SessionLease> {
    prepare_role_child_on(global(), command, root, runtime, role, execution)
}

/// [`prepare_role_child`] against an explicit relay (`None`: no receiver).
fn prepare_role_child_on(
    relay: Option<&Arc<Relay>>,
    command: &mut Command,
    root: &Path,
    runtime: Option<&str>,
    role: &str,
    execution: Option<&str>,
) -> Option<SessionLease> {
    let harness = Harness::from_admitted_runtime(runtime)?;
    let identity = SessionIdentity {
        harness,
        kind: SessionKind::Role,
        role: Some(role.to_string()),
        issue: None,
        sweep_id: execution.map(str::to_string),
        workspace_root: root.to_path_buf(),
    };
    relay?.prepare(command, root, identity)
}

/// A sweep's execution ended (its child exited, or the reaper closed it):
/// stop honouring its token. A no-op without a running receiver.
pub fn end_execution(execution: &str) {
    if let Some(relay) = global() {
        relay.end_execution(execution);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "agent_relay/tests.rs"]
mod tests;
