//! Forge reads through the `github-read` gateway (2AMLogic/2am#4367 P4).
//!
//! The gateway is a transparent read relay: for any request it returns what
//! GitHub would return to the App installation token it mints for that
//! request. A caller authenticates with a gateway key, one per (caller,
//! owner), and stock `gh` (>= 2.100.0) reaches it through `api_host` in the
//! profile's `hosts.yml`. Writes, git and releases never use it.
//!
//! # Config
//!
//! `forge.gateway` in the effective config (a fleet-wide value goes in the
//! fleet tier, a canary in one host's tier):
//!
//! ```json
//! "forge": { "gateway": {
//!   "origin": "https://github-proxy.example.com",
//!   "keyRefs": {
//!     "acme":  "file:/home/ubuntu/.loom/github-gateway/acme.key",
//!     "other": "ssm:///github-proxy/profiles/relay-host-other/key"
//!   }
//! } }
//! ```
//!
//! `keyRefs` (`key_refs` is accepted too) names, per owner, where that
//! owner's key is read: `file:<path>` (an owner-only file outside every
//! checkout) or `ssm://<parameter>` (the AWS CLI, decrypted). The key itself
//! never appears in config, argv, a log line or a span: it is read into
//! memory and written only into the profile's `hosts.yml` (0600, in a 0700
//! directory). An owner with no key ref keeps today's routing.
//!
//! # What changes when it is set
//!
//! [`refresh`] (every [`CHECK_INTERVAL`]) publishes one read profile per
//! configured owner at `.loom/gh-config-gateway/<owner>/`, with `api_host` set, and
//! rewrites `api_host` every time it rewrites `hosts.yml`. Then:
//!
//! - the daemon's reads ([`crate::gh_invocation::GhInvocation`] with read
//!   intent, and the conditional reads of [`crate::forge_etag_store`]) are
//!   offered the gateway profile first by [`route_read`], ahead of any
//!   host-held reader App. A gateway failure withdraws it like a reader, so
//!   the read falls back to the readers, then the writer, exactly as a
//!   reader failure does today;
//! - the agent `gh` front sends the reads it can classify to the gateway
//!   profile ([`crate::agent_gh::read_route`]); everything else stays on the
//!   session's credential (the writer);
//! - the writer profile is untouched: its token, no `api_host`.
//!
//! # Break-glass
//!
//! With `forge.gateway` unset, nothing is published, an existing
//! `.loom/gh-config-gateway/` is removed on the next pass, agents are not told
//! about it, and [`route_read`] is exactly [`crate::forge_identity::route_read`].
//! Below gh 2.100.0 the same holds (logged once): `api_host` would be
//! ignored and the gateway key sent to `api.github.com`. The fleet store
//! fetch ([`crate::fleet_store`]) never uses the gateway, so `fleet.json` can
//! always be read to unset it.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::forge_identity::{Placement, RouteDecision, RouteRequest};

#[path = "forge_gateway/outage.rs"]
pub mod outage;
#[path = "forge_gateway/publish.rs"]
mod publish;
pub use publish::{
    gateway_hosts_yaml, parse_gh_version, profile_api_host, publish_profile, refresh,
    spawn_refresh, teardown, Deps, GhVersion, PassOutcome, Refresher, GATEWAY_CONFIG_YAML,
    MIN_GH_VERSION,
};

#[cfg(test)]
#[path = "forge_gateway/tests.rs"]
mod tests;

/// The config block.
pub const CONFIG_KEY: &str = "forge.gateway";

/// The app id a gateway route carries through the reader chain: withdrawals,
/// spans and the `reader:<app>@<owner>` bucket label name it like a reader.
pub const GATEWAY_APP_ID: &str = "gateway";

/// Exported to agent sessions while the gateway is active: the directory
/// holding the per-owner profiles. Absent (or removed) otherwise.
pub const DIR_ENV: &str = "LOOM_GH_GATEWAY_DIR";

