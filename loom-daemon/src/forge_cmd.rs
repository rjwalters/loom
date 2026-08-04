//! Native `loom-daemon forge` subcommand group — the Rust port of the
//! `loom_tools.forge_cli` (`loom-forge`) and `loom_tools.auto_merge`
//! (`loom-auto-merge`) Python CLIs (epic #4081 Phase 3, family 3).
//!
//! # Scope
//!
//! This ports **exactly** the caller surface the shell scripts use, not the
//! full `forge_cli.py` gh-mirror:
//!
//! - `forge issue <args…>` / `forge pr <args…>` / `forge auth <args…>` — the
//!   read/query surface (`issue list/view`, `pr list`, `auth status`).
//! - `forge auto-merge <pr> [--method …]` — the merge path behind
//!   `merge-pr.sh` (formerly `loom-auto-merge`).
//!
//! # NOT a cache — the passthrough burns full GraphQL (#5056)
//!
//! `forge issue …` / `forge pr …` are a **byte-identical GraphQL passthrough**
//! to `gh` ([`gh_passthrough`]); they inherit `gh issue list`'s full GraphQL
//! rate-limit cost and are **not** the ETag-cached path. The cached,
//! zero-cost-on-`304` listing lives behind the explicit `--cached` flag
//! (`forge issue list --cached …` / `forge pr list --cached …`, dispatched to
//! [`crate::forge_cached_list`]); only that flag routes through the REST/ETag
//! cache. Reach for it (via the `gh-cached` / `$GH_READ` helper) whenever you
//! would otherwise write a raw `gh issue list` in an agent hot path.
//!
//! # Forge routing (option (b): native GitHub, shell fallback carries Gitea)
//!
//! For **GitHub** (the only forge any Loom consumer repo runs today) the
//! subcommands are native:
//! - reads/auth are a byte-identical passthrough to `gh` (the same binary the
//!   scripts' `FORGE=gh` fallback uses — so a consumer workspace with zero pip
//!   installs works unchanged). Being byte-identical, the passthrough also
//!   inherits `gh`'s transport: `forge issue create` is GraphQL-backed and has
//!   **no REST fallback** for GraphQL-quota exhaustion (#5047). That is
//!   deliberate — the passthrough contract is that `forge issue X` behaves
//!   identically to `gh issue X` — so it emits an advisory stderr notice
//!   pointing at `.loom/scripts/create-issue.sh`, which does have the fallback;
//! - `auto-merge` enables GitHub's native auto-merge via the
//!   `enablePullRequestAutoMerge` GraphQL mutation (a pure API call with no
//!   working-tree checkout — the reason `gh pr merge --auto` is avoided from
//!   inside a worktree, ported verbatim from `common/github.py`).
//!
//! For **Gitea** the subcommands *decline* with [`EX_FORGE_DECLINED`] so the
//! caller's existing shell fallback carries the poll-and-merge / query path:
//! `merge-pr.sh`'s `forge_auto_merge` (in `lib/forge-helpers.sh`) already
//! implements the identical Gitea curl poll-and-merge, and the read scripts
//! degrade to `gh`. This keeps the daemon's zero-HTTP-client house style
//! intact. The #4061 config-precedence + Gitea hard-fail semantics are still
//! ported and unit-tested here (they gate whether the Gitea path is taken).
//!
//! # #4061 config semantics (binding, tested)
//!
//! - Forge config resolves from the **canonical repo root**
//!   (`git rev-parse --git-common-dir`), never the worktree CWD.
//! - `LOOM_FORGE_TYPE` beats config; `forge.type == "auto"`/unknown falls
//!   through to remote-URL autodetect defaulting to GitHub.
//! - `resolve_forge_config` returns empty (never errors) on missing/malformed
//!   config.
//! - `GiteaConfig`: `GITEA_TOKEN`/`GITEA_USERNAME` env beat config; init
//!   hard-fails on a missing url or a missing token; the base URL is stripped
//!   of a trailing slash; Basic-auth (username set) rejects a `:` in the
//!   username and refuses `http://` unless `LOOM_ALLOW_INSECURE_BASIC_AUTH=1`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use crate::config_resolver::{get_path, resolve_effective_config};

/// Exit code the native subcommands use to signal "this forge is not handled
/// natively; fall back to the shell path". Distinct from a genuine failure
/// (exit 1) so `merge-pr.sh` can tell a Gitea decline from a GitHub auto-merge
/// error (which must flow into the disabled/clean/unstable detection).
pub const EX_FORGE_DECLINED: i32 = 3;

/// The two forge backends Loom understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeType {
    GitHub,
    Gitea,
}

