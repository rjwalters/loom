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

    fn api(&self, args: &[&str]) -> Result<String, String> {
        let mut cmd = Command::new(&self.gh);
        cmd.arg("api").args(args).stdin(Stdio::null());
        if let Some(dir) = &self.config_dir {
            cmd.env("GH_CONFIG_DIR", dir);
        }
        match crate::cmd_out::run_command(cmd, PROBE_TIMEOUT) {
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

fn cache_dir() -> PathBuf {
    match std::env::var("LOOM_WRITE_SCOPE_CACHE_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => crate::forge_etag_store::host_tmp_base().join("loom-write-scope"),
    }
}

fn memory() -> &'static Mutex<HashMap<String, (Permission, SystemTime)>> {
    static M: OnceLock<Mutex<HashMap<String, (Permission, SystemTime)>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cache key: which credential, which repository. A token in the
/// environment is folded in as a digest, never stored.
pub(crate) fn cache_key(config_dir: Option<&Path>, repo: &str) -> String {
    use sha2::{Digest, Sha256};
    let token = ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();
    let env_dir = std::env::var("GH_CONFIG_DIR").unwrap_or_default();
    let dir = config_dir.map_or(env_dir, |d| d.display().to_string());
    let digest = Sha256::digest(format!("{dir}\0{token}\0{}", repo.to_ascii_lowercase()));
    hex::encode(&digest[..16])
}

/// A probe with the memory + disk cache in front of it.
pub(crate) struct Cached<P> {
    pub(crate) inner: P,
    pub(crate) key_dir: Option<PathBuf>,
}

impl<P: PermissionProbe> PermissionProbe for Cached<P> {
    fn permission(&self, repo: &str) -> Permission {
        let key = cache_key(self.key_dir.as_deref(), repo);
        let now = SystemTime::now();
        let fresh = |at: SystemTime, p: &Permission| {
            let limit = if matches!(p, Permission::Unknown(_)) {
                UNKNOWN_TTL
            } else {
                ttl()
            };
            now.duration_since(at).is_ok_and(|age| age < limit)
        };
        if let Some((p, at)) = memory().lock().ok().and_then(|m| m.get(&key).cloned()) {
            if fresh(at, &p) {
                return p;
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
        if let Ok(mut m) = memory().lock() {
            m.insert(key, (p.clone(), now));
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
        p
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
