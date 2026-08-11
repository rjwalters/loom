//! Periodic install/host **invariant self-check** with repair-or-file (#5035).
//!
//! # Why this exists
//!
//! On 2026-08-03 a single manual cross-host check turned up **nine** distinct
//! install/host drift conditions — an empty `@modelcontextprotocol/sdk` under
//! `mcp-loom/node_modules` (which produced zero work for hours, #5016),
//! `.loom/runtimes/` absent in seven managed clones (#5002), a stale token
//! `.ranking` driving the wrong concurrency cap, stale daemon binaries, a
//! wedged sweep, a stale `.daemon.pid`, and more. Every one is mechanically
//! checkable and most are mechanically repairable, yet **Loom detected some and
//! acted on none** — they were all found by a human asking.
//!
//! This module is the daemon verifying the invariants its own operation depends
//! on. Per the repo's repair-over-gate stance it **repairs what is mechanically
//! safe and idempotent, and reports (files a blame issue) for what is not**.
//!
//! # Scope of this increment
//!
//! The [`Invariant`] registry ([`Invariant::ALL`]) is the **single source of
//! truth** for the checked invariant set — docs point at it rather than
//! re-enumerating (acceptance criterion: not duplicated between check code and
//! docs). This increment implements the three invariants with confirmed live
//! outage / wrong-behavior consequences on 2026-08-03:
//!
//! | Invariant | 2026-08-03 condition | Auto-repairable? |
//! |-----------|----------------------|------------------|
//! | [`Invariant::McpBundleHealth`]  | #1 empty sdk dir / stale `dist/index.js` (#5016) | yes — `npm ci && npm run build` |
//! | [`Invariant::RuntimesPresent`]  | #4 `.loom/runtimes/` absent in 7 clones (#5002) | yes — converge from `defaults/runtimes/` |
//! | [`Invariant::TokenRankingFresh`]| #5 stale `.ranking` → wrong concurrency cap     | yes — re-probe via `tokens check --ranking` |
//!
//! The remaining six conditions (binary freshness, sidecar reachability,
//! telemetry identity, sweep liveness, pid file, stale repo-local `.mcp.json`)
//! are deliberately **out of this increment** — the registry is designed so
//! adding them is a matter of extending [`Invariant::ALL`] plus one check/repair
//! arm each. See the PR description for the deferral note.
//!
//! # Design constraints honored (from the issue)
//!
//! - **Report-only is the default.** [`resolve_repair`] defaults to `false`, so
//!   a freshly-enabled check *observes and logs* without touching anything until
//!   an operator opts into repair — the check can be trusted before it is
//!   allowed to act.
//! - **Default-off overall**, per the FLAGS-OFF daemon convention
//!   ([`resolve_enabled`] defaults to `false`), unlike the default-on
//!   token-ranking-refresh / worktree-reaper loops.
//! - **Repair is conservative and idempotent.** The runtimes repair is a
//!   copy-converge that is a byte-for-byte no-op when current (precedent:
//!   `update-gitignore`, #4280).
//! - **Never repair under a live sweep where the repair could pull the rug.**
//!   The `mcp-loom` `npm ci` repair is skipped when a live sweep is detected
//!   ([`live_sweep_in_progress`]) — an agent may hold an MCP session.
//! - **Report through an existing surface + exactly one issue per condition.**
//!   Non-repairable (or repair-failed) violations file **one** issue per
//!   distinct condition, deduped by a hidden marker
//!   ([`Invariant::issue_marker`]) so a repeat pass never stacks duplicate
//!   comments (#4736 failure mode).
//! - **Bounded / timeout-guarded / never holds a lock a sweep needs.** All
//!   subprocess repairs run with a timeout; the checks are pure filesystem
//!   reads. The loop can never wedge dispatch.
//! - **PATH hazards are real** (#4875): the `npm` repair resolves its tool path
//!   explicitly ([`resolve_tool_path`]) rather than trusting a non-login ssh
//!   `PATH`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::workspace_registry::WorkspaceRegistry;

// ============================================================================
// Constants (env overrides + built-in defaults)
// ============================================================================

/// Master on/off env override. **Default-off** (FLAGS-OFF): unset ⇒ disabled.
/// Truthy (`1`/`true`/`yes`/`on`, case-insensitive) enables; anything else
/// disables even when config enables it.
pub const INSTALL_SELF_CHECK_ENABLE_ENV: &str = "LOOM_INSTALL_SELF_CHECK";

/// Env override for the check cadence (seconds).
pub const INSTALL_SELF_CHECK_INTERVAL_ENV: &str = "LOOM_INSTALL_SELF_CHECK_INTERVAL_SECS";

/// Env override for repair mode. **Default-off** (report-only): unset ⇒
/// report-only. Truthy enables repair (acting on mechanically-safe drift).
pub const INSTALL_SELF_CHECK_REPAIR_ENV: &str = "LOOM_INSTALL_SELF_CHECK_REPAIR";

/// Env override for the token-ranking staleness threshold (seconds).
pub const INSTALL_SELF_CHECK_RANKING_MAX_AGE_ENV: &str =
    "LOOM_INSTALL_SELF_CHECK_RANKING_MAX_AGE_SECS";

/// Default check cadence (30 minutes) — the same "periodic support" slot as the
/// worktree reaper (#4876), slow enough that the filesystem probes are
/// negligible next to normal sweep traffic.
pub const DEFAULT_INTERVAL_SECS: u64 = 1800;

/// Default token-ranking staleness threshold (1 hour). Beyond this, the
/// concurrency cap is being driven from a `.ranking` old enough to have drifted
/// from live rate-limit state (condition #5, 2026-08-03).
pub const DEFAULT_RANKING_MAX_AGE_SECS: u64 = 3600;

/// Timeout for a subprocess repair (`npm ci && npm run build`, `tokens check
/// --ranking`). Generous headroom for a slow install without letting a hung
/// repair wedge the loop.
const DEFAULT_REPAIR_TIMEOUT: Duration = Duration::from_secs(300);

/// Poll granularity while waiting for a repair subprocess to finish.
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Max bytes of captured subprocess output retained in a failure log line.
const MAX_OUTPUT_TAIL_BYTES: usize = 2048;

// ============================================================================
// Invariant registry — THE SINGLE SOURCE OF TRUTH
// ============================================================================

/// A checked install/host invariant.
///
/// [`Invariant::ALL`] is the authoritative list. The docs (the
/// "Install/host invariant self-check" section of `defaults/docs/daemon-reference.md`)
/// point here rather than re-enumerating, so the check code and the docs cannot
/// drift (acceptance criterion). Adding an invariant = one variant here + one
/// arm in [`check`] and (if repairable) [`repair`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Invariant {
    /// #1 (2026-08-03): the `mcp-loom` bundle is loadable — `dist/index.js`
    /// present and, when a lockfile exists, `node_modules` is complete (no empty
    /// `@modelcontextprotocol/sdk`). Repairable: `npm ci && npm run build`.
    McpBundleHealth,
    /// #4 (2026-08-03): every runtime the repo is configured to use has a
    /// populated `.loom/runtimes/<runtime>.json`. Repairable: converge the
    /// missing files from `defaults/runtimes/`.
    RuntimesPresent,
    /// #5 (2026-08-03): the token-pool `.ranking` file is present and fresh
    /// (younger than the staleness threshold), so the dispatch concurrency cap
    /// reflects live rate-limit state. Repairable: re-probe via `tokens check
    /// --ranking`.
    TokenRankingFresh,
}