impl ForgeType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ForgeType::GitHub => "github",
            ForgeType::Gitea => "gitea",
        }
    }

    fn parse(s: &str) -> Option<ForgeType> {
        match s.trim().to_ascii_lowercase().as_str() {
            "github" => Some(ForgeType::GitHub),
            "gitea" => Some(ForgeType::Gitea),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical-root resolution (#3938 / #4061)
// ---------------------------------------------------------------------------

/// Resolve the canonical main-checkout root for forge operations.
///
/// Forge credentials (`forge.gitea.url` / token / detected type) are a
/// per-repo fact, not a per-worktree one — the same reasoning #3938 applied to
/// the `.loom/tokens/` pool. Runs `git rev-parse --git-common-dir` relative to
/// `cwd` (or the process cwd when `cwd` is `None`), makes it absolute, and
/// takes its parent — the canonical repo root whether invoked from the main
/// checkout or a linked worktree.
///
/// Falls back to `cwd` (or the process cwd) unchanged — never fails — when git
/// is unavailable or `cwd` is not inside a git repository at all.
#[must_use]
pub fn canonical_repo_root(cwd: Option<&Path>) -> PathBuf {
    let base: PathBuf = cwd
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let mut command = Command::new("git");
    command.args(["rev-parse", "--git-common-dir"]);
    command.current_dir(&base);

    let output = match command.output() {
        Ok(o) => o,
        Err(_) => return base,
    };
    if !output.status.success() {
        return base;
    }
    let raw = match String::from_utf8(output.stdout) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return base,
    };
    if raw.is_empty() {
        return base;
    }

    let common_dir = PathBuf::from(&raw);
    let abs = if common_dir.is_absolute() {
        common_dir
    } else {
        base.join(common_dir)
    };
    // Canonicalize best-effort; fall back to the joined path if it can't be
    // canonicalized (e.g. a symlinked tmp path in a test).
    let abs = std::fs::canonicalize(&abs).unwrap_or(abs);

    abs.parent().map(Path::to_path_buf).unwrap_or(base)
}

// ---------------------------------------------------------------------------
// Forge config + detection (#4061)
// ---------------------------------------------------------------------------

/// Read the `forge` object from the effective config, resolved from the
/// canonical repo root (not `cwd` when `cwd` is a worktree). Returns an empty
/// object on a missing/malformed config or a non-object `forge` value — never
/// errors.
#[must_use]
pub fn resolve_forge_config(cwd: Option<&Path>) -> Value {
    let root = canonical_repo_root(cwd);
    let effective = resolve_effective_config(&root);
    match get_path(&effective, "forge") {
        Some(v @ Value::Object(_)) => v.clone(),
        _ => Value::Object(serde_json::Map::new()),
    }
}

/// Extract the hostname from a git remote URL (SSH or HTTPS form).
fn parse_host(url: &str) -> Option<String> {
    // SSH: git@host:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some((host, _)) = rest.split_once(':') {
            if !host.is_empty() {
                return Some(host.to_string());
            }
        }
    }
    // HTTPS: https://host/owner/repo(.git)
    for scheme in ["https://", "http://"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            let host = rest.split('/').next().unwrap_or("");
            if !host.is_empty() {
                return Some(host.to_string());
            }
        }
    }
    None
}

/// Get the `origin` remote URL from `cwd`, or `None`.
fn get_remote_url(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

/// Determine forge type from a hostname, consulting the configured Gitea URL.
fn detect_from_host(host: &str, forge_config: &Value) -> ForgeType {
    if host == "github.com" {
        return ForgeType::GitHub;
    }
    if let Some(Value::String(gitea_url)) = get_path(forge_config, "gitea.url") {
        if let Some(cfg_host) = parse_host(gitea_url) {
            if cfg_host == host {
                return ForgeType::Gitea;
            }
        }
    }
    // Default to GitHub for unknown hosts (backward compatible).
    ForgeType::GitHub
}

/// Detect the forge type for the repository containing `cwd`.
///
/// Resolution order (mirrors `common/forge.py::detect_forge`):
/// 1. `LOOM_FORGE_TYPE` env var (`github` | `gitea`);
/// 2. `forge.type` config field (when not `auto`/empty);
/// 3. autodetect from the git remote host;
/// 4. default GitHub.
#[must_use]
pub fn detect_forge(cwd: Option<&Path>) -> ForgeType {
    // 1. Environment override (highest priority).
    if let Ok(env_val) = std::env::var("LOOM_FORGE_TYPE") {
        let trimmed = env_val.trim();
        if !trimmed.is_empty() {
            if let Some(ft) = ForgeType::parse(trimmed) {
                return ft;
            }
            // Invalid value: fall through to the other detection methods.
        }
    }

    let root = canonical_repo_root(cwd);
    let forge_config = resolve_forge_config(cwd);

    // 2. Config override.
    if let Some(Value::String(config_type)) = get_path(&forge_config, "type") {
        let lowered = config_type.trim().to_ascii_lowercase();
        if !lowered.is_empty() && lowered != "auto" {
            if let Some(ft) = ForgeType::parse(&lowered) {
                return ft;
            }
        }
    }

    // 3. Autodetect from the git remote host.
    if let Some(url) = get_remote_url(&root) {
        if let Some(host) = parse_host(&url) {
            return detect_from_host(&host, &forge_config);
        }
    }

    // 4. Default GitHub (backward compatible).
    ForgeType::GitHub
}

// ---------------------------------------------------------------------------
// Gitea config (#4061 hard-fail + precedence semantics)
// ---------------------------------------------------------------------------

/// Resolved Gitea connection config. Constructing one enforces the #4061
/// hard-fail semantics (missing url / missing token error out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GiteaConfig {
    /// Base URL with any trailing slash stripped.
    pub base_url: String,
    /// API token (or Basic-auth password when `username` is set).
    pub token: String,
    /// Basic-auth username, when in Basic-auth mode.
    pub username: Option<String>,
}

