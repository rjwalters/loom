//! Startup autonomy-desired marker **healing** (issue #4331).
//!
//! # Why
//!
//! The autonomy-desired marker (`<loom_dir>/autonomy-desired`, issue #4011) is
//! the durable "a daemon is EXPECTED to be running on this host" signal that the
//! host-side watchdog (`loom-daemon-watchdog.sh`) and `loom-daemon status`
//! (`daemon_install_state.rs`) both key off. `loom-daemon-start.sh` writes it on
//! a successful start and `loom-daemon-stop.sh` removes it on a deliberate stop —
//! so far so good. The gap #4331 closes is that **no code path re-creates an
//! ABSENT marker while a supervised daemon keeps running**:
//!
//! - `loom-daemon restart` (the #4054 supervised-restart primitive) exits
//!   `EXIT_RESTART`; launchd/systemd relaunch the daemon but nothing touches the
//!   marker — a present marker survives, an **absent** one is never healed.
//! - The in-daemon self-update loop uses `loom-daemon-update.sh --no-restart` +
//!   the restart primitive, bypassing the stop/start marker rewrite entirely.
//! - A bare launchd `KeepAlive:SuccessfulExit` / systemd `Restart=on-success`
//!   relaunch likewise never runs the start script.
//!
//! Once the marker is absent while a launchd/systemd-supervised daemon runs
//! (however that state arose — an out-of-band delete, a failed write, or a daemon
//! that predates a marker-writing start and has only rolled via the primitive
//! since), the watchdog logs a misleading `[OK] … nothing to check` and provides
//! **no crash protection** during exactly the unattended windows it exists for.
//!
//! # The fix: heal at ONE startup choke point
//!
//! Rather than patch each restart path, the daemon heals the marker **at
//! startup**: if it detects it is supervised ([`crate::ipc::detect_supervisor`]
//! returns `Some`) and the marker is absent, it re-writes it. That single choke
//! point covers the restart primitive, the self-update loop, AND a bare
//! supervisor relaunch, because all of them end in a fresh daemon process
//! starting up.
//!
//! # Constraints (deliberate)
//!
//! - **Never create a marker for an unsupervised run.** A `--foreground` / debug
//!   / nohup start ([`crate::ipc::detect_supervisor`] ⇒ `None`) must NOT arm the
//!   host-side detector — otherwise every dev run would leave a marker that pages
//!   the operator after the dev exits. Unsupervised ⇒ [`HealOutcome::UnsupervisedSkip`].
//! - **Respect `LOOM_AUTONOMY_MARKER`.** The marker path is resolved with the same
//!   env override the launchd plist / systemd unit export
//!   (`loom-daemon-start.sh`), so healing and the watchdog agree on the path.
//! - **Never overwrite an existing marker.** A present marker already encodes the
//!   operator's start-time intent (its `started_at`, recorded paths); healing
//!   only fills the *absent* case ⇒ [`HealOutcome::AlreadyPresent`] is a no-op.
//! - **Never fatal.** A failed write is logged and the daemon keeps running
//!   ([`HealOutcome::WriteFailed`]) — a daemon without a marker is strictly better
//!   than no daemon.
//!
//! The written marker mirrors `loom-daemon-start.sh`'s `write_intent_marker`
//! field-for-field (`started_at`, `repo_root`, `pid_file`, `heartbeat_file`,
//! `heartbeat_interval_secs`, `use_launchd`, `launchd_label`, `socket_path`) so a
//! healed marker is byte-compatible with a start-script-written one for both the
//! watchdog and `daemon_install_state`'s parsers.

use std::path::{Path, PathBuf};

use crate::daemon_heartbeat::HEARTBEAT_FILENAME;
use crate::daemon_install_state::{DEFAULT_LAUNCHD_LABEL, MARKER_FILENAME};

/// Basename of the daemon pid file under `<loom_dir>`, matching
/// `loom-daemon-start.sh`'s `PID_FILE="$DAEMON_STATE_HOME/.daemon.pid"`.
pub const PID_FILENAME: &str = ".daemon.pid";

