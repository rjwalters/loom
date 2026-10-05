//! The forge identity broker (#9537): one roster of the fleet's GitHub App
//! identities, and one place that answers the three questions every forge
//! call used to answer for itself.
//!
//! 1. **Which credential does a read use?** [`reader_for`]: a reader App
//!    chosen by `hash(owner/repo) mod N` ([`crate::forge_read_pool`], #9248),
//!    skipping any reader withdrawn after a rate-limit or auth failure. When
//!    no reader is usable the caller falls back to the writer.
//! 2. **Which credential does a write use?** Always the writer. Attributed
//!    actions (comments, labels, PRs, pushes, merges, leases, claims) keep one
//!    predictable author, which is what ruleset bypass actors and every
//!    "is this ours?" check rely on.
//! 3. **Is this login ours?** [`FleetLogins`]: the writer, every reader, and
//!    an explicit `legacyLogins` list, compared after [`normalise_login`].
//!
//! # Why readers count as "ours"
//!
//! GitHub attributes an App's whole history to its *current* slug. Renaming
//! the pool Apps to `loom-fleet-reader-N` therefore re-attributed years of
//! legitimate fleet comments (leases, claims, re-check reports) to the reader
//! names. Believing only the writer would silently distrust all of it.
//! Believing a reader grants nothing new: a reader App has read-only
//! permissions, so no comment can be authored under its name from now on.
//!
//! # Roster sources, highest first
//!
//! - `forge.identities` `{writer?, readers, legacyLogins}` when present. The
//!   writer is always `forge.githubApp` (what writes mint from);
//!   `identities.writer` may only restate it, and a mismatch is logged;
//! - otherwise the pre-#9537 keys, unchanged: `forge.githubApp` is the writer
//!   and `forge.githubAppReadPool` / `LOOM_GITHUB_APP_READ_POOL` supplies the
//!   readers. A host with neither configured behaves byte-identically to
//!   before: no readers, writes and reads on the one App.
//!
//! `LOOM_GITHUB_APP_SLUG` still overrides the writer's slug, as it did for
//! `star_liveness::trust`.
//!
//! # Delivery
//!
//! Each reader's installation token for each managed owner is published to
//! `gh-config-by-owner/<owner>/<app-id>/` ([`crate::forge_read_pool::
//! gh_config_dir_for_owner_app`]) by [`refresh_reader_credentials`], with an
//! `identity.json` sidecar carrying the token's `expires_at`. A read only uses
//! a reader directory whose token has more than [`READER_MIN_REMAINING`] left,
//! so a stalled refresh loop degrades to the writer instead of to expired
//! credentials.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::credential_preflight::{GithubAppMinter, GithubAppOutcome};
use crate::dep_recheck::extract::{normalise_login, DEFAULT_BOT_LOGIN};
use crate::forge_read_pool::{self, PoolMember};

/// The writer slug's env override (shared with `star_liveness::trust`).
pub const APP_SLUG_ENV: &str = "LOOM_GITHUB_APP_SLUG";

/// A reader token must have at least this long left to be used for a read.
/// The refresh loop runs every ~5 minutes and the minter re-mints below 10
/// minutes, so a healthy loop never gets near this floor.
pub const READER_MIN_REMAINING: Duration = Duration::from_secs(120);

/// How long a resolved roster is reused before the config is re-read. Reads
/// are hot (every listing poll), config changes are rare, and a stale roster
/// only ever means "the old reader set for one more minute".
const ROSTER_TTL: Duration = Duration::from_secs(60);

/// The sidecar written next to a reader's `hosts.yml`.
const SIDECAR: &str = "identity.json";

/// The daemon's workspace root, recorded by [`spawn_reader_refresh`]. Reads
/// resolve their roster and reader directories against it; outside a daemon
/// (unset) every read stays on the writer.
static WORKSPACE_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// The recorded workspace root, if a daemon recorded one.
#[must_use]
pub fn workspace_root() -> Option<&'static Path> {
    WORKSPACE_ROOT.get().map(PathBuf::as_path)
}

/// One GitHub App the fleet acts as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    /// GitHub App id, kept as a string (only ever passed through).
    pub app_id: String,
    /// The App's slug (its bot login without `[bot]`), when configured.
    pub slug: Option<String>,
    /// Absolute path to the App's private key.
    pub private_key_path: PathBuf,
}

