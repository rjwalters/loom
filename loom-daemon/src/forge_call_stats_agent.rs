//! The agent `gh` front's stamp on its sink rows (#10607).
//!
//! The front ([`crate::agent_gh`]) is `loom-daemon` running as an agent's
//! `gh`. Every row it writes — a served ETag read through the facade
//! (caller `agent_gh_front`) or the one ledger row of a passthrough (caller
//! `agent.gh.<command>`, [`crate::agent_gh::ledger`], W5) — carries two
//! short keys:
//!
//! - `ag`: the agent role (`LOOM_ROLE`, lowercased, one of [`ROLES`]; any
//!   other value is `other`, and an unset one — an interactive session — is
//!   `none`);
//! - `vi`: `served` or `passthrough`.
//!
//! The context is set once per front process by [`set_agent_role`]; nothing
//! else sets it, so every other row stays byte-identical. `ag` marks the
//! rows the **front** wrote. It does not mark everything an agent caused: a
//! `loom-daemon` command run inside a session books its own calls through
//! the facade, under that command's own callers and unstamped, and the front
//! does not book them again (`LOOM_GH_BOOKED`, W5).
//!
//! Where rows land: every spawn path exports the host sink as the generic
//! `LOOM_FORGE_CALL_STATS_DIR` ([`crate::agent_session::isolation`], one
//! mechanism for the tmux, `agent-spawn` and `spawn-worker` paths), and a
//! container parity-mounts it read-write ([`docker_mount_args`],
//! `worker_spawn::containment`). Inside a container the front writes only
//! when its uid owns the directory (MOUNT-CONTRACT §3, uid 1000); otherwise
//! its rows are dropped silently, by design.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// The caller prefix of a passthrough's ledger row (`agent.gh.<command>`).
pub const PASSTHROUGH_CALLER_PREFIX: &str = "agent.gh.";

/// The sink-directory override every spawn path exports (W5).
pub const SINK_DIR_ENV: &str = "LOOM_FORGE_CALL_STATS_DIR";

/// The closed `ag` vocabulary besides `other` / `none`.
pub const ROLES: &[&str] = &[
    "builder",
    "judge",
    "champion",
    "curator",
    "architect",
    "hermit",
    "doctor",
    "guide",
    "driver",
    "auditor",
    "concierge",
    "loom",
];

static ROLE: OnceLock<&'static str> = OnceLock::new();

/// This host's sink directory, created owner-only, for a spawned agent's
/// front to write into: `None` when the sink is off, the path is relative,
/// or the directory cannot be made private (a mount must never make docker
/// create a root-owned one).
#[must_use]
pub fn worker_sink_dir() -> Option<PathBuf> {
    let dir = super::host_sink_dir()?;
    (dir.is_absolute() && crate::forge_etag_store::private_dir(&dir, true)).then_some(dir)
}

/// `-v <dir>:<dir>` (read-write) for a `docker run` whose environment
/// (`env`, injectable) names the daemon's own sink in [`SINK_DIR_ENV`] —
/// what `forge egress container-args` adds for `spawn-claude.sh`, whose
/// `LOOM_*` by-name forwarding already carries the variable itself.
///
/// The value is an env var, so it is not trusted to name a read-write
/// mount. Comparing it with [`host_sink_dir`](super::host_sink_dir) proves
/// nothing: this process resolves its own sink from the same variable. So
/// the directory itself must look like a sink: an absolute, real directory
/// that is private (owned by us, mode `0700`) and holds nothing but sink
/// files ([`is_sink_entry`]). `/`, `/var/run`, `/tmp` and a home directory
/// all fail that, so they get no mount, and nothing about them is changed.
#[must_use]
pub fn docker_mount_args(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Vec<String> {
    let Some(dir) = env(SINK_DIR_ENV)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
    else {
        return Vec::new();
    };
    if !dir.is_absolute() {
        return Vec::new();
    }
    if !is_owner_only_dir(&dir) || !holds_only_sink_files(&dir) {
        return Vec::new();
    }
    vec!["-v".to_string(), format!("{0}:{0}", dir.display())]
}

/// `dir` is a real directory (a symlink is refused, not followed), owned by
/// us, with no group/other bits. A pure check: unlike
/// `forge_etag_store::private_dir`, it never tightens the mode of what it
/// inspects, so a refused directory is left exactly as it was.
fn is_owner_only_dir(dir: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid only reads the caller's effective user id.
        let owned = meta.uid() == unsafe { libc::geteuid() };
        meta.is_dir() && owned && meta.permissions().mode() & 0o077 == 0
    }
    #[cfg(not(unix))]
    {
        meta.is_dir()
    }
}

