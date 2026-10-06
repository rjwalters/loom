//! The one `gh` spawn choke point (#9985, part of the forge egress policy
//! epic #9983, 2am#1911 P3).
//!
//! # Why
//!
//! `loom-daemon` grew ten hand-rolled `gh_bin*` resolver functions and well over a
//! hundred raw `Command::new(gh…)` sites, each deciding for itself whether to
//! honour `LOOM_GH_BIN`, whether to scope `GH_CONFIG_DIR`, whether to export a
//! trace context. Nobody can answer "does every daemon `gh` call route the same
//! way?" by reading the code. This module is the place that answer will live.
//!
//! # What lands in slice 1
//!
//! - [`resolver`] — the single executable resolver (policy launcher →
//!   `LOOM_GH_BIN` → `PATH`). `forge_cmd::gh_bin` re-exports it.
//! - [`GhInvocation`] — the facade every site will build through: typed
//!   [`GhTarget`], [`AccessIntent`], a stable [`Operation`] name, a
//!   [`ParentContext`], and an [`OutputContract`]. It **owns execution**
//!   ([`GhInvocation::execute`]); there is deliberately no accessor returning
//!   the underlying `Command`, so a migrated site cannot route around the
//!   environment this facade assembles.
//! - `tests/gh_spawn_choke_point.rs` — the CI scan whose checked-in allowlist
//!   of today's raw sites may only shrink.
//!
//! - [`telemetry`] (slice 3) — one `invoke github` span per execution and a
//!   local completion record for every non-`ok` [`telemetry::Outcome`].
//! - [`GhInvocation::run`] (slice 3) — the `cmd_out::CmdOutcome` bridge, so
//!   a migrated `run_command` site keeps its exact result classification.
//!
//! - [`accounting`] (#10089) — every execution that reached `gh` is one row
//!   in [`crate::forge_call_stats`], keyed by its [`Operation`], so
//!   `loom-daemon status` and the breaker's own-versus-external line count it.
//!
//! - [`reader_route`] (#9872) — a captured, repo-scoped [`AccessIntent::Read`]
//!   runs under the repo's reader App when one is configured and fresh, with
//!   one writer retry on a credential failure; every row records the identity
//!   role (`reader` / `writer` / `writer-fallback`).
//!
//! `gh-cached` substitution for reads, the async/tokio variant and the Gitea
//! decline move in with the slices that first need them (see #9985's slicing
//! plan).

pub mod accounting;
mod affinity;
pub mod api_kind;
pub(crate) mod cwd_route;
mod outcome;
mod reader_route;
pub mod resolver;
pub mod telemetry;
pub mod transparent;

pub use crate::forge_identity::ReadClass;
pub use affinity::{affinity_key, url_affinity_key};
pub use reader_route::{READ_SHED_ENV, SHED_MARKER};

#[cfg(test)]
mod tests;

use crate::proc_exec::{self, Completion, ExecError};
use crate::telemetry::trace::store::{TRACEPARENT_ENV, W3C_TRACEPARENT_ENV};
use crate::telemetry::trace::TraceContext;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

pub use resolver::{gh_bin, GhBinSource, ResolvedGh};

/// Upper bound on [`GhTarget::bounded`]'s rendering, so a malformed slug can
/// never blow up a span attribute.
pub const TARGET_ATTR_MAX: usize = 100;

/// Whether an invocation reads or mutates forge state.
///
/// Carried so the egress policy (C1) and the managed launcher (C4) can apply
/// different routing to writes, and so telemetry can separate them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessIntent {
    Read,
    Write,
}

impl AccessIntent {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AccessIntent::Read => "read",
            AccessIntent::Write => "write",
        }
    }
}

/// A stable, low-cardinality operation name (`"issue.list"`, `"api.rest"`,
/// `"api.graphql"`, …) — the `github.operation` attribute.
///
/// `&'static str` on purpose: an operation name built from runtime data (an
/// issue number, a path) would make the attribute unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation(&'static str);