/// The fleet's identities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Roster {
    /// The one App every attributed action uses. `None` when no GitHub App is
    /// configured (ambient `gh` auth).
    pub writer: Option<Identity>,
    /// Read-only Apps that carry forge reads.
    pub readers: Vec<Identity>,
    /// Extra logins believed as the fleet (renamed or retired Apps).
    pub legacy_logins: Vec<String>,
}

/// Which kind of call a credential is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads: may use a reader.
    Read,
    /// Attributed actions: always the writer.
    Write,
}

// ---------------------------------------------------------------------------
// Roster resolution
// ---------------------------------------------------------------------------

/// Resolve the roster for the workspace at `root` from the effective config
/// and environment (see the module docs for precedence).
#[must_use]
pub fn resolve(root: &Path) -> Roster {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let slug_env = std::env::var(APP_SLUG_ENV).ok();
    let pool = forge_read_pool::configured_pool(root);
    from_config(&effective, slug_env.as_deref(), &pool)
}

/// Pure core of [`resolve`]: the effective config, the slug env override,
/// and the pre-#9537 read pool.
#[must_use]
pub fn from_config(
    effective: &Value,
    slug_env: Option<&str>,
    legacy_pool: &[PoolMember],
) -> Roster {
    let slug_override = slug_env
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalise_login);
    // The writer is whatever ACTUALLY writes: `forge.githubApp`, the App
    // `github-app-token.sh` mints for the daemon's credential delivery, agent
    // sessions and merge-pr.sh. `forge.identities.writer` may restate it (to
    // give it a slug) but cannot redirect writes, so on a mismatch the
    // configured App wins and the disagreement is logged rather than letting
    // "is this ours?" believe one App while the fleet writes as another.
    let github_app = crate::config_resolver::get_path(effective, "forge.githubApp")
        .and_then(identity_from_value);
    let mut roster =
        if let Some(ids) = crate::config_resolver::get_path(effective, "forge.identities") {
            let declared = ids.get("writer").and_then(identity_from_value);
            Roster {
                writer: reconcile_writer(github_app, declared),
                readers: ids
                    .get("readers")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(identity_from_value).collect())
                    .unwrap_or_default(),
                legacy_logins: ids
                    .get("legacyLogins")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(|s| normalise_login(s.trim()))
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        } else {
            Roster {
                writer: crate::config_resolver::get_path(effective, "forge.githubApp")
                    .and_then(identity_from_value),
                readers: legacy_pool
                    .iter()
                    .map(|m| Identity {
                        app_id: m.app_id.clone(),
                        slug: None,
                        private_key_path: m.private_key_path.clone(),
                    })
                    .collect(),
                legacy_logins: Vec::new(),
            }
        };
    if let (Some(w), Some(s)) = (roster.writer.as_mut(), slug_override) {
        w.slug = Some(s);
    }
    // A reader that is also the writer would let a read and a write share one
    // budget while looking like two identities. Drop it rather than guess.
    if let Some(w) = &roster.writer {
        roster.readers.retain(|r| r.app_id != w.app_id);
    }
    roster
}

/// Config problems an operator should see: the writer declared in
/// `forge.identities` disagreeing with `forge.githubApp`, or declared without
/// it. `forge identities` prints these (the daemon also logs them), so a
/// misconfiguration is visible where an operator checks, not only in a log.
#[must_use]
pub fn config_warnings(effective: &Value) -> Vec<String> {
    let configured = crate::config_resolver::get_path(effective, "forge.githubApp")
        .and_then(identity_from_value);
    let declared = crate::config_resolver::get_path(effective, "forge.identities")
        .and_then(|ids| ids.get("writer"))
        .and_then(identity_from_value);
    match (configured, declared) {
        (Some(c), Some(d)) if c.app_id != d.app_id => vec![format!(
            "forge.identities.writer (app {}) differs from forge.githubApp (app {}); writes use app {} — make them agree",
            d.app_id, c.app_id, c.app_id
        )],
        (None, Some(d)) => vec![format!(
            "forge.identities.writer (app {}) is set but forge.githubApp is not: writes fall back to ambient gh auth — set forge.githubApp",
            d.app_id
        )],
        _ => Vec::new(),
    }
}

/// [`config_warnings`] for the workspace at `root`.
#[must_use]
pub fn config_warnings_for(root: &Path) -> Vec<String> {
    config_warnings(&crate::config_resolver::resolve_effective_config(root))
}

