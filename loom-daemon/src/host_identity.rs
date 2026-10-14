//! This host's identity: one `host.id` per machine (Issue #11530, part of
//! #10023).
//!
//! Every surface that names "this host" — telemetry Resources (`host.id`),
//! the envelope `host_id`, peer-claim self-recognition, collision records,
//! fleet-store keys, the fleet dashboard — resolves the same value through
//! [`host_identity`] (re-exported as `sweep_registry::host_identity`).
//!
//! # Precedence
//!
//! 1. **`fleet.hostId`** ([`CONFIG_KEY`]) in the machine-tier config file
//!    ([`crate::config_resolver::private_defaults_path`], default
//!    `~/.local/share/loom/config/defaults.json`, env
//!    `LOOM_CONFIG_DEFAULTS_FILE`). That file is what `fleet-config render`
//!    writes from fleet-gitops `fleet/hosts/<h>/defaults.json`, so the roster
//!    names the host. It is deliberately *not* read from a repo's committed
//!    `.loom/config.json`: that file is shared by every clone and would name
//!    every machine the same.
//! 2. **`$LOOM_HOST_ID`** ([`HOST_ID_ENV`]). Blank values are ignored.
//! 3. **A persisted generated id** in `~/.loom/host-id` ([`HOST_ID_FILENAME`],
//!    overridable with [`HOST_ID_FILE_ENV`]). It is created once — atomically,
//!    owner-only, first writer wins — and re-read on every later start, so it
//!    is stable across processes, restarts and launch contexts. Its value is
//!    opaque (`loom-host-<16 hex>`), not derived from the hostname.
//!
//! **Never the hostname.** Before #11530 the chain fell back to `$HOSTNAME`
//! and then the `hostname` binary. Both depend on launch context
//! (`$HOSTNAME` is exported by an interactive shell but not by
//! launchd/systemd) and on the OS, which is how one machine came to report
//! under two `host.id`s. Only when no id can be read or persisted at all (no
//! home directory, a read-only `~/.loom`, any other I/O error) does this fall
//! back to [`UNKNOWN_HOST`] (WARN logged once), which never participates in
//! peer-claim self-recognition. `loom-daemon host-id` exits non-zero on it.
//!
//! # Lease identity is separate
//!
//! The ids published in `loom:lease` / `loom:lease-yield` / roster comments
//! are computed by both this daemon and `defaults/scripts/sweep-lease-*.sh`,
//! which still use `LOOM_HOST_ID` > `$HOSTNAME` > `hostname`. So lease
//! publishing keeps that legacy chain through [`lease_host_identity`]; it is
//! not [`host_identity`]. Changing it would make the daemon and the scripts
//! disagree on lease ownership on any host without `LOOM_HOST_ID`.
//!
//! # Migrating a hostname-keyed host
//!
//! A host that ran without `$LOOM_HOST_ID` used to report its hostname. After
//! upgrading it reports a generated `loom-host-…` id instead, once, and keeps
//! it. SigNoz rows, fleet-store host keys, an ingest key bound to the old name
//! and `fleet.captain` all key on `host.id`, so a host that must keep its old
//! name sets `fleet.hostId` (or `LOOM_HOST_ID`) to it before upgrading. The
//! generated id is announced with a one-time WARN naming that remedy.
//!
//! A host that upgraded without doing so is recovered once, by the fleet-sync
//! pass, not here (Issue #11740): when the resolved id is a generated one the
//! fleet store does not know, and exactly one store host is named by this
//! machine's short hostname, [`adopt_roster_name`] rewrites the persisted id
//! to that roster name (WARN logged). The hostname is only ever *matched
//! against the roster*, never adopted on its own, so a launchd-style
//! `MacBook-Air.localdomain` that names no store host changes nothing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Explicit host identity override — the second tier, below `fleet.hostId`.
/// It is also the top of the lease chain ([`lease_host_identity`]).
pub const HOST_ID_ENV: &str = "LOOM_HOST_ID";

/// Overrides the persisted id's path (default `~/.loom/host-id`). Used by
/// tests and by an operator who keeps `~/.loom` somewhere unusual.
pub const HOST_ID_FILE_ENV: &str = "LOOM_HOST_ID_FILE";