/// Whether every entry of `dir` is a regular sink file.
fn holds_only_sink_files(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.into_iter().all(|e| {
        e.is_ok_and(|e| {
            e.file_type().is_ok_and(|t| t.is_file())
                && e.file_name().to_str().is_some_and(is_sink_entry)
        })
    })
}

/// A name the sink itself writes: `calls-<hour>.jsonl`, the bucket book's
/// snapshot, or that snapshot's in-flight temp file
/// (`crate::forge_bucket_book::persist`).
#[must_use]
pub fn is_sink_entry(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let snapshot = crate::forge_bucket_book::SNAPSHOT_FILE;
    name == snapshot
        || name
            .strip_prefix("calls-")
            .and_then(|n| n.strip_suffix(".jsonl"))
            .is_some_and(|h| digits(h.strip_prefix('-').unwrap_or(h)))
        || name
            .strip_prefix('.')
            .and_then(|n| n.strip_prefix(snapshot))
            .and_then(|n| n.strip_prefix('.'))
            .is_some_and(digits)
}

/// The `ag`/`vi` keys, flattened into the sink line; both absent off-front.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStamp {
    /// Agent role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ag: Option<String>,
    /// `served` | `passthrough`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vi: Option<String>,
}

/// `LOOM_ROLE` reduced to the closed vocabulary.
#[must_use]
pub fn role_label(raw: Option<&str>) -> &'static str {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return "none";
    };
    let lower = raw.to_ascii_lowercase();
    ROLES
        .iter()
        .find(|r| **r == lower)
        .copied()
        .unwrap_or("other")
}

/// Mark this process as the agent front for `raw` (`LOOM_ROLE`). First call
/// wins; later calls are no-ops.
pub fn set_agent_role(raw: Option<&str>) {
    let _ = ROLE.set(role_label(raw));
}

/// The stamp for a row by `caller` under `role` (`None` = not the front).
#[must_use]
pub fn stamp_for(role: Option<&str>, caller: &str) -> AgentStamp {
    let Some(role) = role else {
        return AgentStamp::default();
    };
    let via = if caller.starts_with(PASSTHROUGH_CALLER_PREFIX) {
        "passthrough"
    } else {
        "served"
    };
    AgentStamp {
        ag: Some(role.to_string()),
        vi: Some(via.to_string()),
    }
}