/// Pick the roster's writer from the App that writes (`forge.githubApp`) and
/// the one `forge.identities.writer` declares.
fn reconcile_writer(configured: Option<Identity>, declared: Option<Identity>) -> Option<Identity> {
    match (configured, declared) {
        (Some(mut c), Some(d)) => {
            if c.app_id == d.app_id {
                c.slug = c.slug.or(d.slug);
            } else {
                log::warn!(
                    "forge_identity: forge.identities.writer (app {}) differs from forge.githubApp \
                     (app {}), which is what every write mints from; treating app {} as the \
                     writer — fix the config so they agree (#9537)",
                    d.app_id,
                    c.app_id,
                    c.app_id
                );
            }
            Some(c)
        }
        (Some(c), None) => Some(c),
        (None, Some(d)) => {
            log::warn!(
                "forge_identity: forge.identities.writer (app {}) is set but forge.githubApp is \
                 not, so writes fall back to ambient gh auth; set forge.githubApp to the same App \
                 (#9537)",
                d.app_id
            );
            Some(d)
        }
        (None, None) => None,
    }
}

/// `{appId, privateKeyPath, slug?}` (slug falls back to `name`, as
/// `star_liveness::trust` accepted).
fn identity_from_value(v: &Value) -> Option<Identity> {
    let app_id = match v.get("appId")? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let key = v.get("privateKeyPath")?.as_str()?.trim();
    if app_id.is_empty() || key.is_empty() {
        return None;
    }
    let slug = ["slug", "name"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(|s| normalise_login(s.trim()))
        .filter(|s| !s.is_empty());
    Some(Identity {
        app_id,
        slug,
        private_key_path: expand_home(key),
    })
}

/// `~/…` → `$HOME/…`. `github-app-token.sh` does not expand `~`, so a
/// tilde path in config silently reads as "key not readable"; expanding it
/// here keeps the broker's own minting honest about the same config.
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// The roster for `root`, cached for [`ROSTER_TTL`].
#[must_use]
pub fn cached(root: &Path) -> Roster {
    static CACHE: OnceLock<Mutex<Option<(Instant, PathBuf, Roster)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((at, r, roster)) = guard.as_ref() {
            if r == root && at.elapsed() < ROSTER_TTL {
                return roster.clone();
            }
        }
    }
    let roster = resolve(root);
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), root.to_path_buf(), roster.clone()));
    }
    roster
}

// ---------------------------------------------------------------------------
// "Is this login ours?"
// ---------------------------------------------------------------------------

/// The logins believed as the fleet, normalised.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetLogins {
    names: BTreeSet<String>,
    /// Also accept Loom's historical default family: `loom-fleet-dispatch`
    /// and `loom-fleet-dispatch-<digits>`. Exact name or a numeric suffix,
    /// never a bare prefix (the old prefix match trusted
    /// `loom-fleet-dispatch-evil`). On for every roster, so an unconfigured
    /// host keeps working and the answer never depends on what else a process
    /// has seen; harmless on a configured one, since a GitHub App can only act
    /// on repos it is installed on. Off only for [`FleetLogins::single`].
    default_family: bool,
}

impl FleetLogins {
    /// The logins a roster believes.
    #[must_use]
    pub fn of(roster: &Roster) -> Self {
        let mut names = BTreeSet::new();
        names.extend(roster.writer.as_ref().and_then(|w| w.slug.clone()));
        names.extend(roster.readers.iter().filter_map(|r| r.slug.clone()));
        names.extend(roster.legacy_logins.iter().cloned());
        Self {
            names,
            default_family: true,
        }
    }

    /// The logins believed for the workspace at `root`.
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        Self::of(&cached(root))
    }

    /// The logins believed for the daemon's primary workspace, or the
    /// default family alone outside a daemon (no primary root registered).
    #[must_use]
    pub fn current() -> Self {
        workspace_root().map_or_else(|| Self::of(&Roster::default()), Self::for_root)
    }

    /// Exactly one login (an explicit `--bot-login`).
    #[must_use]
    pub fn single(login: &str) -> Self {
        let mut names = BTreeSet::new();
        let n = normalise_login(login.trim());
        if !n.is_empty() {
            names.insert(n);
        }
        Self {
            names,
            default_family: false,
        }
    }

    /// Whether `login` (any spelling: `x`, `x[bot]`, `app/x`) is the fleet.
    #[must_use]
    pub fn contains(&self, login: &str) -> bool {
        let n = normalise_login(login.trim());
        if n.is_empty() {
            return false;
        }
        self.names.contains(&n) || (self.default_family && is_default_family(&n))
    }

    /// The configured names, for diagnostics.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.names.iter().cloned().collect();
        if self.default_family {
            v.push(format!("{DEFAULT_BOT_LOGIN}(-N)"));
        }
        v
    }
}

