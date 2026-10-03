//! **Attended runs** (#10116): `session.output` for an agent started from an
//! attended Claude Code session instead of dispatched by `loom-daemon`.
//!
//! # Why a second way in
//!
//! The daemon's producer ([`super::spawn_task`]) learns which runs exist from
//! the event bus: a `sweep.global.dispatch` opens one. A Loom role run as a
//! subagent of an operator's session (`loom-builder` started by the Agent
//! tool), or a person running `/loom:builder 42`, never goes through dispatch.
//! The daemon never opens a run for it, so the issue's log feed stays empty.
//!
//! This module is the same producer with a different way in. Everything after
//! "a run exists" is reused unchanged: [`super::claude::Cursor`] (the content
//! boundary), [`SessionOutputRecord`](crate::telemetry::kinds::session_output::SessionOutputRecord)'s
//! redaction, the run/status/heartbeat/gap logic in [`super::tick`], the OTLP
//! mapping, and the exporter queue and sender. This module adds only how a run
//! is opened and how it ends:
//!
//! 1. **Who starts it.** `loom-daemon lease ensure`, which every Builder and
//!    Doctor already runs at claim time (`worktree.sh <N>` calls it), and the
//!    `loom-daemon live-output-attend` subcommand for any other entry point.
//! 2. **Which transcript.** The calling agent's own. That is the transcript in
//!    this Claude Code session (`$CLAUDE_CODE_SESSION_ID`), either its main
//!    file or one of its `subagents/agent-*.jsonl`, whose **pending** `Bash`
//!    call names the issue. Claude Code writes a tool call to the transcript
//!    before running it, so the call executing this code is that pending call.
//!    A parent session waiting on a subagent has a pending `Agent` call, not a
//!    `Bash` one, so it does not match. Zero or several matches is refused,
//!    never guessed.
//! 3. **How it ends.** The tailer is a detached `live-output-attend
//!    --foreground` process. It closes the run with `coverage = ended` when the
//!    watched session process exits, when the transcript has been idle for
//!    [`DEFAULT_IDLE_EXIT_SECS`], or at [`DEFAULT_MAX_AGE_SECS`] (the lease
//!    renewer's own cap).
//!
//! # Identity
//!
//! | Attribute | Attended value |
//! |---|---|
//! | `loom.session.output.launch` | `attended` (a daemon run says `daemon`) |
//! | `loom.sweep_id` | `attended-<first 8 of session id>[-<agent id>]`, a pure function of the transcript, so a restarted tailer keeps the same attempt |
//! | `loom.session_id` / `stream_id` | `<session>` or `<session>/agent-<id>`, the same shape [`super::claude::discover`] mints |
//! | `loom.repo` | the claim checkout's `origin` remote, never the transcript's `cwd`, which is the operator session's directory and can be another repo |
//! | `loom.role` | `--role`, else the subagent's `loom-<role>` type, else the session's `/loom:<role>` command |
//! | `loom.attempt` | absent. There is no dispatch counter to number attended runs |
//!
//! # Nothing changes when nothing is configured
//!
//! Starting is a handful of local file reads and never touches the network.
//! It stops at the first missing piece and reports which one: an agent the
//! daemon launched, observability off, live output off, no usable OTLP
//! exporter, no session id, or no identifiable transcript. The detached tailer
//! never shares stdio with the session, and its final drain is bounded by
//! [`FINAL_FLUSH`], so an unreachable collector cannot hold anything up.
//!
//! # No credential crosses
//!
//! The ingest key is read only inside the detached tailer, from the configured
//! key file, and is sent only as the exporter's `Authorization` header, exactly
//! as the daemon does. It is never on a command line or in a record. The
//! tool-call input this module reads to find the transcript is used for that
//! match only and is never placed on a record: the records come from
//! [`super::claude::Cursor`], which drops tool input at parse time.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use chrono::Utc;
use serde_json::Value;