/// `gh`'s update check is a request to `api.github.com` on its own schedule;
/// the daemon and every agent run with it off.
pub const NO_UPDATE_NOTIFIER_ENV: &str = "GH_NO_UPDATE_NOTIFIER";

/// How often [`refresh`] re-reads the config.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// The `.loom/` subdirectory holding the profiles. Distinct from
/// `gh-config-by-owner/` so the bucket-book probes, the writer and reader
/// publications and their path classifier never see a gateway profile, and
/// named `gh-config-*` on purpose: the deployed `.gitignore` block
/// (`.loom/gh-config-*/`) and every stager's credential refusal
/// (`.loom/gh-config` as a prefix) already cover it, so a profile's key can
/// never be committed, even by a stale install (#7818).
pub const DIR_NAME: &str = "gh-config-gateway";

/// Where an owner's key is read from. Holds a location, never a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRef {
    /// `ssm://<parameter>`.
    Ssm(String),
    /// `file:<path>`.
    File(PathBuf),
}

impl KeyRef {
    /// Parse `ssm://<parameter>` or `file:<path>`.
    ///
    /// # Errors
    ///
    /// Any other shape, or an empty location.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if let Some(p) = spec.strip_prefix("ssm://") {
            let p = p.trim();
            return if p.is_empty() {
                Err("ssm:// needs a parameter name".into())
            } else {
                Ok(Self::Ssm(p.to_string()))
            };
        }
        if let Some(p) = spec.strip_prefix("file:") {
            let p = p.trim();
            return if p.is_empty() {
                Err("file: needs a path".into())
            } else {
                Ok(Self::File(PathBuf::from(p)))
            };
        }
        Err("use file:<path> or ssm://<parameter>".into())
    }

    /// Read the key. The error names the source, never the value.
    ///
    /// # Errors
    ///
    /// When the source cannot be read, or holds no plausible key.
    pub fn read(&self, region: Option<&str>) -> Result<String, String> {
        use crate::telemetry_join::{credential_source, Credential, CredentialSpec};
        let fetched = match self {
            Self::Ssm(p) => {
                // An explicit region always: a daemon's environment often
                // has none, and the AWS CLI would then fail or guess.
                let region = region.ok_or_else(|| {
                    format!(
                        "an ssm:// key ref needs a region: set {CONFIG_KEY}.region, or \
                         AWS_REGION / AWS_DEFAULT_REGION in the daemon's environment"
                    )
                })?;
                ssm_get(p, region)?
            }
            Self::File(p) => {
                match credential_source(&CredentialSpec::File { path: p.clone() }, None)
                    .credential()
                {
                    Ok(Credential::Bearer(key)) => key,
                    Ok(_) => {
                        return Err("the source does not hold a single-line gateway key".into())
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
        };
        if valid_key(&fetched) {
            Ok(fetched)
        } else {
            Err("the source does not hold a single-line gateway key".into())
        }
    }
}

/// `aws ssm get-parameter` for `parameter` in `region`, decrypted. Only the
/// parameter's name and the region are on the command line, never a value;
/// an error names the parameter and the exit, never the output.
fn ssm_get(parameter: &str, region: &str) -> Result<String, String> {
    let mut cmd = std::process::Command::new("aws");
    cmd.args(ssm_args(parameter, region))
        .stdin(std::process::Stdio::null());
    match crate::proc_exec::run_bounded(cmd, SSM_TIMEOUT) {
        Ok(crate::proc_exec::Completion::Exited(out)) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(crate::proc_exec::Completion::Exited(out)) => Err(format!(
            "aws ssm get-parameter {parameter} ({region}) exited {}",
            out.status.code().unwrap_or(-1)
        )),
        Ok(_) => Err(format!("aws ssm get-parameter {parameter} ({region}) timed out")),
        Err(e) => Err(format!("aws ssm get-parameter {parameter}: {e}")),
    }
}

/// The `aws` argv of [`ssm_get`].
#[must_use]
pub fn ssm_args(parameter: &str, region: &str) -> Vec<String> {
    [
        "ssm",
        "get-parameter",
        "--name",
        parameter,
        "--with-decryption",
        "--region",
        region,
        "--query",
        "Parameter.Value",
        "--output",
        "text",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// Deadline for one SSM read.
const SSM_TIMEOUT: Duration = Duration::from_secs(30);

/// The region an `ssm://` ref is read in: `forge.gateway.region`, else
/// `AWS_REGION`, else `AWS_DEFAULT_REGION` (the AWS CLI's own order), each
/// only when it is a plausible region name. `None` refuses `ssm://` refs.
#[must_use]
pub fn resolve_region(
    configured: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    configured
        .map(str::to_string)
        .or_else(|| env("AWS_REGION"))
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .map(|r| r.trim().to_string())
        .filter(|r| valid_region(r))
}

/// `us-east-1`-shaped: lowercase letters, digits and dashes.
#[must_use]
pub fn valid_region(r: &str) -> bool {
    (4..=32).contains(&r.len())
        && r.contains('-')
        && r.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A key is one token of URL-safe characters: anything else could not be
/// written into `hosts.yml` as-is.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    (16..=512).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~+/=".contains(&b))
}

/// `forge.gateway`, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayConfig {
    /// The `api_host` every read profile gets.
    pub api_host: String,
    /// Owner (lowercased) → where its key is read.
    pub key_refs: BTreeMap<String, KeyRef>,
    /// `forge.gateway.region`: the AWS region `ssm://` refs are read in,
    /// when set (else the environment's, [`resolve_region`]).
    pub region: Option<String>,
}

impl GatewayConfig {
    /// A stable digest of what the profiles depend on, keys excluded.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        format!("{}|{:?}|{:?}", self.api_host, self.key_refs, self.region)
    }
}

/// The `api_host` of `origin`: `https://host` or a bare `host`, no path,
/// port or credentials, and never GitHub itself (that would send a gateway
/// key to GitHub).
///
/// # Errors
///
/// A reason the origin is refused.
pub fn api_host_of(origin: &str) -> Result<String, String> {
    let o = origin.trim();
    let host = match o.split_once("://") {
        Some(("https", rest)) => rest,
        Some((scheme, _)) => return Err(format!("origin must be https, not {scheme}")),
        None => o,
    };
    let host = host.strip_suffix('/').unwrap_or(host).to_ascii_lowercase();
    // `github.com.` is GitHub too: compare and store without the root dot.
    let host = host.strip_suffix('.').unwrap_or(&host).to_string();
    let ok = host.contains('.')
        && !host.starts_with(['.', '-'])
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !ok {
        return Err("origin must be a bare host name or https://<host>".into());
    }
    if host == "github.com" || host.ends_with(".github.com") {
        return Err("origin must be the gateway, not GitHub".into());
    }
    Ok(host)
}

/// Parse `forge.gateway` from the effective config. `None` when it is
/// absent, `null`, or unusable (with the reasons).
#[must_use]
pub fn parse(effective: &Value) -> (Option<GatewayConfig>, Vec<String>) {
    let mut warnings = Vec::new();
    let block = match crate::config_resolver::get_path(effective, CONFIG_KEY) {
        None | Some(Value::Null) => return (None, warnings),
        Some(Value::Object(m)) => m,
        Some(_) => {
            warnings.push(format!("{CONFIG_KEY} must be an object; the gateway stays off"));
            return (None, warnings);
        }
    };
    let api_host = match block.get("origin").and_then(Value::as_str).map(api_host_of) {
        Some(Ok(h)) => h,
        Some(Err(e)) => {
            warnings.push(format!("{CONFIG_KEY}.origin: {e}; the gateway stays off"));
            return (None, warnings);
        }
        None => {
            warnings.push(format!("{CONFIG_KEY}.origin is required; the gateway stays off"));
            return (None, warnings);
        }
    };
    let refs = block.get("keyRefs").or_else(|| block.get("key_refs"));
    let Some(Value::Object(refs)) = refs else {
        warnings
            .push(format!("{CONFIG_KEY}.keyRefs must map owner -> key ref; the gateway stays off"));
        return (None, warnings);
    };
    let mut key_refs = BTreeMap::new();
    for (owner, spec) in refs {
        if owner.starts_with('_') {
            continue; // a `_note` key
        }
        if !crate::forge_bucket_book::valid_owner(owner) {
            warnings.push(format!("{CONFIG_KEY}.keyRefs: {owner} is not an owner; dropped"));
            continue;
        }
        match spec.as_str().map(KeyRef::parse) {
            Some(Ok(r)) => {
                key_refs.insert(owner.to_ascii_lowercase(), r);
            }
            Some(Err(e)) => warnings.push(format!("{CONFIG_KEY}.keyRefs.{owner}: {e}; dropped")),
            None => {
                warnings.push(format!("{CONFIG_KEY}.keyRefs.{owner} must be a string; dropped"))
            }
        }
    }
    if key_refs.is_empty() {
        warnings.push(format!("{CONFIG_KEY}.keyRefs names no usable owner; the gateway stays off"));
        return (None, warnings);
    }
    let region = match block.get("region") {
        None | Some(Value::Null) => None,
        Some(Value::String(r)) if valid_region(r.trim()) => Some(r.trim().to_string()),
        Some(other) => {
            warnings.push(format!(
                "{CONFIG_KEY}.region must be an AWS region name, not {other}; ignored"
            ));
            None
        }
    };
    (
        Some(GatewayConfig {
            api_host,
            key_refs,
            region,
        }),
        warnings,
    )
}

/// `<workspace>/.loom/gh-config-gateway`.
#[must_use]
pub fn base_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".loom").join(DIR_NAME)
}

