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
//! reads directory names, the presence of `hosts.yml` and each directory's
//! `identity.json` sidecar (which App and installation was minted into it,
//! #10571); it never opens `hosts.yml` or reads a token.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
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
    /// The installation the directory's sidecar names (#10571), if any.
    pub installation: Option<String>,
}

/// The target for writer directory `dir`: keyed by its sidecar, else by the
/// roster and the path / `origin` remote ([`super::dir_identity`]).
fn writer_target(dir: PathBuf) -> Option<ProbeTarget> {
    let id = super::dir_identity(&dir, &super::classify_dir(&dir))?;
    Some(ProbeTarget {
        dir,
        role: IdentityRole::Writer,
        account: id.account,
        owner: id.owner.unwrap_or_else(|| "unknown".to_string()),
        installation: id.installation,
    })
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
    let primary = crate::credential_preflight::github_app_gh_config_dir(workspace_root);
    if has_token_file(&primary) {
        out.extend(writer_target(primary));
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
            out.extend(writer_target(owner_dir.clone()));
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
                installation: installation_of(&dir),
                dir,
                role: IdentityRole::Reader,
                account: crate::observability::ops::ratelimit::app_account_label(&app_id),
                owner: owner_lc.clone(),
            });
        }
    }
    out
}

/// The installation `dir`'s sidecar names, validated as
/// [`super::dir_identity`] does for the header path.
fn installation_of(dir: &Path) -> Option<String> {
    super::dir_identity(dir, &super::classify_dir(dir))?.installation
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
        run_probe(target, program);
    }
    persist_to_sink();
    targets.len()
}

/// Run one probe and book its readings. `false` when it produced none.
fn run_probe(target: &ProbeTarget, program: Option<&Path>) -> bool {
    let CmdOutcome::Ran(out) = probe_invocation(target, program).run() else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let Some(resp) =
        crate::forge_listing::parse_http_response(&String::from_utf8_lossy(&out.stdout))
    else {
        return false;
    };
    let at = chrono::Utc::now().timestamp();
    let readings = parse_probe(&resp.body, at);
    let any = !readings.is_empty();
    for (resource, reading) in readings {
        let key = BucketKey::new(&target.account, &target.owner, resource)
            .with_installation(target.installation.as_deref());
        super::insert(key, reading);
    }
    any
}

/// Snapshot the book into the forge-call sink, when one is configured.
fn persist_to_sink() {
    if let Some(dir) = crate::forge_call_stats::host_sink_dir() {
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = super::persist(&dir, now) {
            log::debug!("forge_bucket_book: snapshot to {} failed: {e}", dir.display());
        }
    }
}

// ---------------------------------------------------------------------------
// On-demand probe after a reader refusal (W4-A)
// ---------------------------------------------------------------------------

/// Fewest seconds between two on-demand probes of one `(app, owner)`.
pub const PROBE_ONE_INTERVAL_SECS: u64 = 60;

/// `(app id, owner lowercased)` -> when it was last probed on demand.
fn probe_one_last() -> &'static Mutex<HashMap<(String, String), SystemTime>> {
    static LAST: OnceLock<Mutex<HashMap<(String, String), SystemTime>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Claim the `(app, owner)` probe slot at `now`: `false` when one was
/// claimed less than [`PROBE_ONE_INTERVAL_SECS`] ago.
fn claim_probe_slot(app_id: &str, owner: &str, now: SystemTime) -> bool {
    let Ok(mut last) = probe_one_last().lock() else {
        return false; // a poisoned lock never adds forge calls
    };
    let key = (app_id.to_string(), owner.to_ascii_lowercase());
    let throttled = last.get(&key).is_some_and(|&at| {
        now.duration_since(at)
            .is_ok_and(|d| d < Duration::from_secs(PROBE_ONE_INTERVAL_SECS))
            || at > now
    });
    if throttled {
        return false;
    }
    last.insert(key, now);
    true
}

/// Probe reader `app_id`'s bucket for `owner` now, so a withdrawal after a
/// refusal that carried no `x-ratelimit-reset` can still use the bucket's
/// real reset (W4-A). Free (`rate_limit` is not charged; booked to
/// [`crate::forge_call_stats::Pool::Other`]), and run under exactly that
/// reader's directory with every token env var removed, like every other
/// probe. Throttled to once per `(app, owner)` per
/// [`PROBE_ONE_INTERVAL_SECS`]; a no-op outside a daemon (no workspace
/// registered). Returns whether a probe booked a reading.
pub fn probe_one(app_id: &str, owner: &str) -> bool {
    let Some(ws) = crate::forge_identity::workspace_root() else {
        return false;
    };
    probe_one_with(ws, app_id, owner, None, SystemTime::now())
}

/// [`probe_one`] against an explicit workspace, `gh` and clock (tests).
/// `owner` is used as given for the directory (the refresh loop publishes
/// it under the spelling it was configured with) and lowercased for the
/// bucket key.
pub fn probe_one_with(
    workspace_root: &Path,
    app_id: &str,
    owner: &str,
    program: Option<&Path>,
    now: SystemTime,
) -> bool {
    if app_id.is_empty() || !super::valid_owner(owner) {
        return false;
    }
    let dir = crate::forge_read_pool::gh_config_dir_for_owner_app(workspace_root, owner, app_id);
    if !crate::forge_identity::dir_is_fresh(&dir, now) {
        return false;
    }
    if !claim_probe_slot(app_id, owner, now) {
        return false;
    }
    let installation = installation_of(&dir);
    let target = ProbeTarget {
        dir,
        role: IdentityRole::Reader,
        account: crate::observability::ops::ratelimit::app_account_label(app_id),
        owner: owner.to_ascii_lowercase(),
        installation,
    };
    let booked = run_probe(&target, program);
    if booked {
        persist_to_sink();
    }
    booked
}