use super::{ResolvedLiveOutput, Run, SessionOutputSink, Source, Tracker};
use crate::telemetry::kinds::session_output::{
    Coverage, Launch, OutputCategory, RunIdentity, RunState,
};
use crate::telemetry::{RepoVisibility, SessionKind};

/// The env var Claude Code sets, in every tool call's environment, to the id
/// of the session (the parent session, for a subagent) that is running it.
pub const SESSION_ID_ENV: &str = "CLAUDE_CODE_SESSION_ID";

/// Prefix of every attended run's `loom.sweep_id`.
pub const SWEEP_ID_PREFIX: &str = "attended-";

/// Absolute cap on one tailer's life, in seconds. Matches `lease ensure`'s own
/// renewal cap (4h) for the same reason: the watched pid is the session's
/// harness, which outlives the subagent that did the work.
pub const DEFAULT_MAX_AGE_SECS: u64 = 14_400;

/// A transcript untouched for this long is treated as finished. Long enough
/// for a ten-minute foreground command plus a CI wait; a run that resumes after
/// it restarts on its next claim step with the same attempt id.
pub const DEFAULT_IDLE_EXIT_SECS: u64 = 1_800;

/// Ceiling on the tailer's own export queue. Smaller than the daemon's
/// default: one run's live tail, not a host's whole backlog.
pub const QUEUE_CAPACITY: usize = 500;

/// Upper bound on the tailer's final drain when its run ends.
pub const FINAL_FLUSH: Duration = Duration::from_secs(5);

/// Bytes read from the end of each candidate transcript when locating the
/// caller. The pending call is the newest line, so this only needs to cover
/// one assistant turn.
const LOCATE_TAIL_BYTES: u64 = 256 * 1024;

/// Locate attempts, in case the pending call has not reached the disk yet.
const LOCATE_ATTEMPTS: usize = 3;
const LOCATE_RETRY: Duration = Duration::from_millis(200);

/// Env markers of a process tree `loom-daemon` launched. Any one of them means
/// the daemon's own producer is the right one, or the run is not attended.
const DAEMON_LAUNCH_MARKERS: &[(&str, Option<&str>)] = &[
    // Set on every sweep child and every role-runner child.
    (crate::provenance::origin::ENV, Some("autonomous")),
    // Set on every sweep child (#8835).
    ("LOOM_SWEEP_ID", None),
    // A scheduled GitHub Actions role run is autonomous too.
    ("GITHUB_ACTIONS", Some("true")),
];

/// The parts of the process environment this module reads, captured up front
/// so the decisions are testable without mutating global env state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttendEnv {
    /// [`SESSION_ID_ENV`], when non-empty.
    pub session_id: Option<String>,
    /// Whether any [`DAEMON_LAUNCH_MARKERS`] entry matches.
    pub daemon_launched: bool,
}

impl AttendEnv {
    /// Read the real process environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// The pure core of [`Self::from_process`].
    #[must_use]
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let non_empty = |key: &str| {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let daemon_launched = DAEMON_LAUNCH_MARKERS.iter().any(|(key, want)| {
            non_empty(key).is_some_and(|value| want.is_none_or(|want| value == want))
        });
        AttendEnv {
            session_id: non_empty(SESSION_ID_ENV),
            daemon_launched,
        }
    }
}

/// The session's own durable pid from the environment
/// (`$LOOM_AGENT_SESSION_PID`, else `$CLAUDE_PID`), for a caller that was not
/// handed one. The same order `lease ensure`'s call site uses.
#[must_use]
pub fn session_pid_from_env() -> Option<u32> {
    ["LOOM_AGENT_SESSION_PID", "CLAUDE_PID"]
        .iter()
        .find_map(|key| std::env::var(key).ok()?.trim().parse().ok())
}

// ============================================================================
// The export gate
// ============================================================================