/// The owner a `…/.loom/gh-config-gateway/<owner>` directory is the profile of.
#[must_use]
pub fn owner_of_profile_dir(dir: &Path) -> Option<String> {
    let owner = dir.file_name()?.to_str()?;
    let parent = dir.parent()?;
    let is_base = parent.file_name()? == DIR_NAME
        && parent.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new(".loom"));
    (is_base && crate::forge_bucket_book::valid_owner(owner)).then(|| owner.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------
// Which profiles are live in this process
// ---------------------------------------------------------------------------

/// The published profiles, as the daemon's refresh last left them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Active {
    pub base: PathBuf,
    pub owners: BTreeSet<String>,
}

/// `Some` once this process runs the refresh loop (the daemon): from then on
/// only what it published routes, whatever the environment says.
static DAEMON: OnceLock<RwLock<Option<Active>>> = OnceLock::new();

/// The agent front's base directory, when the daemon exported one and its
/// `gh` matches the one the daemon checked ([`enable_front`]).
static FRONT: OnceLock<Option<PathBuf>> = OnceLock::new();

fn set_active(active: Option<Active>) {
    let lock = DAEMON.get_or_init(|| RwLock::new(None));
    if let Ok(mut g) = lock.write() {
        *g = active;
    }
}

/// What the daemon published, if anything.
#[must_use]
pub fn active() -> Option<Active> {
    DAEMON.get()?.read().ok()?.clone()
}

