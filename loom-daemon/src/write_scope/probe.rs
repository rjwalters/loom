//! "Can the credential this write would use actually write to OWNER/REPO?"
//! (#9548), as a cached forge probe.
//!
//! Two legs, because the two kinds of credential Loom runs under answer
//! different questions:
//!
//! 1. **`GET /repos/{owner}/{repo}` → `permissions`.** For a user token this
//!    is the viewer's own role; `push`, `maintain` or `admin` is WRITE.
//! 2. **`GET /installation/repositories`.** A GitHub App *installation* token
//!    gets an all-`false` `permissions` object from leg 1 even on a repository
//!    it writes to every day (checked against a live fleet installation), so
//!    leg 1 alone would refuse every App-driven write. An installation token
//!    can write only inside its installation, and this listing is exactly that
//!    set. A user token is refused this endpoint, which is how the two are
//!    told apart.
//!
//! Anything that is not a verified WRITE is a refusal: a probe that could not
//! run is `Unknown`, and the caller fails closed on it. Definitive answers are
//! cached per credential and repository for [`ttl`] (memory, plus a private
//! on-disk entry so short-lived `loom-daemon forge may-write` invocations from
//! shell share one probe); an `Unknown` is kept for a minute only.
//!
//! **The two TTLs stack.** The installation leg is answered from the writer's
//! installation snapshot ([`crate::forge_repo_facts::installation`], W8),
//! which is itself up to an hour old when this probe reads it, and the
//! answer is then cached here for [`ttl`]. So a repository removed from the
//! installation can keep a cached WRITE for up to about two hours with the
//! defaults (snapshot TTL + probe TTL), not one. The write itself is still
//! refused by the forge; what lags is this pre-check.
//!
//! **Stale-while-unverifiable.** When an expired WRITE is re-probed and the
//! probe cannot answer (rate limit, outage), a WRITE verified within the last
//! [`STALE_WRITE_GRACE`] still answers WRITE. Without that, one GitHub blip at
//! the hourly re-probe would hold every write and every dispatch on the host
//! until it cleared. It never outlives a definitive answer: an `Insufficient`
//! from the forge drops the grace at once.
//!
//! Gitea (#9699) probes the same rule against its own API:
//! `GET /api/v1/repos/{owner}/{repo}` carries a `permissions` object
//! (`admin` / `push` / `pull`) computed for the authenticated user, and
//! `push` or `admin` is WRITE. The credential comes from the same places the
//! writes themselves do — `GITEA_TOKEN`, then `.loom/config.json`'s
//! `forge.gitea.token`, then `FORGE_TOKEN`; `GITEA_URL` and `GITEA_USERNAME`
//! beat their config keys — and a connection that cannot be resolved or
//! reached is `Unknown`, never WRITE.
//!
//! **Cache key space.** Every key names its forge and host ([`CacheScope`]),
//! so a GitHub answer can never stand in for a Gitea probe of the same
//! `owner/repo` slug, or the reverse. A Gitea connection that cannot resolve
//! has no key at all: it is never looked up, never stored, and never upgraded
//! by the stale-WRITE grace, so "no Gitea credential" always refuses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What the probe established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Permission {
    /// Verified WRITE (or better).
    Write,
    /// Verified less than WRITE; the text names what it has.
    Insufficient(String),
    /// Could not verify; the text is the failure.
    Unknown(String),
}

/// A permission source, so decisions are testable without a forge.
pub(crate) trait PermissionProbe {
    fn permission(&self, repo: &str) -> Permission;

    /// The forge, host and credential this probe answers for: the key space
    /// [`Cached`] files its answers under. Required, with no default, so a new
    /// probe cannot silently inherit another forge's cache entries.
    fn cache_scope(&self) -> CacheScope;
}

/// Which key space a probe's answers live in (#9699). The forge is always
/// part of the key, so a GitHub entry and a Gitea entry for the same
/// `owner/repo` never collide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CacheScope {
    /// `gh` against `GH_HOST` (default `github.com`), under the cache's
    /// `GH_CONFIG_DIR` and any `GH_TOKEN`/`GITHUB_TOKEN`.
    GitHub,
    /// A resolved Gitea connection: its base URL, and [`GiteaProbe::cache_id`]
    /// (the URL, token and username hashed together).
    Gitea { base_url: String, id: String },
    /// No resolved credential: nothing may be cached for it or read on its
    /// behalf, and the stale-WRITE grace never applies.
    Unresolved,
}