/// The outcome of a startup healing attempt — one variant per branch of the
/// "supervised? marker present?" decision, so `main.rs` can log precisely and
/// tests can assert on the decision without inspecting the filesystem twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealOutcome {
    /// Not supervised ([`crate::ipc::detect_supervisor`] ⇒ `None`): a
    /// `--foreground` / nohup / debug run. NO marker written — arming the
    /// host-side detector for a run nothing will relaunch would page the
    /// operator after a dev session exits.
    UnsupervisedSkip,
    /// Supervised, but the marker already exists: left untouched (it already
    /// encodes the operator's start-time intent).
    AlreadyPresent,
    /// Supervised and the marker was absent: wrote a fresh healing marker at the
    /// contained path.
    Healed(PathBuf),
    /// Supervised, marker absent, but the write failed. Non-fatal — logged and
    /// the daemon keeps running.
    WriteFailed { path: PathBuf, error: String },
}

/// The fields a healing marker records — the exact set
/// `loom-daemon-start.sh`'s `write_intent_marker` emits, so the watchdog and
/// `daemon_install_state` parsers read a healed marker identically to a
/// start-script-written one.
#[derive(Debug, Clone)]
pub struct MarkerFields {
    /// ISO-8601 UTC start timestamp (`%Y-%m-%dT%H:%M:%SZ`, matching the shell's
    /// `date -u`). For a healed marker this is the *heal* time — the best
    /// approximation the daemon has of "expected since" when the original
    /// start-time value was lost.
    pub started_at: String,
    /// The managed repo root, when resolvable (advisory only — not consumed by
    /// the watchdog or `daemon_install_state`, but present in the start marker).
    pub repo_root: Option<PathBuf>,
    /// Pid file path (`<loom_dir>/.daemon.pid`) — the non-launchd liveness signal.
    pub pid_file: PathBuf,
    /// Heartbeat file path (`<loom_dir>/daemon.heartbeat`).
    pub heartbeat_file: PathBuf,
    /// Heartbeat cadence in seconds — the watchdog derives its staleness
    /// threshold from this.
    pub heartbeat_interval_secs: u64,
    /// Whether liveness is probed via launchd (`true`) or the pid file (`false`).
    pub use_launchd: bool,
    /// The launchd label to probe when `use_launchd` (ignored otherwise).
    pub launchd_label: String,
    /// The daemon socket path.
    pub socket_path: PathBuf,
}

/// Resolve `<loom_dir>` the same way `main.rs::resolve_loom_dir` /
/// `daemon_heartbeat` / `daemon_install_state` do: the parent of
/// `LOOM_SOCKET_PATH` when set (so a test daemon pointed at a tempdir socket
/// heals into that tempdir, never the operator's real `~/.loom`), else `~/.loom`.
///
/// `pub(crate)` since #4774: [`crate::daemon_pidfile`] resolves the pid file
/// against the *same* loom dir this module records in the marker's `pid_file=`
/// field, so a self-written pid file and a healed marker can never name
/// different directories.
pub(crate) fn resolve_loom_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        return PathBuf::from(path).parent().map(Path::to_path_buf);
    }
    dirs::home_dir().map(|h| h.join(".loom"))
}

/// Resolve the marker path: `LOOM_AUTONOMY_MARKER` env override first (matching
/// `loom-daemon-start.sh`'s `INTENT_MARKER` and `daemon_install_state`), else
/// `<loom_dir>/autonomy-desired`.
#[must_use]
pub fn resolve_marker_path(loom_dir: &Path) -> PathBuf {
    std::env::var("LOOM_AUTONOMY_MARKER")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| loom_dir.join(MARKER_FILENAME))
}

/// Render the marker file contents — mirrors `loom-daemon-start.sh`'s
/// `write_intent_marker` heredoc so a healed marker parses identically.
#[must_use]
pub fn render_marker(fields: &MarkerFields) -> String {
    let repo_root = fields
        .repo_root
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!(
        "# loom autonomy-desired marker (issue #4011)\n\
         # HEALED at daemon startup by loom-daemon (issue #4331): a supervised\n\
         # daemon started while this marker was absent, so crash protection was\n\
         # disarmed. Re-written here so the watchdog / `loom-daemon status` see\n\
         # the running daemon as EXPECTED again. Removed ONLY by an operator-\n\
         # initiated loom-daemon-stop.sh. Do not hand-edit.\n\
         started_at={started_at}\n\
         repo_root={repo_root}\n\
         pid_file={pid_file}\n\
         heartbeat_file={heartbeat_file}\n\
         heartbeat_interval_secs={heartbeat_interval_secs}\n\
         use_launchd={use_launchd}\n\
         launchd_label={launchd_label}\n\
         socket_path={socket_path}\n",
        started_at = fields.started_at,
        repo_root = repo_root,
        pid_file = fields.pid_file.display(),
        heartbeat_file = fields.heartbeat_file.display(),
        heartbeat_interval_secs = fields.heartbeat_interval_secs,
        use_launchd = fields.use_launchd,
        launchd_label = fields.launchd_label,
        socket_path = fields.socket_path.display(),
    )
}