/// What the tailer needs in order to export, resolved from config without
/// reading the ingest key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPlan {
    /// OTLP endpoints that passed the daemon's own policy pass.
    pub endpoints: Vec<String>,
    /// Where the ingest key lives. Read only by the detached tailer.
    pub key_file: String,
    pub batch_size: usize,
    pub flush_interval: Duration,
    pub capacity: usize,
    pub live: ResolvedLiveOutput,
}

/// The export plan for `workspace_root`, or the first reason there is none.
///
/// Every check the daemon makes before it would publish `session.output`, in
/// the same order and through the same functions: observability on, live
/// output on, an OTLP exporter that survives [`super::super::planned_otlp_endpoints`]'s
/// policy pass, and an ingest key file. No network, no subprocess.
///
/// # Errors
///
/// The reason export is off, worded for the one-line diagnostic.
pub fn export_plan(workspace_root: &Path) -> Result<ExportPlan, String> {
    let config = super::super::read_config(workspace_root);
    if !super::super::resolve_enabled(&config) {
        return Err("observability is not enabled".to_string());
    }
    let live = super::read_config(workspace_root);
    if !super::resolve_enabled(&live) {
        return Err("live output is not enabled (observability.liveOutput.enabled)".to_string());
    }
    let endpoints = super::super::planned_otlp_endpoints(&config);
    if endpoints.is_empty() {
        return Err(
            "no usable OTLP exporter is configured (session.output is OTLP-only)".to_string()
        );
    }
    let Some(key_file) = super::super::resolve_ingest_key_file(&config) else {
        return Err("observability.ingestKeyFile is not configured".to_string());
    };
    Ok(ExportPlan {
        endpoints,
        key_file,
        batch_size: super::super::resolve_batch_size(&config),
        flush_interval: Duration::from_secs(super::super::resolve_flush_interval_secs(&config)),
        capacity: super::super::resolve_queue_capacity(&config).min(QUEUE_CAPACITY),
        live: super::resolve(&live),
    })
}

// ============================================================================
// Locating the caller's transcript
// ============================================================================

/// The attending agent's own transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub path: PathBuf,
    /// `<session>` or `<session>/agent-<id>`: the stream key every record
    /// carries, never a path.
    pub stream_id: String,
    /// The Claude Code session id (the parent session, for a subagent).
    pub session_id: String,
    /// The subagent's id, when the transcript is a subagent's.
    pub agent_id: Option<String>,
    /// The role the transcript declares, when it declares one.
    pub role: Option<String>,
}

impl Located {
    /// Describe an explicit transcript path (`--transcript`), recognising the
    /// `<session>/subagents/agent-<id>.jsonl` layout.
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        let stem = path.file_stem()?.to_string_lossy().into_owned();
        let parent = path.parent()?;
        let (session_id, agent_id, stream_id) =
            if parent.file_name().is_some_and(|name| name == "subagents") {
                let session = parent.parent()?.file_name()?.to_string_lossy().into_owned();
                let agent = stem.strip_prefix("agent-").map(str::to_string);
                let stream = format!("{session}/{stem}");
                (session, agent, stream)
            } else {
                (stem.clone(), None, stem)
            };
        Some(Located {
            path: path.to_path_buf(),
            stream_id,
            session_id,
            agent_id,
            role: declared_role(path),
        })
    }

    /// The attempt id records are grouped by: a pure function of the
    /// transcript, so a restarted tailer reports the same attempt.
    #[must_use]
    pub fn sweep_id(&self) -> String {
        let session: String = self.session_id.chars().take(8).collect();
        match &self.agent_id {
            Some(agent) => format!("{SWEEP_ID_PREFIX}{session}-{agent}"),
            None => format!("{SWEEP_ID_PREFIX}{session}"),
        }
    }
}

/// The role a transcript declares: a subagent's `loom-<role>` type from its
/// `.meta.json`, else a `/loom:<role>` slash command at the session's head.
/// Never inferred from free text.
fn declared_role(path: &Path) -> Option<String> {
    let meta = path.with_extension("meta.json");
    let agent_type = std::fs::read_to_string(meta)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("agentType")?.as_str().map(str::to_string));
    if let Some(role) = agent_type.as_deref().and_then(loom_role) {
        return Some(role);
    }
    let head = crate::transcript_tokens::read_head(path)?;
    crate::activity::transcript_parse::slash_command_role(&head)
}