/// The front's half: honour [`DIR_ENV`] only when the `gh` this front execs
/// (`next_gh`) is the one the daemon version-checked. Once per process.
pub fn enable_front(next_gh: &Path) {
    let base = std::env::var_os(DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|base| publish::gh_marker_matches(base, next_gh))
        .filter(|base| !outage::front_outage_active(base, SystemTime::now()));
    let _ = FRONT.set(base);
}

/// Whether any gateway profile can route in this process. `false` while
/// `forge.gateway` is unset, so callers skip even the local work of
/// classifying a call.
#[must_use]
pub fn any_live() -> bool {
    match DAEMON.get() {
        Some(_) => active().is_some(),
        None => FRONT.get().is_some_and(Option::is_some),
    }
}

/// The live gateway profile directory for `owner`, if any.
#[must_use]
pub fn profile_for(owner: &str) -> Option<PathBuf> {
    let owner = owner.to_ascii_lowercase();
    let base = if let Some(lock) = DAEMON.get() {
        let guard = lock.read().ok()?;
        let a = guard.as_ref()?;
        if !a.owners.contains(&owner) {
            return None;
        }
        a.base.clone()
    } else {
        FRONT.get()?.clone()?
    };
    let dir = base.join(&owner);
    dir.join("hosts.yml").is_file().then_some(dir)
}