/// `loom-fleet-dispatch` or `loom-fleet-dispatch-<digits>` (normalised).
#[must_use]
pub fn is_default_family(norm: &str) -> bool {
    norm == DEFAULT_BOT_LOGIN
        || norm
            .strip_prefix(DEFAULT_BOT_LOGIN)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// The role `login` plays in `roster`, for `forge is-fleet`: `writer`,
/// `reader`, `legacy`, `default` (the unconfigured family), or `None`.
#[must_use]
pub fn role_of(roster: &Roster, login: &str) -> Option<&'static str> {
    let n = normalise_login(login.trim());
    if n.is_empty() {
        return None;
    }
    let slug_is = |i: &Identity| i.slug.as_deref() == Some(n.as_str());
    if roster.writer.as_ref().is_some_and(slug_is) {
        return Some("writer");
    }
    if roster.readers.iter().any(slug_is) {
        return Some("reader");
    }
    if roster.legacy_logins.contains(&n) {
        return Some("legacy");
    }
    is_default_family(&n).then_some("default")
}

// ---------------------------------------------------------------------------
// Read selection and delivery
// ---------------------------------------------------------------------------

/// The reader that should serve reads for `owner_repo` right now, or `None`
/// (no readers, or every reader withdrawn for this repo or entirely).
///
/// Walks forward from the #9376 hash index, so the fallback order is itself
/// deterministic across hosts, skipping a reader withdrawn as an App (rate
/// limit, bad credentials) or for this one repo (not covered by its
/// installation).
#[must_use]
pub fn reader_for<'a>(roster: &'a Roster, owner_repo: &str) -> Option<&'a Identity> {
    reader_for_at(roster, owner_repo, SystemTime::now())
}

/// [`reader_for`] with an injected clock.
#[must_use]
pub fn reader_for_at<'a>(
    roster: &'a Roster,
    owner_repo: &str,
    now: SystemTime,
) -> Option<&'a Identity> {
    // #9986: the gateway owns the pool on a `required` egress host.
    // `workspace_root()` is `None` when `WORKSPACE_ROOT` was never registered
    // (CLI subcommands, not the daemon). Then only the env/machine policy tiers
    // are consulted, so a repo-tier-only `required` policy is not honoured
    // here — a deliberate fail-open for the repo tier alone: the daemon (which
    // mints and publishes) registers its workspace at startup via
    // `forge_identity::spawn_reader_refresh`, and
    // env/machine `required` policies still apply.
    if crate::forge_egress::publication::github_credential_forbidden(workspace_root()) {
        return None;
    }
    let n = roster.readers.len();
    let start = forge_read_pool::assignment_index(owner_repo, n)?;
    (0..n)
        .map(|off| &roster.readers[(start + off) % n])
        .find(|r| {
            !forge_read_pool::is_withdrawn_at(&r.app_id, now)
                && !repo_withdrawn_at(&r.app_id, owner_repo, now)
        })
}

/// `(app id, owner/repo)` -> eligible again. A coverage failure is about one
/// repo the reader's installation does not include; withdrawing the whole App
/// for it would knock the reader off every repo it does serve.
fn repo_withdrawals() -> &'static Mutex<std::collections::HashMap<(String, String), SystemTime>> {
    static MAP: OnceLock<Mutex<std::collections::HashMap<(String, String), SystemTime>>> =
        OnceLock::new();
    MAP.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// How long a reader stays off one repo after a coverage failure. Longer than
/// an App-wide withdrawal: an installation's repo set changes rarely, and each
/// re-probe of an uncovered repo is a guaranteed wasted call.
pub const REPO_WITHDRAWAL: Duration = Duration::from_secs(3600);

fn repo_withdrawn_at(app_id: &str, owner_repo: &str, now: SystemTime) -> bool {
    let Ok(map) = repo_withdrawals().lock() else {
        return false;
    };
    map.get(&(app_id.to_string(), owner_repo.to_ascii_lowercase()))
        .is_some_and(|&until| now < until)
}