/// Write the marker owner-only (`0o600`, matching the shell's `umask 077`) via a
/// temp file + atomic rename, so a concurrent watchdog read never sees a torn
/// file. Returns a human-readable reason on any I/O failure (never fatal).
fn write_marker_atomic(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!("could not create {}: {e}", parent.display()));
        }
    }
    let tmp = path.with_extension(format!("autonomy.tmp.{}", uuid::Uuid::new_v4()));

    let write_result = {
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
                .and_then(|mut f| f.write_all(contents.as_bytes()))
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, contents.as_bytes())
        }
    };
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("could not write temp marker {}: {e}", tmp.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("could not rename marker into place {}: {e}", path.display()));
    }
    Ok(())
}

/// Core, side-effect-scoped healing decision: given the resolved supervisor, the
/// marker path, and the fields to write, decide-and-do. Split from [`heal_on_startup`]
/// so tests drive every branch against tempdir fixtures without touching real env
/// vars or `~/.loom`.
///
/// - `supervisor == None` ⇒ [`HealOutcome::UnsupervisedSkip`] (no write).
/// - marker already exists ⇒ [`HealOutcome::AlreadyPresent`] (no write).
/// - marker absent ⇒ write it: [`HealOutcome::Healed`] on success, else
///   [`HealOutcome::WriteFailed`].
#[must_use]
pub fn heal_marker(
    supervisor: Option<&str>,
    marker_path: &Path,
    fields: &MarkerFields,
) -> HealOutcome {
    if supervisor.is_none() {
        return HealOutcome::UnsupervisedSkip;
    }
    if marker_path.exists() {
        return HealOutcome::AlreadyPresent;
    }
    match write_marker_atomic(marker_path, &render_marker(fields)) {
        Ok(()) => HealOutcome::Healed(marker_path.to_path_buf()),
        Err(error) => HealOutcome::WriteFailed {
            path: marker_path.to_path_buf(),
            error,
        },
    }
}