/// The variables an agent session gets: `GH_NO_UPDATE_NOTIFIER=1` always,
/// and [`DIR_ENV`] set while the gateway is active, removed (`None`)
/// otherwise so an inherited value can never route.
#[must_use]
pub fn agent_env() -> Vec<(&'static str, Option<OsString>)> {
    vec![
        (NO_UPDATE_NOTIFIER_ENV, Some(OsString::from("1"))),
        (DIR_ENV, active().map(|a| a.base.into_os_string())),
    ]
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// The read router the facade and the conditional reads use: the gateway
/// profile for the repo's owner when one is live and not withdrawn, else
/// [`crate::forge_identity::route_read`] unchanged.
#[must_use]
pub fn route_read(req: &RouteRequest<'_>, now: SystemTime) -> RouteDecision {
    decide(req, now, &profile_for).unwrap_or_else(|| crate::forge_identity::route_read(req, now))
}

/// The gateway's answer for `req`, with the profile lookup injected.
#[must_use]
pub fn decide(
    req: &RouteRequest<'_>,
    now: SystemTime,
    lookup: &dyn Fn(&str) -> Option<PathBuf>,
) -> Option<RouteDecision> {
    if !req
        .host
        .is_none_or(|h| h.eq_ignore_ascii_case("github.com"))
    {
        return None;
    }
    let owner = crate::credential_preflight::owner_of_nwo(req.owner_repo).to_ascii_lowercase();
    if owner.is_empty() || is_withdrawn(&owner, req, now) {
        return None;
    }
    let dir = lookup(&owner)?;
    Some(RouteDecision::Reader {
        dir,
        app_id: GATEWAY_APP_ID.to_string(),
        placement: Placement::Home,
    })
}

/// `decision`, unless it is the gateway: then the reader pool's answer for
/// `req`. For a read rerunning past a gateway fault that did not withdraw it
/// ([`outage::Fault::Rerun`]), so the rerun reaches the readers before the
/// writer.
#[must_use]
pub fn past_gateway(
    decision: RouteDecision,
    req: &RouteRequest<'_>,
    now: SystemTime,
) -> RouteDecision {
    match decision {
        RouteDecision::Reader { ref app_id, .. } if app_id == GATEWAY_APP_ID => {
            crate::forge_identity::route_read(req, now)
        }
        other => other,
    }
}

/// A gateway failure withdraws it through the reader tables
/// ([`crate::forge_identity::withdraw_after`]); honour every scope.
fn is_withdrawn(owner: &str, req: &RouteRequest<'_>, now: SystemTime) -> bool {
    crate::forge_read_pool::is_withdrawn_at(GATEWAY_APP_ID, now)
        || crate::forge_identity::repo_withdrawn_at(GATEWAY_APP_ID, req.owner_repo, now)
        || crate::forge_read_pool::is_withdrawn_scoped_at(GATEWAY_APP_ID, owner, req.resource, now)
}

/// The `route` label of a `loom.forge.calls` row: `gateway` for a call the
/// gateway served, `direct_read` for a read that reached GitHub directly (a
/// reader App or the writer: the leak a classifier gap shows up as), and
/// `direct` for everything else.
#[must_use]
pub fn route_label(cred_kind: &str, read: bool) -> &'static str {
    if cred_kind == publish::CRED_KIND {
        "gateway"
    } else if read {
        "direct_read"
    } else {
        "direct"
    }
}

/// The closed `route` vocabulary (ingest validation).
pub const ROUTES: &[&str] = &["gateway", "direct_read", "direct", "none"];

/// The credential kind a gateway profile books as.
pub use publish::CRED_KIND;
