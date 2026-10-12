//! `loom-daemon branch-update strategy` (#10771) — the one answer to "may this
//! repository's PR branches be rebased?".
//!
//! # Why this exists
//!
//! Every role that brings a PR branch up to date (Doctor, Judge, Builder, the
//! stacked-PR scripts) used to rebase onto the default branch and force-push.
//! In a repository that forbids rebase that is wrong three ways at once:
//!
//! - it deletes merge commits other agents already pushed, along with the
//!   record of how their conflicts were resolved (#9282);
//! - every commit gets a new SHA, so anything that cites a head (a reviewer's
//!   or ratification key's sign-off, an evidence record) now points at a
//!   commit the PR no longer contains (#10771's 2026-10-11 evidence);
//! - the author's next ordinary push is rejected as non-fast-forward.
//!
//! A pinned `--force-with-lease` (#9487) does not help: the lease is true, the
//! rewrite is still a rewrite.
//!
//! So the roles ask this subcommand first. `merge` means: `git merge` the
//! default branch in (a merge commit), resolve conflicts, and push **without
//! force**; a rejected push means fetch + merge again, never force. `rebase`
//! keeps the old behaviour, with #9487's pinned lease.
//!
//! # Precedence (first match wins)
//!
//! 1. `LOOM_BRANCH_UPDATE_STRATEGY` (`merge` | `rebase` | `auto`).
//! 2. `branchUpdateStrategy` in the effective `.loom/config.json` stack of the
//!    primary checkout (same tiers every other config key reads).
//! 3. `auto` (the default): the forge's own merge settings. `rebase` only when
//!    the repository is **known** to allow rebase merges
//!    (`allow_rebase_merge = true`); everything else — disabled, hidden from
//!    this credential, unreadable, Gitea (not probed natively), no resolvable
//!    `owner/repo` — is `merge`.
//!
//! An override that is set but not one of the three words is `merge`, not a
//! fall-through: the operator expressed an intent we cannot read, and merge is
//! the answer that can never destroy history.
//!
//! # Caching
//!
//! A forge answer is cached for [`CACHE_TTL`] in the git common dir (shared by
//! every linked worktree, so one role run's several calls cost one API read).
//! Only a successful read is cached: an unknown is retried next call.
//!
//! # Contract
//!
//! Always exits 0 and prints exactly `merge` or `rebase` on stdout; the reason
//! goes to stderr (suppressed by `--quiet`). There is no error outcome a caller
//! could mishandle into a rebase.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config_resolver::{get_path, resolve_effective_config};
use crate::forge_cmd::{canonical_repo_root, detect_forge, gh_bin, ForgeType, FORGE_CMD_TIMEOUT};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// Environment override, highest precedence.
pub const ENV_OVERRIDE: &str = "LOOM_BRANCH_UPDATE_STRATEGY";

/// `.loom/config.json` key, below the env override.
pub const CONFIG_KEY: &str = "branchUpdateStrategy";

/// How long a forge answer is reused. Long enough to cover one role run's
/// several calls, short enough that a settings change is seen the same hour.
pub const CACHE_TTL: Duration = Duration::from_secs(600);

/// Cache file name, inside `git rev-parse --git-common-dir`.
const CACHE_FILE: &str = "loom-branch-update-strategy.json";

/// How a PR branch is brought up to date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// `git merge <remote>/<default>` with a merge commit, then a plain push.
    Merge,
    /// `git rebase <remote>/<default>`, then a pinned `--force-with-lease`.
    Rebase,
}

impl Strategy {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Merge => "merge",
            Strategy::Rebase => "rebase",
        }
    }
}

/// An override's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setting {
    Merge,
    Rebase,
    Auto,
}

fn parse_setting(raw: &str) -> Option<Setting> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "merge" => Some(Setting::Merge),
        "rebase" => Some(Setting::Rebase),
        "auto" => Some(Setting::Auto),
        _ => None,
    }
}

