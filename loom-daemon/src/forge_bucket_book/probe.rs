//! One free `gh api rate_limit` probe per published credential directory
//! (W1), after every reader-refresh pass.
//!
//! The probe is what makes a bucket visible when nothing on this host spent
//! from it recently — a reader whose repos were quiet, a cross-owner writer
//! token. GitHub does not charge `rate_limit`, and the facade books it to
//! the free [`crate::forge_call_stats::Pool::Other`].
//!
//! Safety: each probe runs with that directory as `GH_CONFIG_DIR` and every
//! token env var removed ([`GhInvocation::without_token_env`]), so it can
//! never spend — or report on — an operator's personal token. Enumeration
//! reads directory names, the presence of `hosts.yml` and a reader's
//! `identity.json` sidecar; it never opens `hosts.yml` or reads a token.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::{BucketKey, Reading, Resource, Source};
use crate::cmd_out::CmdOutcome;
use crate::forge_identity::IdentityRole;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation, ParentContext};

/// The facade operation name every probe records under.
pub const PROBE_OPERATION: &str = "api.rate_limit";

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// One directory to probe and the bucket owner its readings belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeTarget {
    pub dir: PathBuf,
    /// `Reader` for a reader App's directory, else `Writer`.
    pub role: IdentityRole,
    /// The account label readings are booked under.
    pub account: String,
    /// The owner readings are booked under (`unknown` when the workspace's
    /// own owner cannot be resolved locally).
    pub owner: String,
}

/// Every credential directory published under `workspace_root` worth a
/// probe at `now`: the workspace's writer directory, each per-owner writer
/// directory, and each reader directory whose sidecar is fresh. A writer
/// directory without a `hosts.yml` (an owner directory that only holds
/// reader subdirectories) is skipped.
#[must_use]
pub fn probe_targets(workspace_root: &Path, now: SystemTime) -> Vec<ProbeTarget> {
    let mut out = Vec::new();
    let has_token_file = |dir: &Path| dir.join("hosts.yml").is_file();
    let writer = super::writer_account(workspace_root);
    let primary = crate::credential_preflight::github_app_gh_config_dir(workspace_root);
    if has_token_file(&primary) {
        out.push(ProbeTarget {
            dir: primary,
            role: IdentityRole::Writer,
            account: writer.clone(),
            owner: super::primary_owner(workspace_root).unwrap_or_else(|| "unknown".to_string()),
        });
    }
    let by_owner = workspace_root.join(".loom").join("gh-config-by-owner");
    let mut owners: Vec<(String, PathBuf)> = read_subdirs(&by_owner)
        .into_iter()
        .filter(|(name, _)| super::valid_owner(name))
        .collect();
    owners.sort();
    for (owner, owner_dir) in owners {
        let owner_lc = owner.to_ascii_lowercase();
        if has_token_file(&owner_dir) {
            out.push(ProbeTarget {
                dir: owner_dir.clone(),
                role: IdentityRole::Writer,
                account: writer.clone(),
                owner: owner_lc.clone(),
            });
        }
        let mut readers: Vec<(String, PathBuf)> = read_subdirs(&owner_dir)
            .into_iter()
            .filter(|(name, _)| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()))
            .collect();
        readers.sort();
        for (app_id, dir) in readers {
            let sidecar_matches =
                crate::forge_identity::read_sidecar(&dir).is_some_and(|s| s.app_id == app_id);
            if !sidecar_matches || !crate::forge_identity::dir_is_fresh(&dir, now) {
                continue;
            }
            out.push(ProbeTarget {
                dir,
                role: IdentityRole::Reader,
                account: crate::observability::ops::ratelimit::app_account_label(&app_id),
                owner: owner_lc.clone(),
            });
        }
    }
    out
}

/// `(name, path)` of each real subdirectory of `dir` (symlinks excluded).
fn read_subdirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.path())))
        .collect()
}

/// The probe for one target: `gh api --include rate_limit` under exactly
/// that directory, token env stripped. `program` pins a stub `gh` (tests).
#[must_use]
pub fn probe_invocation(target: &ProbeTarget, program: Option<&Path>) -> GhInvocation {
    let inv = GhInvocation::new(
        Operation::new(PROBE_OPERATION),
        AccessIntent::Read,
        GhTarget::None,
        PROBE_TIMEOUT,
    )
    .parent(ParentContext::Missing)
    .args(["api", "--include", "rate_limit"])
    .gh_config_dir(Some(&target.dir))
    .without_token_env()
    .identity_role(target.role);
    match program {
        Some(p) => inv.program(p),
        None => inv,
    }
}

/// The `core`, `graphql` and `search` readings in a `rate_limit` body.
#[must_use]
pub fn parse_probe(body: &str, now: i64) -> Vec<(Resource, Reading)> {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(resources) = json.get("resources") else {
        return Vec::new();
    };
    [Resource::Core, Resource::Graphql, Resource::Search]
        .into_iter()
        .filter_map(|resource| {
            let r = resources.get(resource.as_str())?;
            let n = |k: &str| r.get(k).and_then(Value::as_u64);
            Some((
                resource,
                Reading {
                    limit: n("limit"),
                    remaining: n("remaining"),
                    used: n("used"),
                    reset_epoch: r.get("reset").and_then(Value::as_i64)?,
                    observed_at: now,
                    source: Source::Probe,
                },
            ))
        })
        .collect()
}

/// Probe every [`probe_targets`] directory under `workspace_root`, book the
/// readings, and snapshot the book into the forge-call sink. Called after
/// every reader-refresh pass; a host with no published App credential makes
/// no probe.
pub fn probe_all(workspace_root: &Path) {
    let probes = probe_all_with(workspace_root, None, SystemTime::now());
    log::debug!("forge_bucket_book: {probes} rate_limit probe(s) (free) — W1");
}

/// [`probe_all`] with an injectable `gh` and clock; returns the number of
/// probes made.
pub fn probe_all_with(workspace_root: &Path, program: Option<&Path>, now: SystemTime) -> usize {
    let targets = probe_targets(workspace_root, now);
    for target in &targets {
        let CmdOutcome::Ran(out) = probe_invocation(target, program).run() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let Some(resp) =
            crate::forge_listing::parse_http_response(&String::from_utf8_lossy(&out.stdout))
        else {
            continue;
        };
        let at = chrono::Utc::now().timestamp();
        for (resource, reading) in parse_probe(&resp.body, at) {
            super::insert(BucketKey::new(&target.account, &target.owner, resource), reading);
        }
    }
    if let Some(dir) = crate::forge_call_stats::host_sink_dir() {
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = super::persist(&dir, now) {
            log::debug!("forge_bucket_book: snapshot to {} failed: {e}", dir.display());
        }
    }
    targets.len()
}
