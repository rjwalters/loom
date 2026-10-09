//! One stable `host.id` per machine (Issue #10023).
//!
//! Every surface that names "this host" — telemetry Resources (`host.id`,
//! `service.instance.id`), the envelope `host_id`, peer-claim self-
//! recognition, collision records, fleet-store keys, lease records, and the
//! host's shell exporters — resolves the same value through [`resolve`].
//!
//! # Precedence
//!
//! 1. **`$LOOM_HOST_ID`** ([`HOST_ID_ENV`]) — the explicit, provisioned fleet
//!    name. Blank values are ignored.
//! 2. **The configured roster id** — `fleet.hostId` ([`CONFIG_KEY`]) in the
//!    host-level private defaults tier
//!    ([`crate::config_resolver::private_defaults_path`],
//!    `~/.local/share/loom/config/defaults.json`). Deliberately *not* read
//!    from a repo's committed `.loom/config.json`: that file is shared by
//!    every host that clones the repo, so a host id there would name every
//!    machine the same.
//! 3. **A persisted generated id** — `~/.loom/host-id` ([`HOST_ID_FILENAME`],
//!    overridable with [`HOST_ID_FILE_ENV`]), created atomically and
//!    owner-only on first use and re-read on every later start, so it is
//!    stable across processes, restarts and launch contexts.
//!
//! **Never the hostname.** Before #10023 the chain fell back to `$HOSTNAME`
//! and then the `hostname` binary. Both depend on launch context (`$HOSTNAME`
//! is exported by an interactive shell but not by launchd/systemd) and on the
//! OS (macOS `hostname` returns the ComputerName or a `*.localdomain` name),
//! which is how one machine came to report under two `host.id`s in SigNoz.
//!
//! Only when no id can be persisted at all (no home directory, a read-only
//! `~/.loom`, any other I/O error) does this fall back to [`UNKNOWN_HOST`],
//! which never participates in peer-claim self-recognition. The daemon keeps
//! running under it (WARN logged once); `loom-daemon host-id` instead exits
//! non-zero, so a shell caller can tell "no identity" from an identity and
//! never publishes a lease or claim under the shared sentinel. At startup the
//! daemon exports its resolved id to its children ([`export_for_children`]),
//! so a script it spawns cannot resolve a different one.
//!
//! # Migrating a hostname-keyed host
//!
//! A host that has always run without `$LOOM_HOST_ID` reported its hostname.
//! After upgrading it reports a generated `loom-host-…` id instead, once, and
//! keeps it. Fleet-store host keys, a native ingest key bound to the old name
//! (#4830) and in-flight peer claims/leases are keyed on the old value, so a
//! host that must keep its old name pins `LOOM_HOST_ID=<old name>` (or
//! `fleet.hostId`) before upgrading. Nothing re-keys silently: the generated
//! id is announced with a one-time WARN naming that remedy.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Explicit host identity override — the top of the precedence chain. Pin it
/// to the host's fleet/tailnet name at provisioning time.
pub const HOST_ID_ENV: &str = "LOOM_HOST_ID";

/// Overrides the persisted id's path (default `~/.loom/host-id`). Used by
/// tests and by an operator who keeps `~/.loom` somewhere unusual.
pub const HOST_ID_FILE_ENV: &str = "LOOM_HOST_ID_FILE";

/// Basename of the persisted id under `~/.loom`.
pub const HOST_ID_FILENAME: &str = "host-id";

/// Dotted key of the configured roster id in the host-level config tier.
pub const CONFIG_KEY: &str = "fleet.hostId";

/// Prefix of a generated id. Distinct from the `host-XXXXXXXX` shape of
/// [`crate::sweep_registry::opaque_host_id`] so the two are never confused.
pub const GENERATED_PREFIX: &str = "loom-host-";

/// Sentinel returned when no identity can be resolved or persisted.
///
/// **This value must never participate in self-claim recognition.** Two
/// hosts that both fail identity resolution would otherwise advertise the
/// same string and each would treat the other's peer claim as its own,
/// silently defeating the collision-detection machinery in #4028/#5017.
pub const UNKNOWN_HOST: &str = "unknown-host";

/// Where a resolved id came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostIdSource {
    /// `$LOOM_HOST_ID`.
    Env,
    /// `fleet.hostId` in the host-level config tier.
    Config,
    /// An existing persisted id file.
    Persisted,
    /// A persisted id file this call created.
    Generated,
    /// Nothing resolvable; [`UNKNOWN_HOST`].
    Unknown,
}

impl HostIdSource {
    /// Stable lowercase name, as printed by `loom-daemon host-id --source`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Config => "config",
            Self::Persisted => "persisted",
            Self::Generated => "generated",
            Self::Unknown => "unknown",
        }
    }
}

/// A resolved host identity and its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHostId {
    pub id: String,
    pub source: HostIdSource,
    /// The persisted id file consulted, when one was.
    pub path: Option<PathBuf>,
}