/// The forge's merge flags. `Option` on purpose: a flag this credential cannot
/// see must read as "unknown", never as `false`-and-therefore-a-decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeSettings {
    #[serde(default)]
    pub allow_merge_commit: Option<bool>,
    #[serde(default)]
    pub allow_squash_merge: Option<bool>,
    #[serde(default)]
    pub allow_rebase_merge: Option<bool>,
}

/// What the forge probe produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// The repository's settings, as read (or cached).
    Known(MergeSettings),
    /// The settings could not be read; the string says why.
    Unknown(String),
}

/// The answer plus the one-line reason printed to stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub strategy: Strategy,
    pub reason: String,
}

fn merge(reason: String) -> Decision {
    Decision {
        strategy: Strategy::Merge,
        reason,
    }
}

/// The pure decision. `env` and `config` are the raw override values (an
/// empty env string counts as unset); `probe` is only called in `auto` mode.
pub fn decide(
    env: Option<&str>,
    config: Option<&Value>,
    probe: impl FnOnce() -> Probe,
) -> Decision {
    let explicit = |source: &str, raw: &str| -> Result<Setting, Decision> {
        parse_setting(raw).ok_or_else(|| {
            merge(format!("{source} is '{raw}', not merge|rebase|auto — failing toward merge"))
        })
    };

    let setting = match env.filter(|v| !v.trim().is_empty()) {
        Some(raw) => match explicit(ENV_OVERRIDE, raw) {
            Ok(s) => Some((s, ENV_OVERRIDE)),
            Err(d) => return d,
        },
        None => match config {
            None | Some(Value::Null) => None,
            Some(Value::String(raw)) => match explicit(CONFIG_KEY, raw) {
                Ok(s) => Some((s, CONFIG_KEY)),
                Err(d) => return d,
            },
            Some(other) => {
                return merge(format!(
                    "{CONFIG_KEY} is {other}, not a string — failing toward merge"
                ))
            }
        },
    };

    match setting {
        Some((Setting::Merge, source)) => merge(format!("{source}=merge")),
        Some((Setting::Rebase, source)) => Decision {
            strategy: Strategy::Rebase,
            reason: format!("{source}=rebase"),
        },
        Some((Setting::Auto, _)) | None => auto(probe()),
    }
}

