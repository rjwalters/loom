//! The one `gh` spawn choke point (#9985, part of the forge egress policy
//! epic #9983, 2am#1911 P3).
//!
//! # Why
//!
//! `loom-daemon` grew ten hand-rolled `fn gh_bin*` resolvers and well over a
//! hundred raw `Command::new(gh…)` sites, each deciding for itself whether to
//! honour `LOOM_GH_BIN`, whether to scope `GH_CONFIG_DIR`, whether to export a
//! trace context. Nobody can answer "does every daemon `gh` call route the same
//! way?" by reading the code. This module is the place that answer will live.
//!
//! # What lands in slice 1
//!
//! - [`resolver`] — the single executable resolver (policy launcher →
//!   `LOOM_GH_BIN` → `PATH`). `forge_cmd::gh_bin()` delegates to it.
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
//! `gh-cached` substitution for reads, the async/tokio variant and the Gitea
//! decline move in with the slices that first need them (see #9985's slicing
//! plan).

pub mod accounting;
mod outcome;
pub mod resolver;
pub mod telemetry;

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
}

/// What [`GhInvocation::execute`] observed, per [`OutputContract`].
#[derive(Debug)]
pub enum GhCompletion {
    /// A [`OutputContract::Captured`] run; exit vs timeout stay distinct.
    Captured(Completion),
    /// A [`OutputContract::Passthrough`] run's exit status.
    Passthrough(ExitStatus),
}

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
        }
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
    /// - `GH_CONFIG_DIR`: the cross-owner credential registered for the
    ///   working directory, else for the target's owner (the
    ///   `credential_preflight::apply_gh_config_for_{root,owner_slug}`
    ///   lookups). Absent ⇒ the child inherits the process-global value.
    /// - `GH_REPO`: the typed target, else the machine-global `LOOM_REPO`
    ///   override (`gh_repo_env::apply_loom_repo_override`'s contract).
    /// - `LOOM_TRACEPARENT` / `TRACEPARENT`: set together from
    ///   `child_context` — the invocation's own span when it is exported, so
    ///   the managed launcher (C4) parents its HTTP spans under it — or both
    ///   removed when there is none.
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
            .cwd
            .as_deref()
            .and_then(crate::credential_preflight::gh_config_dir_for_root)
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
        if let Some(repo) = slug.map(OsString::from).or(loom_repo) {
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
    /// # Errors
    ///
    /// [`ExecError::Spawn`] when `gh` could not be started;
    /// [`ExecError::Collect`] when it started but its result could not be
    /// collected (side effects may have happened — never retry a write on it).
    pub fn execute(self) -> Result<GhCompletion, ExecError> {
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
        }
    }
}

#[cfg(test)]
#[path = "migrated_sites_tests.rs"]
mod migrated_sites_tests;

#[cfg(test)]
#[path = "migrated_sites_tests_b.rs"]
mod migrated_sites_tests_b;

#[cfg(test)]
#[path = "migrated_sites_tests_c.rs"]
mod migrated_sites_tests_c;