impl Operation {
    /// # Panics
    ///
    /// In debug builds, when `name` is not a valid operation name (see
    /// [`Operation::is_valid`]). Names are compile-time literals, so this
    /// fires in the first test that exercises the site.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        debug_assert!(Self::is_valid(name), "invalid gh operation name: {name:?}");
        Self(name)
    }

    /// Lowercase ASCII segments of `[a-z0-9_]` separated by single dots, the
    /// first segment starting with a letter.
    #[must_use]
    pub fn is_valid(name: &str) -> bool {
        !name.is_empty()
            && name.split('.').all(|seg| {
                !seg.is_empty()
                    && seg
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            && name.as_bytes()[0].is_ascii_lowercase()
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

/// The repository an invocation acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhTarget {
    /// Not repository-scoped (`gh api rate_limit`, `gh auth status`), or the
    /// repository is resolved by `gh` from the working directory's remote.
    None,
    /// An explicit `owner/repo`.
    Repo { owner: String, repo: String },
}

impl GhTarget {
    /// Parse an `owner/repo` slug.
    ///
    /// # Errors
    ///
    /// When the slug is not exactly two non-empty, whitespace-free segments.
    /// The error never echoes the input.
    pub fn repo(slug: &str) -> Result<Self, String> {
        let mut parts = slug.split('/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(owner), Some(repo), None)
                if !owner.is_empty()
                    && !repo.is_empty()
                    && !slug.chars().any(char::is_whitespace) =>
            {
                Ok(GhTarget::Repo {
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                })
            }
            _ => Err("gh target must be an `owner/repo` slug".to_string()),
        }
    }

    /// The `owner/repo` slug, when there is one.
    #[must_use]
    pub fn slug(&self) -> Option<String> {
        match self {
            GhTarget::None => None,
            GhTarget::Repo { owner, repo } => Some(format!("{owner}/{repo}")),
        }
    }

    /// The `github.target` attribute value, capped at [`TARGET_ATTR_MAX`]
    /// characters (`"none"` when not repository-scoped).
    #[must_use]
    pub fn bounded(&self) -> String {
        self.slug()
            .map_or_else(|| "none".to_string(), |s| s.chars().take(TARGET_ATTR_MAX).collect())
    }
}

/// The caller's trace context, handed to the child so the managed launcher
/// (C4) can inject the HTTP `traceparent` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentContext {
    /// The invocation runs inside a traced execution (a sweep, a role run).
    Parent(TraceContext),
    /// No parent: an ad-hoc daemon tick. Recorded as `context_source=missing`;
    /// a third-party `TRACEPARENT` in the environment is never inherited.
    Missing,
}

impl ParentContext {
    /// The process's ambient execution context: the validated
    /// `LOOM_TRACEPARENT` a traced sweep exports to its children (so a CLI
    /// run inside a sweep parents its invocations to that sweep's trace),
    /// else [`ParentContext::Missing`]. The daemon itself carries none, so
    /// its concurrent invocations never share an ambient parent — a daemon
    /// seam that has one passes it explicitly with [`GhInvocation::parent`].
    #[must_use]
    pub fn ambient() -> Self {
        telemetry::ambient_parent().map_or(ParentContext::Missing, ParentContext::Parent)
    }

    /// The `context_source` attribute value.
    #[must_use]
    pub fn source(&self) -> &'static str {
        match self {
            ParentContext::Parent(_) => "parent",
            ParentContext::Missing => "missing",
        }
    }
}

/// How the child's output is handled. Interactive passthrough is never forced
/// into captured output, nor the reverse (2am#1911 "preserve domain-level
/// traits").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputContract {
    /// Run under [`proc_exec::run_bounded`]: stdout/stderr captured and drained
    /// concurrently, the process group killed at `timeout`.
    Captured { timeout: Duration },
    /// Inherit stdio and wait (the `forge_cmd::gh_passthrough` shape).
    Passthrough,
    /// A git credential helper (`gh auth git-credential …`): stdin fed from
    /// the buffer given to [`GhInvocation::credential_helper`], stdout
    /// inherited (it is git's answer), stderr discarded, no deadline.
    /// Completes as [`GhCompletion::Passthrough`]; the credential never
    /// passes through the facade's captured output, telemetry or accounting.
    CredentialHelper,
}