/// Production entry point: resolve the marker path + fields from the process
/// environment (mirroring `loom-daemon-start.sh` / `daemon_install_state`) and
/// heal if supervised. `heartbeat_interval_secs` is the already-resolved cadence
/// (main hoists it so the marker and the running heartbeat loop agree).
///
/// Returns `None` only when no loom dir can be resolved at all (no
/// `LOOM_SOCKET_PATH` and no home directory); otherwise the [`HealOutcome`].
#[must_use]
pub fn heal_on_startup(heartbeat_interval_secs: u64) -> Option<HealOutcome> {
    let supervisor = crate::ipc::detect_supervisor();
    // Short-circuit before touching the filesystem: an unsupervised run must not
    // even resolve/create the loom dir for the sake of a marker it won't write.
    if supervisor.is_none() {
        return Some(HealOutcome::UnsupervisedSkip);
    }

    let loom_dir = resolve_loom_dir()?;
    let marker_path = resolve_marker_path(&loom_dir);

    let use_launchd = supervisor.as_deref() == Some("launchd");
    let launchd_label = std::env::var("LOOM_LAUNCHD_LABEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_LAUNCHD_LABEL.to_string());
    let repo_root = std::env::var("LOOM_WORKSPACE")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let socket_path = std::env::var("LOOM_SOCKET_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| loom_dir.join("loom-daemon.sock"));

    let fields = MarkerFields {
        started_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        repo_root,
        pid_file: loom_dir.join(PID_FILENAME),
        heartbeat_file: loom_dir.join(HEARTBEAT_FILENAME),
        heartbeat_interval_secs,
        use_launchd,
        launchd_label,
        socket_path,
    };

    Some(heal_marker(supervisor.as_deref(), &marker_path, &fields))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_fields(dir: &Path) -> MarkerFields {
        MarkerFields {
            started_at: "2026-07-28T00:00:00Z".to_string(),
            repo_root: Some(dir.to_path_buf()),
            pid_file: dir.join(PID_FILENAME),
            heartbeat_file: dir.join(HEARTBEAT_FILENAME),
            heartbeat_interval_secs: 60,
            use_launchd: true,
            launchd_label: "com.example.loom-test".to_string(),
            socket_path: dir.join("loom-daemon.sock"),
        }
    }

    #[test]
    fn unsupervised_never_writes_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(MARKER_FILENAME);
        let outcome = heal_marker(None, &marker, &sample_fields(dir.path()));
        assert_eq!(outcome, HealOutcome::UnsupervisedSkip);
        assert!(!marker.exists(), "unsupervised run must NOT arm the detector");
    }

    #[test]
    fn supervised_absent_marker_is_healed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(MARKER_FILENAME);
        assert!(!marker.exists());
        let outcome = heal_marker(Some("launchd"), &marker, &sample_fields(dir.path()));
        assert_eq!(outcome, HealOutcome::Healed(marker.clone()));
        assert!(marker.exists(), "supervised + absent marker ⇒ healed");
    }

    #[test]
    fn supervised_present_marker_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(MARKER_FILENAME);
        fs::write(&marker, "started_at=ORIGINAL\n").unwrap();
        let outcome = heal_marker(Some("systemd"), &marker, &sample_fields(dir.path()));
        assert_eq!(outcome, HealOutcome::AlreadyPresent);
        // The original content — including its start-time intent — survives.
        assert_eq!(fs::read_to_string(&marker).unwrap(), "started_at=ORIGINAL\n");
    }

    #[test]
    fn healed_marker_is_parseable_by_the_install_state_probe() {
        // The healed marker must be byte-compatible with what
        // `daemon_install_state` parses — classify it and confirm the fields the
        // watchdog/status care about round-trip.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(MARKER_FILENAME);
        // Point at a pid we own so the probe resolves a live process, and force
        // the pid-file path (use_launchd=false) so the test is host-independent.
        let pid = std::process::id();
        let pid_file = dir.path().join(PID_FILENAME);
        fs::write(&pid_file, pid.to_string()).unwrap();
        let mut fields = sample_fields(dir.path());
        fields.use_launchd = false;
        assert_eq!(
            heal_marker(Some("systemd"), &marker, &fields),
            HealOutcome::Healed(marker.clone())
        );

        let env = crate::daemon_install_state::EnvOverrides {
            launchd_override: Some(false),
            launchd_label_override: None,
            launchd_domain_override: None,
            heartbeat_stale_secs_override: None,
            startup_grace_secs_override: Some(0),
            is_darwin: false,
        };
        // Pin a synthetic process age well beyond the zero grace window
        // (#4406): the real `ps -o etime=` lookup reports `00:00` for a
        // sub-second-old test binary, which parses to 0 and satisfies
        // `age <= grace`, flaking this assertion to AliveStarting.
        let report = crate::daemon_install_state::classify_with_process_age_fn(
            dir.path(),
            &marker,
            &env,
            |_| Some(1_000_000),
        );
        // Marker present + our own live pid ⇒ NOT the not-expected/dead states.
        assert_eq!(report.state, crate::daemon_install_state::InstallState::AliveButUnresponsive,);
        assert_eq!(report.started_at.as_deref(), Some("2026-07-28T00:00:00Z"));
        assert_eq!(report.pid, Some(pid));
    }

    #[test]
    fn healed_marker_has_owner_only_permissions() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(MARKER_FILENAME);
            let _ = heal_marker(Some("launchd"), &marker, &sample_fields(dir.path()));
            let mode = fs::metadata(&marker).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "marker must be owner-only (umask 077 parity)");
        }
    }

    #[test]
    fn render_marker_emits_all_watchdog_fields() {
        let dir = tempfile::tempdir().unwrap();
        let rendered = render_marker(&sample_fields(dir.path()));
        for key in [
            "started_at=",
            "pid_file=",
            "heartbeat_file=",
            "heartbeat_interval_secs=60",
            "use_launchd=true",
            "launchd_label=com.example.loom-test",
            "socket_path=",
        ] {
            assert!(rendered.contains(key), "rendered marker missing {key:?}:\n{rendered}");
        }
        // The heal provenance comment is present so an operator can tell a healed
        // marker from a start-script-written one.
        assert!(rendered.contains("HEALED at daemon startup"));
    }

    #[test]
    fn no_stray_temp_files_after_a_heal() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(MARKER_FILENAME);
        let _ = heal_marker(Some("launchd"), &marker, &sample_fields(dir.path()));
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp files should be renamed away: {leftovers:?}");
    }
}
