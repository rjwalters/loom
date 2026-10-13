//! Forge-host selection for terminal activity ingestion (#11486).
//!
//! `GetTerminalOutput` parses each output batch for forge creation URLs
//! (`https://<host>/owner/repo/issues/N`, `.../pull/N`, `.../pulls/N`) via
//! [`parse_forge_events`], which only matches URLs on one host. That host used
//! to be a literal `github.com`, so a terminal in a Gitea workspace never
//! produced issue/PR-created activity. This module picks the host from the
//! **terminal's own** repository, through the existing forge resolver
//! ([`detect_forge`] / [`resolve_forge_config`], which resolve a worktree to
//! its canonical repo root), so no second configuration scheme exists.
//!
//! Resolution contract (see [`ForgeHostResolution`]):
//!
//! - no workspace path on the terminal → legacy `github.com` default;
//! - forge resolves to GitHub (including "not a git repo" and unknown remote
//!   hosts, which `detect_forge` already treats as GitHub) → `github.com`;
//! - forge resolves to Gitea → the host of `forge.gitea.url`, else the host
//!   of the `origin` remote;
//! - Gitea with neither yielding a host → [`ForgeHostResolution::Unresolved`]:
//!   creation URLs are **not** matched (rather than silently treating the
//!   workspace as GitHub) and a warning names the reason;
//! - a config tier that exists but cannot be read (malformed JSON, …) is
//!   reported in a warning, since that tier's `forge` block reads as unset.
//!
//! Resolution only reads config files and runs local `git rev-parse` /
//! `git remote get-url` — never a token and never a forge API request. Results
//! are cached per workspace path for [`CACHE_TTL`] so the per-poll output path
//! does not spawn git on every call; callers resolve before taking the
//! terminal-manager or activity-database locks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::activity::ActivityDb;
use crate::activity::PromptForgeEventType;
use crate::config_resolver::{get_path, unreadable_tiers};
use crate::forge_cmd::{
    canonical_repo_root, detect_forge, get_remote_url, parse_host, resolve_forge_config, ForgeType,
};
use crate::forge_parser::{parse_forge_events, ParsedForgeEvent};
use crate::types::TerminalInfo;

/// Legacy host used when a terminal has no repository context.
pub const DEFAULT_FORGE_HOST: &str = "github.com";

/// How long a workspace's resolved host is reused before re-resolving, so a
/// config edit is picked up without restarting the daemon.
pub const CACHE_TTL: Duration = Duration::from_secs(60);

/// Outcome of resolving the forge host for one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeHostResolution {
    /// The terminal has no workspace path: legacy `github.com` default.
    NoContext,
    /// Host resolved from the workspace's forge configuration / remote.
    /// `warning` is set when a config tier exists but could not be read.
    Resolved {
        host: String,
        warning: Option<String>,
    },
    /// The workspace is configured for a forge whose host cannot be
    /// determined; creation URLs are not matched for it.
    Unresolved { reason: String },
}

impl ForgeHostResolution {
    /// The host to match creation URLs against, or `None` to match none.
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        match self {
            Self::NoContext => Some(DEFAULT_FORGE_HOST),
            Self::Resolved { host, .. } => Some(host),
            Self::Unresolved { .. } => None,
        }
    }

    /// A diagnostic worth logging, if any.
    #[must_use]
    pub fn warning(&self) -> Option<&str> {
        match self {
            Self::NoContext => None,
            Self::Resolved { warning, .. } => warning.as_deref(),
            Self::Unresolved { reason } => Some(reason),
        }
    }
}