/// `auto`: rebase only when the forge positively allows rebase merges.
fn auto(probe: Probe) -> Decision {
    match probe {
        Probe::Known(MergeSettings {
            allow_rebase_merge: Some(true),
            ..
        }) => Decision {
            strategy: Strategy::Rebase,
            reason: "auto: the repository allows rebase merges (allow_rebase_merge=true)"
                .to_string(),
        },
        Probe::Known(MergeSettings {
            allow_rebase_merge: Some(false),
            ..
        }) => merge(
            "auto: the repository disables rebase merges (allow_rebase_merge=false)".to_string(),
        ),
        Probe::Known(_) => merge(
            "auto: allow_rebase_merge is not visible to this credential — failing toward merge"
                .to_string(),
        ),
        Probe::Unknown(why) => {
            merge(format!("auto: merge settings unknown ({why}) — failing toward merge"))
        }
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    nwo: String,
    fetched_at: u64,
    settings: MergeSettings,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A cached answer for `nwo`, if one younger than `ttl` exists at `path`.
fn read_cache(path: &Path, nwo: &str, now: u64, ttl: Duration) -> Option<MergeSettings> {
    let text = std::fs::read_to_string(path).ok()?;
    let entry: CacheEntry = serde_json::from_str(&text).ok()?;
    let fresh = now.saturating_sub(entry.fetched_at) < ttl.as_secs() && entry.fetched_at <= now;
    (entry.nwo == nwo && fresh).then_some(entry.settings)
}

/// Best-effort: a cache that cannot be written only costs a second API read.
fn write_cache(path: &Path, nwo: &str, now: u64, settings: MergeSettings) {
    let entry = CacheEntry {
        nwo: nwo.to_string(),
        fetched_at: now,
        settings,
    };
    let Ok(body) = serde_json::to_string(&entry) else {
        return;
    };
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

// ---------------------------------------------------------------------------
// Probing the forge
// ---------------------------------------------------------------------------

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn git_common_dir(dir: &Path) -> Option<PathBuf> {
    let raw = git_out(dir, &["rev-parse", "--git-common-dir"])?;
    let p = PathBuf::from(raw);
    Some(if p.is_absolute() { p } else { dir.join(p) })
}

/// `owner/repo` from `origin`'s URL — offline, no API call.
fn origin_nwo(dir: &Path) -> Option<String> {
    let url = git_out(dir, &["remote", "get-url", "origin"])?;
    crate::forge_cmd::parse_nwo_from_remote_url(&url)
}

/// One `GET repos/<nwo>` through the gh choke point.
fn github_settings(nwo: &str) -> Probe {
    let outcome = GhInvocation::new(
        Operation::new("branch_update.repo_settings"),
        AccessIntent::Read,
        GhTarget::None,
        FORGE_CMD_TIMEOUT,
    )
    .forge_op(crate::forge_call_stats::ops::REPO_VIEW)
    // The allow_*_merge flags may be hidden from a reader App (W4-C
    // writer_only), exactly as for `forge merge-method`.
    .writer_identity()
    .program(gh_bin())
    .args(["api", &format!("repos/{nwo}")])
    .run();
    match crate::cmd_out::decode_json::<MergeSettings, _>(outcome, |_| false) {
        crate::cmd_out::Query::Populated(s) => Probe::Known(s),
        crate::cmd_out::Query::Empty => Probe::Known(MergeSettings::default()),
        crate::cmd_out::Query::Malformed { error, .. } => {
            Probe::Unknown(format!("repos/{nwo} was unreadable: {error}"))
        }
        crate::cmd_out::Query::Failed { stderr, .. } => {
            Probe::Unknown(format!("repos/{nwo} failed: {}", stderr.trim()))
        }
        crate::cmd_out::Query::Unavailable(u) => {
            Probe::Unknown(format!("repos/{nwo} unavailable: {u}"))
        }
    }
}

/// The `auto` probe for the checkout at `dir`: cache, then forge.
fn probe_forge(dir: &Path, nwo_arg: Option<&str>, use_cache: bool) -> Probe {
    if detect_forge(Some(dir)) == ForgeType::Gitea {
        return Probe::Unknown("Gitea merge settings are not probed natively".to_string());
    }
    let Some(nwo) = nwo_arg.map(str::to_string).or_else(|| origin_nwo(dir)) else {
        return Probe::Unknown("no owner/repo resolvable from origin".to_string());
    };
    let cache = git_common_dir(dir).map(|d| d.join(CACHE_FILE));
    let now = now_secs();
    if use_cache {
        if let Some(settings) = cache
            .as_deref()
            .and_then(|p| read_cache(p, &nwo, now, CACHE_TTL))
        {
            return Probe::Known(settings);
        }
    }
    let probe = github_settings(&nwo);
    if let (Probe::Known(settings), Some(path)) = (&probe, cache.as_deref()) {
        write_cache(path, &nwo, now, *settings);
    }
    probe
}

/// Resolve the strategy for the checkout at `dir` (default: the cwd).
#[must_use]
pub fn resolve(dir: Option<&Path>, nwo: Option<&str>, use_cache: bool) -> Decision {
    let dir = dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let env = std::env::var(ENV_OVERRIDE).ok();
    let config = resolve_effective_config(&canonical_repo_root(Some(&dir)));
    decide(env.as_deref(), get_path(&config, CONFIG_KEY), || {
        probe_forge(&dir, nwo, use_cache)
    })
}

/// `loom-daemon branch-update strategy`: print `merge`/`rebase`, reason on
/// stderr unless `quiet`. Never fails.
pub fn handle_strategy(dir: Option<&Path>, nwo: Option<&str>, use_cache: bool, quiet: bool) {
    let d = resolve(dir, nwo, use_cache);
    if !quiet {
        eprintln!("branch-update: {} ({})", d.strategy.as_str(), d.reason);
    }
    println!("{}", d.strategy.as_str());
}

#[cfg(test)]
#[path = "branch_update_tests.rs"]
mod tests;