/// This host's identity string. See the module docs for the precedence.
#[must_use]
pub fn host_identity() -> String {
    resolve().id
}

/// Resolve this host's identity with its provenance.
#[must_use]
pub fn resolve() -> ResolvedHostId {
    if let Some(id) = env_host_id() {
        return ResolvedHostId {
            id,
            source: HostIdSource::Env,
            path: None,
        };
    }
    if let Some(id) = configured_host_id() {
        return ResolvedHostId {
            id,
            source: HostIdSource::Config,
            path: None,
        };
    }
    let Some(path) = host_id_file_path() else {
        return in_process_fallback();
    };
    resolve_persisted(&path)
}

fn env_host_id() -> Option<String> {
    std::env::var(HOST_ID_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `fleet.hostId` from the host-level private defaults tier only (see the
/// module docs for why repo tiers are excluded).
fn configured_host_id() -> Option<String> {
    let path = crate::config_resolver::private_defaults_path()?;
    if !path.is_file() {
        return None;
    }
    let tier = crate::config_resolver::soft_read_json_object(&path);
    crate::config_resolver::get_path(&tier, CONFIG_KEY)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The persisted id's path: [`HOST_ID_FILE_ENV`] when set, else
/// `~/.loom/host-id`.
///
/// Under `cfg(test)` the `$HOME` default is refused (the same structural
/// isolation `config_resolver::private_defaults_path` applies, #4657): unit
/// tests link into one binary and must never create or read the operator's
/// real `~/.loom/host-id`. They set [`HOST_ID_FILE_ENV`] instead.
#[must_use]
pub fn host_id_file_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(HOST_ID_FILE_ENV) {
        let p = p.trim();
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        dirs::home_dir().map(|h| h.join(".loom").join(HOST_ID_FILENAME))
    }
}

/// Unit-test-only stand-in when no id file path is configured: one id per
/// test process, never written anywhere. Production resolves a path whenever
/// a home directory exists; without one it is [`UNKNOWN_HOST`].
fn in_process_fallback() -> ResolvedHostId {
    #[cfg(test)]
    {
        static TEST_ID: OnceLock<String> = OnceLock::new();
        ResolvedHostId {
            id: TEST_ID.get_or_init(generate_id).clone(),
            source: HostIdSource::Persisted,
            path: None,
        }
    }
    #[cfg(not(test))]
    {
        warn_once(&format!(
            "host identity: no ${HOST_ID_ENV}, no {CONFIG_KEY}, and no home directory to persist \
             an id in — falling back to {UNKNOWN_HOST:?}, which never participates in peer-claim \
             self-recognition. Set ${HOST_ID_ENV} to give this host a real identity."
        ));
        ResolvedHostId {
            id: UNKNOWN_HOST.to_string(),
            source: HostIdSource::Unknown,
            path: None,
        }
    }
}

/// Process-lifetime cache of persisted ids by path, so a file deleted while
/// the daemon runs cannot change the identity mid-process.
fn cache() -> &'static Mutex<std::collections::HashMap<PathBuf, String>> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<PathBuf, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn resolve_persisted(path: &Path) -> ResolvedHostId {
    let cached = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .cloned();
    if let Some(id) = cached {
        return ResolvedHostId {
            id,
            source: HostIdSource::Persisted,
            path: Some(path.to_path_buf()),
        };
    }
    match load_or_create(path) {
        Ok((id, created)) => {
            cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(path.to_path_buf(), id.clone());
            if created {
                warn_once(&format!(
                    "host identity: generated a new persisted host id {id:?} at {} (no \
                     ${HOST_ID_ENV} or {CONFIG_KEY} set). Telemetry, leases, peer claims and \
                     fleet-store entries from this host are now keyed on it. If this host \
                     previously reported under its hostname and must keep that name (a native \
                     ingest key bound to it, existing fleet-store entries), pin \
                     {HOST_ID_ENV}=<old name> and restart (Issue #10023).",
                    path.display()
                ));
            }
            ResolvedHostId {
                id,
                source: if created {
                    HostIdSource::Generated
                } else {
                    HostIdSource::Persisted
                },
                path: Some(path.to_path_buf()),
            }
        }
        Err(error) => {
            warn_once(&format!(
                "host identity: could not read or create the persisted host id at {}: {error} — \
                 falling back to {UNKNOWN_HOST:?}. Set ${HOST_ID_ENV} to give this host a real \
                 identity.",
                path.display()
            ));
            ResolvedHostId {
                id: UNKNOWN_HOST.to_string(),
                source: HostIdSource::Unknown,
                path: Some(path.to_path_buf()),
            }
        }
    }
}

fn warn_once(message: &str) {
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| log::warn!("{message}"));
}

/// A fresh id: [`GENERATED_PREFIX`] plus 12 lowercase hex chars of a v4 UUID.
#[must_use]
pub fn generate_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("{GENERATED_PREFIX}{}", &hex[..12])
}