/// Seconds a definitive answer is reused. `LOOM_WRITE_SCOPE_TTL_SECS`
/// overrides; default one hour.
pub(crate) fn ttl() -> Duration {
    std::env::var("LOOM_WRITE_SCOPE_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(Duration::from_secs(3600), Duration::from_secs)
}

const UNKNOWN_TTL: Duration = Duration::from_secs(60);
/// How long a verified WRITE may stand in for a probe that cannot answer.
pub(crate) const STALE_WRITE_GRACE: Duration = Duration::from_secs(24 * 3600);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// The production probe: `gh api` under a given credential environment.
pub(crate) struct GhProbe {
    gh: PathBuf,
    /// `GH_CONFIG_DIR` to run under; `None` inherits the process's.
    config_dir: Option<PathBuf>,
}

impl GhProbe {
    pub(crate) fn new(gh: PathBuf, config_dir: Option<PathBuf>) -> Self {
        Self { gh, config_dir }
    }

    /// Counted through the `gh` facade as `write_scope.probe` (#10089). The
    /// explicit `config_dir` is the credential under test, so it overrides
    /// the facade's working-directory lookup; `None` inherits the process's.
    fn api(&self, args: &[&str]) -> Result<String, String> {
        use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
        let inv = GhInvocation::new(
            Operation::new("write_scope.probe"),
            AccessIntent::Read,
            GhTarget::None,
            PROBE_TIMEOUT,
        )
        .program(&self.gh)
        .gh_config_dir(self.config_dir.as_deref())
        // Asker-dependent: the probe measures THIS credential's write scope.
        .writer_identity()
        .arg("api")
        .args(args);
        match inv.run() {
            crate::cmd_out::CmdOutcome::Ran(o) if o.status.success() => {
                Ok(String::from_utf8_lossy(&o.stdout).into_owned())
            }
            other => Err(other
                .failure_reason("gh api")
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()),
        }
    }
}

/// Leg 1's finding for a `permissions` object with no role set. It is what a
/// user with no access gets — and what an App installation token gets even on
/// a repository it writes to, so on its own it is no verdict about an App.
pub(crate) const NO_REPOSITORY_ROLE: &str = "repository role `none`";

/// Whether leg 1 named a real role below WRITE (`pull`, `triage`). Only a
/// user token is ever told one, and for a user token leg 1 is the whole
/// answer: definitive, whatever the installation listing would have said.
pub(crate) fn names_a_lesser_role(p: &Permission) -> bool {
    matches!(p, Permission::Insufficient(why) if why != NO_REPOSITORY_ROLE)
}

/// Classify leg 1's `permissions` object.
pub(crate) fn classify_repo_permissions(json: &str) -> Option<Permission> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let p = v.as_object()?;
    let has = |k: &str| p.get(k).and_then(serde_json::Value::as_bool) == Some(true);
    if has("admin") || has("maintain") || has("push") {
        return Some(Permission::Write);
    }
    let role = ["triage", "pull"]
        .into_iter()
        .find(|k| has(k))
        .unwrap_or("none");
    Some(Permission::Insufficient(format!("repository role `{role}`")))
}

impl PermissionProbe for GhProbe {
    fn permission(&self, repo: &str) -> Permission {
        // W8: the writer's own installation snapshot answers the installation
        // leg; leg 1 runs only for a user token or when it cannot answer.
        let cred =
            crate::forge_repo_facts::installation::Credential::writer(self.config_dir.clone());
        let snapshot = crate::forge_repo_facts::installation::lookup(&self.gh, &cred, repo);
        if let Some(p) = super::probe_snapshot::from_snapshot(&snapshot, || self.repo_leg(repo)) {
            return p;
        }
        self.legacy_permission(repo)
    }

    fn cache_scope(&self) -> CacheScope {
        CacheScope::GitHub
    }
}