/// `loom-builder` → `builder`, for a known Loom role only.
fn loom_role(agent_type: &str) -> Option<String> {
    let name = agent_type.strip_prefix("loom-")?.to_ascii_lowercase();
    crate::activity::transcript_parse::ROLE_KEYWORDS
        .contains(&name.as_str())
        .then_some(name)
}

/// Find the calling agent's transcript in session `session_id` under
/// `projects_dir`: the one whose pending `Bash` call names `issue`.
///
/// # Errors
///
/// Why no single transcript qualified. Ambiguity is an error, never a guess.
pub fn locate(projects_dir: &Path, session_id: &str, issue: u32) -> Result<Located, String> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("{SESSION_ID_ENV} is not a plain session id"));
    }
    let candidates = session_transcripts(projects_dir, session_id);
    if candidates.is_empty() {
        return Err(format!("no transcript for session {session_id}"));
    }
    let matching: Vec<&PathBuf> = candidates
        .iter()
        .filter(|path| pending_bash_names_issue(path, issue))
        .collect();
    match matching.as_slice() {
        [only] => Located::from_path(only).ok_or_else(|| "unreadable transcript path".to_string()),
        [] => Err(format!(
            "none of session {session_id}'s {} transcript(s) has a running command naming #{issue}",
            candidates.len()
        )),
        several => Err(format!(
            "{} transcripts in session {session_id} have a running command naming #{issue}; \
             not guessing",
            several.len()
        )),
    }
}

/// [`locate`], retried briefly in case the pending call is not on disk yet.
fn locate_with_retry(projects_dir: &Path, session_id: &str, issue: u32) -> Result<Located, String> {
    let mut result = locate(projects_dir, session_id, issue);
    for _ in 1..LOCATE_ATTEMPTS {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(LOCATE_RETRY);
        result = locate(projects_dir, session_id, issue);
    }
    result
}