impl Invariant {
    /// The authoritative set of checked invariants — the single source of truth
    /// (see the module docs).
    pub const ALL: &'static [Invariant] = &[
        Self::McpBundleHealth,
        Self::RuntimesPresent,
        Self::TokenRankingFresh,
    ];

    /// Stable machine identifier — used in the issue-dedup marker and logs.
    /// Never change an existing id (it keys the filed-issue dedup).
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::McpBundleHealth => "mcp-bundle-health",
            Self::RuntimesPresent => "runtimes-present",
            Self::TokenRankingFresh => "token-ranking-fresh",
        }
    }

    /// Human-readable one-line title (used as the filed-issue title prefix).
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::McpBundleHealth => "mcp-loom bundle is not loadable",
            Self::RuntimesPresent => ".loom/runtimes/ is missing configured runtimes",
            Self::TokenRankingFresh => "token-pool .ranking is stale or missing",
        }
    }

    /// Whether a violation of this invariant is mechanically auto-repairable.
    /// A non-repairable violation is reported (filed as an issue), never acted
    /// on. All three current invariants are repairable; the flag exists so a
    /// future non-repairable invariant (e.g. binary freshness — rolling is a
    /// deliberate act) drops into the report path automatically.
    #[must_use]
    pub fn auto_repairable(self) -> bool {
        match self {
            Self::McpBundleHealth | Self::RuntimesPresent | Self::TokenRankingFresh => true,
        }
    }

    /// Hidden HTML-comment marker embedded in a filed issue's body so a later
    /// pass finds the existing issue and does **not** file a duplicate.
    #[must_use]
    pub fn issue_marker(self) -> String {
        format!("<!-- loom:install-self-check:{} -->", self.id())
    }
}

// ============================================================================
// Outcome types
// ============================================================================

/// The result of checking one invariant against a repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvariantStatus {
    /// The invariant holds.
    Ok,
    /// The invariant is violated; `detail` is human-readable evidence.
    Violation(String),
    /// The invariant does not apply to this repo (e.g. no `mcp-loom` source
    /// tree, or no token pool bootstrapped); `reason` explains why.
    Skipped(String),
}

impl InvariantStatus {
    /// True only for [`InvariantStatus::Violation`].
    #[must_use]
    pub fn is_violation(&self) -> bool {
        matches!(self, Self::Violation(_))
    }
}

/// One invariant paired with its check status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantOutcome {
    pub invariant: Invariant,
    pub status: InvariantStatus,
}

/// The aggregate result of one self-check pass over a repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfCheckReport {
    pub outcomes: Vec<InvariantOutcome>,
}

impl SelfCheckReport {
    /// The subset of outcomes that are violations.
    #[must_use]
    pub fn violations(&self) -> Vec<&InvariantOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.status.is_violation())
            .collect()
    }

    /// True when no invariant is violated (skips are fine).
    #[must_use]
    pub fn is_all_ok(&self) -> bool {
        !self.outcomes.iter().any(|o| o.status.is_violation())
    }
}

// ============================================================================
// Check options
// ============================================================================

/// Tunables for a check pass. Constructed from resolved config/env.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckOptions {
    /// Token `.ranking` is considered stale beyond this age.
    pub ranking_max_age: Duration,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self {
            ranking_max_age: Duration::from_secs(DEFAULT_RANKING_MAX_AGE_SECS),
        }
    }
}

// ============================================================================
// Checks (pure filesystem reads — never spawn, never block)
// ============================================================================

/// Run every invariant in [`Invariant::ALL`] against `repo_root`.
#[must_use]
pub fn run_checks(repo_root: &Path, opts: CheckOptions) -> SelfCheckReport {
    let outcomes = Invariant::ALL
        .iter()
        .map(|&invariant| InvariantOutcome {
            invariant,
            status: check(invariant, repo_root, opts),
        })
        .collect();
    SelfCheckReport { outcomes }
}

/// Dispatch a single invariant's check.
#[must_use]
pub fn check(invariant: Invariant, repo_root: &Path, opts: CheckOptions) -> InvariantStatus {
    match invariant {
        Invariant::McpBundleHealth => check_mcp_bundle(repo_root),
        Invariant::RuntimesPresent => check_runtimes_present(repo_root),
        Invariant::TokenRankingFresh => check_token_ranking_fresh(repo_root, opts.ranking_max_age),
    }
}

/// Condition #1: the `mcp-loom` bundle is loadable.
///
/// - No `mcp-loom` source tree ⇒ `Skipped` (a consumer repo whose bundle lives
///   in an installed location, not built from source here).
/// - `dist/index.js` missing or empty ⇒ `Violation` (the "stale/absent build"
///   symptom that produced zero work for hours, #5016).
/// - A `package-lock.json` present but `node_modules/@modelcontextprotocol/sdk`
///   missing or an empty directory ⇒ `Violation` (the exact 2026-08-03 shape).
fn check_mcp_bundle(repo_root: &Path) -> InvariantStatus {
    let mcp_dir = repo_root.join("mcp-loom");
    if !mcp_dir.is_dir() {
        return InvariantStatus::Skipped(
            "no mcp-loom source tree in this repo (bundle built/installed elsewhere)".to_string(),
        );
    }

    let dist = mcp_dir.join("dist").join("index.js");
    match std::fs::metadata(&dist) {
        Ok(m) if m.len() > 0 => {}
        Ok(_) => {
            return InvariantStatus::Violation(format!("{} is empty", dist.display()));
        }
        Err(_) => {
            return InvariantStatus::Violation(format!(
                "{} is missing (bundle not built)",
                dist.display()
            ));
        }
    }

    let lockfile = mcp_dir.join("package-lock.json");
    if lockfile.is_file() {
        let sdk = mcp_dir
            .join("node_modules")
            .join("@modelcontextprotocol")
            .join("sdk");
        if !dir_has_entries(&sdk) {
            return InvariantStatus::Violation(format!(
                "{} is missing or empty despite a lockfile — node_modules is incomplete \
                 (run `npm ci`)",
                sdk.display()
            ));
        }
    }

    InvariantStatus::Ok
}

/// Condition #4: every configured runtime has a populated
/// `.loom/runtimes/<runtime>.json`.
fn check_runtimes_present(repo_root: &Path) -> InvariantStatus {
    let runtimes_dir = repo_root.join(".loom").join("runtimes");
    let configured = configured_runtimes(repo_root);

    let missing: Vec<String> = configured
        .iter()
        .filter(|rt| {
            let f = runtimes_dir.join(format!("{rt}.json"));
            !matches!(std::fs::metadata(&f), Ok(m) if m.len() > 0)
        })
        .cloned()
        .collect();

    if missing.is_empty() {
        InvariantStatus::Ok
    } else if !runtimes_dir.is_dir() {
        InvariantStatus::Violation(format!(
            "{} is absent; configured runtimes have no runtime files: {}",
            runtimes_dir.display(),
            missing.join(", ")
        ))
    } else {
        InvariantStatus::Violation(format!(
            "{} is missing runtime files for configured runtimes: {}",
            runtimes_dir.display(),
            missing.join(", ")
        ))
    }
}

/// Condition #5: the token-pool `.ranking` is present and fresh.
///
/// - No token pool bootstrapped (no `*.token` files) ⇒ `Skipped` (ranking is
///   irrelevant when there is nothing to rank).
/// - `.ranking` absent, or older than `max_age` ⇒ `Violation`.
fn check_token_ranking_fresh(repo_root: &Path, max_age: Duration) -> InvariantStatus {
    let tokens_dir = crate::tokens_pool::paths::resolve_tokens_dir(repo_root);
    if !crate::tokens_pool::paths::has_token_files(&tokens_dir) {
        return InvariantStatus::Skipped(format!(
            "no token pool bootstrapped at {} (nothing to rank)",
            tokens_dir.display()
        ));
    }

    let ranking = tokens_dir.join(".ranking");
    let modified = match std::fs::metadata(&ranking).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => {
            return InvariantStatus::Violation(format!(
                "{} is missing despite a bootstrapped pool — the concurrency cap is running \
                 without a live ranking",
                ranking.display()
            ));
        }
    };

    match SystemTime::now().duration_since(modified) {
        Ok(age) if age > max_age => InvariantStatus::Violation(format!(
            "{} is {}s old (threshold {}s) — the concurrency cap may be driven from a stale \
             ranking",
            ranking.display(),
            age.as_secs(),
            max_age.as_secs()
        )),
        _ => InvariantStatus::Ok,
    }
}