/// What [`GhInvocation::execute`] observed, per [`OutputContract`].
#[derive(Debug)]
pub enum GhCompletion {
    /// A [`OutputContract::Captured`] run; exit vs timeout stay distinct.
    Captured(Completion),
    /// A [`OutputContract::Passthrough`] or
    /// [`OutputContract::CredentialHelper`] run's exit status.
    Passthrough(ExitStatus),
    /// Not run (W4-C): a [`ReadClass::Hygiene`] or
    /// [`ReadClass::Observability`] read whose readers for `owner`'s
    /// `resource` were all withdrawn, deferred instead of spending the
    /// writer's bucket. No request was sent. Classified as
    /// [`crate::cmd_out::Unavailable::Shed`] — "no answer", never a
    /// negative one.
    Shed {
        owner: String,
        resource: crate::forge_bucket_book::Resource,
        until: std::time::SystemTime,
    },
}

/// The environment variables `gh` documents as taking precedence over a
/// `GH_CONFIG_DIR`'s stored credential. [`GhInvocation::without_token_env`]
/// removes every one, so a reader-only call cannot silently spend a token the
/// daemon's own environment happens to carry (#10263).
pub const TOKEN_ENV_VARS: [&str; 4] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

/// One environment change the facade applies to the child: `Some` sets the
/// variable, `None` removes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvEntry {
    pub key: &'static str,
    pub value: Option<OsString>,
}

/// A single `gh` invocation, built and executed through the facade.
#[derive(Debug, Clone)]
pub struct GhInvocation {
    operation: Operation,
    intent: AccessIntent,
    target: GhTarget,
    parent: ParentContext,
    contract: OutputContract,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    program: Option<String>,
    config_dir: Option<PathBuf>,
    /// An explicit child `PATH` ([`GhInvocation::child_path`]).
    path: Option<OsString>,
    /// Remove [`TOKEN_ENV_VARS`] from the child (#10263).
    strip_token_env: bool,
    /// Further variables removed from the child, applied last
    /// ([`GhInvocation::strip_env`]).
    stripped_env: Vec<&'static str>,
    /// The [`OutputContract::CredentialHelper`] request written to stdin.
    stdin_input: Vec<u8>,
    /// Never route this read to a reader App (#9872).
    writer_only: bool,
    /// The identity role the accounting row records; `None` = writer.
    role: Option<crate::forge_identity::IdentityRole>,
    /// The #9777 call identity this execution is accounted under (#9831).
    /// Empty by default: an unmapped site records `operation = "unknown"`
    /// (see [`accounting`]), visible rather than absent.
    identity: crate::forge_call_stats::CallIdentity,
    /// The repository the facade derived for an untargeted read (W4-C,
    /// [`cwd_route`]). It steers only the **reader** attempt (its slug, its
    /// `GH_REPO`) and the accounting row; `target` is never changed, so the
    /// writer attempt's environment is exactly the pre-W4-C one.
    route_slug: Option<String>,
    /// How this read may be treated when its readers run dry (W4-C).
    /// [`ReadClass::Gate`] — the default — is never shed.
    read_class: ReadClass,
    /// The derivation already counted this call's local repo disagreement
    /// (`facade.cwd_route.disagree`), so its accounting row must not count
    /// it again (one counter, one count per call).
    disagree_counted: bool,
}