/// Every transcript of session `session_id`, in any project directory: the
/// session's working directory is the operator's, not the claim's, so the
/// project slug cannot be derived from where this runs.
fn session_transcripts(projects_dir: &Path, session_id: &str) -> Vec<PathBuf> {
    let Ok(projects) = std::fs::read_dir(projects_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for project in projects.flatten() {
        let main = project.path().join(format!("{session_id}.jsonl"));
        if main.is_file() || project.path().join(session_id).is_dir() {
            out.extend(
                crate::transcript_tokens::session_transcripts(&main)
                    .into_iter()
                    .filter(|path| path.is_file()),
            );
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Whether the newest lines of `path` hold a `Bash` call with no result yet
/// whose command names `issue`. The command text is read for this match only.
fn pending_bash_names_issue(path: &Path, issue: u32) -> bool {
    let Some(tail) = read_tail(path, LOCATE_TAIL_BYTES) else {
        return false;
    };
    let mut pending: Vec<(String, String)> = Vec::new();
    for line in tail.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(content) = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for block in content {
            let block_type = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match (kind, block_type) {
                ("assistant", "tool_use")
                    if block.get("name").and_then(Value::as_str) == Some("Bash") =>
                {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        let command = block
                            .get("input")
                            .and_then(|input| input.get("command"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        pending.push((id.to_string(), command.to_string()));
                    }
                }
                ("user", "tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        pending.retain(|(open, _)| open != id);
                    }
                }
                _ => {}
            }
        }
    }
    pending
        .iter()
        .any(|(_, command)| names_issue(command, issue))
}

/// Whether `text` contains `issue` as a whole number (`issue-42` and
/// `worktree.sh 42` name 42; `142` and `420` do not).
fn names_issue(text: &str, issue: u32) -> bool {
    let needle = issue.to_string();
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text[from..].find(&needle) {
        let at = from + offset;
        let end = at + needle.len();
        let clear_before = at == 0 || !bytes[at - 1].is_ascii_digit();
        let clear_after = end >= bytes.len() || !bytes[end].is_ascii_digit();
        if clear_before && clear_after {
            return true;
        }
        from = at + 1;
    }
    false
}

/// The last `max` bytes of `path`, starting at a line boundary.
fn read_tail(path: &Path, max: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buffer = Vec::new();
    file.take(max).read_to_end(&mut buffer).ok()?;
    let text = String::from_utf8_lossy(&buffer).into_owned();
    if start == 0 {
        return Some(text);
    }
    Some(
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default(),
    )
}

// ============================================================================
// One tailer per transcript
// ============================================================================

/// Where a workspace's attended tailers keep their lock and queue files.
fn state_dir(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".loom")
        .join("logs")
        .join("live-output-attended")
}

/// A stream id as a file stem.
fn file_key(stream_id: &str) -> String {
    stream_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// An exclusive `flock` on one stream's lock file, released when dropped or
/// when the process exits, so a dead tailer never blocks the next one.
#[derive(Debug)]
struct StreamLock {
    path: PathBuf,
    _file: std::fs::File,
}

impl StreamLock {
    /// `Ok(None)` when another tailer holds it.
    fn try_acquire(path: &Path) -> std::io::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd as _;
            // SAFETY: `flock` on a descriptor this function owns.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                return Ok(None);
            }
        }
        Ok(Some(StreamLock {
            path: path.to_path_buf(),
            _file: file,
        }))
    }

    /// Remove the lock file while still holding it. A tailer that opens the
    /// path afterwards gets a fresh file, which is harmless: this one is only
    /// draining by then.
    fn release(self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ============================================================================
// Starting and running
// ============================================================================

/// One `live-output-attend` (or `lease ensure`) request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRequest {
    pub issue: u32,
    /// Explicit role, overriding what the transcript declares.
    pub role: Option<String>,
    /// The session's durable pid; the run ends when it exits.
    pub watch_pid: Option<u32>,
    /// Any directory inside the claim's checkout.
    pub workspace: PathBuf,
    /// The transcript to read, skipping [`locate`].
    pub transcript: Option<PathBuf>,
    pub max_age_secs: u64,
    pub idle_exit_secs: u64,
}

/// Why an attended run stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The watched session process exited.
    SessionExited,
    /// The transcript was idle for the idle limit.
    Idle,
    /// The tailer reached its maximum age.
    MaxAge,
    /// The transcript disappeared.
    TranscriptGone,
}

impl EndReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::SessionExited => "the session process exited",
            EndReason::Idle => "the transcript went idle",
            EndReason::MaxAge => "the tailer reached its maximum age",
            EndReason::TranscriptGone => "the transcript disappeared",
        }
    }
}

/// What one request did. Every variant is a success to the caller: this is
/// the one-line diagnostic, never an error channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `loom-daemon` launched this agent; its own producer covers it.
    DaemonLaunched,
    /// Export is off, and why.
    NotConfigured(String),
    /// No Claude Code session id in the environment.
    NoSession,
    /// The calling agent's transcript could not be identified.
    NotLocated(String),
    /// A tailer already holds this transcript.
    AlreadyRunning,
    /// A detached tailer is now publishing.
    Started { pid: u32, sweep_id: String },
    /// The tailer could not be started.
    SpawnFailed(String),
    /// A foreground tailer's run ended.
    Ended { sweep_id: String, reason: EndReason },
}