/// Withdraw reader `app_id` for `owner_repo` only, until `until`.
pub fn withdraw_reader_for_repo_until(app_id: &str, owner_repo: &str, until: SystemTime) {
    if let Ok(mut map) = repo_withdrawals().lock() {
        let e = map
            .entry((app_id.to_string(), owner_repo.to_ascii_lowercase()))
            .or_insert(until);
        if until > *e {
            *e = until;
        }
    }
}

/// The directory a reader's token for `owner` is published to.
#[must_use]
pub fn reader_dir(workspace_root: &Path, owner: &str, reader: &Identity) -> PathBuf {
    forge_read_pool::gh_config_dir_for_owner_app(workspace_root, owner, &reader.app_id)
}

/// What [`refresh_reader_credentials`] records beside a reader's token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Sidecar {
    /// The reader App id.
    pub app_id: String,
    /// Its slug, when known.
    pub slug: Option<String>,
    /// The installation the token belongs to.
    pub installation_id: String,
    /// The token's expiry (RFC 3339, from the minter).
    pub expires_at: String,
}

/// The sidecar in `dir`, if readable.
#[must_use]
pub fn read_sidecar(dir: &Path) -> Option<Sidecar> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(SIDECAR)).ok()?).ok()
}

/// Whether `dir` holds a reader token usable at `now`.
#[must_use]
pub fn dir_is_fresh(dir: &Path, now: SystemTime) -> bool {
    if !dir.join("hosts.yml").is_file() {
        return false;
    }
    let Some(side) = read_sidecar(dir) else {
        return false;
    };
    let Ok(exp) = chrono::DateTime::parse_from_rfc3339(&side.expires_at) else {
        return false;
    };
    let exp: SystemTime = exp.with_timezone(&chrono::Utc).into();
    exp.duration_since(now)
        .is_ok_and(|left| left >= READER_MIN_REMAINING)
}

/// A usable reader credential for a read of `owner_repo` against `host`:
/// `(GH_CONFIG_DIR, reader app id)`. `None` sends the read to the writer:
/// no primary workspace yet, a non-github.com host, no readers, every reader
/// withdrawn, or the chosen reader's token missing or near expiry.
#[must_use]
pub fn read_credential(owner_repo: &str, host: Option<&str>) -> Option<(PathBuf, String)> {
    if host.is_some_and(|h| !h.eq_ignore_ascii_case("github.com")) {
        return None;
    }
    let ws = workspace_root()?;
    read_credential_in(ws, &cached(ws), owner_repo, SystemTime::now())
}

/// Pure-ish core of [`read_credential`] (reads only the sidecar/token files).
#[must_use]
pub fn read_credential_in(
    workspace_root: &Path,
    roster: &Roster,
    owner_repo: &str,
    now: SystemTime,
) -> Option<(PathBuf, String)> {
    let owner = crate::credential_preflight::owner_of_nwo(owner_repo);
    if owner.is_empty() {
        return None;
    }
    // Walk the same deterministic order reader_for uses, but also skip a
    // reader whose token for this owner is missing or near expiry, so one
    // reader's stalled refresh moves its repos to the next reader rather than
    // straight onto the writer's budget.
    let n = roster.readers.len();
    let start = forge_read_pool::assignment_index(owner_repo, n)?;
    (0..n)
        .map(|off| &roster.readers[(start + off) % n])
        .find_map(|r| {
            if forge_read_pool::is_withdrawn_at(&r.app_id, now)
                || repo_withdrawn_at(&r.app_id, owner_repo, now)
            {
                return None;
            }
            let dir = reader_dir(workspace_root, owner, r);
            dir_is_fresh(&dir, now).then(|| (dir, r.app_id.clone()))
        })
}

/// Point `cmd` at a reader for a read of `owner_repo`, returning the reader's
/// app id when it did. `None` leaves `cmd` untouched for the caller's writer
/// path (`apply_gh_config_for_*`).
pub fn apply_read_credential(
    cmd: &mut Command,
    owner_repo: &str,
    host: Option<&str>,
) -> Option<String> {
    let (dir, app_id) = read_credential(owner_repo, host)?;
    cmd.env("GH_CONFIG_DIR", dir);
    Some(app_id)
}

/// What a failed read says about the reader that served it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The App as a whole cannot serve right now (rate limit, bad
    /// credentials): withdraw it everywhere.
    App,
    /// This repo is outside the reader's installation (404, "not accessible
    /// by integration"): withdraw it for this repo only.
    Coverage,
}

