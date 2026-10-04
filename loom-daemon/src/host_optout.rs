//! The durable **host opt-out** marker (Issue #10179).
//!
//! # Why
//!
//! `loom-daemon-stop.sh` (and the #9588 operator-stop record) say "stopped for
//! now". Several independent paths — the watchdog, a supervised relaunch, the
//! auto-update roll, `loom update` / `resync-installed.sh`, an agent running
//! `loom-daemon-start.sh` — each re-provision or restart a daemon, so a stopped
//! daemon kept coming back. This marker is the strongest state: "no autonomous
//! daemon belongs on this host until a human says so".
//!
//! # The marker
//!
//! `<loom_dir>/autonomy-disabled` — a sibling of the resolved
//! `autonomy-desired` marker, so a `LOOM_AUTONOMY_MARKER` override moves both
//! together (an override whose file name is not `autonomy-desired` gets
//! `<override>.disabled`, so unrelated test fixtures sharing a directory never
//! collide). Three lines: `reason=`, `who=`, `when=` (RFC3339). The default
//! `loom_dir` is the machine-level `~/.loom`, not a per-repo `.loom/`, so one
//! marker covers every repo on the host.
//!
//! It outranks `autonomy-desired` and the `.stopped` operator-stop record.
//! An unreadable or unparseable marker still counts as disabled (fail closed).
//!
//! Every start / re-provision path calls [`check_or_refuse`] (or
//! [`refuse_if_disabled_exit`]) before any side effect.
//! `loom-daemon host enable` is the only way to clear it, and it does not
//! start anything.

use std::path::{Path, PathBuf};

use crate::autonomy_marker::{resolve_loom_dir, resolve_marker_path};
use crate::daemon_install_state::MARKER_FILENAME;

/// File name of the opt-out marker next to the default `autonomy-desired`.
pub const DISABLED_FILENAME: &str = "autonomy-disabled";

/// The operator's record of why this host opted out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Free-text reason (required at disable time).
    pub reason: String,
    /// Who disabled it.
    pub who: String,
    /// When (RFC3339, UTC).
    pub when: String,
}

/// What is on disk at the marker path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// No marker: the host has not opted out.
    Enabled,
    /// A well-formed marker.
    Disabled(Record),
    /// A marker exists but cannot be read or parsed. Counts as disabled.
    Malformed,
}

/// The refusal an entry point surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    entry_point: String,
    state: State,
    path: PathBuf,
}