impl GhInvocation {
    /// A new invocation. Defaults: the ambient parent context
    /// ([`ParentContext::ambient`]), no working directory (the daemon's own),
    /// captured output with `timeout`.
    #[must_use]
    pub fn new(
        operation: Operation,
        intent: AccessIntent,
        target: GhTarget,
        timeout: Duration,
    ) -> Self {
        Self {
            operation,
            intent,
            target,
            parent: ParentContext::ambient(),
            contract: OutputContract::Captured { timeout },
            args: Vec::new(),
            cwd: None,
            program: None,
            config_dir: None,
            path: None,
            strip_token_env: false,
            stripped_env: Vec::new(),
            stdin_input: Vec::new(),
            writer_only: false,
            role: None,
            identity: crate::forge_call_stats::CallIdentity::default(),
            route_slug: None,
            read_class: ReadClass::Gate,
            disagree_counted: false,
        }
    }

    /// Account this execution under an inventoried forge operation (#9831).
    /// [`Operation`] is the low-cardinality *telemetry* name; this is the
    /// inventory row (`defaults/forge/operations/*.toml`) the call serves.
    #[must_use]
    pub fn forge_op(mut self, op: crate::forge_call_stats::ForgeOp) -> Self {
        self.identity.operation = crate::forge_call_stats::CallIdentity::for_op(op).operation;
        self
    }

    /// The origin host and `owner/repo` the call acts on, for accounting only
    /// — they change nothing about how the child runs. For a site whose
    /// [`GhTarget`] is deliberately [`GhTarget::None`] (the credential must
    /// stay the working directory's) but which still knows its repository.
    /// `origin` and `repo` stay separate fields: merging them is exactly what
    /// lets two forges sharing one slug collapse into one row.
    #[must_use]
    pub fn identity_scope(mut self, origin: Option<&str>, repo: Option<&str>) -> Self {
        let id = std::mem::take(&mut self.identity);
        let id = match origin {
            Some(o) => id.with_origin(o),
            None => id,
        };
        self.identity = match repo {
            Some(r) => id.with_repo(r),
            None => id,
        };
        self
    }