/// The set of runtimes the repo is configured to use: `runtimes.default`
/// (falling back to `"claude"` when unset) unioned with every value under
/// `runtimes.roles`.
#[must_use]
pub fn configured_runtimes(repo_root: &Path) -> BTreeSet<String> {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let mut set = BTreeSet::new();

    let default_rt = crate::config_resolver::get_path(&effective, "runtimes.default")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "claude".to_string());
    set.insert(default_rt);

    if let Some(roles) =
        crate::config_resolver::get_path(&effective, "runtimes.roles").and_then(|v| v.as_object())
    {
        for v in roles.values() {
            if let Some(rt) = v.as_str() {
                set.insert(rt.to_string());
            }
        }
    }

    set
}

/// True iff `dir` exists, is a directory, and has at least one entry.
fn dir_has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

// ============================================================================
// Repair
// ============================================================================

/// The result of attempting to repair one violated invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairOutcome {
    /// The repair ran and converged the invariant; `evidence` describes the
    /// before/after (acceptance criterion: repairs logged with before/after).
    Repaired(String),
    /// The repair was deliberately not attempted; `reason` explains why (not
    /// auto-repairable, live sweep, or repair mode off). Not a failure.
    NotAttempted(String),
    /// The repair was attempted and failed; `reason` explains what went wrong.
    /// A failure routes the violation to the issue-filing path.
    Failed(String),
}

impl RepairOutcome {
    /// True only for a completed, converging repair.
    #[must_use]
    pub fn is_repaired(&self) -> bool {
        matches!(self, Self::Repaired(_))
    }
}

/// Injectable context for a repair pass — production leaves the binary
/// overrides `None` (resolved from PATH / `current_exe`); tests point them at
/// fakes.
#[derive(Debug, Clone)]
pub struct RepairContext {
    /// Explicit `npm` binary (tests). Production resolves via
    /// [`resolve_tool_path`].
    pub npm_bin: Option<PathBuf>,
    /// Explicit daemon binary for `tokens check --ranking` (tests). Production
    /// uses `std::env::current_exe()`.
    pub daemon_bin: Option<PathBuf>,
    /// Subprocess repair timeout.
    pub timeout: Duration,
}

impl Default for RepairContext {
    fn default() -> Self {
        Self {
            npm_bin: None,
            daemon_bin: None,
            timeout: DEFAULT_REPAIR_TIMEOUT,
        }
    }
}

/// Attempt to repair a violated `invariant` in `repo_root`. Only call this for
/// an invariant whose [`check`] returned [`InvariantStatus::Violation`].
#[must_use]
pub fn repair(invariant: Invariant, repo_root: &Path, ctx: &RepairContext) -> RepairOutcome {
    if !invariant.auto_repairable() {
        return RepairOutcome::NotAttempted(
            "invariant is not auto-repairable — reporting instead".to_string(),
        );
    }
    match invariant {
        Invariant::RuntimesPresent => repair_runtimes(repo_root),
        Invariant::TokenRankingFresh => repair_token_ranking(repo_root, ctx),
        Invariant::McpBundleHealth => repair_mcp_bundle(repo_root, ctx),
    }
}

/// Idempotent copy-converge of missing `.loom/runtimes/<rt>.json` from
/// `defaults/runtimes/`. A byte-for-byte no-op when current (precedent:
/// `update-gitignore`, #4280). Reports (does not act) when `defaults/runtimes/`
/// is absent — a consumer clone converges its runtimes through
/// `resync-installed.sh` during `loom update`, which this daemon must not run
/// on the operator's behalf here.
fn repair_runtimes(repo_root: &Path) -> RepairOutcome {
    let defaults_dir = repo_root.join("defaults").join("runtimes");
    if !defaults_dir.is_dir() {
        return RepairOutcome::NotAttempted(format!(
            "{} is absent — cannot converge from source here; run `resync-installed.sh` / \
             `loom update` on this clone",
            defaults_dir.display()
        ));
    }

    let runtimes_dir = repo_root.join(".loom").join("runtimes");
    let configured = configured_runtimes(repo_root);

    let mut copied = Vec::new();
    for rt in &configured {
        let dst = runtimes_dir.join(format!("{rt}.json"));
        if matches!(std::fs::metadata(&dst), Ok(m) if m.len() > 0) {
            continue; // already present and non-empty — idempotent skip
        }
        let src = defaults_dir.join(format!("{rt}.json"));
        if !src.is_file() {
            // A configured runtime with no shipped default (a custom runtime):
            // nothing to converge from — leave it for the report.
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(&runtimes_dir) {
            return RepairOutcome::Failed(format!(
                "could not create {}: {e}",
                runtimes_dir.display()
            ));
        }
        if let Err(e) = std::fs::copy(&src, &dst) {
            return RepairOutcome::Failed(format!(
                "could not copy {} -> {}: {e}",
                src.display(),
                dst.display()
            ));
        }
        copied.push(format!("{rt}.json"));
    }

    if copied.is_empty() {
        RepairOutcome::NotAttempted(
            "no missing runtime file had a shipped default to converge from (custom runtime?)"
                .to_string(),
        )
    } else {
        RepairOutcome::Repaired(format!(
            "converged {} runtime file(s) from {}: {}",
            copied.len(),
            defaults_dir.display(),
            copied.join(", ")
        ))
    }
}

/// Re-probe the token pool and rewrite `.ranking` by shelling to the daemon's
/// own `tokens check --ranking` subcommand (the same mechanism
/// [`crate::token_ranking_refresh`] uses). Bounded by `ctx.timeout`.
fn repair_token_ranking(repo_root: &Path, ctx: &RepairContext) -> RepairOutcome {
    let bin = match &ctx.daemon_bin {
        Some(p) => p.clone(),
        None => match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                return RepairOutcome::Failed(format!("could not resolve current_exe(): {e}"))
            }
        },
    };
    let argv = ["tokens", "check", "--ranking", "--workspace"];
    match run_with_timeout(&bin, &argv, Some(repo_root), repo_root, ctx.timeout) {
        Ok(()) => RepairOutcome::Repaired(format!(
            "re-probed token pool and rewrote .ranking via `{} tokens check --ranking`",
            bin.display()
        )),
        Err(e) => RepairOutcome::Failed(e),
    }
}

/// Reinstall + rebuild the `mcp-loom` bundle (`npm ci && npm run build`) with an
/// explicitly-resolved `npm` (PATH hazards, #4875). **Skipped when a live sweep
/// is in progress** — an agent may hold an MCP session and `npm ci` would pull
/// the rug (issue design constraint).
fn repair_mcp_bundle(repo_root: &Path, ctx: &RepairContext) -> RepairOutcome {
    if live_sweep_in_progress(repo_root) {
        return RepairOutcome::NotAttempted(
            "a live sweep is in progress — refusing `npm ci` in mcp-loom (would pull the rug on \
             an in-flight MCP session)"
                .to_string(),
        );
    }
    let mcp_dir = repo_root.join("mcp-loom");
    if !mcp_dir.is_dir() {
        return RepairOutcome::NotAttempted(format!("{} is absent", mcp_dir.display()));
    }
    let npm =
        match &ctx.npm_bin {
            Some(p) => p.clone(),
            None => match resolve_tool_path("npm") {
                Some(p) => p,
                None => return RepairOutcome::Failed(
                    "could not resolve `npm` on PATH or the common Homebrew/usr-local locations \
                     (#4875) — cannot rebuild the bundle"
                        .to_string(),
                ),
            },
        };

    if let Err(e) = run_with_timeout(&npm, &["ci"], None, &mcp_dir, ctx.timeout) {
        return RepairOutcome::Failed(format!("`npm ci` failed: {e}"));
    }
    if let Err(e) = run_with_timeout(&npm, &["run", "build"], None, &mcp_dir, ctx.timeout) {
        return RepairOutcome::Failed(format!("`npm run build` failed: {e}"));
    }
    RepairOutcome::Repaired(format!(
        "ran `{} ci && {} run build` in {}",
        npm.display(),
        npm.display(),
        mcp_dir.display()
    ))
}