/// Build a [`GiteaConfig`] from the resolved `forge` object, applying env
/// precedence and the #4061 hard-fail rules. `GITEA_TOKEN` / `GITEA_USERNAME`
/// env vars beat the config values.
///
/// Errors when the base URL is missing (after trailing-slash stripping), when
/// no token is available, when a Basic-auth username contains a `:`, or when
/// Basic auth is attempted over `http://` without
/// `LOOM_ALLOW_INSECURE_BASIC_AUTH=1`.
pub fn gitea_config_from_forge(forge: &Value) -> Result<GiteaConfig> {
    let gitea = match get_path(forge, "gitea") {
        Some(v @ Value::Object(_)) => v.clone(),
        _ => Value::Object(serde_json::Map::new()),
    };

    let cfg_str = |key: &str| -> String {
        match get_path(&gitea, key) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        }
    };

    // Base URL (required), trailing slash stripped.
    let base_url = cfg_str("url").trim_end_matches('/').to_string();
    if base_url.is_empty() {
        bail!(
            "Gitea base URL is required. Set forge.gitea.url in .loom/config.json \
             (e.g. \"https://gitea.example.com\")"
        );
    }

    // Token: env beats config.
    let token = match std::env::var("GITEA_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => cfg_str("token"),
    };
    if token.is_empty() {
        bail!(
            "Gitea API token is required. Set GITEA_TOKEN env var or \
             forge.gitea.token in .loom/config.json"
        );
    }

    // Username: env beats config. When set, switches to Basic auth.
    let username_val = match std::env::var("GITEA_USERNAME") {
        Ok(u) if !u.is_empty() => u,
        _ => cfg_str("username"),
    };

    let username = if username_val.is_empty() {
        None
    } else {
        if username_val.contains(':') {
            bail!(
                "Gitea username must not contain ':' (HTTP Basic Auth disallows \
                 colons in usernames)."
            );
        }
        if base_url.starts_with("http://")
            && std::env::var("LOOM_ALLOW_INSECURE_BASIC_AUTH").as_deref() != Ok("1")
        {
            bail!(
                "Gitea Basic Auth requires HTTPS to avoid leaking credentials. Set \
                 forge.gitea.url to an https:// URL, or set \
                 LOOM_ALLOW_INSECURE_BASIC_AUTH=1 to override (not recommended)."
            );
        }
        Some(username_val)
    };

    Ok(GiteaConfig {
        base_url,
        token,
        username,
    })
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