impl GhProbe {
    /// Leg 1 alone: `GET repos/{repo}` → `permissions`.
    fn repo_leg(&self, repo: &str) -> Result<Option<Permission>, String> {
        let path = format!("repos/{repo}");
        self.api(&[&path, "--jq", ".permissions // {}"])
            .map(|s| classify_repo_permissions(&s))
    }

    /// Both legs, as before W8 (the snapshot is switched off).
    fn legacy_permission(&self, repo: &str) -> Permission {
        let path = format!("repos/{repo}");
        let leg1 = self
            .api(&[&path, "--jq", ".permissions // {}"])
            .map(|s| classify_repo_permissions(&s));
        if let Ok(Some(Permission::Write)) = leg1 {
            return Permission::Write;
        }
        let leg2 = self.api(&[
            "installation/repositories",
            "--paginate",
            "--jq",
            ".repositories[].full_name",
        ]);
        match (leg1, leg2) {
            (_, Ok(list)) if list.lines().any(|l| l.trim().eq_ignore_ascii_case(repo)) => {
                Permission::Write
            }
            (_, Ok(_)) => Permission::Insufficient("not in this App installation".into()),
            (Ok(Some(p)), Err(_)) => p,
            (Ok(None), Err(e)) => Permission::Unknown(format!("unparseable permissions ({e})")),
            (Err(e), Err(_)) => Permission::Unknown(e),
        }
    }
}

/// Classify Gitea's `permissions` object from a `GET
/// /api/v1/repos/{owner}/{repo}` response body (#9699). Gitea computes it for
/// the authenticated user: `admin` / `push` / `pull`. `push` or `admin` is
/// WRITE; anything less is `Insufficient`, naming the role it does have.
/// `None` when the body is not a repo object with a `permissions` object —
/// the caller answers `Unknown` on it.
pub(crate) fn classify_gitea_permissions(body: &str) -> Option<Permission> {
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let p = v.get("permissions")?.as_object()?;
    let has = |k: &str| p.get(k).and_then(serde_json::Value::as_bool) == Some(true);
    if has("admin") || has("push") {
        return Some(Permission::Write);
    }
    let role = if has("pull") { "pull" } else { "none" };
    Some(Permission::Insufficient(format!("repository role `{role}`")))
}

/// Split curl's `-o - -w "\n%{http_code}"` output into the response body and
/// the numeric status. The marker is the *last* newline: a JSON body never
/// contains a raw one.
fn split_body_and_status(raw: &str) -> Option<(&str, u16)> {
    let (body, code) = raw.rsplit_once('\n')?;
    Some((body, code.trim().parse().ok()?))
}

/// The Gitea production probe (#9699): `GET {base}/api/v1/repos/{owner}/{repo}`
/// with the resolved credential, classified by
/// [`classify_gitea_permissions`]. House style is the sync `curl` subprocess
/// (`tokens_pool::check`, `script_helpers::usage`): the probe runs deep inside
/// sync daemon passes, and — as there — the credential is never placed in
/// argv, where it would be world-readable via `/proc/<pid>/cmdline` and `ps`
/// (#5982); the `Authorization` header is written to curl's stdin (`-H @-`).
///
/// Anything that is not a 200 carrying a parseable `permissions` object is
/// `Unknown`, which the caller fails closed on: a wrong URL, a refused
/// credential or an outage refuses the write rather than guessing.
pub(crate) struct GiteaProbe {
    /// The resolved connection; `Err` carries why it could not resolve (no
    /// credential configured, Basic auth over `http://`, …) and answers
    /// `Unknown`.
    resolved: Result<crate::forge_cmd::GiteaConfig, String>,
    /// The resolved connection hashed once (digest only — the token is never
    /// stored), for [`cache_key`]; empty when unresolved.
    id: String,
}