/// Conservative live-sweep detector: any `.loom-in-use` marker under
/// `.loom/worktrees/` means an agent session may be live in this repo. Cheap
/// (one shallow directory walk) and errs toward "sweep in progress" on any
/// read error.
#[must_use]
pub fn live_sweep_in_progress(repo_root: &Path) -> bool {
    let worktrees = repo_root.join(".loom").join("worktrees");
    let entries = match std::fs::read_dir(&worktrees) {
        Ok(e) => e,
        Err(_) => return false, // no worktrees dir ⇒ no live sweep here
    };
    for entry in entries.filter_map(Result::ok) {
        if entry.path().join(".loom-in-use").exists() {
            return true;
        }
    }
    false
}

/// Resolve a tool (`npm`, `node`, `gh`) explicitly rather than trusting a
/// non-login ssh `PATH` (#4875). Tries `$PATH` first, then the common
/// Homebrew / usr-local locations Loom hosts actually install to.
#[must_use]
pub fn resolve_tool_path(tool: &str) -> Option<PathBuf> {
    resolve_tool_path_in(tool, std::env::var_os("PATH").as_deref())
}

/// [`resolve_tool_path`] with the search path injected rather than read from
/// the process environment.
///
/// Exists so the `$PATH` branch can be tested without
/// `std::env::set_var("PATH", …)`: `PATH` is process-global and Rust's test
/// harness runs tests as threads in one process, so mutating it breaks every
/// concurrently-running test that spawns a bare-name `git`/`gh` (#5961).
#[must_use]
fn resolve_tool_path_in(tool: &str, search_path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if let Some(path) = search_path {
        for dir in std::env::split_paths(path) {
            let candidate = dir.join(tool);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
        let candidate = Path::new(dir).join(tool);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Run `bin arg… [maybe_workspace]` with `cwd` as the working directory,
/// capturing combined output to a temp file (never a pipe — avoids the
/// pipe-buffer deadlock) and killing it after `timeout`. `Ok(())` on a zero
/// exit; `Err(reason)` (with an output tail) otherwise.
fn run_with_timeout(
    bin: &Path,
    args: &[&str],
    trailing_workspace: Option<&Path>,
    cwd: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let log_path =
        std::env::temp_dir().join(format!("loom-install-self-check-{}.log", uuid::Uuid::new_v4()));
    let out_file = std::fs::File::create(&log_path)
        .map_err(|e| format!("could not create output file: {e}"))?;
    let stderr_file = match out_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return Err(format!("could not clone output handle: {e}"));
        }
    };

    let mut command = Command::new(bin);
    command.args(args);
    if let Some(ws) = trailing_workspace {
        command.arg(ws);
    }
    let mut child = match command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return Err(format!("could not spawn `{}`: {e}", bin.display()));
        }
    };

    let start = Instant::now();
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(()),
            Ok(Some(status)) => {
                let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
                break Err(format!(
                    "`{}` exited with {status}: {}",
                    bin.display(),
                    truncate_tail(&tail)
                ));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!(
                        "`{}` timed out after {}s",
                        bin.display(),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(PROBE_POLL_INTERVAL);
            }
            Err(e) => break Err(format!("could not poll `{}`: {e}", bin.display())),
        }
    };
    let _ = std::fs::remove_file(&log_path);
    result
}

/// Truncate captured output to the last [`MAX_OUTPUT_TAIL_BYTES`] bytes,
/// trimmed, on a char boundary.
fn truncate_tail(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_TAIL_BYTES {
        return s.trim().to_string();
    }
    let start = s.len() - MAX_OUTPUT_TAIL_BYTES;
    let start = (start..s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    s[start..].trim().to_string()
}

// ============================================================================
// Violation reporting (file exactly one issue per distinct condition)
// ============================================================================

/// Files (or finds an existing) blame issue for a non-repairable or
/// repair-failed violation. Abstracted behind a trait so the dedup logic is
/// testable with a fake — production uses [`GhIssueFiler`].
pub trait ViolationReporter {
    /// Whether an open issue already carries `marker` in its body. `Err` on a
    /// forge lookup failure (the caller then declines to file, to avoid
    /// duplicates on a transient error).
    fn has_open_issue(&self, marker: &str) -> Result<bool, String>;

    /// File a new issue. Returns the created issue number.
    fn file_issue(&self, title: &str, body: &str) -> Result<u64, String>;
}

/// What a `report_violation` call did — for logging and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportOutcome {
    /// A new issue was filed (number).
    Filed(u64),
    /// An open issue with this condition's marker already exists (number when
    /// the reporter surfaces it) — no duplicate filed.
    AlreadyOpen,
    /// The forge lookup/creation failed — logged, no duplicate risk taken.
    Failed(String),
}

/// File exactly one issue for `outcome`'s violated invariant, deduped by the
/// invariant's hidden marker. A no-op returning [`ReportOutcome::AlreadyOpen`]
/// when an open issue already carries the marker — this is what prevents the
/// duplicate-comment stacking failure mode (#4736).
pub fn report_violation<R: ViolationReporter>(
    reporter: &R,
    repo_root: &Path,
    outcome: &InvariantOutcome,
) -> ReportOutcome {
    let invariant = outcome.invariant;
    let marker = invariant.issue_marker();

    match reporter.has_open_issue(&marker) {
        Ok(true) => return ReportOutcome::AlreadyOpen,
        Ok(false) => {}
        Err(e) => return ReportOutcome::Failed(format!("forge lookup failed: {e}")),
    }

    let detail = match &outcome.status {
        InvariantStatus::Violation(d) => d.as_str(),
        _ => "(no detail)",
    };
    let title = format!("install self-check: {}", invariant.title());
    let body = format!(
        "The daemon install/host self-check (#5035) found a violation it could not \
         auto-repair.\n\n\
         - **Invariant**: `{id}`\n\
         - **Repo**: `{repo}`\n\
         - **Evidence**: {detail}\n\n\
         This issue is filed once per distinct condition; the self-check dedupes on the marker \
         below and will not stack duplicate comments on repeat passes.\n\n\
         {marker}\n",
        id = invariant.id(),
        repo = repo_root.display(),
        detail = detail,
        marker = marker,
    );

    match reporter.file_issue(&title, &body) {
        Ok(n) => ReportOutcome::Filed(n),
        Err(e) => ReportOutcome::Failed(e),
    }
}

/// Production [`ViolationReporter`]: shells to `gh` (REST-cached listing for the
/// dedup search, `create-issue.sh` for filing per CLAUDE.md's rate-limit-safe
/// convention). Kept intentionally thin; the interesting dedup logic lives in
/// [`report_violation`] and is exercised against a fake in tests.
pub struct GhIssueFiler {
    repo_root: PathBuf,
}

impl GhIssueFiler {
    #[must_use]
    pub fn new(repo_root: PathBuf) -> Self {
        Self { repo_root }
    }
}