/// Classify a failed read, or `None` when the failure is not the
/// credential's (a 5xx, a network error): retrying on the writer would not
/// help and withdrawing the reader would be wrong.
#[must_use]
pub fn classify_failure(stderr: &str, http_status: Option<u16>) -> Option<Failure> {
    let s = stderr.to_ascii_lowercase();
    // Rate limits come as 403 or 429 with a telling message, so check the
    // text before the status: a rate-limited 403 is App-wide, not coverage.
    if s.contains("rate limit")
        || s.contains("bad credentials")
        || matches!(http_status, Some(401 | 429))
        || s.contains("http 401")
        || s.contains("http 429")
    {
        return Some(Failure::App);
    }
    if matches!(http_status, Some(403 | 404))
        || s.contains("resource not accessible by integration")
        // GraphQL (`gh pr view`, `gh issue view`) has no HTTP status for a
        // repo outside the installation: it says it cannot resolve the repo
        // (#9872 routes those reads too).
        || s.contains("could not resolve to a repository")
        || s.contains("http 403")
        || s.contains("http 404")
    {
        return Some(Failure::Coverage);
    }
    None
}

/// Which identity served one attempt of a read (#9872): recorded on the
/// accounting row so `loom-daemon status` can show reads by pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityRole {
    /// A reader App's own installation pool.
    Reader,
    /// The writer credential, chosen up front (no usable reader, a write, or
    /// a read pinned with `GhInvocation::writer_identity`).
    Writer,
    /// The writer, re-running a read a reader failed on.
    WriterFallback,
}

impl IdentityRole {
    /// The accounting value: `reader` / `writer` / `writer-fallback`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityRole::Reader => "reader",
            IdentityRole::Writer => "writer",
            IdentityRole::WriterFallback => "writer-fallback",
        }
    }
}

/// The one reader-then-writer retry shape (#9537, shared since #9872 by
/// `forge_etag_store::fetch_conditional` and the `GhInvocation` choke point).
///
/// With no `reader_dir`, `run` is called once on the writer. Otherwise it runs
/// on the reader first; a success, or a failure that is not the credential's
/// (`failure_of` → `None`), is returned as-is. On [`Failure::App`] the reader
/// is withdrawn (`withdraw`, handed the reader's failed result) and the read
/// re-runs once on the writer. On a
/// 403/404 ([`Failure::Coverage`]) the read re-runs on the writer and the
/// reader is withdrawn **only if the writer succeeds**: a resource missing for
/// everyone must not take the repo's reader offline.
///
/// # Errors
///
/// Whatever `run` returns; an `Err` from the reader attempt is not retried
/// (it means the call could not be made, not that the reader was refused).
pub fn reader_then_writer<T, E>(
    reader_dir: Option<&Path>,
    mut run: impl FnMut(Option<&Path>, IdentityRole) -> Result<T, E>,
    succeeded: impl Fn(&T) -> bool,
    failure_of: impl Fn(&T) -> Option<Failure>,
    withdraw: impl FnOnce(Failure, &T),
) -> Result<T, E> {
    let Some(dir) = reader_dir else {
        return run(None, IdentityRole::Writer);
    };
    let first = run(Some(dir), IdentityRole::Reader)?;
    if succeeded(&first) {
        return Ok(first);
    }
    let Some(failure) = failure_of(&first) else {
        return Ok(first);
    };
    if failure == Failure::App {
        withdraw(failure, &first);
        return run(None, IdentityRole::WriterFallback);
    }
    let second = run(None, IdentityRole::WriterFallback)?;
    if succeeded(&second) {
        withdraw(failure, &first);
    }
    Ok(second)
}

/// Whether a failed read is the credential's fault (see [`classify_failure`]).
#[must_use]
pub fn is_credential_failure(stderr: &str, http_status: Option<u16>) -> bool {
    classify_failure(stderr, http_status).is_some()
}

/// Withdraw reader `app_id` App-wide (a mint failure, or no repo to scope to).
pub fn withdraw_reader(app_id: &str, why: &str) {
    log::warn!(
        "forge_identity: reader app {app_id} withdrawn for {}s ({why}); reads fall back to \
         the next reader or the writer — #9537",
        forge_read_pool::DEFAULT_WITHDRAWAL.as_secs()
    );
    forge_read_pool::withdraw(app_id);
}