impl GiteaProbe {
    /// Resolve the credential writes from `root` will carry. `GITEA_URL`
    /// beats `.loom/config.json`'s `forge.gitea.url`; the token falls back
    /// `GITEA_TOKEN`, then config, then `FORGE_TOKEN` (the generic fallback
    /// `defaults/docs/forge-authentication.md` documents — a repository's own
    /// config should outrank a machine-wide generic token); `GITEA_USERNAME`
    /// beats config. Validation (no `:` in a Basic-auth username, HTTPS for
    /// Basic auth) is `gitea_config_from_forge`'s, not duplicated here.
    pub(crate) fn for_root(root: &Path) -> Self {
        use sha2::{Digest, Sha256};
        let resolved = Self::resolve(root);
        let id = resolved
            .as_ref()
            .ok()
            .map(|c| {
                hex::encode(
                    &Sha256::digest(format!(
                        "{}\0{}\0{}",
                        c.base_url,
                        c.token,
                        c.username.as_deref().unwrap_or_default()
                    ))[..16],
                )
            })
            .unwrap_or_default();
        Self { resolved, id }
    }

    /// The resolved connection's digest; empty when the connection could not
    /// resolve. Production reads it through [`PermissionProbe::cache_scope`].
    #[cfg(test)]
    pub(crate) fn cache_id(&self) -> &str {
        &self.id
    }

    fn resolve(root: &Path) -> Result<crate::forge_cmd::GiteaConfig, String> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let mut forge = crate::forge_cmd::resolve_forge_config(Some(root));
        if !forge.is_object() {
            forge = serde_json::Value::Object(serde_json::Map::new());
        }
        let obj = forge.as_object_mut().ok_or("forge is not an object")?;
        let gitea = obj
            .entry("gitea")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let Some(o) = gitea.as_object_mut() else {
            return Err("`forge.gitea` is not an object".into());
        };
        // The config's own token, read before the env overrides land: the
        // fallback order is GITEA_TOKEN, then config, then FORGE_TOKEN.
        let config_token = o
            .get("token")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string);
        let mut set = |key: &str, value: Option<String>| {
            if let Some(v) = value {
                o.insert(key.to_string(), serde_json::Value::String(v));
            }
        };
        set("url", env("GITEA_URL"));
        set(
            "token",
            env("GITEA_TOKEN")
                .or(config_token)
                .or_else(|| env("FORGE_TOKEN")),
        );
        set("username", env("GITEA_USERNAME"));
        let cfg = crate::forge_cmd::gitea_config_from_forge(&forge).map_err(|e| e.to_string())?;
        // The header goes to curl's stdin (`-H @-`), one header per line: a
        // CR or LF in the credential would smuggle extra header lines in.
        let has_break = |v: &str| v.contains(['\r', '\n']);
        if has_break(&cfg.token) || cfg.username.as_deref().is_some_and(has_break) {
            return Err("the Gitea credential contains a line break".into());
        }
        Ok(cfg)
    }

    fn probe(&self, cfg: &crate::forge_cmd::GiteaConfig, repo: &str) -> Permission {
        let auth = match &cfg.username {
            // base64 of `user:password`, computed here so the credential
            // still never reaches argv (curl's own `-u` would put it there).
            Some(u) => {
                use base64::engine::general_purpose;
                use base64::Engine as _;
                format!(
                    "Authorization: Basic {}\n",
                    general_purpose::STANDARD.encode(format!("{u}:{}", cfg.token))
                )
            }
            None => format!("Authorization: token {}\n", cfg.token),
        };
        let mut cmd = Command::new("curl");
        // `-q` must come first: it stops `~/.curlrc` adding `--location`, a
        // proxy or anything else to a credentialed request. `--globoff`
        // keeps `{}`/`[]` in the interpolated repo slug literal.
        cmd.args(["-q", "--globoff", "--silent", "--show-error", "--max-time"])
            .arg(PROBE_TIMEOUT.as_secs_f64().to_string())
            .arg("-o")
            .arg("-")
            .arg("-w")
            .arg("\n%{http_code}")
            .arg("-H")
            .arg("@-")
            .arg(format!("{}/api/v1/repos/{repo}", cfg.base_url))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let Ok(mut child) = cmd.spawn() else {
            return Permission::Unknown("could not spawn curl".into());
        };
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write as _;
            let _ = stdin.write_all(auth.as_bytes());
        }
        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(e) => return Permission::Unknown(format!("curl: {e}")),
        };
        if !output.status.success() {
            let why = String::from_utf8_lossy(&output.stderr);
            return Permission::Unknown(format!(
                "curl exit {}: {}",
                output.status.code().unwrap_or(-1),
                why.trim().lines().next().unwrap_or_default()
            ));
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        // A 200 with a parseable `permissions` object is the only definitive
        // answer; every other status (a wrong URL 404s, a refused credential
        // 401/403s) is `Unknown` — the issue's fail-closed rule.
        let Some((_, 200)) = split_body_and_status(&raw) else {
            let status = raw.rsplit('\n').next().unwrap_or("").trim();
            return Permission::Unknown(format!(
                "the Gitea API answered {}",
                if status.is_empty() {
                    "nothing parseable"
                } else {
                    status
                }
            ));
        };
        classify_gitea_permissions(body_of(&raw))
            .unwrap_or_else(|| Permission::Unknown("unparseable repo payload".into()))
    }
}

