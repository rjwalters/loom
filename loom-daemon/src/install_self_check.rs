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
//! | [`Invariant::ForgeEgressAligned`]| #9984 `gh` would not reach the mandated API origin | no — files/refreshes an issue naming the finding codes; closes it once aligned |
//! | [`Invariant::GhFrontWired`]| #10516 interactive sessions do not reach the agent `gh` front | no — `loom update` re-provisions the SessionStart hook |
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

use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use crate::workspace_registry::WorkspaceRegistry;

mod forge_egress_invariant;
pub mod gh_front_invariant;

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
    /// #9984: when a forge egress policy is configured, `loom-daemon forge
    /// egress assert` is aligned for this repo (routing exit 0). Not
    /// auto-repairable: the issue names the finding codes and closes itself
    /// once the routing verdict is aligned again.
    ForgeEgressAligned,
    /// #10516: interactive sessions (and their Task subagents) reach the
    /// agent `gh` front — the SessionStart `gh-front-env.sh` hook is wired and
    /// its prefix resolves `gh` to the front (or the managed launcher). Not
    /// auto-repairable: see [`gh_front_invariant`].
    GhFrontWired,
}

impl Invariant {
    /// The authoritative set of checked invariants — the single source of truth
    /// (see the module docs).
    pub const ALL: &'static [Invariant] = &[
        Self::McpBundleHealth,
        Self::RuntimesPresent,
        Self::TokenRankingFresh,
        Self::ForgeEgressAligned,
        Self::GhFrontWired,
    ];

    /// Stable machine identifier — used in the issue-dedup marker and logs.
    /// Never change an existing id (it keys the filed-issue dedup).
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::McpBundleHealth => "mcp-bundle-health",
            Self::RuntimesPresent => "runtimes-present",
            Self::TokenRankingFresh => "token-ranking-fresh",
            Self::ForgeEgressAligned => "forge-egress-aligned",
            Self::GhFrontWired => "gh-front-wired",
        }
    }

    /// Human-readable one-line title (used as the filed-issue title prefix).
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::McpBundleHealth => "mcp-loom bundle is not loadable",
            Self::RuntimesPresent => ".loom/runtimes/ is missing configured runtimes",
            Self::TokenRankingFresh => "token-pool .ranking is stale or missing",
            Self::ForgeEgressAligned => "forge egress routing is not aligned with policy",
            Self::GhFrontWired => "interactive sessions do not reach the agent gh front",
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
            Self::ForgeEgressAligned | Self::GhFrontWired => false,
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
#[derive(Debug, Clone)]
pub struct CheckOptions {
    /// Token `.ranking` is considered stale beyond this age.
    pub ranking_max_age: Duration,
    /// Override the forge-egress policy sources (#9999). `None` (production)
    /// means `PolicySources::from_process`.
    pub forge_egress_sources: Option<crate::forge_egress::policy::PolicySources>,
}

impl CheckOptions {
    /// Defaults with forge-egress sources pinned to "unconfigured", so a
    /// test never consults the host's policy.
    #[must_use]
    pub fn hermetic() -> Self {
        Self {
            forge_egress_sources: Some(crate::forge_egress::policy::PolicySources::default()),
            ..Self::default()
        }
    }
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self {
            ranking_max_age: Duration::from_secs(DEFAULT_RANKING_MAX_AGE_SECS),
            forge_egress_sources: None,
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
            status: check(invariant, repo_root, &opts),
        })
        .collect();
    SelfCheckReport { outcomes }
}

/// Dispatch a single invariant's check.
#[must_use]
pub fn check(invariant: Invariant, repo_root: &Path, opts: &CheckOptions) -> InvariantStatus {
    match invariant {
        Invariant::McpBundleHealth => check_mcp_bundle(repo_root),
        Invariant::RuntimesPresent => check_runtimes_present(repo_root),
        Invariant::TokenRankingFresh => check_token_ranking_fresh(repo_root, opts.ranking_max_age),
        Invariant::ForgeEgressAligned => match opts.forge_egress_sources.as_ref() {
            Some(sources) => forge_egress_invariant::check_with(sources, repo_root),
            None => forge_egress_invariant::check(repo_root),
        },
        Invariant::GhFrontWired => gh_front_invariant::check(repo_root),
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
        Invariant::ForgeEgressAligned | Invariant::GhFrontWired => unreachable_repair(),
    }
}