/// Withdraw reader `app_id` after `failure` on a read of `owner_repo`: the
/// whole App for [`Failure::App`] (until `app_until` when the forge reported
/// a reset, else the default window), just this repo for
/// [`Failure::Coverage`].
pub fn withdraw_after(
    app_id: &str,
    owner_repo: &str,
    failure: Failure,
    app_until: Option<SystemTime>,
    why: &str,
) {
    match failure {
        Failure::App => {
            log::warn!(
                "forge_identity: reader app {app_id} withdrawn App-wide after a failed read \
                 ({why}); reads fall back to the next reader or the writer — #9537"
            );
            match app_until {
                Some(t) => forge_read_pool::withdraw_until(app_id, t),
                None => forge_read_pool::withdraw(app_id),
            }
        }
        Failure::Coverage => {
            log::info!(
                "forge_identity: reader app {app_id} does not cover {owner_repo} ({why}); \
                 withdrawn for that repo for {}s, other repos unaffected — #9537",
                REPO_WITHDRAWAL.as_secs()
            );
            withdraw_reader_for_repo_until(app_id, owner_repo, SystemTime::now() + REPO_WITHDRAWAL);
        }
    }
}

// ---------------------------------------------------------------------------
// Minting
// ---------------------------------------------------------------------------

/// Mints `identity`'s token through `github-app-token.sh`, overriding the
/// configured App with `LOOM_GITHUB_APP_ID` / `LOOM_GITHUB_APP_KEY_PATH` on
/// the child only. The script keys its caches by app id, so readers and the
/// writer never share a cached token.
pub struct IdentityMinter {
    /// `github-app-token.sh`.
    pub script_path: PathBuf,
    /// Working directory for the subprocess.
    pub cwd: PathBuf,
    /// The App to mint as.
    pub identity: Identity,
}

impl IdentityMinter {
    fn run(&self, owner_repo: &str, force: bool) -> GithubAppOutcome {
        // `env NAME=value … bash script …`: the override lands on the minter's
        // own process only, through the same bounded-subprocess helper the
        // writer's minter uses. Neither value is secret (an App id and a key
        // *path*), so carrying them in argv is fine.
        let script = self.script_path.to_string_lossy().to_string();
        let id_kv = format!("LOOM_GITHUB_APP_ID={}", self.identity.app_id);
        let key_kv = format!(
            "LOOM_GITHUB_APP_KEY_PATH={}",
            self.identity.private_key_path.to_string_lossy()
        );
        let mut args: Vec<&str> = vec![
            id_kv.as_str(),
            key_kv.as_str(),
            "bash",
            script.as_str(),
            "get-token",
        ];
        if force {
            args.push("--force");
        }
        args.push(owner_repo);
        let timeout = crate::credential_preflight::resolve_github_app_mint_timeout(&self.cwd);
        crate::credential_preflight::mint_with_retry(
            crate::credential_preflight::GITHUB_APP_MINT_ATTEMPTS,
            crate::credential_preflight::GITHUB_APP_MINT_RETRY_DELAY,
            |_| crate::main_health_gate::run_capture_with_timeout("env", &args, &self.cwd, timeout),
        )
    }
}

impl GithubAppMinter for IdentityMinter {
    fn mint(&self, owner_repo: &str) -> GithubAppOutcome {
        self.run(owner_repo, false)
    }
    fn mint_forced(&self, owner_repo: &str) -> GithubAppOutcome {
        self.run(owner_repo, true)
    }
}

/// One reader × owner outcome of [`refresh_reader_credentials`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshOutcome {
    /// The reader.
    pub app_id: String,
    /// The owner the token is for.
    pub owner: String,
    /// `Ok(expires_at)` or `Err(reason)`.
    pub result: Result<String, String>,
}