impl Outcome {
    /// The diagnostic line.
    #[must_use]
    pub fn describe(&self, issue: u32) -> String {
        match self {
            Outcome::DaemonLaunched => format!(
                "issue #{issue}: launched by loom-daemon, whose own producer covers it; no \
                 attended tailer"
            ),
            Outcome::NotConfigured(why) => {
                format!("issue #{issue}: not publishing live output, {why}")
            }
            Outcome::NoSession => format!(
                "issue #{issue}: not publishing live output, no Claude Code session \
                 ({SESSION_ID_ENV} unset)"
            ),
            Outcome::NotLocated(why) => format!(
                "issue #{issue}: not publishing live output, this agent's transcript was not \
                 identified: {why}"
            ),
            Outcome::AlreadyRunning => {
                format!("issue #{issue}: a live-output tailer already follows this transcript")
            }
            Outcome::Started { pid, sweep_id } => {
                format!("issue #{issue}: publishing live output as {sweep_id} (tailer pid {pid})")
            }
            Outcome::SpawnFailed(why) => {
                format!("issue #{issue}: could not start the live-output tailer: {why}")
            }
            Outcome::Ended { sweep_id, reason } => {
                format!("issue #{issue}: {sweep_id} ended ({})", reason.as_str())
            }
        }
    }
}

/// The checkout root a request names, for config and the `origin` remote.
fn root_of(request: &StartRequest) -> Result<PathBuf, Outcome> {
    crate::repo_root::resolve_repo_root(&request.workspace.to_string_lossy())
        .map_err(|_| Outcome::NotConfigured("not inside a Loom checkout".to_string()))
}

/// Decide, locate, and detach a tailer. Returns at once; never touches the
/// network.
#[must_use]
pub fn start(request: &StartRequest, env: &AttendEnv) -> Outcome {
    let projects = crate::transcript_tokens::claude_projects_dir();
    start_with(request, env, projects.as_deref(), spawn_detached)
}

/// [`start`] with the projects directory and the spawn injected.
fn start_with(
    request: &StartRequest,
    env: &AttendEnv,
    projects_dir: Option<&Path>,
    spawn: impl FnOnce(Command) -> std::io::Result<u32>,
) -> Outcome {
    if env.daemon_launched {
        return Outcome::DaemonLaunched;
    }
    let root = match root_of(request) {
        Ok(root) => root,
        Err(outcome) => return outcome,
    };
    if let Err(why) = export_plan(&root) {
        return Outcome::NotConfigured(why);
    }
    let located = match &request.transcript {
        Some(path) => match Located::from_path(path) {
            Some(located) => located,
            None => return Outcome::NotLocated(format!("{} is not a transcript", path.display())),
        },
        None => {
            let Some(session_id) = env.session_id.as_deref() else {
                return Outcome::NoSession;
            };
            let Some(projects) = projects_dir else {
                return Outcome::NotLocated("no Claude projects directory".to_string());
            };
            match locate_with_retry(projects, session_id, request.issue) {
                Ok(located) => located,
                Err(why) => return Outcome::NotLocated(why),
            }
        }
    };
    let lock_path = state_dir(&root).join(format!("{}.lock", file_key(&located.stream_id)));
    match StreamLock::try_acquire(&lock_path) {
        // Probe only: the tailer takes the lock for itself. If two starts
        // race past this, the loser's tailer finds it held and exits.
        Ok(Some(probe)) => drop(probe),
        Ok(None) => return Outcome::AlreadyRunning,
        Err(error) => return Outcome::SpawnFailed(format!("{}: {error}", lock_path.display())),
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return Outcome::SpawnFailed(error.to_string()),
    };
    let mut command = Command::new(exe);
    command
        .arg("live-output-attend")
        .arg("--foreground")
        .arg("--issue")
        .arg(request.issue.to_string())
        .arg("--workspace")
        .arg(&root)
        .arg("--transcript")
        .arg(&located.path)
        .arg("--max-age")
        .arg(request.max_age_secs.to_string())
        .arg("--idle-exit")
        .arg(request.idle_exit_secs.to_string());
    if let Some(role) = &request.role {
        command.arg("--role").arg(role);
    }
    if let Some(pid) = request.watch_pid {
        command.arg("--watch-pid").arg(pid.to_string());
    }
    command.current_dir(&root);
    match spawn(command) {
        Ok(pid) => Outcome::Started {
            pid,
            sweep_id: located.sweep_id(),
        },
        Err(error) => Outcome::SpawnFailed(error.to_string()),
    }
}

