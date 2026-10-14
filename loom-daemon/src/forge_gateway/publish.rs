//! Publishing the gateway read profiles: the daemon's refresh pass, the gh
//! version gate, and the profile files themselves.

use std::collections::{BTreeSet, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{base_dir, parse, set_active, Active, GatewayConfig, KeyRef, CHECK_INTERVAL};

/// The credential kind (`tk`) a gateway profile's rows carry.
pub const CRED_KIND: &str = "gateway";

/// Below this, `gh` ignores `api_host` without a word and would send the
/// gateway key to `api.github.com`.
pub const MIN_GH_VERSION: GhVersion = GhVersion(2, 100, 0);

/// How often the profiles are rewritten even when nothing changed, so a
/// rotated key (a store edit plus apply) is picked up.
const REPUBLISH: Duration = crate::credential_preflight::GITHUB_APP_REFRESH_INTERVAL;

/// The file beside the profiles naming the `gh` the daemon version-checked.
const GH_MARKER: &str = "gh-ok";

/// `gh --version` deadline: it never touches the network.
const GH_VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// A `gh` release, `MAJOR.MINOR.PATCH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GhVersion(pub u32, pub u32, pub u32);

impl std::fmt::Display for GhVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The version in `gh --version` output (`gh version 2.102.0 (2026-…)`).
#[must_use]
pub fn parse_gh_version(out: &str) -> Option<GhVersion> {
    let line = out.lines().next()?;
    let mut words = line.split_whitespace();
    words.find(|w| *w == "version")?;
    let v = words.next()?.trim_start_matches('v');
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(str::parse::<u32>);
    let (a, b, c) = (parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?);
    Some(GhVersion(a, b, c))
}

/// What a pass needs from the outside world (tests inject it).
pub trait Deps {
    /// The `gh` the daemon runs, canonicalized, and its version.
    fn gh(&self, base: &Path) -> Option<(PathBuf, GhVersion)>;
    /// Read an owner's key.
    ///
    /// # Errors
    ///
    /// A reason that never contains the key.
    fn read_key(&self, r: &KeyRef, aws: &super::AwsAccess<'_>) -> Result<String, String>;
}

struct RealDeps;

impl Deps for RealDeps {
    fn gh(&self, base: &Path) -> Option<(PathBuf, GhVersion)> {
        let program = crate::gh_invocation::resolver::resolve().program;
        let path = canonical_program(&program)?;
        // Through the facade, under the (credential-less) base directory as
        // GH_CONFIG_DIR: a `gh` wrapper that mints a token when the caller
        // chose none (the workers' App shim) passes straight through.
        let out = crate::gh_invocation::GhInvocation::new(
            crate::gh_invocation::Operation::new("gh.version"),
            crate::gh_invocation::AccessIntent::Read,
            crate::gh_invocation::GhTarget::None,
            GH_VERSION_TIMEOUT,
        )
        .program(&program)
        .arg("--version")
        .gh_config_dir(Some(base))
        .without_token_env()
        .gateway_exempt()
        .execute()
        .ok()?;
        let crate::gh_invocation::GhCompletion::Captured(crate::proc_exec::Completion::Exited(out)) =
            out
        else {
            return None;
        };
        let v = parse_gh_version(&String::from_utf8_lossy(&out.stdout))?;
        Some((path, v))
    }

    fn read_key(&self, r: &KeyRef, aws: &super::AwsAccess<'_>) -> Result<String, String> {
        r.read(aws, super::CONFIG_KEY)
    }
}

/// `program` (a path, or a bare name looked up on `PATH`), canonicalized.
pub(super) fn canonical_program(program: &str) -> Option<PathBuf> {
    let p = Path::new(program);
    let found = if p.components().count() > 1 {
        Some(p.to_path_buf())
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(program))
            .find(|c| crate::agent_gh::next_gh::is_executable(c))
    }?;
    std::fs::canonicalize(found).ok()
}

/// Whether `base`'s marker names `gh` (canonicalized). The front calls this.
#[must_use]
pub fn gh_marker_matches(base: &Path, gh: &Path) -> bool {
    let Ok(recorded) = std::fs::read_to_string(base.join(GH_MARKER)) else {
        return false;
    };
    std::fs::canonicalize(gh).is_ok_and(|c| c.to_string_lossy() == recorded.trim())
}

/// Whether `path` is a real (non-symlink) directory owned by this uid that
/// no other user can write to: what the daemon publishes (0700).
fn private_dir(path: &Path) -> bool {
    owned_and_closed(path, true)
}

/// As [`private_dir`], for a regular file (the daemon writes 0600).
fn private_file(path: &Path) -> bool {
    owned_and_closed(path, false)
}

fn owned_and_closed(path: &Path, dir: bool) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(m) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let kind = if dir { m.is_dir() } else { m.is_file() };
    // SAFETY: geteuid only reads the caller's effective user id.
    kind && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0
}