/// The response body from combined `-o - -w "\n%{http_code}"` output.
fn body_of(raw: &str) -> &str {
    raw.rsplit_once('\n').map_or(raw, |(body, _)| body)
}

impl PermissionProbe for GiteaProbe {
    fn permission(&self, repo: &str) -> Permission {
        match &self.resolved {
            Ok(cfg) => self.probe(cfg, repo),
            Err(why) => Permission::Unknown(why.clone()),
        }
    }

    fn cache_scope(&self) -> CacheScope {
        match &self.resolved {
            Ok(cfg) => CacheScope::Gitea {
                base_url: cfg.base_url.clone(),
                id: self.id.clone(),
            },
            Err(_) => CacheScope::Unresolved,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DiskEntry {
    write: bool,
    detail: String,
    at: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(crate) fn cache_dir() -> PathBuf {
    match std::env::var("LOOM_WRITE_SCOPE_CACHE_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => crate::forge_etag_store::host_tmp_base().join("loom-write-scope"),
    }
}

/// One remembered answer, and when WRITE was last actually verified.
#[derive(Clone)]
struct MemEntry {
    p: Permission,
    at: SystemTime,
    last_write: Option<SystemTime>,
}

fn memory() -> &'static Mutex<HashMap<String, MemEntry>> {
    static M: OnceLock<Mutex<HashMap<String, MemEntry>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cache key: which forge and host, which credential, which repository;
/// `None` for [`CacheScope::Unresolved`], which is never cached. The forge
/// name leads the key material, so the GitHub and Gitea key spaces cannot
/// overlap for the same `owner/repo` (#9699).
///
/// GitHub material is `GH_HOST`, the `GH_CONFIG_DIR` and `GH_TOKEN` /
/// `GITHUB_TOKEN` (a token is folded in as a digest, never stored). Gitea's is
/// the base URL plus [`GiteaProbe::cache_id`] — the resolved connection hashed
/// once when the probe was built, so the key always matches exactly the
/// credential a write would carry (a config-file token included).
pub(crate) fn cache_key(
    config_dir: Option<&Path>,
    scope: &CacheScope,
    repo: &str,
) -> Option<String> {
    use sha2::{Digest, Sha256};
    let repo = repo.to_ascii_lowercase();
    let material = match scope {
        CacheScope::Unresolved => return None,
        CacheScope::GitHub => {
            let token = ["GH_TOKEN", "GITHUB_TOKEN"]
                .iter()
                .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
                .unwrap_or_default();
            let host = std::env::var("GH_HOST")
                .ok()
                .filter(|h| !h.trim().is_empty())
                .unwrap_or_else(|| "github.com".into())
                .to_ascii_lowercase();
            let env_dir = std::env::var("GH_CONFIG_DIR").unwrap_or_default();
            let dir = config_dir.map_or(env_dir, |d| d.display().to_string());
            format!("github\0{host}\0{dir}\0{token}\0{repo}")
        }
        CacheScope::Gitea { base_url, id } => {
            if id.is_empty() {
                return None;
            }
            format!("gitea\0{base_url}\0{id}\0{repo}")
        }
    };
    Some(hex::encode(&Sha256::digest(material)[..16]))
}

/// A probe with the memory + disk cache in front of it. The key space comes
/// from the probe itself ([`PermissionProbe::cache_scope`]), never from the
/// caller, so a probe cannot be filed under another forge's answers.
pub(crate) struct Cached<P> {
    pub(crate) inner: P,
    pub(crate) key_dir: Option<PathBuf>,
}

/// An `Unknown` becomes WRITE when WRITE was verified recently enough.
fn with_grace(p: Permission, last_write: Option<SystemTime>, now: SystemTime) -> Permission {
    let recent = last_write
        .and_then(|at| now.duration_since(at).ok())
        .is_some_and(|age| age < STALE_WRITE_GRACE);
    match p {
        Permission::Unknown(why) if recent => {
            log::debug!(
                "write_scope: probe could not answer ({why}); using the WRITE verified <24h ago"
            );
            Permission::Write
        }
        other => other,
    }
}

impl<P: PermissionProbe> PermissionProbe for Cached<P> {
    fn cache_scope(&self) -> CacheScope {
        self.inner.cache_scope()
    }

    fn permission(&self, repo: &str) -> Permission {
        let Some(key) = cache_key(self.key_dir.as_deref(), &self.inner.cache_scope(), repo) else {
            // No resolved credential: no cache entry may answer for it and no
            // grace may upgrade it. Even a WRITE from such a probe is a
            // contradiction, so it refuses too.
            return match self.inner.permission(repo) {
                Permission::Write => {
                    Permission::Unknown("no resolved credential to verify WRITE".into())
                }
                other => other,
            };
        };
        let now = SystemTime::now();
        let fresh = |at: SystemTime, p: &Permission| {
            let limit = if matches!(p, Permission::Unknown(_)) {
                UNKNOWN_TTL
            } else {
                ttl()
            };
            now.duration_since(at).is_ok_and(|age| age < limit)
        };
        let mem = memory().lock().ok().and_then(|m| m.get(&key).cloned());
        let mut last_write = mem.as_ref().and_then(|e| e.last_write);
        if let Some(e) = &mem {
            if fresh(e.at, &e.p) {
                return with_grace(e.p.clone(), last_write, now);
            }
        }
        let path = cache_dir().join(format!("{key}.json"));
        if crate::forge_etag_store::private_dir(&cache_dir(), false) {
            if let Some(e) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<DiskEntry>(&s).ok())
            {
                let at = UNIX_EPOCH + Duration::from_secs(e.at);
                let p = if e.write {
                    last_write = last_write.max(Some(at));
                    Permission::Write
                } else {
                    Permission::Insufficient(e.detail)
                };
                if fresh(at, &p) {
                    return p;
                }
            }
        }
        let p = self.inner.permission(repo);
        match &p {
            Permission::Write => last_write = Some(now),
            Permission::Insufficient(_) => last_write = None,
            Permission::Unknown(_) => {}
        }
        if let Ok(mut m) = memory().lock() {
            m.insert(
                key,
                MemEntry {
                    p: p.clone(),
                    at: now,
                    last_write,
                },
            );
        }
        let entry = match &p {
            Permission::Write => Some((true, String::new())),
            Permission::Insufficient(d) => Some((false, d.clone())),
            Permission::Unknown(_) => None,
        };
        if let Some((write, detail)) = entry {
            write_entry(
                &path,
                &DiskEntry {
                    write,
                    detail,
                    at: now_secs(),
                },
            );
        }
        with_grace(p, last_write, now)
    }
}

fn write_entry(path: &Path, entry: &DiskEntry) {
    use std::io::Write;
    let Some(dir) = path.parent() else { return };
    if !crate::forge_etag_store::private_dir(dir, true) {
        return;
    }
    let Ok(body) = serde_json::to_string(entry) else {
        return;
    };
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), now_secs()));
    let Ok(mut f) = crate::forge_etag_store::create_private_file(&tmp) else {
        return;
    };
    if f.write_all(body.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Forget every in-memory answer (tests).
#[cfg(test)]
pub(crate) fn clear_memory() {
    if let Ok(mut m) = memory().lock() {
        m.clear();
    }
}