/// Basename of the persisted id under `~/.loom`.
pub const HOST_ID_FILENAME: &str = "host-id";

/// Dotted key of the roster host id in the machine-tier config file.
pub const CONFIG_KEY: &str = "fleet.hostId";

/// Prefix of a generated id. Distinct from the `host-XXXXXXXX` shape of
/// [`crate::sweep_registry::opaque_host_id`] so the two are never confused.
pub const GENERATED_PREFIX: &str = "loom-host-";

/// Longest persisted id accepted, in bytes.
const MAX_ID_LEN: usize = 128;

/// Sentinel returned when no identity can be resolved or persisted. Exported
/// so callers that reason about self-claim recognition (e.g.
/// [`crate::peer_claims::PeerClaimView`]) can special-case it without
/// duplicating the literal string (Issue #5063).
///
/// **This value must never participate in self-claim recognition.** Two
/// hosts that both fail identity resolution would otherwise advertise the
/// same string and each would treat the other's peer claim as its own,
/// silently defeating the collision-detection machinery in #4028/#5017.
pub const UNKNOWN_HOST: &str = "unknown-host";

/// Where a resolved id came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostIdSource {
    /// `fleet.hostId` in the machine-tier config file.
    Config,
    /// `$LOOM_HOST_ID`.
    Env,
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
            Self::Config => "config",
            Self::Env => "env",
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
    if let Some(id) = configured_host_id() {
        return ResolvedHostId {
            id,
            source: HostIdSource::Config,
            path: None,
        };
    }
    if let Some(id) = env_host_id() {
        return ResolvedHostId {
            id,
            source: HostIdSource::Env,
            path: None,
        };
    }
    match host_id_file_path() {
        Some(path) => resolve_persisted(&path),
        None => no_id_file_path(),
    }
}