/// Resolve the `gh` binary name (honoring `LOOM_GH_BIN` for tests / overrides).
fn gh_bin() -> String {
    std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

/// Passthrough the given `gh` args (entity prepended), inheriting stdio and
/// propagating the exit code. On GitHub this is byte-identical to the scripts'
/// `FORGE=gh` fallback. Never returns on success/failure of `gh` (calls
/// `std::process::exit`); returns `Err` only when `gh` cannot be spawned.
/// Whether a `forge <entity> <args…>` invocation is an issue *creation*, i.e.
/// `forge issue create …`. Used only to emit the #5047 advisory notice.
fn is_issue_create(entity: &str, args: &[String]) -> bool {
    entity == "issue" && args.first().is_some_and(|a| a == "create")
}

fn gh_passthrough(entity: &str, args: &[String]) -> Result<()> {
    let ft = detect_forge(None);
    if ft == ForgeType::Gitea {
        eprintln!(
            "loom-daemon forge: gitea is not handled natively; falling back to the \
             caller's shell path"
        );
        std::process::exit(EX_FORGE_DECLINED);
    }

    // #5047: `forge issue` is a byte-identical passthrough to `gh issue`, so
    // `forge issue create` inherits `gh issue create`'s GraphQL cost and dies
    // on exactly the same GraphQL-quota exhaustion. It is NOT an escape hatch,
    // and being a *daemon* subcommand it looks like one. Keep the passthrough
    // contract intact (silently rerouting one subcommand's transport would
    // break every caller that parses gh's output) and point at the tool that
    // does have the REST fallback. Advisory only: stderr, no behavior change.
    if is_issue_create(entity, args) {
        eprintln!(
            "loom-daemon forge issue create: NOTE — this is a byte-identical passthrough \
             to `gh issue create` (GraphQL-backed) with NO REST fallback. If GraphQL quota \
             is exhausted, use `.loom/scripts/create-issue.sh` instead (see \
             docs/github-authentication.md → \"Filing issues under GraphQL exhaustion\")."
        );
    }

    let mut command = Command::new(gh_bin());
    command.arg(entity);
    command.args(args);
    let status = command
        .status()
        .map_err(|e| anyhow!("failed to exec gh: {e}"))?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Enable GitHub auto-merge for `pr` via the `enablePullRequestAutoMerge`
/// GraphQL mutation (ported from `common/github.py::auto_merge_pull_request`).
/// The `poll_interval` / `timeout` args are accepted for CLI compatibility but
/// ignored on GitHub (the server queues the merge). Returns the process exit
/// code to use.
fn github_auto_merge(pr: u32, method: &str) -> i32 {
    let gh = gh_bin();

    // Resolve the repo NWO (owner/repo).
    let nwo = match repo_nwo(&gh) {
        Some(n) => n,
        None => {
            eprintln!("Failed to enable auto-merge for PR #{pr}: could not resolve repository NWO");
            return 1;
        }
    };
    let (owner, repo) = match nwo.split_once('/') {
        Some((o, r)) => (o, r),
        None => {
            eprintln!("Failed to enable auto-merge for PR #{pr}: malformed repo NWO {nwo:?}");
            return 1;
        }
    };

    // Step 1: look up the PR node_id (required by the mutation).
    let node_out = Command::new(&gh)
        .args([
            "api",
            &format!("repos/{owner}/{repo}/pulls/{pr}"),
            "--jq",
            ".node_id",
        ])
        .output();
    let node_id = match node_out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            eprintln!(
                "Failed to enable auto-merge for PR #{pr}: could not fetch node_id: {}",
                err.trim()
            );
            return 1;
        }
        Err(e) => {
            eprintln!("Failed to enable auto-merge for PR #{pr}: could not fetch node_id: {e}");
            return 1;
        }
    };
    if node_id.is_empty() {
        eprintln!("Failed to enable auto-merge for PR #{pr}: empty node_id");
        return 1;
    }

    // Step 2: enable auto-merge via GraphQL. mergeMethod must be uppercase.
    let merge_method = method.to_ascii_uppercase();
    let mutation = "mutation($pullRequestId: ID!, $mergeMethod: PullRequestMergeMethod!) {\
  enablePullRequestAutoMerge(input: {\
    pullRequestId: $pullRequestId,\
    mergeMethod: $mergeMethod\
  }) {\
    pullRequest { number autoMergeRequest { enabledAt } }\
  }\
}";
    let result = Command::new(&gh)
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={mutation}"),
            "-F",
            &format!("pullRequestId={node_id}"),
            "-F",
            &format!("mergeMethod={merge_method}"),
        ])
        .output();
    match result {
        Ok(o) if o.status.success() => {
            println!("Auto-merge enabled for PR #{pr}");
            0
        }
        Ok(o) => {
            // Surface gh's stderr verbatim so merge-pr.sh's disabled/clean/
            // unstable substring detection keeps working.
            let err = String::from_utf8_lossy(&o.stderr);
            let out = String::from_utf8_lossy(&o.stdout);
            let detail = if err.trim().is_empty() {
                out.trim()
            } else {
                err.trim()
            };
            eprintln!("Failed to enable auto-merge for PR #{pr}: {detail}");
            1
        }
        Err(e) => {
            eprintln!("Failed to enable auto-merge for PR #{pr}: {e}");
            1
        }
    }
}

/// Resolve `owner/repo`. Prefers `gh repo view --json nameWithOwner`
/// (GitHub); on failure — e.g. GraphQL/API unavailability such as rate-limit
/// exhaustion (#4447) — falls back to parsing `git remote get-url origin`,
/// mirroring the shell helper `forge_get_repo_nwo`
/// (`defaults/scripts/lib/forge-helpers.sh`), which has always had this
/// fallback. The git fallback works offline / under API exhaustion because it
/// never calls out to `gh`.
fn repo_nwo(gh: &str) -> Option<String> {
    repo_nwo_in(gh, None)
}