/// Never reached: [`Invariant::auto_repairable`] is false for the invariants
/// routed here, so [`repair`] returns before its `match`.
fn unreachable_repair() -> RepairOutcome {
    RepairOutcome::NotAttempted("not auto-repairable".to_string())
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
        // Surviving a deleted-inode `current_exe()` during a deferred
        // `auto_update` roll (issue #6471) — same helper
        // `token_ranking_refresh::ScriptRankingRefreshRunner::resolve_bin`
        // uses for the identical self-spawn shape.
        None => match crate::daemon_bin_resolve::resolve_daemon_bin() {
            Ok(p) => p,
            Err(e) => return RepairOutcome::Failed(e),
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

    /// The open issue carrying `marker`, as `(number, body)` — for the
    /// refresh/close lifecycle (#9984). Default: none found.
    fn find_open_issue(&self, _marker: &str) -> Result<Option<(u64, String)>, String> {
        Ok(None)
    }

    /// Replace an open issue's body (refresh). Default: unsupported.
    fn update_issue_body(&self, _number: u64, _body: &str) -> Result<(), String> {
        Err("unsupported by this reporter".to_string())
    }

    /// Close a resolved issue with a comment. Default: unsupported.
    fn close_issue(&self, _number: u64, _comment: &str) -> Result<(), String> {
        Err("unsupported by this reporter".to_string())
    }
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

/// A self-check `gh` call through the facade (#10089): counted in
/// `forge_call_stats`, bounded, and run under `root`'s owner credential.
fn gh_self_check(op: &'static str, intent: AccessIntent, root: &Path) -> GhInvocation {
    let timeout = Duration::from_secs(60);
    GhInvocation::new(Operation::new(op), intent, GhTarget::None, timeout).current_dir(root)
}

impl ViolationReporter for GhIssueFiler {
    fn has_open_issue(&self, marker: &str) -> Result<bool, String> {
        // `gh issue list --search "<marker>" --state open` — GitHub full-text
        // search matches the hidden HTML comment in the body.
        // #10089: through the facade, which also selects the owner-correct
        // credential for a cross-owner repo_root (#5431).
        let output = gh_self_check("self_check.issue_search", AccessIntent::Read, &self.repo_root)
            .args([
                "issue", "list", "--state", "open", "--search", marker, "--json", "number",
            ])
            .run()
            .into_result()
            .map_err(|e| format!("could not run gh: {e}"))?;
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
        let fields = ["--title", title, "--body", body, "--label", "loom:triage"];
        let output = if script.is_file() {
            let mut cmd = Command::new(&script);
            cmd.args(fields).current_dir(&self.repo_root);
            // #5431: the owner-correct credential, inherited by the script's `gh`.
            crate::credential_preflight::apply_gh_config_for_root(&mut cmd, &self.repo_root);
            cmd.output().map_err(|e| e.to_string())
        } else {
            gh_self_check("self_check.issue_create", AccessIntent::Write, &self.repo_root)
                .args(["issue", "create"])
                .args(fields)
                .run()
                .into_result()
                .map_err(|e| e.to_string())
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

    fn find_open_issue(&self, marker: &str) -> Result<Option<(u64, String)>, String> {
        forge_egress_invariant::gh_find_open_issue(&self.repo_root, marker)
    }

    fn update_issue_body(&self, number: u64, body: &str) -> Result<(), String> {
        forge_egress_invariant::gh_issue_op(
            &self.repo_root,
            "issue.edit",
            &["edit", &number.to_string(), "--body", body],
        )
    }

    fn close_issue(&self, number: u64, comment: &str) -> Result<(), String> {
        forge_egress_invariant::gh_issue_op(
            &self.repo_root,
            "issue.close",
            &["close", &number.to_string(), "--comment", comment],
        )
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
        if outcome.invariant == Invariant::ForgeEgressAligned {
            // #9984: file/refresh on violation, close once aligned.
            forge_egress_invariant::reconcile(repo_root, outcome, repair_mode, reporter);
            continue;
        }
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
                    forge_egress_sources: None,
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
mod tests;