    /// Append `gh` arguments (the subcommand onward; never the program).
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_os_string()));
        self
    }

    /// Append one `gh` argument; see [`GhInvocation::args`].
    #[must_use]
    pub fn arg(self, arg: impl AsRef<OsStr>) -> Self {
        self.args([arg])
    }

    /// Run in `dir`. Also keys the cross-owner `GH_CONFIG_DIR` lookup.
    #[must_use]
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Pin the executable to a caller-injected program — the seam for sites
    /// that hand a `gh_bin: &Path` down their call chain so tests can pass a
    /// stub. A bare `gh` is "not injected": it goes through the [`resolver`]
    /// like every other invocation, so production keeps the policy →
    /// `LOOM_GH_BIN` → `PATH` ladder.
    #[must_use]
    pub fn program(mut self, program: impl AsRef<OsStr>) -> Self {
        let program = program.as_ref().to_string_lossy();
        self.program = (program != "gh").then(|| program.into_owned());
        self
    }

    /// Run under an explicit `GH_CONFIG_DIR` — a credential the caller chose
    /// itself (a repo's reader App, a store's writer App, #9537) — instead of
    /// the facade's working-directory / owner lookup. `None` keeps the lookup.
    #[must_use]
    pub fn gh_config_dir(mut self, dir: Option<&Path>) -> Self {
        self.config_dir = dir.map(Path::to_path_buf);
        self
    }

    /// Give the child an explicit `PATH` (#10089) — for a site that cannot
    /// rely on the daemon's inherited one (`fleet::drain`'s claim resets under
    /// launchd/systemd, #4831). It also steers the lookup of a bare `gh`
    /// program, exactly as `Command::env("PATH", …)` did at the raw site.
    #[must_use]
    pub fn child_path(mut self, path: impl Into<OsString>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Remove every [`TOKEN_ENV_VARS`] entry from the child's environment, so
    /// the `GH_CONFIG_DIR` this invocation runs under is the **only**
    /// credential `gh` can see. For a reader-only read (#10263): `gh` prefers
    /// an env token over a stored one, so without this a daemon started from
    /// a shell holding the operator's PAT would spend that PAT on a call the
    /// caller believes runs as a reader App.
    #[must_use]
    pub fn without_token_env(mut self) -> Self {
        self.strip_token_env = true;
        self
    }

    /// Remove `key` from the child's environment, after everything else the
    /// facade sets — so `strip_env("GH_REPO")` wins over the `LOOM_REPO`
    /// mapping. For a site that must reproduce a `gh` command which itself
    /// ignores the variable (`gh repo view` and `GH_REPO`), so the child sees
    /// exactly what that command would have resolved from.
    #[must_use]
    pub fn strip_env(mut self, key: &'static str) -> Self {
        self.stripped_env.push(key);
        self
    }

    /// Keep this read on the writer credential (#9872): for a read whose
    /// answer depends on **who** asks — a permission or write-scope probe,
    /// `viewer`, `/user` — which a reader App would answer for itself.
    #[must_use]
    pub fn writer_identity(mut self) -> Self {
        self.writer_only = true;
        self
    }

    /// Classify this read for reader exhaustion (W4-C). The default,
    /// [`ReadClass::Gate`], keeps today's writer fallback; a
    /// [`ReadClass::Hygiene`] or [`ReadClass::Observability`] read is shed
    /// ([`GhCompletion::Shed`]) instead of spending the writer's bucket when
    /// every reader for its owner and resource is withdrawn. Mark a read
    /// non-`Gate` only when its consumer maps "no answer" to skip / unknown
    /// and nothing it decides (a dispatch, claim, merge, reap, label flip)
    /// rests on it.
    #[must_use]
    pub fn read_class(mut self, class: ReadClass) -> Self {
        self.read_class = class;
        self
    }

    /// This read's [`ReadClass`].
    #[must_use]
    pub fn class(&self) -> ReadClass {
        self.read_class
    }

    /// Record this execution under `role` (#9872) — for a caller that routes
    /// its own reads (`forge_etag_store`, `ci_telemetry`). The choke point's
    /// own routing sets it itself.
    #[must_use]
    pub fn identity_role(mut self, role: crate::forge_identity::IdentityRole) -> Self {
        self.role = Some(role);
        self
    }

    /// Record the reader rate-limit bucket this execution spent (#10232), for
    /// a caller that routes its own reads; see
    /// [`crate::forge_identity::reader_bucket`].
    #[must_use]
    pub fn identity_bucket(mut self, bucket: &str) -> Self {
        self.identity = std::mem::take(&mut self.identity).with_bucket(bucket);
        self
    }

    #[must_use]
    pub fn parent(mut self, parent: ParentContext) -> Self {
        self.parent = parent;
        self
    }

    /// Switch to interactive passthrough (inherited stdio, no deadline).
    #[must_use]
    pub fn passthrough(mut self) -> Self {
        self.contract = OutputContract::Passthrough;
        self
    }

    /// Run as a git credential helper ([`OutputContract::CredentialHelper`]),
    /// writing `request` to the child's stdin.
    #[must_use]
    pub fn credential_helper(mut self, request: impl Into<Vec<u8>>) -> Self {
        self.contract = OutputContract::CredentialHelper;
        self.stdin_input = request.into();
        self
    }

    #[must_use]
    pub fn operation(&self) -> Operation {
        self.operation
    }

    #[must_use]
    pub fn intent(&self) -> AccessIntent {
        self.intent
    }

    #[must_use]
    pub fn target(&self) -> &GhTarget {
        &self.target
    }

    #[must_use]
    pub fn context_source(&self) -> &'static str {
        self.parent.source()
    }

    #[must_use]
    pub fn contract(&self) -> OutputContract {
        self.contract
    }

    /// The environment the child receives, in application order, when its
    /// `traceparent` is `child_context` (see [`telemetry::InvocationSpan`]).
    ///
    /// - `GH_CONFIG_DIR`: the explicit [`GhInvocation::gh_config_dir`], else
    ///   the cross-owner credential registered for the
    ///   working directory, else for the target's owner (the
    ///   `credential_preflight::apply_gh_config_for_{root,owner_slug}`
    ///   lookups). Absent ⇒ the child inherits the process-global value.
    /// - `GH_REPO`: the typed target, else — on the **reader** attempt of a
    ///   derived route only (W4-C, [`cwd_route`]) — the derived repo, else
    ///   the machine-global `LOOM_REPO` override
    ///   (`gh_repo_env::apply_loom_repo_override`'s contract). A writer
    ///   attempt never sees the derived repo.
    /// - `LOOM_TRACEPARENT` / `TRACEPARENT`: set together from
    ///   `child_context` — the invocation's own span when it is exported, so
    ///   the managed launcher (C4) parents its HTTP spans under it — or both
    ///   removed when there is none.
    /// - [`TOKEN_ENV_VARS`]: removed, only under
    ///   [`GhInvocation::without_token_env`].
    /// - Each [`GhInvocation::strip_env`] key: removed, last.
    #[must_use]
    pub fn env_plan(&self, child_context: Option<&TraceContext>) -> Vec<EnvEntry> {
        self.env_plan_with(std::env::var_os("LOOM_REPO"), child_context)
    }

    fn env_plan_with(
        &self,
        loom_repo: Option<OsString>,
        child_context: Option<&TraceContext>,
    ) -> Vec<EnvEntry> {
        let mut plan = Vec::new();
        let slug = self.target.slug();
        let config_dir = self
            .config_dir
            .clone()
            .or_else(|| {
                self.cwd
                    .as_deref()
                    .and_then(crate::credential_preflight::gh_config_dir_for_root)
            })
            .or_else(|| {
                slug.as_deref()
                    .and_then(crate::credential_preflight::gh_config_dir_for_owner_slug)
            });
        if let Some(dir) = config_dir {
            plan.push(EnvEntry {
                key: "GH_CONFIG_DIR",
                value: Some(dir.into_os_string()),
            });
        }
        // W4-C: only the reader attempt of a derived route names its repo;
        // every writer attempt keeps the `LOOM_REPO` mapping exactly.
        let reader_route = self
            .route_slug
            .as_ref()
            .filter(|_| self.role == Some(crate::forge_identity::IdentityRole::Reader))
            .map(OsString::from);
        if let Some(repo) = slug.map(OsString::from).or(reader_route).or(loom_repo) {
            plan.push(EnvEntry {
                key: "GH_REPO",
                value: Some(repo),
            });
        }
        let traceparent = child_context.map(|ctx| OsString::from(ctx.traceparent()));
        for key in [TRACEPARENT_ENV, W3C_TRACEPARENT_ENV] {
            plan.push(EnvEntry {
                key,
                value: traceparent.clone(),
            });
        }
        if self.strip_token_env {
            for key in TOKEN_ENV_VARS {
                plan.push(EnvEntry { key, value: None });
            }
        }
        if let Some(path) = &self.path {
            plan.push(EnvEntry {
                key: "PATH",
                value: Some(path.clone()),
            });
        }
        for key in &self.stripped_env {
            plan.push(EnvEntry { key, value: None });
        }
        plan
    }

    /// Assemble the child. Private: the facade owns execution.
    fn command(&self, program: &str, child_context: Option<&TraceContext>) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(&self.args);
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        for entry in self.env_plan(child_context) {
            match entry.value {
                Some(value) => cmd.env(entry.key, value),
                None => cmd.env_remove(entry.key),
            };
        }
        cmd
    }

    /// Resolve the executable ([`resolver::resolve`]) and run the invocation
    /// under its [`OutputContract`], recording one `invoke github` span (and,
    /// unless it succeeded, a local completion record — [`telemetry`]).
    ///
    /// An eligible read runs under the repo's reader App first and is retried
    /// once on the writer after a credential failure ([`reader_route`]); each
    /// attempt is its own span and accounting row.
    ///
    /// # Errors
    ///
    /// [`ExecError::Spawn`] when `gh` could not be started;
    /// [`ExecError::Collect`] when it started but its result could not be
    /// collected (side effects may have happened — never retry a write on it).
    pub fn execute(self) -> Result<GhCompletion, ExecError> {
        #[cfg(test)]
        if let Some(routed) = test_routing::run(&self) {
            return routed;
        }
        let lookup = |req: &crate::forge_identity::RouteRequest<'_>| {
            crate::forge_identity::route_read(req, std::time::SystemTime::now())
        };
        // `LOOM_READ_ROUTING=legacy`: the pre-W4-C path exactly — no
        // derivation, no read class, the unconditional writer fallback.
        if crate::forge_identity::RoutingMode::current()
            == crate::forge_identity::RoutingMode::Legacy
        {
            return self.execute_routed(&lookup, &reader_route::withdraw_reader);
        }
        let policy = reader_route::ShedPolicy::current();
        self.with_derived_route()
            .execute_routed_v2(&lookup, &reader_route::withdraw_reader, policy)
    }

    /// Run exactly once under the credential already chosen (no routing).
    fn execute_direct(self) -> Result<GhCompletion, ExecError> {
        if let Some(program) = self.program.clone() {
            return self.execute_with(&program, GhBinSource::Injected);
        }
        let resolved = resolver::resolve();
        self.execute_with(&resolved.program, resolved.source)
    }

    fn execute_with(self, program: &str, source: GhBinSource) -> Result<GhCompletion, ExecError> {
        let span = telemetry::InvocationSpan::open(&self);
        let mut cmd = self.command(program, span.child_context(&self.parent));
        match self.contract {
            OutputContract::Captured { timeout } => {
                cmd.stdin(Stdio::null());
                let result = proc_exec::run_bounded(cmd, timeout);
                let (outcome, code) = telemetry::classify_captured(&result);
                let captured = match &result {
                    Ok(Completion::Exited(out)) => Some((&out.stdout[..], &out.stderr[..])),
                    Ok(Completion::TimedOut { stdout, stderr }) => Some((&stdout[..], &stderr[..])),
                    Err(_) => None,
                };
                accounting::record(&self, outcome, captured);
                span.finish(&self, source, outcome, code);
                result.map(GhCompletion::Captured)
            }
            OutputContract::Passthrough => {
                cmd.stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
                let result = cmd
                    .spawn()
                    .map_err(ExecError::Spawn)
                    .and_then(|mut child| child.wait().map_err(ExecError::Collect));
                let (outcome, code) = telemetry::classify_passthrough(&result);
                accounting::record(&self, outcome, None);
                span.finish(&self, source, outcome, code);
                result.map(GhCompletion::Passthrough)
            }
            OutputContract::CredentialHelper => {
                cmd.stdin(Stdio::piped())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::null());
                let result = cmd.spawn().map_err(ExecError::Spawn).and_then(|mut child| {
                    // Always reap the child, even when the request could not
                    // be written (it exited early, closing the pipe).
                    let written = child.stdin.take().map_or(Ok(()), |mut stdin| {
                        std::io::Write::write_all(&mut stdin, &self.stdin_input)
                    });
                    let status = child.wait().map_err(ExecError::Collect)?;
                    written.map_err(ExecError::Collect).map(|()| status)
                });
                let (outcome, code) = telemetry::classify_passthrough(&result);
                accounting::record(&self, outcome, None);
                span.finish(&self, source, outcome, code);
                result.map(GhCompletion::Passthrough)
            }
        }
    }
}

#[cfg(test)]
#[path = "test_routing.rs"]
pub(crate) mod test_routing;

#[cfg(test)]
#[path = "w4c_sites_tests.rs"]
mod w4c_sites_tests;

#[cfg(test)]
#[path = "migrated_sites_tests.rs"]
mod migrated_sites_tests;

#[cfg(test)]
#[path = "rest_readers_tests.rs"]
mod rest_readers_tests;

#[cfg(test)]
#[path = "migrated_sites_tests_b.rs"]
mod migrated_sites_tests_b;

#[cfg(test)]
#[path = "migrated_sites_tests_c.rs"]
mod migrated_sites_tests_c;

#[cfg(test)]
#[path = "migrated_sites_tests_d.rs"]
mod migrated_sites_tests_d;