fn repo_nwo_in(gh: &str, cwd: Option<&Path>) -> Option<String> {
    let mut cmd = Command::new(gh);
    cmd.args([
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "--jq",
        ".nameWithOwner",
    ]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Ok(out) = cmd.output() {
        if out.status.success() {
            if let Ok(raw) = String::from_utf8(out.stdout) {
                let nwo = raw.trim().to_string();
                if !nwo.is_empty() {
                    return Some(nwo);
                }
            }
        }
    }
    // `gh repo view` failed or returned nothing — fall back to the local git
    // remote, exactly as the shell helper does.
    let dir = cwd
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let remote_url = get_remote_url(&dir)?;
    parse_nwo_from_remote_url(&remote_url)
}

/// Extract `owner/repo` from a git remote URL (SSH `git@host:owner/repo.git`
/// or HTTPS `https://host/owner/repo(.git)` form), mirroring the shell sed in
/// `forge_get_repo_nwo` (`defaults/scripts/lib/forge-helpers.sh`).
fn parse_nwo_from_remote_url(url: &str) -> Option<String> {
    let trimmed = url.strip_suffix(".git").unwrap_or(url);
    let mut parts: Vec<&str> = trimmed.rsplit(['/', ':']).take(2).collect();
    if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    parts.reverse();
    Some(parts.join("/"))
}

/// Handle `loom-daemon forge auto-merge`. GitHub goes native (GraphQL); Gitea
/// declines with [`EX_FORGE_DECLINED`] so the shell `forge_auto_merge` carries
/// the poll-and-merge. Never returns (exits the process).
pub fn handle_auto_merge(pr: u32, method: &str) -> Result<()> {
    let ft = detect_forge(None);
    match ft {
        ForgeType::GitHub => std::process::exit(github_auto_merge(pr, method)),
        ForgeType::Gitea => {
            eprintln!(
                "loom-daemon forge auto-merge: gitea is not handled natively; falling \
                 back to the caller's shell poll-and-merge"
            );
            std::process::exit(EX_FORGE_DECLINED);
        }
    }
}

/// Sub-actions for `loom-daemon forge`.
pub enum ForgeCmd {
    /// `forge issue <args…>` — passthrough to `gh issue` on GitHub.
    ///
    /// This is a generic, subcommand-agnostic passthrough: it does not parse
    /// `args` to special-case `create` vs. `list`/`view`/`edit`. As a
    /// consequence `forge issue create` is byte-identical to running `gh
    /// issue create` directly and carries the same GraphQL cost — it does
    /// **not** get the REST fallback documented in
    /// `.loom/docs/gh-issue-create-rest-fallback.md` /
    /// `forge_gh_create_issue_rl_safe` (#5047). Callers that need
    /// exhausted-GraphQL resilience for issue creation should use that
    /// shell recipe/helper directly rather than routing through this
    /// passthrough.
    Issue(Vec<String>),
    /// `forge pr <args…>` — passthrough to `gh pr` on GitHub.
    Pr(Vec<String>),
    /// `forge auth <args…>` — passthrough to `gh auth` on GitHub.
    Auth(Vec<String>),
    /// `forge auto-merge <pr> [--method M]`.
    AutoMerge { pr: u32, method: String },
}

/// Dispatch a parsed `forge` subcommand. Handlers exit the process directly
/// (to propagate `gh`'s exit code faithfully), so this only returns `Err` when
/// a child process cannot be spawned.
pub fn dispatch(cmd: ForgeCmd) -> Result<()> {
    match cmd {
        // `forge <issue|pr> list --cached …` routes to the agent-facing ETag
        // cache (#5056); it serves-and-exits(0) or declines-and-exits(3) so the
        // caller falls back to plain `gh`. Everything else is the byte-identical
        // GraphQL passthrough — NOT the cached path.
        ForgeCmd::Issue(args) if crate::forge_cached_list::is_cached_list(&args) => {
            crate::forge_cached_list::handle("issue", &args)
        }
        ForgeCmd::Pr(args) if crate::forge_cached_list::is_cached_list(&args) => {
            crate::forge_cached_list::handle("pr", &args)
        }
        ForgeCmd::Issue(args) => gh_passthrough("issue", &args),
        ForgeCmd::Pr(args) => gh_passthrough("pr", &args),
        ForgeCmd::Auth(args) => gh_passthrough("auth", &args),
        ForgeCmd::AutoMerge { pr, method } => handle_auto_merge(pr, &method),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;
    use std::process::Command as StdCommand;
    use tempfile::tempdir;

    /// Disable the private-defaults config tier so tests only see the config
    /// they write into the temp repo root.
    fn isolate_config_env() {
        std::env::set_var("LOOM_CONFIG_DEFAULTS_FILE", "");
    }

    fn clear_forge_env() {
        std::env::remove_var("LOOM_FORGE_TYPE");
        std::env::remove_var("GITEA_TOKEN");
        std::env::remove_var("GITEA_USERNAME");
        std::env::remove_var("LOOM_ALLOW_INSECURE_BASIC_AUTH");
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed in {}", dir.display());
    }

    fn init_repo(dir: &Path, remote: &str) {
        git(dir, &["init", "-q"]);
        git(dir, &["remote", "add", "origin", remote]);
    }

    fn write_config(root: &Path, forge: Value) {
        let loom = root.join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(
            loom.join("config.json"),
            serde_json::to_string(&json!({ "forge": forge })).unwrap(),
        )
        .unwrap();
    }

    // ===== #5047: issue-create advisory notice targeting =====

    #[test]
    fn is_issue_create_matches_only_issue_create() {
        let create = vec!["create".to_string(), "--title".to_string(), "T".to_string()];
        assert!(is_issue_create("issue", &create));

        // Other issue subcommands are untouched (no notice, no behavior change).
        assert!(!is_issue_create("issue", &["view".to_string(), "42".to_string()]));
        assert!(!is_issue_create("issue", &["list".to_string()]));
        assert!(!is_issue_create("issue", &[]));

        // `pr create` is not issue creation -- it is not GraphQL-quota
        // equivalent and has no create-issue.sh counterpart.
        assert!(!is_issue_create("pr", &create));
        assert!(!is_issue_create("auth", &create));
    }

    // ===== host parsing =====

    #[test]
    fn parse_host_ssh_and_https() {
        assert_eq!(
            parse_host("git@gitea.example.com:o/r.git").as_deref(),
            Some("gitea.example.com")
        );
        assert_eq!(parse_host("https://github.com/o/r").as_deref(), Some("github.com"));
        assert_eq!(parse_host("http://gitea.local/o/r.git").as_deref(), Some("gitea.local"));
        assert_eq!(parse_host("not-a-url"), None);
    }

    // ===== #4061: LOOM_FORGE_TYPE beats config =====

    #[test]
    #[serial]
    fn env_forge_type_beats_config() {
        isolate_config_env();
        clear_forge_env();
        let dir = tempdir().unwrap();
        init_repo(dir.path(), "https://github.com/o/r.git");
        // Config says gitea, but env says github → env wins.
        write_config(
            dir.path(),
            json!({ "type": "gitea", "gitea": { "url": "https://g.example.com" } }),
        );
        std::env::set_var("LOOM_FORGE_TYPE", "github");
        assert_eq!(detect_forge(Some(dir.path())), ForgeType::GitHub);
        clear_forge_env();
    }

    #[test]
    #[serial]
    fn config_type_used_when_env_absent() {
        isolate_config_env();
        clear_forge_env();
        let dir = tempdir().unwrap();
        init_repo(dir.path(), "https://github.com/o/r.git");
        write_config(
            dir.path(),
            json!({ "type": "gitea", "gitea": { "url": "https://g.example.com" } }),
        );
        assert_eq!(detect_forge(Some(dir.path())), ForgeType::Gitea);
        clear_forge_env();
    }

    // ===== #4061: auto / unknown falls through to host autodetect → github default =====

    #[test]
    #[serial]
    fn auto_type_falls_through_to_host_autodetect() {
        isolate_config_env();
        clear_forge_env();
        let dir = tempdir().unwrap();
        init_repo(dir.path(), "https://github.com/o/r.git");
        write_config(dir.path(), json!({ "type": "auto" }));
        assert_eq!(detect_forge(Some(dir.path())), ForgeType::GitHub);
        clear_forge_env();
    }

    #[test]
    #[serial]
    fn unknown_host_defaults_to_github() {
        isolate_config_env();
        clear_forge_env();
        let dir = tempdir().unwrap();
        init_repo(dir.path(), "https://example.com/o/r.git");
        // No config at all.
        assert_eq!(detect_forge(Some(dir.path())), ForgeType::GitHub);
        clear_forge_env();
    }

    #[test]
    #[serial]
    fn configured_gitea_host_autodetects_gitea() {
        isolate_config_env();
        clear_forge_env();
        let dir = tempdir().unwrap();
        init_repo(dir.path(), "https://gitea.corp.example/o/r.git");
        write_config(dir.path(), json!({ "gitea": { "url": "https://gitea.corp.example" } }));
        assert_eq!(detect_forge(Some(dir.path())), ForgeType::Gitea);
        clear_forge_env();
    }

    // ===== #4061: missing / malformed config yields empty, never panics =====

    #[test]
    #[serial]
    fn missing_config_yields_empty_forge() {
        isolate_config_env();
        clear_forge_env();
        let dir = tempdir().unwrap();
        init_repo(dir.path(), "https://github.com/o/r.git");
        let forge = resolve_forge_config(Some(dir.path()));
        assert_eq!(forge, json!({}));
        // And detection still resolves (to github) without panicking.
        assert_eq!(detect_forge(Some(dir.path())), ForgeType::GitHub);
        clear_forge_env();
    }

    #[test]
    #[serial]
    fn malformed_config_yields_empty_forge() {
        isolate_config_env();
        clear_forge_env();
        let dir = tempdir().unwrap();
        init_repo(dir.path(), "https://github.com/o/r.git");
        let loom = dir.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(loom.join("config.json"), "{ this is not json").unwrap();
        // Must not panic; forge resolves empty; detection falls back to github.
        let forge = resolve_forge_config(Some(dir.path()));
        assert_eq!(forge, json!({}));
        assert_eq!(detect_forge(Some(dir.path())), ForgeType::GitHub);
        clear_forge_env();
    }

    // ===== #4061: Gitea hard-fail pair =====

    #[test]
    #[serial]
    fn gitea_missing_url_hard_fails() {
        clear_forge_env();
        std::env::set_var("GITEA_TOKEN", "tok");
        let forge = json!({ "gitea": { "token": "x" } });
        let err = gitea_config_from_forge(&forge).unwrap_err();
        assert!(err.to_string().contains("base URL is required"), "got: {err}");
        clear_forge_env();
    }

    #[test]
    #[serial]
    fn gitea_missing_token_hard_fails() {
        clear_forge_env();
        let forge = json!({ "gitea": { "url": "https://g.example.com" } });
        let err = gitea_config_from_forge(&forge).unwrap_err();
        assert!(err.to_string().contains("token is required"), "got: {err}");
        clear_forge_env();
    }

    // ===== #4061: GITEA_TOKEN / GITEA_USERNAME beat config =====

    #[test]
    #[serial]
    fn gitea_env_token_beats_config() {
        clear_forge_env();
        std::env::set_var("GITEA_TOKEN", "env-token");
        let forge = json!({ "gitea": { "url": "https://g.example.com", "token": "config-token" } });
        let cfg = gitea_config_from_forge(&forge).unwrap();
        assert_eq!(cfg.token, "env-token");
        clear_forge_env();
    }

    #[test]
    #[serial]
    fn gitea_env_username_beats_config_and_switches_basic() {
        clear_forge_env();
        std::env::set_var("GITEA_TOKEN", "tok");
        std::env::set_var("GITEA_USERNAME", "env-user");
        let forge =
            json!({ "gitea": { "url": "https://g.example.com", "username": "config-user" } });
        let cfg = gitea_config_from_forge(&forge).unwrap();
        assert_eq!(cfg.username.as_deref(), Some("env-user"));
        clear_forge_env();
    }

    // ===== #4061: trailing-slash stripping =====

    #[test]
    #[serial]
    fn gitea_url_trailing_slash_stripped() {
        clear_forge_env();
        std::env::set_var("GITEA_TOKEN", "tok");
        let forge = json!({ "gitea": { "url": "https://g.example.com/" } });
        let cfg = gitea_config_from_forge(&forge).unwrap();
        assert_eq!(cfg.base_url, "https://g.example.com");
        clear_forge_env();
    }

    // ===== #4061: Basic-auth guards =====

    #[test]
    #[serial]
    fn gitea_basic_auth_colon_username_rejected() {
        clear_forge_env();
        std::env::set_var("GITEA_TOKEN", "tok");
        std::env::set_var("GITEA_USERNAME", "bad:user");
        let forge = json!({ "gitea": { "url": "https://g.example.com" } });
        let err = gitea_config_from_forge(&forge).unwrap_err();
        assert!(err.to_string().contains("must not contain ':'"), "got: {err}");
        clear_forge_env();
    }

    #[test]
    #[serial]
    fn gitea_basic_auth_over_http_rejected_without_override() {
        clear_forge_env();
        std::env::set_var("GITEA_TOKEN", "tok");
        std::env::set_var("GITEA_USERNAME", "user");
        let forge = json!({ "gitea": { "url": "http://g.example.com" } });
        let err = gitea_config_from_forge(&forge).unwrap_err();
        assert!(err.to_string().contains("requires HTTPS"), "got: {err}");
        // With the override it succeeds.
        std::env::set_var("LOOM_ALLOW_INSECURE_BASIC_AUTH", "1");
        let cfg = gitea_config_from_forge(&forge).unwrap();
        assert_eq!(cfg.username.as_deref(), Some("user"));
        clear_forge_env();
    }

    // ===== #4061 / #3938: worktree-CWD resolves from canonical root =====

    #[test]
    #[serial]
    fn forge_config_resolves_from_canonical_root_not_worktree_cwd() {
        isolate_config_env();
        clear_forge_env();

        // Main checkout with a committed file so we can branch + add a worktree.
        let main = tempdir().unwrap();
        init_repo(main.path(), "https://github.com/o/r.git");
        git(main.path(), &["config", "user.email", "t@t"]);
        git(main.path(), &["config", "user.name", "t"]);
        std::fs::write(main.path().join("README.md"), "x").unwrap();
        git(main.path(), &["add", "-A"]);
        git(main.path(), &["commit", "-qm", "init"]);

        // The forge config lives ONLY in the main checkout.
        write_config(
            main.path(),
            json!({ "type": "gitea", "gitea": { "url": "https://g.example.com" } }),
        );

        // Create a linked worktree with NO .loom/config.json of its own.
        let wt_parent = tempdir().unwrap();
        let wt = wt_parent.path().join("issue-4273");
        git(
            main.path(),
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "feature/issue-4273",
            ],
        );
        assert!(!wt.join(".loom/config.json").exists());

        // Resolving from INSIDE the worktree must find the main checkout's
        // config (gitea), not fall through to github because the worktree has
        // no config of its own.
        let forge = resolve_forge_config(Some(&wt));
        assert_eq!(get_path(&forge, "type").and_then(Value::as_str), Some("gitea"));
        assert_eq!(detect_forge(Some(&wt)), ForgeType::Gitea);

        // Clean up the worktree registration.
        git(main.path(), &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        clear_forge_env();
    }

    // ===== #4447: repo_nwo() git-remote fallback under gh/GraphQL outage =====

    fn write_failing_gh(dir: &Path) -> PathBuf {
        let path = dir.join("fake-gh.sh");
        std::fs::write(
            &path,
            "#!/bin/sh\necho 'gh: API rate limit exceeded for user ID 1 (HTTP 403)' 1>&2\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    #[test]
    fn repo_nwo_falls_back_to_git_remote_when_gh_fails_ssh() {
        let dir = tempdir().unwrap();
        init_repo(dir.path(), "git@github.com:acme/widgets.git");
        let gh = write_failing_gh(dir.path());
        let nwo = repo_nwo_in(gh.to_str().unwrap(), Some(dir.path()));
        assert_eq!(nwo.as_deref(), Some("acme/widgets"));
    }

    #[test]
    fn repo_nwo_falls_back_to_git_remote_when_gh_fails_https() {
        let dir = tempdir().unwrap();
        init_repo(dir.path(), "https://github.com/acme/widgets.git");
        let gh = write_failing_gh(dir.path());
        let nwo = repo_nwo_in(gh.to_str().unwrap(), Some(dir.path()));
        assert_eq!(nwo.as_deref(), Some("acme/widgets"));
    }

    #[test]
    fn repo_nwo_prefers_gh_when_it_succeeds() {
        let dir = tempdir().unwrap();
        // Deliberately mismatched remote to prove `gh`'s answer wins, not the
        // git-remote fallback.
        init_repo(dir.path(), "git@github.com:other/mismatch.git");
        let path = dir.path().join("fake-gh.sh");
        std::fs::write(&path, "#!/bin/sh\nprintf 'acme/widgets'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        let nwo = repo_nwo_in(path.to_str().unwrap(), Some(dir.path()));
        assert_eq!(nwo.as_deref(), Some("acme/widgets"));
    }

    #[test]
    fn repo_nwo_returns_none_when_both_gh_and_git_remote_fail() {
        let dir = tempdir().unwrap();
        // git-init'd but with NO origin remote configured.
        git(dir.path(), &["init", "-q"]);
        let gh = write_failing_gh(dir.path());
        let nwo = repo_nwo_in(gh.to_str().unwrap(), Some(dir.path()));
        assert_eq!(nwo, None);
    }

    #[test]
    fn parse_nwo_from_remote_url_handles_ssh_and_https() {
        assert_eq!(
            parse_nwo_from_remote_url("git@github.com:acme/widgets.git").as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(
            parse_nwo_from_remote_url("https://github.com/acme/widgets.git").as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(
            parse_nwo_from_remote_url("https://github.com/acme/widgets").as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(parse_nwo_from_remote_url("not-a-remote"), None);
    }
}