/// Whether `id` is a usable persisted id: non-empty, at most 128 bytes, and
/// only `[A-Za-z0-9._-]` (URL- and label-safe, like `forgeEvents.hostId`).
#[must_use]
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Export the resolved id as `$LOOM_HOST_ID` for every child this process
/// spawns afterwards, so a script the daemon launches reports the daemon's
/// own id even when its `$HOME` or `~/.loom` differs (sandbox, other user).
///
/// Call it **once, at daemon startup, before any task that spawns a child
/// exists** — the same window `GH_CONFIG_DIR`'s one-time `set_var` relies on
/// (#4458): a later `set_var` would race the `environ` reads inside a
/// concurrent `Command::spawn`. An env-sourced id is already exported, and
/// [`UNKNOWN_HOST`] is never exported: a child then resolves (and, via
/// `loom-daemon host-id`'s non-zero exit, refuses) on its own rather than
/// inheriting the shared sentinel as if it were an identity.
pub fn export_for_children() -> ResolvedHostId {
    let resolved = resolve();
    if !matches!(resolved.source, HostIdSource::Env | HostIdSource::Unknown) {
        std::env::set_var(HOST_ID_ENV, &resolved.id);
    }
    resolved
}

/// Read the id at `path`, creating it first when absent. Returns the id and
/// whether this call created it.
///
/// Creation is atomic and first-writer-wins: the id is written to an
/// owner-only (`0600`) temp file in the same directory, then hard-linked into
/// place, which fails if another process got there first — in which case its
/// id is read back. On a filesystem that refuses hard links (EPERM/ENOTSUP on
/// some mounts) the id is created in place with `O_EXCL` instead, which is
/// equally first-writer-wins. A present-but-invalid file (empty, or
/// containing something that is not an id) is replaced by `rename`; no valid
/// id exists for it to re-key.
///
/// Invalid-file replacement is serialized by an exclusive `flock` on a
/// sidecar lock file ([`RecoveryLock`]), and validity is re-checked once the
/// lock is held, so only the first replacer renames: later ones see its valid
/// id and leave it alone. Every caller therefore reports the same surviving
/// id, which the process-lifetime cache and `export_for_children` then
/// agree with. The returned id is always **re-read from `path`** after
/// placement, never assumed to be this call's candidate.
pub fn load_or_create(path: &Path) -> std::io::Result<(String, bool)> {
    load_or_create_with(path, |from, to| std::fs::hard_link(from, to))
}

fn load_or_create_with(
    path: &Path,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<(String, bool)> {
    if let Some(id) = read_valid(path)? {
        return Ok((id, false));
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let candidate = generate_id();
    let tmp = dir.join(format!(".{HOST_ID_FILENAME}.tmp.{}", uuid::Uuid::new_v4().simple()));
    write_owner_only(&tmp, &candidate)?;
    let placed = match link(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read_valid(path)? {
            Some(_) => Ok(()),
            // Present but invalid: replace it under the recovery lock, and
            // re-check validity once held — a replacer that got there first
            // has already put a valid id in place, which must win.
            None => {
                let _lock = RecoveryLock::acquire(dir)?;
                match read_valid(path)? {
                    Some(_) => Ok(()),
                    None => std::fs::rename(&tmp, path),
                }
            }
        },
        // Hard links unsupported here: exclusive create in place.
        Err(_) => match write_owner_only(path, &candidate) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
            _ => Ok(()),
        },
    };
    let _ = std::fs::remove_file(&tmp);
    placed?;
    let id = read_settled(path)?
        .ok_or_else(|| std::io::Error::other("persisted host id vanished after creation"))?;
    let created = id == candidate;
    Ok((id, created))
}

/// Exclusive advisory lock (`flock` on `.<HOST_ID_FILENAME>.lock` beside the
/// id file) held while replacing an invalid id file. Separate opens contend
/// even within one process, so it serializes threads and processes alike; the
/// kernel releases it when the file is dropped, so a crashed holder cannot
/// leave it stuck. Taken only on the rare invalid-file path.
struct RecoveryLock(#[allow(dead_code)] std::fs::File);

impl RecoveryLock {
    fn acquire(dir: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(format!(".{HOST_ID_FILENAME}.lock")))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            loop {
                // SAFETY: `file` owns a valid open descriptor for this call.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        Ok(Self(file))
    }
}

/// [`read_valid`], retried briefly while the file reads empty: an `O_EXCL`
/// creator (the no-hard-link path) has the file in place a moment before its
/// id is written into it.
fn read_settled(path: &Path) -> std::io::Result<Option<String>> {
    for _ in 0..20 {
        if let Some(id) = read_valid(path)? {
            return Ok(Some(id));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    read_valid(path)
}

fn read_valid(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let id = contents.trim();
            if is_valid_id(id) {
                Ok(Some(id.to_string()))
            } else {
                log::warn!(
                    "host identity: ignoring invalid persisted host id at {}",
                    path.display()
                );
                Ok(None)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn write_owner_only(path: &Path, id: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(format!("{id}\n").as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
#[path = "host_identity_tests.rs"]
mod tests;