impl ViolationReporter for GhIssueFiler {
    fn has_open_issue(&self, marker: &str) -> Result<bool, String> {
        // `gh issue list --search "<marker>" --state open` — GitHub full-text
        // search matches the hidden HTML comment in the body.
        let output = Command::new("gh")
            .args([
                "issue", "list", "--state", "open", "--search", marker, "--json", "number",
            ])
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| format!("could not spawn gh: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "gh issue list exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let rows: serde_json::Value = serde_json::from_str(stdout.trim())
            .map_err(|e| format!("could not parse gh json: {e}"))?;
        Ok(rows.as_array().is_some_and(|a| !a.is_empty()))
    }

    fn file_issue(&self, title: &str, body: &str) -> Result<u64, String> {
        // Prefer the rate-limit-safe `create-issue.sh` (REST fallback, #5047)
        // when present; fall back to `gh issue create`.
        let script = self
            .repo_root
            .join(".loom")
            .join("scripts")
            .join("create-issue.sh");
        let output = if script.is_file() {
            Command::new(&script)
                .args(["--title", title, "--body", body, "--label", "loom:triage"])
                .current_dir(&self.repo_root)
                .output()
        } else {
            Command::new("gh")
                .args([
                    "issue",
                    "create",
                    "--title",
                    title,
                    "--body",
                    body,
                    "--label",
                    "loom:triage",
                ])
                .current_dir(&self.repo_root)
                .output()
        }
        .map_err(|e| format!("could not spawn issue-create: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "issue-create exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        // Parse the trailing issue number from the created issue URL.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let num = stdout
            .rsplit(|c: char| c == '/' || c.is_whitespace())
            .find_map(|tok| tok.trim().parse::<u64>().ok())
            .unwrap_or(0);
        Ok(num)
    }
}

// ============================================================================
// Config (.loom/config.json → autonomous.installSelfCheck)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.installSelfCheck` this module
/// consumes. Each field is `Option` so an absent key falls through to the
/// env-var / built-in-default resolution — precedence **env > config >
/// default**, matching every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallSelfCheckConfig {
    /// `enabled` (default **false** — FLAGS-OFF).
    pub enabled: Option<bool>,
    /// `intervalSecs` (a zero/invalid value drops to `None`).
    pub interval_secs: Option<u64>,
    /// `repair` (default **false** — report-only).
    pub repair: Option<bool>,
    /// `tokenRankingMaxAgeSecs` (a zero/invalid value drops to `None`).
    pub ranking_max_age_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.installSelfCheck`, soft-failing every
/// field to `None` on a missing file, malformed JSON, or a missing
/// `autonomous` / `installSelfCheck` block.
#[must_use]
pub fn read_config(repo_root: &Path) -> InstallSelfCheckConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.installSelfCheck")
    else {
        return InstallSelfCheckConfig::default();
    };

    InstallSelfCheckConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        repair: block.get("repair").and_then(serde_json::Value::as_bool),
        ranking_max_age_secs: block
            .get("tokenRankingMaxAgeSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

fn env_truthy(name: &str) -> Option<bool> {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Resolve whether the loop is enabled — **env > config > default(false)**.
#[must_use]
pub fn resolve_enabled(config: &InstallSelfCheckConfig) -> bool {
    if let Some(v) = env_truthy(INSTALL_SELF_CHECK_ENABLE_ENV) {
        return v;
    }
    config.enabled.unwrap_or(false)
}

/// Resolve repair (act) mode — **env > config > default(false = report-only)**.
#[must_use]
pub fn resolve_repair(config: &InstallSelfCheckConfig) -> bool {
    if let Some(v) = env_truthy(INSTALL_SELF_CHECK_REPAIR_ENV) {
        return v;
    }
    config.repair.unwrap_or(false)
}

/// Resolve the check cadence — **env > config > default**.
#[must_use]
pub fn resolve_interval(config: &InstallSelfCheckConfig) -> Duration {
    std::env::var(INSTALL_SELF_CHECK_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.interval_secs)
        .map_or_else(|| Duration::from_secs(DEFAULT_INTERVAL_SECS), Duration::from_secs)
}

/// Resolve the token-ranking staleness threshold — **env > config > default**.
#[must_use]
pub fn resolve_ranking_max_age(config: &InstallSelfCheckConfig) -> Duration {
    std::env::var(INSTALL_SELF_CHECK_RANKING_MAX_AGE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.ranking_max_age_secs)
        .map_or_else(|| Duration::from_secs(DEFAULT_RANKING_MAX_AGE_SECS), Duration::from_secs)
}

// ============================================================================
// One pass (check → repair-or-report, with logging) + runtime wiring
// ============================================================================

/// Run one full self-check pass over `repo_root`: check every invariant, then —
/// in repair mode — repair the mechanically-safe violations and file one issue
/// per non-repairable / repair-failed condition. In report-only mode (default)
/// it only logs.
///
/// Returns the [`SelfCheckReport`] so callers/tests can assert on it.
pub fn run_pass<R: ViolationReporter>(
    repo_root: &Path,
    repair_mode: bool,
    check_opts: CheckOptions,
    repair_ctx: &RepairContext,
    reporter: &R,
) -> SelfCheckReport {
    let report = run_checks(repo_root, check_opts);

    for outcome in &report.outcomes {
        match &outcome.status {
            InvariantStatus::Ok => log::debug!(
                "install_self_check: {} OK ({})",
                outcome.invariant.id(),
                repo_root.display()
            ),
            InvariantStatus::Skipped(reason) => log::debug!(
                "install_self_check: {} skipped ({}): {reason}",
                outcome.invariant.id(),
                repo_root.display()
            ),
            InvariantStatus::Violation(detail) => {
                log::warn!(
                    "install_self_check: {} VIOLATED ({}): {detail}",
                    outcome.invariant.id(),
                    repo_root.display()
                );
                handle_violation(repo_root, outcome, repair_mode, repair_ctx, reporter);
            }
        }
    }

    report
}

/// Repair (or, in report-only mode, log) a single violation; on a
/// non-repairable / repair-failed / report-only-non-repairable path, file one
/// deduped issue.
fn handle_violation<R: ViolationReporter>(
    repo_root: &Path,
    outcome: &InvariantOutcome,
    repair_mode: bool,
    repair_ctx: &RepairContext,
    reporter: &R,
) {
    if !repair_mode {
        // Report-only default: observe and log, take no action (filing an issue
        // is itself an action reserved for repair/act mode).
        log::info!(
            "install_self_check: report-only — not acting on {} in {} (enable \
             LOOM_INSTALL_SELF_CHECK_REPAIR=1 to repair/file)",
            outcome.invariant.id(),
            repo_root.display()
        );
        return;
    }

    let repair_outcome = repair(outcome.invariant, repo_root, repair_ctx);
    match &repair_outcome {
        RepairOutcome::Repaired(evidence) => {
            log::info!(
                "install_self_check: repaired {} in {}: {evidence}",
                outcome.invariant.id(),
                repo_root.display()
            );
        }
        RepairOutcome::NotAttempted(reason) | RepairOutcome::Failed(reason) => {
            log::warn!(
                "install_self_check: {} in {} not repaired ({reason}); filing an issue",
                outcome.invariant.id(),
                repo_root.display()
            );
            match report_violation(reporter, repo_root, outcome) {
                ReportOutcome::Filed(n) => log::warn!(
                    "install_self_check: filed issue #{n} for {} in {}",
                    outcome.invariant.id(),
                    repo_root.display()
                ),
                ReportOutcome::AlreadyOpen => log::info!(
                    "install_self_check: an open issue already tracks {} in {} — not duplicating",
                    outcome.invariant.id(),
                    repo_root.display()
                ),
                ReportOutcome::Failed(e) => log::warn!(
                    "install_self_check: could not file issue for {} in {}: {e}",
                    outcome.invariant.id(),
                    repo_root.display()
                ),
            }
        }
    }
}

/// Spawn the **multi-workspace** self-check loop on the shared daemon runtime
/// (mirrors [`crate::worktree_reaper`] / [`crate::token_ranking_refresh`]).
///
/// Every `interval` it re-reads [`WorkspaceRegistry::effective_roots`] against
/// `fallback_root` (an empty registry ⇒ the single `fallback_root`) and runs
/// one pass per registered root, each gated by that root's own
/// `autonomous.installSelfCheck` config (precedence env > config >
/// default(off)). Passes run on a blocking thread (`spawn_blocking`) — the
/// checks are filesystem reads and any repair may shell out — so a pass never
/// parks a runtime worker or wedges dispatch.
pub fn spawn_multi_install_self_check_task(
    fallback_root: PathBuf,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "install_self_check: starting multi-workspace loop (interval={}s)",
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;

            let roots = WorkspaceRegistry::load_default()
                .unwrap_or_else(|e| {
                    log::warn!(
                        "install_self_check: could not load workspace registry ({e}); using fallback"
                    );
                    WorkspaceRegistry::default()
                })
                .effective_roots(&fallback_root);

            for root in roots {
                let config = read_config(&root);
                if !resolve_enabled(&config) {
                    log::debug!(
                        "install_self_check: {} disabled (autonomous.installSelfCheck.enabled=false \
                         or LOOM_INSTALL_SELF_CHECK unset-falsy) — skipping",
                        root.display()
                    );
                    continue;
                }
                let repair_mode = resolve_repair(&config);
                let check_opts = CheckOptions {
                    ranking_max_age: resolve_ranking_max_age(&config),
                };
                let root_for_task = root.clone();
                let joined = tokio::task::spawn_blocking(move || {
                    let reporter = GhIssueFiler::new(root_for_task.clone());
                    run_pass(
                        &root_for_task,
                        repair_mode,
                        check_opts,
                        &RepairContext::default(),
                        &reporter,
                    );
                })
                .await;
                if let Err(e) = joined {
                    log::error!(
                        "install_self_check: pass for {} panicked ({e}); continuing to the next repo",
                        root.display()
                    );
                }
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn write_config(root: &Path, contents: &str) {
        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::write(root.join(".loom").join("config.json"), contents).unwrap();
    }

    fn write_fake_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    // ===================================================================
    // Invariant registry — single source of truth
    // ===================================================================

    #[test]
    fn test_invariant_ids_are_unique_and_stable() {
        let ids: BTreeSet<&str> = Invariant::ALL.iter().map(|i| i.id()).collect();
        assert_eq!(ids.len(), Invariant::ALL.len(), "invariant ids must be unique");
        // The three priority invariants from the 2026-08-03 incident.
        assert!(ids.contains("mcp-bundle-health"));
        assert!(ids.contains("runtimes-present"));
        assert!(ids.contains("token-ranking-fresh"));
    }

    #[test]
    fn test_issue_marker_encodes_id() {
        for inv in Invariant::ALL {
            let marker = inv.issue_marker();
            assert!(marker.contains(inv.id()));
            assert!(marker.starts_with("<!-- loom:install-self-check:"));
        }
    }

    // ===================================================================
    // Condition #1 — MCP bundle health
    // ===================================================================

    fn make_mcp(root: &Path, dist_bytes: Option<&str>, lockfile: bool, sdk_entries: bool) {
        let mcp = root.join("mcp-loom");
        fs::create_dir_all(mcp.join("dist")).unwrap();
        if let Some(b) = dist_bytes {
            fs::write(mcp.join("dist").join("index.js"), b).unwrap();
        }
        if lockfile {
            fs::write(mcp.join("package-lock.json"), "{}").unwrap();
        }
        let sdk = mcp
            .join("node_modules")
            .join("@modelcontextprotocol")
            .join("sdk");
        fs::create_dir_all(&sdk).unwrap();
        if sdk_entries {
            fs::write(sdk.join("package.json"), "{}").unwrap();
        }
    }

    #[test]
    fn test_mcp_skipped_when_no_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(check_mcp_bundle(tmp.path()), InvariantStatus::Skipped(_)));
    }

    #[test]
    fn test_mcp_ok_when_dist_and_sdk_present() {
        let tmp = tempfile::tempdir().unwrap();
        make_mcp(tmp.path(), Some("console.log(1)"), true, true);
        assert_eq!(check_mcp_bundle(tmp.path()), InvariantStatus::Ok);
    }

    #[test]
    fn test_mcp_violation_when_dist_missing() {
        let tmp = tempfile::tempdir().unwrap();
        make_mcp(tmp.path(), None, true, true);
        let status = check_mcp_bundle(tmp.path());
        let InvariantStatus::Violation(detail) = status else {
            panic!("expected violation, got {status:?}");
        };
        assert!(detail.contains("index.js"), "{detail}");
    }

    /// The exact 2026-08-03 condition #1: a lockfile exists but the SDK
    /// directory is empty (`node_modules` incomplete) — the state that produced
    /// zero work for hours (#5016).
    #[test]
    fn test_mcp_violation_when_sdk_dir_empty_with_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        make_mcp(tmp.path(), Some("console.log(1)"), true, false);
        let status = check_mcp_bundle(tmp.path());
        let InvariantStatus::Violation(detail) = status else {
            panic!("expected violation, got {status:?}");
        };
        assert!(detail.contains("sdk"), "{detail}");
        assert!(detail.contains("npm ci"), "{detail}");
    }

    /// No lockfile ⇒ the node_modules-completeness check is not applied (a
    /// dependency-free bundle is legitimately without node_modules).
    #[test]
    fn test_mcp_ok_when_no_lockfile_even_if_sdk_empty() {
        let tmp = tempfile::tempdir().unwrap();
        make_mcp(tmp.path(), Some("console.log(1)"), false, false);
        assert_eq!(check_mcp_bundle(tmp.path()), InvariantStatus::Ok);
    }

    // ===================================================================
    // Condition #4 — .loom/runtimes/ presence
    // ===================================================================

    #[test]
    fn test_runtimes_default_claude_when_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        let set = configured_runtimes(tmp.path());
        assert!(set.contains("claude"), "default runtime should be claude");
    }

    #[test]
    fn test_runtimes_reads_default_and_roles() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"runtimes": {"default": "codex", "roles": {"builder": "aider", "judge": "claude"}}}"#,
        );
        let set = configured_runtimes(tmp.path());
        assert!(set.contains("codex"));
        assert!(set.contains("aider"));
        assert!(set.contains("claude"));
    }

    /// The exact 2026-08-03 condition #4: `.loom/runtimes/` absent.
    #[test]
    fn test_runtimes_violation_when_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        // No .loom/runtimes/ at all; default runtime is claude.
        let status = check_runtimes_present(tmp.path());
        let InvariantStatus::Violation(detail) = status else {
            panic!("expected violation, got {status:?}");
        };
        assert!(detail.contains("absent"), "{detail}");
        assert!(detail.contains("claude"), "{detail}");
    }

    #[test]
    fn test_runtimes_ok_when_configured_files_present() {
        let tmp = tempfile::tempdir().unwrap();
        let rt = tmp.path().join(".loom").join("runtimes");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("claude.json"), "{}").unwrap();
        assert_eq!(check_runtimes_present(tmp.path()), InvariantStatus::Ok);
    }

    #[test]
    fn test_runtimes_violation_when_a_configured_runtime_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"runtimes": {"default": "claude", "roles": {"builder": "codex"}}}"#,
        );
        let rt = tmp.path().join(".loom").join("runtimes");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("claude.json"), "{}").unwrap();
        // codex.json missing.
        let status = check_runtimes_present(tmp.path());
        let InvariantStatus::Violation(detail) = status else {
            panic!("expected violation, got {status:?}");
        };
        assert!(detail.contains("codex"), "{detail}");
    }

    // ===================================================================
    // Condition #4 — runtimes repair idempotency
    // ===================================================================

    fn make_defaults_runtimes(root: &Path, names: &[&str]) {
        let d = root.join("defaults").join("runtimes");
        fs::create_dir_all(&d).unwrap();
        for n in names {
            fs::write(d.join(format!("{n}.json")), format!("{{\"runtime\":\"{n}\"}}")).unwrap();
        }
    }

    #[test]
    fn test_repair_runtimes_converges_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        make_defaults_runtimes(tmp.path(), &["claude", "codex", "aider"]);
        // configured = claude (default)
        assert!(matches!(check_runtimes_present(tmp.path()), InvariantStatus::Violation(_)));

        let outcome = repair_runtimes(tmp.path());
        assert!(outcome.is_repaired(), "{outcome:?}");
        // Now the check passes.
        assert_eq!(check_runtimes_present(tmp.path()), InvariantStatus::Ok);
        assert!(tmp
            .path()
            .join(".loom")
            .join("runtimes")
            .join("claude.json")
            .is_file());
    }

    #[test]
    fn test_repair_runtimes_is_idempotent_noop_when_current() {
        let tmp = tempfile::tempdir().unwrap();
        make_defaults_runtimes(tmp.path(), &["claude"]);
        // First repair converges.
        assert!(repair_runtimes(tmp.path()).is_repaired());
        // Second repair is a no-op (nothing missing).
        let second = repair_runtimes(tmp.path());
        assert!(
            matches!(second, RepairOutcome::NotAttempted(_)),
            "second repair should be a no-op, got {second:?}"
        );
    }

    #[test]
    fn test_repair_runtimes_reports_when_defaults_absent() {
        let tmp = tempfile::tempdir().unwrap();
        // No defaults/runtimes/ — a consumer clone.
        let outcome = repair_runtimes(tmp.path());
        assert!(matches!(outcome, RepairOutcome::NotAttempted(_)), "{outcome:?}");
    }

    // ===================================================================
    // Condition #5 — token .ranking staleness
    // ===================================================================

    fn bootstrap_pool(root: &Path) -> PathBuf {
        let dir = root.join(".loom").join("tokens");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("agent-1.token"), "x").unwrap();
        dir
    }

    #[test]
    fn test_ranking_skipped_when_no_pool() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
        let status = check_token_ranking_fresh(tmp.path(), Duration::from_secs(3600));
        std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
        assert!(matches!(status, InvariantStatus::Skipped(_)), "{status:?}");
    }

    #[test]
    #[serial]
    fn test_ranking_violation_when_missing_with_pool() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
        bootstrap_pool(tmp.path());
        // No .ranking file written.
        let status = check_token_ranking_fresh(tmp.path(), Duration::from_secs(3600));
        std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
        let InvariantStatus::Violation(detail) = status else {
            panic!("expected violation, got {status:?}");
        };
        assert!(detail.contains(".ranking"), "{detail}");
    }

    #[test]
    #[serial]
    fn test_ranking_ok_when_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
        let dir = bootstrap_pool(tmp.path());
        fs::write(dir.join(".ranking"), "agent-1\n").unwrap();
        let status = check_token_ranking_fresh(tmp.path(), Duration::from_secs(3600));
        std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
        assert_eq!(status, InvariantStatus::Ok);
    }

    /// The exact 2026-08-03 condition #5: a `.ranking` old enough to have
    /// drifted from live rate-limit state. We backdate the file's mtime and use
    /// a tight threshold.
    #[test]
    #[serial]
    fn test_ranking_violation_when_stale() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
        let dir = bootstrap_pool(tmp.path());
        let ranking = dir.join(".ranking");
        fs::write(&ranking, "agent-1\n").unwrap();
        // Backdate mtime by two hours.
        let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
        let f = fs::File::options().write(true).open(&ranking).unwrap();
        f.set_modified(two_hours_ago).unwrap();
        drop(f);

        let status = check_token_ranking_fresh(tmp.path(), Duration::from_secs(3600));
        std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
        let InvariantStatus::Violation(detail) = status else {
            panic!("expected violation, got {status:?}");
        };
        assert!(detail.contains("old"), "{detail}");
    }

    // ===================================================================
    // Repair — token ranking via injected daemon bin
    // ===================================================================

    #[test]
    fn test_repair_token_ranking_success_via_fake_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = write_fake_bin(tmp.path(), "fake-daemon.sh", "exit 0");
        let ctx = RepairContext {
            daemon_bin: Some(bin),
            ..RepairContext::default()
        };
        let outcome = repair_token_ranking(tmp.path(), &ctx);
        assert!(outcome.is_repaired(), "{outcome:?}");
    }

    #[test]
    fn test_repair_token_ranking_failure_via_fake_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = write_fake_bin(tmp.path(), "fake-daemon.sh", "echo boom; exit 1");
        let ctx = RepairContext {
            daemon_bin: Some(bin),
            ..RepairContext::default()
        };
        let outcome = repair_token_ranking(tmp.path(), &ctx);
        assert!(matches!(outcome, RepairOutcome::Failed(_)), "{outcome:?}");
    }

    // ===================================================================
    // Repair — MCP live-sweep guard
    // ===================================================================

    #[test]
    fn test_mcp_repair_refused_under_live_sweep() {
        let tmp = tempfile::tempdir().unwrap();
        make_mcp(tmp.path(), Some("x"), true, false);
        // Simulate a live sweep: a worktree with a .loom-in-use marker.
        let wt = tmp.path().join(".loom").join("worktrees").join("issue-1");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".loom-in-use"), "").unwrap();
        assert!(live_sweep_in_progress(tmp.path()));

        let outcome = repair_mcp_bundle(tmp.path(), &RepairContext::default());
        let RepairOutcome::NotAttempted(reason) = outcome else {
            panic!("expected NotAttempted, got {outcome:?}");
        };
        assert!(reason.contains("live sweep"), "{reason}");
    }

    #[test]
    fn test_live_sweep_false_when_no_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!live_sweep_in_progress(tmp.path()));
    }

    // ===================================================================
    // Issue filing — exactly one per condition (dedup)
    // ===================================================================

    struct FakeReporter {
        already_open: bool,
        lookup_calls: Arc<AtomicUsize>,
        file_calls: Arc<AtomicUsize>,
        next_issue: u64,
    }

    impl ViolationReporter for FakeReporter {
        fn has_open_issue(&self, _marker: &str) -> Result<bool, String> {
            self.lookup_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.already_open)
        }
        fn file_issue(&self, _title: &str, _body: &str) -> Result<u64, String> {
            self.file_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.next_issue)
        }
    }

    fn violation_outcome() -> InvariantOutcome {
        InvariantOutcome {
            invariant: Invariant::RuntimesPresent,
            status: InvariantStatus::Violation("runtimes dir absent".to_string()),
        }
    }

    #[test]
    fn test_report_files_one_issue_when_none_open() {
        let tmp = tempfile::tempdir().unwrap();
        let reporter = FakeReporter {
            already_open: false,
            lookup_calls: Arc::new(AtomicUsize::new(0)),
            file_calls: Arc::new(AtomicUsize::new(0)),
            next_issue: 4242,
        };
        let outcome = report_violation(&reporter, tmp.path(), &violation_outcome());
        assert_eq!(outcome, ReportOutcome::Filed(4242));
        assert_eq!(reporter.file_calls.load(Ordering::SeqCst), 1);
    }

    /// The #4736 failure mode this dedup exists to prevent: a repeat pass with
    /// an already-open issue must NOT file (or comment) again.
    #[test]
    fn test_report_does_not_duplicate_when_already_open() {
        let tmp = tempfile::tempdir().unwrap();
        let reporter = FakeReporter {
            already_open: true,
            lookup_calls: Arc::new(AtomicUsize::new(0)),
            file_calls: Arc::new(AtomicUsize::new(0)),
            next_issue: 1,
        };
        let outcome = report_violation(&reporter, tmp.path(), &violation_outcome());
        assert_eq!(outcome, ReportOutcome::AlreadyOpen);
        assert_eq!(reporter.file_calls.load(Ordering::SeqCst), 0, "must not file a duplicate");
    }

    #[test]
    fn test_report_body_carries_dedup_marker() {
        // Prove the filed body contains the marker the next pass dedupes on.
        struct CapturingReporter {
            body: std::sync::Mutex<String>,
        }
        impl ViolationReporter for CapturingReporter {
            fn has_open_issue(&self, _marker: &str) -> Result<bool, String> {
                Ok(false)
            }
            fn file_issue(&self, _title: &str, body: &str) -> Result<u64, String> {
                *self.body.lock().unwrap() = body.to_string();
                Ok(7)
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let reporter = CapturingReporter {
            body: std::sync::Mutex::new(String::new()),
        };
        let out = report_violation(&reporter, tmp.path(), &violation_outcome());
        assert_eq!(out, ReportOutcome::Filed(7));
        let body = reporter.body.lock().unwrap().clone();
        assert!(body.contains(&Invariant::RuntimesPresent.issue_marker()), "{body}");
    }

    // ===================================================================
    // Config surface + precedence (env > config > default)
    // ===================================================================

    #[test]
    fn test_config_missing_file_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_config(tmp.path()), InstallSelfCheckConfig::default());
    }

    #[test]
    fn test_config_reads_all_fields() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"installSelfCheck": {"enabled": true, "intervalSecs": 900, "repair": true, "tokenRankingMaxAgeSecs": 120}}}"#,
        );
        assert_eq!(
            read_config(tmp.path()),
            InstallSelfCheckConfig {
                enabled: Some(true),
                interval_secs: Some(900),
                repair: Some(true),
                ranking_max_age_secs: Some(120),
            }
        );
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_default_is_false() {
        std::env::remove_var(INSTALL_SELF_CHECK_ENABLE_ENV);
        assert!(
            !resolve_enabled(&InstallSelfCheckConfig::default()),
            "absent config + unset env ⇒ default OFF (FLAGS-OFF)"
        );
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_env_overrides_config() {
        std::env::set_var(INSTALL_SELF_CHECK_ENABLE_ENV, "1");
        assert!(resolve_enabled(&InstallSelfCheckConfig {
            enabled: Some(false),
            ..Default::default()
        }));
        std::env::set_var(INSTALL_SELF_CHECK_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&InstallSelfCheckConfig {
            enabled: Some(true),
            ..Default::default()
        }));
        std::env::remove_var(INSTALL_SELF_CHECK_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_repair_default_is_report_only() {
        std::env::remove_var(INSTALL_SELF_CHECK_REPAIR_ENV);
        assert!(
            !resolve_repair(&InstallSelfCheckConfig::default()),
            "report-only is the default"
        );
        assert!(resolve_repair(&InstallSelfCheckConfig {
            repair: Some(true),
            ..Default::default()
        }));
    }

    #[test]
    #[serial]
    fn test_resolve_interval_precedence() {
        std::env::remove_var(INSTALL_SELF_CHECK_INTERVAL_ENV);
        assert_eq!(
            resolve_interval(&InstallSelfCheckConfig::default()),
            Duration::from_secs(DEFAULT_INTERVAL_SECS)
        );
        assert_eq!(
            resolve_interval(&InstallSelfCheckConfig {
                interval_secs: Some(300),
                ..Default::default()
            }),
            Duration::from_secs(300)
        );
        std::env::set_var(INSTALL_SELF_CHECK_INTERVAL_ENV, "45");
        assert_eq!(
            resolve_interval(&InstallSelfCheckConfig {
                interval_secs: Some(300),
                ..Default::default()
            }),
            Duration::from_secs(45)
        );
        std::env::remove_var(INSTALL_SELF_CHECK_INTERVAL_ENV);
    }

    // ===================================================================
    // run_pass — report-only does not act; repair mode repairs
    // ===================================================================

    #[test]
    #[serial]
    fn test_run_pass_report_only_does_not_repair_or_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
        make_defaults_runtimes(tmp.path(), &["claude"]);
        // runtimes violation present (no .loom/runtimes yet).
        let reporter = FakeReporter {
            already_open: false,
            lookup_calls: Arc::new(AtomicUsize::new(0)),
            file_calls: Arc::new(AtomicUsize::new(0)),
            next_issue: 1,
        };
        let report = run_pass(
            tmp.path(),
            false, // report-only
            CheckOptions::default(),
            &RepairContext::default(),
            &reporter,
        );
        std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
        assert!(!report.is_all_ok(), "runtimes violation expected");
        // report-only: no repair happened, and the .loom/runtimes dir is still absent.
        assert!(!tmp
            .path()
            .join(".loom")
            .join("runtimes")
            .join("claude.json")
            .exists());
        assert_eq!(reporter.file_calls.load(Ordering::SeqCst), 0, "report-only must not file");
    }

    #[test]
    #[serial]
    fn test_run_pass_repair_mode_converges_runtimes() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");
        make_defaults_runtimes(tmp.path(), &["claude"]);
        let reporter = FakeReporter {
            already_open: false,
            lookup_calls: Arc::new(AtomicUsize::new(0)),
            file_calls: Arc::new(AtomicUsize::new(0)),
            next_issue: 1,
        };
        run_pass(tmp.path(), true, CheckOptions::default(), &RepairContext::default(), &reporter);
        std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
        // repair mode converged the runtimes file.
        assert!(tmp
            .path()
            .join(".loom")
            .join("runtimes")
            .join("claude.json")
            .is_file());
    }

    // ===================================================================
    // resolve_tool_path
    // ===================================================================

    /// The search path is **injected**, never `set_var`'d (#5961): this test
    /// previously replaced the process-global `PATH` with a tempdir and never
    /// restored it, so every test that ran afterwards in the same process lost
    /// its ability to spawn a bare-name `git`/`gh`.
    #[test]
    fn test_resolve_tool_path_finds_on_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = write_fake_bin(tmp.path(), "mytool", "exit 0");
        let resolved = resolve_tool_path_in("mytool", Some(tmp.path().as_os_str()));
        assert_eq!(resolved.as_deref(), Some(bin.as_path()));
    }

    /// A tool absent from the injected search path falls through to the
    /// hard-coded Homebrew / usr-local fallbacks and is not found there
    /// either — the "explicitly resolve rather than trust `PATH`" contract
    /// (#4875) with no process-global mutation.
    #[test]
    fn test_resolve_tool_path_returns_none_when_absent_everywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved =
            resolve_tool_path_in("definitely-not-a-real-tool-5961", Some(tmp.path().as_os_str()));
        assert_eq!(resolved, None);
    }

    /// The public entry point still resolves a real tool end-to-end — the
    /// production wiring [`resolve_tool_path_in`] must not silently drop. Not
    /// asserted against a synthetic `PATH`, because injecting one into this
    /// process is exactly the process-global mutation this change removes
    /// (#5961).
    #[test]
    fn test_resolve_tool_path_resolves_a_real_tool_via_the_process_env() {
        let sh = resolve_tool_path("sh").expect("`sh` resolves on any supported host");
        assert!(sh.ends_with("sh"), "resolved an unexpected path: {}", sh.display());
        assert!(is_executable(&sh), "resolved a non-executable path: {}", sh.display());
    }
}