/// Normalize a host parsed from a URL: drop any `user@` userinfo and lowercase.
fn normalize_host(host: &str) -> Option<String> {
    let host = host.rsplit('@').next().unwrap_or(host).trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

fn unreadable_tier_warning(root: &Path) -> Option<String> {
    let dead = unreadable_tiers(root);
    if dead.is_empty() {
        return None;
    }
    let list: Vec<String> = dead
        .iter()
        .map(|(p, why)| format!("{} ({why})", p.display()))
        .collect();
    Some(format!(
        "forge host for {}: config tier(s) unreadable, their `forge` settings are ignored: {}",
        root.display(),
        list.join("; ")
    ))
}

/// Resolve the forge host for `workspace` (uncached). See the module docs.
#[must_use]
pub fn resolve_forge_host(workspace: Option<&Path>) -> ForgeHostResolution {
    let Some(ws) = workspace else {
        return ForgeHostResolution::NoContext;
    };
    let root = canonical_repo_root(Some(ws));
    let warning = unreadable_tier_warning(&root);
    match detect_forge(Some(ws)) {
        ForgeType::GitHub => ForgeHostResolution::Resolved {
            host: DEFAULT_FORGE_HOST.to_string(),
            warning,
        },
        ForgeType::Gitea => {
            let forge_config = resolve_forge_config(Some(ws));
            let configured = match get_path(&forge_config, "gitea.url") {
                Some(Value::String(url)) => Some(url.clone()),
                _ => None,
            };
            let from_config = configured.as_deref().and_then(parse_host);
            let host = from_config
                .or_else(|| get_remote_url(&root).as_deref().and_then(parse_host))
                .and_then(|h| normalize_host(&h));
            match host {
                Some(host) => ForgeHostResolution::Resolved { host, warning },
                None => {
                    let url_note = match configured {
                        Some(url) => format!("forge.gitea.url {url:?} has no parseable host"),
                        None => "forge.gitea.url is not set".to_string(),
                    };
                    let mut reason = format!(
                        "forge host for {}: forge resolves to gitea but {url_note} and the \
                         origin remote yields no host; issue/PR creation URLs will not be \
                         recorded for this workspace",
                        root.display()
                    );
                    if let Some(w) = warning {
                        reason.push_str("; ");
                        reason.push_str(&w);
                    }
                    ForgeHostResolution::Unresolved { reason }
                }
            }
        }
    }
}

type Cache = Mutex<HashMap<PathBuf, (Instant, ForgeHostResolution)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// [`resolve_forge_host`] with a per-workspace [`CACHE_TTL`] cache. A warning
/// is logged when the resolution is (re)computed, i.e. at most once per TTL
/// per workspace. The cache lock is never held while resolving.
#[must_use]
pub fn cached_forge_host(workspace: Option<&Path>) -> ForgeHostResolution {
    let Some(ws) = workspace else {
        return ForgeHostResolution::NoContext;
    };
    if let Ok(map) = cache().lock() {
        if let Some((at, res)) = map.get(ws) {
            if at.elapsed() < CACHE_TTL {
                return res.clone();
            }
        }
    }
    let res = resolve_forge_host(Some(ws));
    if let Some(w) = res.warning() {
        log::warn!("{w}");
    }
    if let Ok(mut map) = cache().lock() {
        map.insert(ws.to_path_buf(), (Instant::now(), res.clone()));
    }
    res
}

/// The forge host for a terminal's workspace (`worktree_path`, else
/// `working_dir` — the same precedence the git-commit correlation uses).
#[must_use]
pub fn host_for_terminal(info: Option<&TerminalInfo>) -> Option<String> {
    let ws = info.and_then(|i| i.worktree_path.as_ref().or(i.working_dir.as_ref()));
    cached_forge_host(ws.map(Path::new))
        .host()
        .map(str::to_string)
}

/// Parse `output` for forge events matching creation URLs on `host`; with
/// `None`, creation URLs are skipped and host-independent events still parse.
#[must_use]
pub fn parse_events_for_host(output: &str, host: Option<&str>) -> Vec<ParsedForgeEvent> {
    match host {
        Some(h) => parse_forge_events(output, h),
        None => parse_forge_events(output, DEFAULT_FORGE_HOST)
            .into_iter()
            .filter(|e| {
                !matches!(
                    e.event_type,
                    PromptForgeEventType::IssueCreated | PromptForgeEventType::PrCreated
                )
            })
            .collect(),
    }
}

/// Parse `output` for forge events on `host` and record each against
/// `input_id` — the ingestion step of `GetTerminalOutput`.
pub fn record_forge_events(
    db: &ActivityDb,
    output: &str,
    host: Option<&str>,
    input_id: Option<i64>,
) {
    for parsed_event in parse_events_for_host(output, host) {
        let prompt_event = parsed_event.to_prompt_forge_event(input_id);
        if let Err(e) = db.record_prompt_forge_event(&prompt_event) {
            log::warn!("Failed to record forge event: {e}");
        } else {
            log::debug!(
                "Recorded forge event: {:?} (issue: {:?}, pr: {:?})",
                prompt_event.event_type,
                prompt_event.issue_number,
                prompt_event.pr_number
            );
        }
    }
}

#[cfg(test)]
#[path = "forge_host_tests.rs"]
mod tests;