/// [`stamp_for`] with this process's context.
pub(super) fn stamp(caller: &str) -> AgentStamp {
    stamp_for(ROLE.get().copied(), caller)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_vocabulary_is_closed() {
        assert_eq!(role_label(None), "none");
        assert_eq!(role_label(Some("  ")), "none");
        assert_eq!(role_label(Some("Builder")), "builder");
        assert_eq!(role_label(Some("loom")), "loom");
        assert_eq!(role_label(Some("builder; rm -rf /")), "other");
        assert_eq!(role_label(Some("shepherd")), "other");
    }

    #[test]
    fn only_the_front_stamps_and_via_follows_the_caller() {
        assert_eq!(stamp_for(None, "agent.gh.pr"), AgentStamp::default());
        let served = stamp_for(Some("judge"), "agent_gh_front");
        assert_eq!(served.ag.as_deref(), Some("judge"));
        assert_eq!(served.vi.as_deref(), Some("served"));
        let passed = stamp_for(Some("builder"), "agent.gh.pr");
        assert_eq!(passed.vi.as_deref(), Some("passthrough"));
        // Absent keys stay off the line: a daemon row is byte-identical.
        let json = serde_json::to_string(&AgentStamp::default()).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn docker_mount_is_the_private_host_sink_only() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display().to_string();
        let env = |v: Option<&str>| {
            let v = v.map(std::ffi::OsString::from);
            move |k: &str| (k == SINK_DIR_ENV).then(|| v.clone()).flatten()
        };
        super::super::set_test_sink_dir(Some(dir.path().to_path_buf()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(docker_mount_args(env(Some(&d))), ["-v".to_string(), format!("{d}:{d}")]);
        for none in [
            None,
            Some(""),
            Some("off"),
            Some("rel/dir"),
            Some("/no/such/dir"),
            // An env var must not name an arbitrary rw mount.
            Some("/"),
            Some("/var/run"),
            Some("/tmp"),
        ] {
            assert!(docker_mount_args(env(none)).is_empty(), "{none:?}");
        }
        super::super::set_test_sink_dir(None);
    }

    /// Review S1, reopened by the generic variable: `container-args`
    /// resolves its own sink from the same variable it is asked to mount,
    /// so a private directory named there (a home directory, say) used to
    /// pass as "the host sink". Only a directory holding nothing but sink
    /// files is mounted.
    #[cfg(unix)]
    #[test]
    fn docker_mount_refuses_a_private_directory_that_is_not_a_sink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display().to_string();
        let env = |k: &str| (k == SINK_DIR_ENV).then(|| std::ffi::OsString::from(&d));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        // What the container-args process sees: its sink IS the named dir.
        super::super::set_test_sink_dir(Some(dir.path().to_path_buf()));
        for name in [
            "calls-493000.jsonl",
            "bucket-book.json",
            ".bucket-book.json.4242",
        ] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        assert_eq!(docker_mount_args(env), ["-v".to_string(), format!("{d}:{d}")]);
        // A home directory: ours, and full of other things — refused, and
        // left at its own mode (never tightened by the check).
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join(".profile"), "").unwrap();
        assert!(docker_mount_args(env).is_empty(), "a 0755 home");
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the refused directory's mode is untouched");
        std::fs::remove_file(dir.path().join(".profile")).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(dir.path().join(".ssh")).unwrap();
        assert!(docker_mount_args(env).is_empty(), "a sub-directory");
        std::fs::remove_dir(dir.path().join(".ssh")).unwrap();
        std::fs::write(dir.path().join(".bashrc"), "").unwrap();
        assert!(docker_mount_args(env).is_empty(), "a foreign file");
        std::fs::remove_file(dir.path().join(".bashrc")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("calls-1.jsonl")).unwrap();
        assert!(docker_mount_args(env).is_empty(), "a symlink named like a sink file");
        std::fs::remove_file(dir.path().join("calls-1.jsonl")).unwrap();
        // The directory itself reached through a symlink.
        let link = tempfile::tempdir().unwrap();
        let alias = link.path().join("sink");
        std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
        let a = alias.display().to_string();
        let via_link = |k: &str| (k == SINK_DIR_ENV).then(|| std::ffi::OsString::from(&a));
        assert!(docker_mount_args(via_link).is_empty(), "a symlinked sink");
        super::super::set_test_sink_dir(None);
    }

    #[test]
    fn sink_entries_are_only_what_the_sink_writes() {
        for ok in [
            "calls-0.jsonl",
            "calls-493000.jsonl",
            "calls--1.jsonl",
            "bucket-book.json",
        ] {
            assert!(is_sink_entry(ok), "{ok}");
        }
        assert!(is_sink_entry(".bucket-book.json.12"));
        for bad in [
            "calls-.jsonl",
            "calls-1.json",
            "calls-1x.jsonl",
            ".bucket-book.json.",
            ".bucket-book.json.x",
            "bucket-book.json.bak",
            ".ssh",
            "docker.sock",
        ] {
            assert!(!is_sink_entry(bad), "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn docker_mount_refuses_a_non_private_sink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display().to_string();
        let env = |k: &str| (k == SINK_DIR_ENV).then(|| std::ffi::OsString::from(&d));
        super::super::set_test_sink_dir(Some(dir.path().to_path_buf()));
        // Writable by others: not a directory only we control.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!crate::forge_etag_store::private_dir(dir.path(), false));
        assert!(docker_mount_args(env).is_empty());
        super::super::set_test_sink_dir(None);
        // A sink exported as off names no directory at all.
        let off = |k: &str| (k == SINK_DIR_ENV).then(|| std::ffi::OsString::from("off"));
        assert!(docker_mount_args(off).is_empty());
    }
}