impl Refusal {
    /// The one-line description used by status / health too.
    #[must_use]
    pub fn summary(&self) -> String {
        summary(&self.state)
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.state {
            State::Disabled(r) => write!(
                f,
                "{entry}: refusing - this host is disabled by operator.\n  reason: {reason}\n  who:    {who}\n  when:   {when}\n  marker: {path}\nNo daemon may be started, restarted or re-provisioned here. A human re-enables it with: loom-daemon host enable",
                entry = self.entry_point,
                reason = r.reason,
                who = r.who,
                when = r.when,
                path = self.path.display(),
            ),
            _ => write!(
                f,
                "{entry}: refusing - this host is disabled by operator (the opt-out marker {path} is unreadable or malformed; failing closed).\nA human re-enables it with: loom-daemon host enable",
                entry = self.entry_point,
                path = self.path.display(),
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// The opt-out marker path for a resolved `autonomy-desired` marker path.
#[must_use]
pub fn disabled_path(marker: &Path) -> PathBuf {
    if marker.file_name().is_some_and(|n| n == MARKER_FILENAME) {
        marker.with_file_name(DISABLED_FILENAME)
    } else {
        let mut s = marker.as_os_str().to_os_string();
        s.push(".disabled");
        PathBuf::from(s)
    }
}

/// `(autonomy-desired, autonomy-disabled)` paths for this process's
/// environment, or `None` when no loom dir resolves at all.
#[must_use]
pub fn current_paths() -> Option<(PathBuf, PathBuf)> {
    let desired = resolve_marker_path(&resolve_loom_dir()?);
    let disabled = disabled_path(&desired);
    Some((desired, disabled))
}

/// The opt-out marker path for this process's environment.
#[must_use]
pub fn current_path() -> Option<PathBuf> {
    current_paths().map(|(_, d)| d)
}

/// Parse marker text. Missing fields read `unknown`; no `reason=` at all (or
/// an empty file) is [`State::Malformed`].
#[must_use]
pub fn parse(text: &str) -> State {
    let field = |key: &str| -> Option<String> {
        let prefix = format!("{key}=");
        text.lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let Some(reason) = field("reason") else {
        return State::Malformed;
    };
    State::Disabled(Record {
        reason,
        who: field("who").unwrap_or_else(|| "unknown".into()),
        when: field("when").unwrap_or_else(|| "unknown".into()),
    })
}

/// Read the marker at an explicit path.
#[must_use]
pub fn read_at(path: &Path) -> State {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::Enabled,
        Err(_) => State::Malformed,
    }
}

/// Read the marker for this process's environment.
#[must_use]
pub fn read_current() -> State {
    current_path().map_or(State::Enabled, |p| read_at(&p))
}

/// `disabled by operator: <reason> (<when>)`, or `""` when enabled.
#[must_use]
pub fn summary(state: &State) -> String {
    match state {
        State::Enabled => String::new(),
        State::Disabled(r) => format!("disabled by operator: {} ({})", r.reason, r.when),
        State::Malformed => "disabled by operator: marker unreadable (fail closed)".to_string(),
    }
}

/// [`check_or_refuse`] against an explicit marker path.
///
/// # Errors
///
/// [`Refusal`] when the host is disabled (or the marker is malformed).
pub fn check_at(path: &Path, entry_point: &str) -> Result<(), Refusal> {
    match read_at(path) {
        State::Enabled => Ok(()),
        state => Err(Refusal {
            entry_point: entry_point.to_string(),
            state,
            path: path.to_path_buf(),
        }),
    }
}

/// The single check every start / re-provision path calls before any side
/// effect.
///
/// # Errors
///
/// [`Refusal`] (its `Display` names reason, who, when and the enable command).
pub fn check_or_refuse(entry_point: &str) -> Result<(), Refusal> {
    match current_path() {
        Some(p) => check_at(&p, entry_point),
        None => Ok(()),
    }
}

/// Exit code of `loom-daemon host check` when the host is disabled. Distinct
/// from 1/2 on purpose: an older binary without `host` exits 1 or 2 for the
/// unknown subcommand, so shell guards refuse ONLY on this affirmative signal.
pub const HOST_CHECK_DISABLED_EXIT: i32 = 10;

/// [`check_or_refuse`] for entry points that are a process: print the refusal
/// on stderr and exit 1.
pub fn refuse_if_disabled_exit(entry_point: &str) {
    refuse_if_disabled_exit_with(entry_point, 1);
}

/// [`refuse_if_disabled_exit`] with an explicit exit code (`host check` uses
/// [`HOST_CHECK_DISABLED_EXIT`]).
pub fn refuse_if_disabled_exit_with(entry_point: &str, code: i32) {
    if let Err(r) = check_or_refuse(entry_point) {
        eprintln!("{r}");
        std::process::exit(code);
    }
}

/// Write the marker (atomic, owner-only). Idempotent: an existing well-formed
/// marker keeps its original reason/who/when and `Ok(false)` is returned;
/// `Ok(true)` means a new marker was written.
///
/// # Errors
///
/// A human-readable reason on I/O failure or an empty `reason`.
pub fn write_at(path: &Path, reason: &str, who: &str) -> Result<bool, String> {
    let reason = reason.replace(['\n', '\r'], " ");
    if reason.trim().is_empty() {
        return Err("a non-empty --reason is required".to_string());
    }
    if matches!(read_at(path), State::Disabled(_)) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let body = format!(
        "# loom host opt-out (#10179): no autonomous daemon belongs on this host.\n\
         # Cleared only by `loom-daemon host enable`. Do not hand-edit.\n\
         reason={reason}\nwho={who}\nwhen={when}\n",
        who = who.replace(['\n', '\r'], " "),
        when = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
    );
    let tmp = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
    let write = {
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .and_then(|mut f| f.write_all(body.as_bytes()))
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, body.as_bytes())
        }
    };
    if let Err(e) = write.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("could not write {}: {e}", path.display()));
    }
    Ok(true)
}

/// Remove the marker. `Ok(true)` when one was removed, `Ok(false)` when none
/// existed (idempotent).
///
/// # Errors
///
/// A human-readable reason on I/O failure.
pub fn clear_at(path: &Path) -> Result<bool, String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("could not remove {}: {e}", path.display())),
    }
}