/// Start `command` detached: no stdio shared with the session, its own
/// process group, never waited for.
fn spawn_detached(mut command: Command) -> std::io::Result<u32> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command.spawn().map(|child| child.id())
}

/// The identity every record of this attended run carries.
fn identity(
    located: &Located,
    issue: u32,
    role: Option<String>,
    repo: Option<String>,
) -> RunIdentity {
    RunIdentity {
        repo,
        visibility: RepoVisibility::Private,
        issue: Some(issue),
        session_kind: Some(SessionKind::Sweep),
        sweep_id: Some(located.sweep_id()),
        session_id: Some(located.stream_id.clone()),
        attempt: None,
        runtime: "claude".to_string(),
        role: role.or_else(|| located.role.clone()),
        launch: Launch::Attended,
    }
}

/// The run's end conditions, in priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Limits {
    max_age: Duration,
    idle_exit: Duration,
}

/// Whether the run is over: pure, so each condition is testable.
fn end_reason(
    age: Duration,
    session_alive: bool,
    idle_for: Option<Duration>,
    limits: Limits,
) -> Option<EndReason> {
    if !session_alive {
        return Some(EndReason::SessionExited);
    }
    match idle_for {
        None => return Some(EndReason::TranscriptGone),
        Some(idle) if idle >= limits.idle_exit => return Some(EndReason::Idle),
        Some(_) => {}
    }
    (age >= limits.max_age).then_some(EndReason::MaxAge)
}

/// How long since `path` last changed; `None` when it is gone.
fn idle_for(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(
        SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default(),
    )
}

/// Follow one attended run until it ends, publishing through `sink`. The
/// daemon producer's own [`super::tick`] does the reading, so an attended run
/// gets exactly the records, heartbeats and gaps a daemon run gets.
async fn drive(
    identity: RunIdentity,
    located: &Located,
    workspace_root: &Path,
    sink: &SessionOutputSink,
    live: ResolvedLiveOutput,
    limits: Limits,
    mut session_alive: impl FnMut() -> bool,
) -> EndReason {
    let started = Utc::now();
    let root_key = workspace_root.display().to_string();
    let issue = identity.issue.unwrap_or_default();
    let status_stream = identity
        .sweep_id
        .clone()
        .unwrap_or_else(|| located.sweep_id());
    let source = Source::Fixed {
        stream_id: located.stream_id.clone(),
        path: located.path.clone(),
    };
    let mut tracker = Tracker::default();
    let run = tracker.insert(
        &root_key,
        issue,
        Run::new(identity, workspace_root.to_path_buf(), source, status_stream, started),
    );
    sink.push(run.status(OutputCategory::Coverage, started, Coverage::Degraded, RunState::Running));

    let mut slug_cache: HashMap<String, String> = HashMap::new();
    let mut ticker = tokio::time::interval(live.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let reason = loop {
        ticker.tick().await;
        super::tick(&mut tracker, sink, &mut slug_cache, live).await;
        let age = Utc::now()
            .signed_duration_since(started)
            .to_std()
            .unwrap_or_default();
        if let Some(reason) = end_reason(age, session_alive(), idle_for(&located.path), limits) {
            break reason;
        }
    };
    if let Some(mut run) = tracker.close(&root_key, issue) {
        sink.push(run.status(
            OutputCategory::Coverage,
            Utc::now(),
            Coverage::Ended,
            RunState::Ended,
        ));
    }
    reason
}

/// Run the tailer in this process until its run ends: what the detached
/// child started by [`start`] executes.
pub async fn run_foreground(request: &StartRequest, env: &AttendEnv) -> Outcome {
    if env.daemon_launched {
        return Outcome::DaemonLaunched;
    }
    let root = match root_of(request) {
        Ok(root) => root,
        Err(outcome) => return outcome,
    };
    let plan = match export_plan(&root) {
        Ok(plan) => plan,
        Err(why) => return Outcome::NotConfigured(why),
    };
    let Some(located) = request.transcript.as_deref().and_then(Located::from_path) else {
        return Outcome::NotLocated("--foreground needs --transcript".to_string());
    };
    let key = file_key(&located.stream_id);
    let lock = match StreamLock::try_acquire(&state_dir(&root).join(format!("{key}.lock"))) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Outcome::AlreadyRunning,
        Err(error) => return Outcome::SpawnFailed(error.to_string()),
    };
    let ingest_key = match super::super::read_ingest_key(&plan.key_file) {
        Ok(ingest_key) => ingest_key,
        Err(detail) => {
            lock.release();
            return Outcome::NotConfigured(detail);
        }
    };
    let Some((sink, queue_files)) = build_sink(&plan, &state_dir(&root), &key, &ingest_key) else {
        lock.release();
        return Outcome::NotConfigured("no OTLP exporter could be constructed".to_string());
    };
    drop(ingest_key);
    let repo = crate::forge_etag_store::remote_identity(&root).map(|(_host, slug)| slug);
    let identity = identity(&located, request.issue, request.role.clone(), repo);
    let sweep_id = located.sweep_id();
    let limits = Limits {
        max_age: Duration::from_secs(request.max_age_secs),
        idle_exit: Duration::from_secs(request.idle_exit_secs),
    };
    let watch_pid = request.watch_pid;
    let watch_since = Utc::now();
    let alive = move || {
        watch_pid.is_none_or(|pid| {
            crate::sweep_registry::reaper::pid_identity::pid_alive_since(pid, watch_since)
        })
    };
    let reason = drive(identity, &located, &root, &sink, plan.live, limits, alive).await;
    super::super::shutdown::flush_before_shutdown(FINAL_FLUSH).await;
    // A tailer has no next boot to drain a backlog on, so an undelivered tail
    // is dropped with its queue rather than left to accumulate.
    for file in queue_files {
        let _ = std::fs::remove_file(file);
    }
    lock.release();
    Outcome::Ended { sweep_id, reason }
}