fn non_blank(v: &str) -> Option<String> {
    let t = v.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn env_host_id() -> Option<String> {
    std::env::var(HOST_ID_ENV)
        .ok()
        .as_deref()
        .and_then(non_blank)
}

/// `fleet.hostId` from the machine-tier config file only (see the module
/// docs for why repo tiers are excluded). A missing file, missing key, blank
/// or non-string value is `None`.
#[must_use]
pub fn configured_host_id() -> Option<String> {
    let path = crate::config_resolver::private_defaults_path()?;
    if !path.is_file() {
        return None;
    }
    let tier = crate::config_resolver::soft_read_json_object(&path);
    crate::config_resolver::get_path(&tier, CONFIG_KEY)
        .and_then(serde_json::Value::as_str)
        .and_then(non_blank)
}

/// The persisted id's path: [`HOST_ID_FILE_ENV`] when set, else
/// `~/.loom/host-id`.
///
/// Under `cfg(test)` the home default is refused (the same structural
/// isolation `config_resolver::private_defaults_path` applies, #4657): unit
/// tests link into one binary and must never create or read the operator's
/// real `~/.loom/host-id`. They set [`HOST_ID_FILE_ENV`] instead.
#[must_use]
pub fn host_id_file_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var(HOST_ID_FILE_ENV)
        .ok()
        .as_deref()
        .and_then(non_blank)
    {
        return Some(PathBuf::from(p));
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

/// No id file path. In production that means no home directory:
/// [`UNKNOWN_HOST`]. Under `cfg(test)` (where the home default is refused)
/// it is one generated id per test process, never written anywhere, so tests
/// that do not care about identity still see a real, stable one.
fn no_id_file_path() -> ResolvedHostId {
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
            "host identity: no {CONFIG_KEY}, no ${HOST_ID_ENV}, and no home directory to persist \
             an id in — falling back to {UNKNOWN_HOST:?}, which never participates in peer-claim \
             self-recognition. Set {CONFIG_KEY} or ${HOST_ID_ENV} to give this host an identity."
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

/// Drop the in-process persisted-id cache (a stand-in for a restart).
#[cfg(test)]
pub(crate) fn clear_cache() {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
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
                     {CONFIG_KEY} or ${HOST_ID_ENV} set). Telemetry host.id, peer claims and \
                     fleet-store entries from this host are now keyed on it. If this host \
                     previously reported under its hostname and must keep that name (an ingest \
                     key bound to it, existing fleet-store entries, dashboards), set \
                     {CONFIG_KEY} in fleet/hosts/<host>/defaults.json or {HOST_ID_ENV}=<old \
                     name> and restart (Issue #11530).",
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
                 falling back to {UNKNOWN_HOST:?} (never the hostname). Set {CONFIG_KEY} or \
                 ${HOST_ID_ENV} to give this host an identity.",
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

/// A fresh id: [`GENERATED_PREFIX`] plus 16 lowercase hex chars of random
/// bytes (a v4 UUID). Never derived from the hostname.
#[must_use]
pub fn generate_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("{GENERATED_PREFIX}{}", &hex[..16])
}

/// Whether `id` is acceptable as a persisted id: non-empty, at most 128
/// bytes, and free of whitespace and control characters.
#[must_use]
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Read the id at `path`, creating it first when absent or empty. Returns the
/// id and whether this call created it.
///
/// Creation is atomic and first-writer-wins: the id is written to an
/// owner-only (`0600`) temp file in the same directory, then hard-linked into
/// place, which fails if another process got there first — in which case its
/// id is read back. On a filesystem that refuses hard links the id is created
/// in place with `O_EXCL` instead, which is equally first-writer-wins. An
/// empty file (a crashed `O_EXCL` writer) is replaced by `rename` under an
/// exclusive lock, re-checked once held, so concurrent replacers converge.
/// A non-empty file that is not a usable id is an error, never overwritten:
/// it may be an operator's mistake worth seeing, not a value to discard.
/// The returned id is always **re-read from `path`** after placement.
pub fn load_or_create(path: &Path) -> std::io::Result<(String, bool)> {
    load_or_create_with(path, |from, to| std::fs::hard_link(from, to))
}

pub(crate) fn load_or_create_with(
    path: &Path,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<(String, bool)> {
    if let Some(id) = read_id(path)? {
        return Ok((id, false));
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let candidate = generate_id();
    let tmp = dir.join(format!(".{HOST_ID_FILENAME}.tmp.{}", uuid::Uuid::new_v4().simple()));
    write_owner_only(&tmp, &candidate)?;
    let placed = match link(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read_id(path) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => RecoveryLock::acquire(dir).and_then(|_lock| match read_id(path)? {
                Some(_) => Ok(()),
                None => std::fs::rename(&tmp, path),
            }),
            Err(e) => Err(e),
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

/// Exclusive advisory lock (`flock` on `.host-id.lock` beside the id file),
/// held only while replacing an empty id file. Separate opens contend even
/// within one process; the kernel releases it on drop, so a crashed holder
/// cannot leave it stuck.
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

/// [`read_id`], retried briefly while the file reads empty: an `O_EXCL`
/// creator has the file in place a moment before its id is written into it.
fn read_settled(path: &Path) -> std::io::Result<Option<String>> {
    for _ in 0..20 {
        if let Some(id) = read_id(path)? {
            return Ok(Some(id));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    read_id(path)
}

/// `Ok(None)` when the file is absent or blank, the trimmed id when it is
/// usable, and `InvalidData` when it holds something that is not an id.
fn read_id(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let id = contents.trim();
            if id.is_empty() {
                Ok(None)
            } else if is_valid_id(id) {
                Ok(Some(id.to_string()))
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "the file does not hold a usable host id (one token, at most 128 bytes)",
                ))
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

/// This machine's short hostnames, the candidates [`adopt_roster_name`]'s
/// caller matches against the fleet-store roster (Issue #11740): `$HOSTNAME`,
/// the `hostname` binary and, on macOS, `scutil --get LocalHostName` (the
/// name launchd-started processes see differs from a login shell's). Each is
/// cut at its first `.`; blanks and duplicates are dropped. Never an identity
/// on its own.
#[must_use]
pub fn hostname_candidates() -> Vec<String> {
    let run = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let mut raw = vec![std::env::var("HOSTNAME").ok(), run("hostname", &[])];
    if cfg!(target_os = "macos") {
        raw.push(run("scutil", &["--get", "LocalHostName"]));
    }
    let mut out: Vec<String> = Vec::new();
    for name in raw.into_iter().flatten() {
        let short = name
            .trim()
            .split('.')
            .next()
            .unwrap_or_default()
            .to_string();
        if !short.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&short)) {
            out.push(short);
        }
    }
    out
}

/// Replace a **generated** persisted id with the roster name `name` (Issue
/// #11740), so a host the fleet store knows by its hostname keeps receiving
/// its config after #11530 stopped resolving hostnames.
///
/// Refuses (`Ok(false)`, nothing written) unless the id currently resolves
/// from the persisted file *and* that file holds a generated
/// (`loom-host-…`) id: an explicit `fleet.hostId`, `LOOM_HOST_ID`, or an
/// operator-written id file always wins over a hostname match. The new id is
/// written atomically (owner-only temp file, then `rename`) and the in-process
/// cache is updated, so later [`host_identity`] calls in this process return
/// it.
pub fn adopt_roster_name(name: &str) -> std::io::Result<bool> {
    let current = resolve();
    match (current.source, current.path) {
        (HostIdSource::Persisted | HostIdSource::Generated, Some(path)) => {
            adopt_at(&path, &current.id, name)
        }
        _ => Ok(false),
    }
}

pub(crate) fn adopt_at(path: &Path, expected: &str, name: &str) -> std::io::Result<bool> {
    if !expected.starts_with(GENERATED_PREFIX) || !is_valid_id(name) {
        return Ok(false);
    }
    // Re-read under the recovery lock: only the generated id we resolved may
    // be replaced, never a value another process or the operator just wrote.
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let _lock = RecoveryLock::acquire(dir)?;
    if read_id(path)?.as_deref() != Some(expected) {
        return Ok(false);
    }
    let tmp = dir.join(format!(".{HOST_ID_FILENAME}.tmp.{}", uuid::Uuid::new_v4().simple()));
    let placed = write_owner_only(&tmp, name).and_then(|()| std::fs::rename(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    placed?;
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_path_buf(), name.to_string());
    Ok(true)
}

/// The **lease** host identity: `LOOM_HOST_ID` > `$HOSTNAME` > the
/// `hostname` binary > [`UNKNOWN_HOST`] — the pre-#11530 chain, unchanged.
///
/// Used only where an id is published into, or compared against, a
/// lease-shaped forge comment (`loom:lease`, `loom:lease-yield`, PR leases,
/// role-runner roster markers), because `defaults/scripts/sweep-lease-*.sh`
/// compute the same id with this same chain. See the module docs. Do not use
/// it for telemetry or any other "which host is this" question; use
/// [`host_identity`].
#[must_use]
pub fn lease_host_identity() -> String {
    static FALLBACK_WARNED: OnceLock<()> = OnceLock::new();
    let warn_once = |message: String| {
        FALLBACK_WARNED.get_or_init(|| log::warn!("{message}"));
    };
    if let Some(id) = env_host_id() {
        return id;
    }
    if let Some(t) = std::env::var("HOSTNAME")
        .ok()
        .as_deref()
        .and_then(non_blank)
    {
        warn_once(format!(
            "lease host identity resolved from $HOSTNAME ({t:?}), not an explicit $LOOM_HOST_ID \
             — it can differ by launch context (Issue #5063). Set $LOOM_HOST_ID to pin it."
        ));
        return t;
    }
    if let Ok(out) = Command::new("hostname").output() {
        if out.status.success() {
            if let Some(s) = non_blank(&String::from_utf8_lossy(&out.stdout)) {
                warn_once(format!(
                    "lease host identity resolved from the `hostname` binary ({s:?}), not an \
                     explicit $LOOM_HOST_ID (Issue #5063). Set $LOOM_HOST_ID to pin it."
                ));
                return s;
            }
        }
    }
    warn_once(format!(
        "lease host identity could not be resolved from $LOOM_HOST_ID, $HOSTNAME, or the \
         `hostname` binary — falling back to {UNKNOWN_HOST:?}. Set $LOOM_HOST_ID."
    ));
    UNKNOWN_HOST.to_string()
}

#[cfg(test)]
#[path = "host_identity_tests.rs"]
pub(crate) mod tests;