/// Best-effort identity of whoever is running the command.
#[must_use]
pub fn current_who() -> String {
    let user = ["LOOM_OPERATOR", "USER", "LOGNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "unknown".to_string());
    let host = std::env::var("HOSTNAME").unwrap_or_default();
    if host.is_empty() {
        user
    } else {
        format!("{user}@{host}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(dir: &Path) -> PathBuf {
        dir.join(MARKER_FILENAME)
    }

    #[test]
    fn path_is_sibling_or_suffixed() {
        let d = Path::new("/x/y");
        assert_eq!(disabled_path(&d.join("autonomy-desired")), d.join("autonomy-disabled"));
        assert_eq!(disabled_path(&d.join("m")), d.join("m.disabled"));
    }

    #[test]
    fn absent_is_enabled_and_check_passes() {
        let t = tempfile::tempdir().unwrap();
        let p = disabled_path(&marker(t.path()));
        assert_eq!(read_at(&p), State::Enabled);
        assert!(check_at(&p, "x").is_ok());
    }

    #[test]
    fn write_then_refuse_names_reason_who_when_and_enable() {
        let t = tempfile::tempdir().unwrap();
        let p = disabled_path(&marker(t.path()));
        assert!(write_at(&p, "cost freeze", "alice",).unwrap());
        let msg = check_at(&p, "daemon-start").unwrap_err().to_string();
        assert!(msg.contains("daemon-start"));
        assert!(msg.contains("cost freeze"));
        assert!(msg.contains("alice"));
        assert!(msg.contains("20"), "has a timestamp: {msg}");
        assert!(msg.contains("loom-daemon host enable"));
    }

    #[test]
    fn write_is_idempotent_and_keeps_first_record() {
        let t = tempfile::tempdir().unwrap();
        let p = disabled_path(&marker(t.path()));
        assert!(write_at(&p, "first", "a").unwrap());
        assert!(!write_at(&p, "second", "b").unwrap());
        let State::Disabled(r) = read_at(&p) else {
            panic!()
        };
        assert_eq!(r.reason, "first");
    }

    #[test]
    fn empty_reason_rejected() {
        let t = tempfile::tempdir().unwrap();
        let p = disabled_path(&marker(t.path()));
        assert!(write_at(&p, "  ", "a").is_err());
        assert!(!p.exists());
    }

    #[test]
    fn malformed_fails_closed_with_generic_message() {
        let t = tempfile::tempdir().unwrap();
        let p = disabled_path(&marker(t.path()));
        std::fs::write(&p, "garbage\n").unwrap();
        assert_eq!(read_at(&p), State::Malformed);
        let msg = check_at(&p, "watchdog").unwrap_err().to_string();
        assert!(msg.contains("failing closed"));
        assert!(msg.contains("loom-daemon host enable"));
    }

    #[test]
    fn clear_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let p = disabled_path(&marker(t.path()));
        write_at(&p, "r", "w").unwrap();
        assert!(clear_at(&p).unwrap());
        assert!(!clear_at(&p).unwrap());
        assert_eq!(read_at(&p), State::Enabled);
    }

    #[test]
    fn summary_format() {
        let s = State::Disabled(Record {
            reason: "r".into(),
            who: "w".into(),
            when: "2026-01-01T00:00:00Z".into(),
        });
        assert_eq!(summary(&s), "disabled by operator: r (2026-01-01T00:00:00Z)");
        assert_eq!(summary(&State::Enabled), "");
    }

    #[test]
    fn heal_marker_never_rearms_while_disabled() {
        use crate::autonomy_marker::{heal_marker, HealOutcome, MarkerFields};
        let t = tempfile::tempdir().unwrap();
        let m = marker(t.path());
        write_at(&disabled_path(&m), "r", "w").unwrap();
        let f = MarkerFields {
            started_at: "x".into(),
            repo_root: None,
            pid_file: t.path().join("p"),
            heartbeat_file: t.path().join("h"),
            heartbeat_interval_secs: 1,
            use_launchd: false,
            launchd_label: "l".into(),
            socket_path: t.path().join("s"),
        };
        assert_eq!(heal_marker(Some("systemd"), &m, &f), HealOutcome::HostDisabled);
        assert!(!m.exists());
    }
}