/// Whether a consumer other than the daemon may trust `base` (the value of
/// `LOOM_GH_GATEWAY_DIR`): the shape the daemon publishes ([`base_dir`]),
/// and a base directory it alone could have written. The front and a CLI
/// subcommand share this one check.
#[must_use]
pub fn safe_base(base: &Path) -> bool {
    base.file_name().is_some_and(|n| n == super::DIR_NAME)
        && base
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == ".loom")
        && private_dir(base)
}

/// Whether a consumer may trust the published profile `dir` (`<base>/<owner>`)
/// and its `hosts.yml`: neither a symlink, both ours and closed to others.
#[must_use]
pub fn safe_profile(dir: &Path) -> bool {
    private_dir(dir) && private_file(&dir.join("hosts.yml"))
}

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassOutcome {
    /// `forge.gateway` is unset (or unusable): nothing is published.
    Off,
    /// `gh` is missing or below [`MIN_GH_VERSION`]: nothing is published.
    GhTooOld(Option<GhVersion>),
    /// Profiles (re)written for `owners`; `failed` could not be.
    Published {
        owners: Vec<String>,
        failed: Vec<String>,
    },
    /// Nothing was due.
    Unchanged,
}

/// The refresh loop's memory between passes.
#[derive(Debug, Default)]
pub struct Refresher {
    fingerprint: Option<String>,
    at: Option<Instant>,
    published: BTreeSet<String>,
    logged: HashSet<String>,
}

impl Refresher {
    fn log_once(&mut self, key: String, f: impl FnOnce(&str)) {
        if self.logged.insert(key.clone()) {
            f(&key);
        }
    }

    /// One pass for the workspace at `ws` against its live config.
    pub fn pass(&mut self, ws: &Path, now: Instant) -> PassOutcome {
        let effective = crate::config_resolver::resolve_effective_config(ws);
        self.pass_with(ws, &effective, now, &RealDeps)
    }

    /// [`Refresher::pass`] with the config and the outside world injected.
    pub fn pass_with(
        &mut self,
        ws: &Path,
        effective: &Value,
        now: Instant,
        deps: &dyn Deps,
    ) -> PassOutcome {
        let base = base_dir(ws);
        let (cfg, warnings) = parse(effective);
        for w in warnings {
            self.log_once(w, |w| log::warn!("forge_gateway: {w} — 2am#4367"));
        }
        let Some(cfg) = cfg else {
            if self.fingerprint.take().is_some() || base.exists() {
                log::info!(
                    "forge_gateway: forge.gateway is unset; removing the gateway read profiles, \
                     reads use the readers and the writer as before — 2am#4367"
                );
            }
            self.off(&base);
            return PassOutcome::Off;
        };
        let fp = cfg.fingerprint();
        let missing = self
            .published
            .iter()
            .any(|o| !base.join(o).join("hosts.yml").is_file());
        let due = self.fingerprint.as_deref() != Some(fp.as_str())
            || self.at.is_none_or(|at| now.duration_since(at) >= REPUBLISH)
            || missing;
        if !due {
            return PassOutcome::Unchanged;
        }
        self.fingerprint = Some(fp);
        self.at = Some(now);
        if let Err(e) = ensure_private_dir(&base) {
            self.log_once(format!("cannot create {}: {e}", base.display()), |m| {
                log::warn!("forge_gateway: {m}; the gateway stays off — 2am#4367");
            });
            self.off(&base);
            return PassOutcome::Off;
        }
        let gh = deps.gh(&base);
        let seen = gh.as_ref().map(|(_, v)| *v);
        let Some((gh_path, version)) = gh.filter(|(_, v)| *v >= MIN_GH_VERSION) else {
            let shown = seen.map_or_else(|| "unknown".to_string(), |v| v.to_string());
            self.log_once(format!("gh {shown}"), |_| {
                log::warn!(
                    "forge_gateway: forge.gateway is set but gh is {shown}, below \
                     {MIN_GH_VERSION} (older gh ignores api_host); this host keeps today's read \
                     routing until gh is upgraded — 2am#4367"
                );
            });
            self.off(&base);
            // Re-checked at the next republish, not every minute.
            self.fingerprint = Some(cfg.fingerprint());
            self.at = Some(now);
            return PassOutcome::GhTooOld(seen);
        };
        self.publish_all(&base, &cfg, &gh_path, version, deps)
    }

