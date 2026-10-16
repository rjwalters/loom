//! `loom-daemon prompt-tree session-start` (#12509): the attended-session
//! half of the prompt-tree telemetry.
//!
//! A user-scope `SessionStart` hook (`defaults/hooks/prompt-tree-session.sh`)
//! calls the binary with the hook JSON on stdin. The **front half**
//! ([`start`]) is local and fast: it reads `session_id`, `cwd` and `source`
//! (nothing else from the payload), refuses for a daemon-launched agent,
//! checks an OTLP target exists, and spawns the **detached half** ([`run`]),
//! which does the network reads and posts one `session.prompt_tree` record.
//! Nothing here can block or print into the session; a failed export leaves
//! one line in `<root>/.loom/logs/prompt-tree/last.log` and no retry queue.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::Utc;
use serde::Deserialize;

use super::remote::{read_with_remote, Expected, GitRemote, Remote};
use crate::presence::beat::sanitize_session;
use crate::presence::flush::{resolve_target, OtlpTransport};
use crate::presence::journal::Transport;
use crate::telemetry::kinds::session_prompt_tree::SessionPromptTreeRecord;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

/// Bound for the single OTLP post.
pub const EXPORT_BOUND: Duration = Duration::from_secs(5);

/// The only hook-payload fields read.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct HookPayload {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Parse the hook JSON; anything unreadable is an empty payload.
#[must_use]
pub fn parse_payload(raw: &str) -> HookPayload {
    serde_json::from_str(raw).unwrap_or_default()
}

/// What the detached half is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub cwd: PathBuf,
    pub session_id: String,
    pub source: Option<String>,
}

/// What the front half did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// The detached half was started.
    Spawned,
    /// Nothing to do, and why.
    Skipped(&'static str),
}

/// The checkout a path is in: `git rev-parse --show-toplevel`.
#[must_use]
pub fn toplevel(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!top.is_empty()).then(|| PathBuf::from(top))
}

/// The front half, with every environment dependency injected: local reads
/// only, no network, and the spawn is the only side effect.
pub fn start_with(
    daemon_launched: bool,
    payload: &HookPayload,
    process_cwd: &Path,
    toplevel: impl Fn(&Path) -> Option<PathBuf>,
    has_target: impl Fn(&Path) -> bool,
    spawn: impl FnOnce(&Spec) -> std::io::Result<()>,
) -> Started {
    if daemon_launched {
        return Started::Skipped("daemon-launched agent");
    }
    let cwd = payload
        .cwd
        .as_deref()
        .filter(|c| !c.is_empty())
        .map_or_else(|| process_cwd.to_path_buf(), PathBuf::from);
    let Some(top) = toplevel(&cwd) else {
        return Started::Skipped("not in a git checkout");
    };
    // Config resolves against the main checkout; the prompts a session reads
    // come from the checkout it started in (`top`).
    let config_root = std::env::var_os("LOOM_PROJECT_ROOT")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| top.clone());
    if !has_target(&config_root) {
        return Started::Skipped("no OTLP target");
    }
    let spec = Spec {
        cwd: top,
        session_id: sanitize_session(payload.session_id.as_deref().unwrap_or_default()),
        source: payload.source.clone().filter(|s| !s.is_empty()).map(|s| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(24)
                .collect()
        }),
    };
    match spawn(&spec) {
        Ok(()) => Started::Spawned,
        Err(_) => Started::Skipped("spawn failed"),
    }
}

/// The front half against the real process: stdout stays empty.
pub fn start(stdin: &str) -> Started {
    let payload = parse_payload(stdin);
    let daemon_launched =
        crate::observability::session_output::attended::AttendEnv::from_process().daemon_launched;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    start_with(
        daemon_launched,
        &payload,
        &cwd,
        toplevel,
        |root| resolve_target(root).is_ok(),
        spawn_detached,
    )
}

/// Re-exec this binary as the detached half: no stdio, its own process group
/// (so no controlling terminal prompt can appear), never waited for.
fn spawn_detached(spec: &Spec) -> std::io::Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["prompt-tree", "session-start", "--foreground", "--cwd"])
        .arg(&spec.cwd)
        .arg("--session-id")
        .arg(&spec.session_id);
    if let Some(source) = &spec.source {
        command.arg("--source").arg(source);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command.spawn().map(drop)
}

/// Build the record for `spec`, with the remote reads injected.
#[must_use]
pub fn build_record(
    spec: &Spec,
    expected: &Expected,
    remote: &dyn Remote,
) -> Option<SessionPromptTreeRecord> {
    let prompt = read_with_remote(&spec.cwd, expected, remote)?;
    Some(SessionPromptTreeRecord {
        captured_at: Utc::now(),
        session_id: spec.session_id.clone(),
        source: spec.source.clone(),
        prompt,
    })
}

/// The detached half: read, post once, exit. Errors are logged, never raised.
pub fn run(spec: &Spec) {
    let log_root = std::env::var_os("LOOM_PROJECT_ROOT")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| spec.cwd.clone());
    if let Err(why) = run_inner(spec, &log_root) {
        log_failure(&log_root, &why);
    }
}

fn run_inner(spec: &Spec, config_root: &Path) -> Result<(), String> {
    let target = resolve_target(config_root).map_err(|e| format!("{e:#}"))?;
    let record = build_record(spec, &Expected::from_env(), &GitRemote)
        .ok_or_else(|| "not a git checkout".to_string())?;
    let host = crate::host_identity::host_identity();
    let envelope = TelemetryEnvelope::new(host, TelemetryRecord::SessionPromptTree(record));
    let exporter = crate::observability::otlp::OtlpExporter::with_headers_file(
        target.endpoint,
        target.key,
        target.headers_file.as_deref(),
    )
    .map_err(|e| format!("building OTLP exporter: {e}"))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(OtlpTransport::new(exporter).send(&[envelope]));
    });
    match rx.recv_timeout(EXPORT_BOUND) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(_) => Err(format!("export outlived {}s", EXPORT_BOUND.as_secs())),
    }
}

fn log_failure(root: &Path, why: &str) {
    let dir = root.join(".loom/logs/prompt-tree");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ =
            std::fs::write(dir.join("last.log"), format!("{} {why}\n", Utc::now().to_rfc3339()));
    }
}