/// Mint and publish every reader's token for every owner in `owner_repos`
/// (one representative `owner/repo` per owner). A reader that fails to mint
/// for an owner just leaves that owner's directory unpublished, so only that
/// owner's reads fall back to the writer.
pub fn refresh_reader_credentials(
    workspace_root: &Path,
    roster: &Roster,
    owner_repos: &[String],
    make_minter: &dyn Fn(&Identity) -> Box<dyn GithubAppMinter>,
) -> Vec<RefreshOutcome> {
    let mut out = Vec::new();
    for reader in &roster.readers {
        let minter = make_minter(reader);
        for owner_repo in owner_repos {
            let owner = crate::credential_preflight::owner_of_nwo(owner_repo).to_string();
            if owner.is_empty() {
                continue;
            }
            let result = match minter.mint(owner_repo) {
                GithubAppOutcome::Minted {
                    token,
                    installation_id,
                    expires_at,
                    ..
                } => {
                    let dir = reader_dir(workspace_root, &owner, reader);
                    publish(&dir, &token, reader, &installation_id, &expires_at)
                        .map(|()| expires_at)
                        .map_err(|e| format!("could not publish to {}: {e}", dir.display()))
                }
                GithubAppOutcome::Error(reason) => Err(reason),
                GithubAppOutcome::NotConfigured => {
                    Err("github-app-token.sh reported not_configured for this reader".to_string())
                }
            };
            // No withdrawal on a mint failure: the usual cause is this App not
            // being installed on `owner`, an owner-level coverage gap. That
            // owner's reader directory simply stays unpublished (or ages out),
            // so its reads use the writer while every other owner keeps the
            // reader.
            if let Err(reason) = &result {
                log::warn!(
                    "forge_identity: reader app {} could not refresh its token for {owner} \
                     ({reason}); {owner}'s reads use the writer until it can — #9537",
                    reader.app_id
                );
            }
            out.push(RefreshOutcome {
                app_id: reader.app_id.clone(),
                owner,
                result,
            });
        }
    }
    out
}

/// Publish the token (via the writer's own atomic delivery), then the
/// sidecar. The sidecar is written last, so a reader of the directory never
/// sees a fresh sidecar in front of a stale token.
fn publish(
    dir: &Path,
    token: &str,
    reader: &Identity,
    installation_id: &str,
    expires_at: &str,
) -> std::io::Result<()> {
    crate::credential_preflight::publish_github_app_token(dir, token)?;
    let side = Sidecar {
        app_id: reader.app_id.clone(),
        slug: reader.slug.clone(),
        installation_id: installation_id.to_string(),
        expires_at: expires_at.to_string(),
    };
    let tmp = dir.join(format!("{SIDECAR}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(&side).unwrap_or_default())?;
    std::fs::rename(tmp, dir.join(SIDECAR))
}

/// Keep every reader's token fresh for every managed owner: one pass now,
/// then every [`crate::credential_preflight::GITHUB_APP_REFRESH_INTERVAL`].
/// The roster and the owner set are re-read each pass (the primary owner plus
/// every per-owner refresh source), so a reader added to config or an owner
/// discovered later is picked up without a restart. A pass with no readers is
/// a cheap no-op, so the loop is safe to spawn on every host.
pub fn spawn_reader_refresh(workspace_root: PathBuf, primary_owner_repo: Option<String>) {
    WORKSPACE_ROOT.get_or_init(|| workspace_root.clone());
    tokio::spawn(async move {
        loop {
            let ws = workspace_root.clone();
            let primary = primary_owner_repo.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let roster = resolve(&ws);
                if roster.readers.is_empty() {
                    return;
                }
                let Some(factory) = real_minter_factory(&ws) else {
                    return;
                };
                let mut owners: Vec<String> = primary.into_iter().collect();
                for (owner_repo, _) in crate::credential_preflight::owner_refresh_sources() {
                    let owner = crate::credential_preflight::owner_of_nwo(&owner_repo).to_string();
                    if !owners
                        .iter()
                        .any(|o| crate::credential_preflight::owner_of_nwo(o) == owner)
                    {
                        owners.push(owner_repo);
                    }
                }
                let outcomes = refresh_reader_credentials(&ws, &roster, &owners, &factory);
                let ok = outcomes.iter().filter(|o| o.result.is_ok()).count();
                log::debug!(
                    "forge_identity: reader refresh — {ok}/{} reader×owner token(s) fresh — #9537",
                    outcomes.len()
                );
            })
            .await;
            tokio::time::sleep(crate::credential_preflight::GITHUB_APP_REFRESH_INTERVAL).await;
        }
    });
}

/// The production minter factory: [`IdentityMinter`] over the workspace's
/// `github-app-token.sh`. `None` when the script is not installed.
#[must_use]
pub fn real_minter_factory(
    workspace_root: &Path,
) -> Option<impl Fn(&Identity) -> Box<dyn GithubAppMinter>> {
    let script = crate::credential_preflight::resolve_github_app_script(workspace_root)?;
    let cwd = workspace_root.to_path_buf();
    Some(move |identity: &Identity| -> Box<dyn GithubAppMinter> {
        Box::new(IdentityMinter {
            script_path: script.clone(),
            cwd: cwd.clone(),
            identity: identity.clone(),
        })
    })
}

#[cfg(test)]
#[path = "forge_identity/tests.rs"]
mod tests;