/// The tailer's own OTLP queues and senders, over the plan's endpoints.
#[cfg(feature = "otlp")]
fn build_sink(
    plan: &ExportPlan,
    queue_dir: &Path,
    key: &str,
    ingest_key: &str,
) -> Option<(SessionOutputSink, Vec<PathBuf>)> {
    use std::sync::Arc;

    let host_id = crate::sweep_registry::host_identity();
    let mut queues = Vec::new();
    let mut files = Vec::new();
    for (index, endpoint) in plan.endpoints.iter().enumerate() {
        let Ok(exporter) =
            super::super::otlp::OtlpExporter::new(endpoint.clone(), ingest_key.to_string())
        else {
            continue;
        };
        let file = queue_dir.join(format!("{key}.otlp-{index}.jsonl"));
        let queue = Arc::new(super::super::queue::DurableQueue::open(file.clone(), plan.capacity));
        let status = Arc::new(super::super::ExportStatus::started(
            &host_id,
            endpoint,
            "otlp",
            plan.flush_interval.as_secs(),
        ));
        let _sender = super::super::sender::spawn_task(
            queue.clone(),
            exporter,
            plan.batch_size,
            plan.flush_interval,
            status,
        );
        queues.push(queue);
        files.push(file);
    }
    SessionOutputSink::new(queues, host_id).map(|sink| (sink, files))
}

/// Without the `otlp` feature there is no exporter to build; the policy pass
/// already refused every OTLP entry, so this is unreachable in practice.
#[cfg(not(feature = "otlp"))]
fn build_sink(
    _plan: &ExportPlan,
    _queue_dir: &Path,
    _key: &str,
    _ingest_key: &str,
) -> Option<(SessionOutputSink, Vec<PathBuf>)> {
    None
}

#[cfg(test)]
#[path = "attended_tests.rs"]
mod tests;