    fn publish_all(
        &mut self,
        base: &Path,
        cfg: &GatewayConfig,
        gh_path: &Path,
        version: GhVersion,
        deps: &dyn Deps,
    ) -> PassOutcome {
        let mut owners = Vec::new();
        let mut failed = Vec::new();
        let region = super::resolve_region(cfg.region.as_deref(), &|k| {
            std::env::var(k).ok().filter(|v| !v.is_empty())
        });
        let aws = super::AwsAccess {
            region: region.as_deref(),
            profile: &cfg.aws_profile,
        };
        for (owner, r) in &cfg.key_refs {
            let dir = base.join(owner);
            let result = deps.read_key(r, &aws).and_then(|key| {
                publish_profile(&dir, &key, &cfg.api_host).map_err(|e| e.to_string())
            });
            match result {
                Ok(()) => owners.push(owner.clone()),
                Err(why) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    self.log_once(format!("{owner}: {why}"), |m| {
                        log::warn!(
                            "forge_gateway: no gateway profile for {m}; that owner's reads keep \
                             today's routing — 2am#4367"
                        );
                    });
                    failed.push(owner.clone());
                }
            }
        }
        remove_strays(base, &cfg.key_refs.keys().cloned().collect());
        let marker = write_private(&base.join(GH_MARKER), gh_path.to_string_lossy().as_bytes());
        if marker.is_err() {
            // Without the marker no agent front routes; the daemon still does.
            log::warn!("forge_gateway: could not write {GH_MARKER} in {}", base.display());
        }
        let set: BTreeSet<String> = owners.iter().cloned().collect();
        if set != self.published {
            log::info!(
                "forge_gateway: gateway read profiles live for [{}] via {} (gh {version}) — 2am#4367",
                owners.join(", "),
                cfg.api_host
            );
        }
        self.published = set.clone();
        set_active((!set.is_empty()).then(|| Active {
            base: base.to_path_buf(),
            owners: set,
        }));
        PassOutcome::Published { owners, failed }
    }

    fn off(&mut self, base: &Path) {
        set_active(None);
        self.published.clear();
        self.at = None;
        teardown(base);
    }
}

/// Remove every profile under `base` whose owner is not in `keep`.
fn remove_strays(base: &Path, keep: &BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if e.path().is_dir() && !keep.contains(&name) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Remove every gateway profile.
pub fn teardown(base: &Path) {
    if base.exists() {
        let _ = std::fs::remove_dir_all(base);
    }
}

fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `bytes` to `path` atomically, created 0600 (never readable by
/// others, not even for an instant).
pub(super) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    // A leftover from a crashed run may carry any mode: never reuse it.
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let written = opts
        .open(&tmp)
        .and_then(|mut f| {
            // The umask cannot widen 0600, but make the mode explicit anyway
            // before a byte of the key is written.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            f.write_all(bytes)
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The gateway profile's `config.yml`: `version: 1` skips gh's account
/// migration (which calls `/user`). No `git_protocol`: a gateway profile is
/// never a git credential.
pub const GATEWAY_CONFIG_YAML: &str = "version: 1\n";

/// The gateway profile's `hosts.yml`: the key and `api_host`, nothing else.
/// Unlike the App profiles (`credential_preflight::gh_hosts_yaml`) it names
/// no `user` and no `git_protocol`, so `gh auth git-credential` run under it
/// has nothing to hand git, and the key cannot become a git password.
#[must_use]
pub fn gateway_hosts_yaml(key: &str, api_host: &str) -> String {
    format!("github.com:\n    oauth_token: {key}\n    api_host: {api_host}\n")
}

/// The `api_host` a published profile in `dir` names (the page walk keeps
/// its next links on it).
#[must_use]
pub fn profile_api_host(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("hosts.yml")).ok()?;
    text.lines()
        .find_map(|l| l.trim().strip_prefix("api_host:"))
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

/// Publish one read profile: `config.yml` and a `hosts.yml` carrying `key`
/// with `api_host`, both 0600 in a 0700 `dir`. `api_host` is written every
/// time `hosts.yml` is.
///
/// # Errors
///
/// When a file cannot be written.
pub fn publish_profile(dir: &Path, key: &str, api_host: &str) -> std::io::Result<()> {
    if !super::valid_key(key) || super::api_host_of(api_host).as_deref() != Ok(api_host) {
        return Err(std::io::Error::other("refusing an invalid gateway key or api_host"));
    }
    ensure_private_dir(dir)?;
    write_private(&dir.join("config.yml"), GATEWAY_CONFIG_YAML.as_bytes())?;
    write_private(&dir.join("hosts.yml"), gateway_hosts_yaml(key, api_host).as_bytes())
}

/// One pass for `ws` (see [`Refresher`]).
pub fn refresh(state: &mut Refresher, ws: &Path) -> PassOutcome {
    state.pass(ws, Instant::now())
}

/// Run [`refresh`] every [`CHECK_INTERVAL`] for the daemon's workspace. From
/// the first call on, only what this loop published routes in this process.
pub fn spawn_refresh(workspace_root: PathBuf) {
    set_active(None);
    tokio::spawn(async move {
        let mut state = Refresher::default();
        loop {
            let ws = workspace_root.clone();
            state = tokio::task::spawn_blocking(move || {
                let _ = refresh(&mut state, &ws);
                state
            })
            .await
            .unwrap_or_default();
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}
