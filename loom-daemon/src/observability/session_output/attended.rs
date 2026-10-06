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
//! 2. **Which transcript.** The calling agent's own, proven through the
//!    process tree rather than guessed from text ([`caller`]). Claude Code
//!    writes a tool call to the transcript before running it, and runs it in
//!    a shell that is an ancestor of this process. So the caller's transcript
//!    is the one in this session (`$CLAUDE_CODE_SESSION_ID`) whose **pending**
//!    `Bash` call is exactly the command that ancestor shell runs. This also
//!    covers a claim step that never spells the issue number, such as the
//!    Doctor's `worktree.sh "$ISSUE_NUM"`. Zero or several matches start
//!    nothing.
//! 3. **Top-level sessions, only for a slash-command turn.** A top-level
//!    transcript is the whole conversation, so it is followed only when the
//!    turn holding the claim was opened by a `/loom:<role>` command whose
//!    arguments name the issue ([`turn`]), on top of the parent-shell binding
//!    in 2. The run then ends at that turn's next prompt. An operator's main
//!    agent claiming inline, in a turn no such command opened, can run for
//!    hours across many issues and repos with no prompt to end it, so it is
//!    refused with a recorded reason (#10129).
//! 4. **Which lines.** The run owns its transcript from the claim line up to
//!    the agent's next task: a prompt from outside, such as a coordinator's
//!    message, or a newer claim on the same transcript ([`segment`]). A claim
//!    for another issue takes the transcript over at its own line, so neither
//!    run publishes the other's lines.
//! 5. **How it ends.** The tailer is a detached `live-output-attend
//!    --foreground` process. It closes the run with `coverage = ended` at the
//!    end of its segment, when the watched session process exits, when the
//!    transcript has been idle for [`DEFAULT_IDLE_EXIT_SECS`], or at
//!    [`DEFAULT_MAX_AGE_SECS`] (the lease renewer's own cap). A foreground
//!    subagent's run also ends once its parent records its result
//!    ([`returned`], #10125).
//!
//! # Identity
//!
//! | Attribute | Attended value |
//! |---|---|
//! | `loom.session.output.launch` | `attended` (a daemon run says `daemon`) |
//! | `loom.sweep_id` | `attended-<first 8 of session id>[-<agent id>]`, a pure function of the transcript, so a restarted tailer keeps the same attempt |
//! | `loom.session_id` / content `stream_id` | `<session>` or `<session>/agent-<id>`, the same shape [`super::claude::discover`] mints |
//! | status `stream_id` | `<sweep_id>@<issue>:<claim offset>`: unique per run, so two runs on one transcript never share a status `event_id`, and the same for a restarted tailer of the same claim (#10136) |
//! | `loom.repo` | the claim checkout's `origin` remote, never the transcript's `cwd`, which is the operator session's directory and can be another repo |
//! | `loom.role` | `--role`, else the subagent's `loom-<role>` type |
//! | `loom.attempt` | absent. There is no dispatch counter to number attended runs |
//!
//! # Nothing changes when nothing is configured
//!
//! Starting is a handful of local file reads and never touches the network.
//! Its one write is the outcome line in `last-start.log` ([`upkeep`]), since
//! the claim step's stderr is discarded. It stops at the first missing piece
//! and reports which one: an agent the
//! daemon launched, observability off, live output off, no usable OTLP
//! exporter, no session id, or no identifiable transcript. Reading the
//! process tree comes last, after export is known to be on. The detached tailer
//! never shares stdio with the session, and its final drain is bounded by
//! [`FINAL_FLUSH`], so an unreachable collector cannot hold anything up.
//!
//! # No credential crosses
//!
//! The ingest key is read only inside the detached tailer, from the configured
//! key file, and is sent only as the exporter's `Authorization` header, exactly
//! as the daemon does. It is never on a command line or in a record. The
//! tool-call input and ancestor argv this module reads to find the transcript
//! are used for that match only and are never placed on a record: the records
//! come from [`super::claude::Cursor`], which drops tool input at parse time.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use chrono::Utc;
use serde_json::Value;

#[path = "attended_caller.rs"]
pub mod caller;
#[path = "attended_return.rs"]
pub mod returned;
#[path = "attended_segment.rs"]
pub mod segment;
#[path = "attended_turn.rs"]
pub mod turn;
#[path = "attended_upkeep.rs"]
pub mod upkeep;

use caller::Caller;
use segment::{Claim, Segment};

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
    /// Byte offset of the line holding the claim call: where the run's own
    /// lines begin. `0` until a claim is located.
    pub from: u64,
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
            from: 0,
        })
    }

    /// Whether this is a top-level session's own transcript rather than a
    /// subagent's.
    #[must_use]
    pub fn is_top_level(&self) -> bool {
        self.agent_id.is_none()
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
/// `projects_dir`: the one whose pending `Bash` call is the command `caller`'s
/// ancestor shell is running.
///
/// The issue number plays no part. A sibling's running command can contain it
/// by chance, and a Doctor's claim step (`worktree.sh "$ISSUE_NUM"`) does not
/// contain it at all.
///
/// # Errors
///
/// Why no single transcript qualified. Ambiguity is an error, never a guess.
pub fn locate(projects_dir: &Path, session_id: &str, caller: &Caller) -> Result<Located, String> {
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
    let matching: Vec<(&PathBuf, u64)> = candidates
        .iter()
        .filter_map(|path| callers_pending_call(path, caller).map(|from| (path, from)))
        .collect();
    match matching.as_slice() {
        [(only, from)] => Located::from_path(only)
            .map(|located| Located {
                from: *from,
                ..located
            })
            .ok_or_else(|| "unreadable transcript path".to_string()),
        [] => Err(format!(
            "none of session {session_id}'s {} transcript(s) has a running Bash call that is \
             this process's own command",
            candidates.len()
        )),
        several => Err(format!(
            "{} transcripts in session {session_id} have a running Bash call that is this \
             process's own command; not guessing",
            several.len()
        )),
    }
}

/// [`locate`], retried briefly in case the pending call is not on disk yet.
fn locate_with_retry(
    projects_dir: &Path,
    session_id: &str,
    caller: &Caller,
) -> Result<Located, String> {
    let mut result = locate(projects_dir, session_id, caller);
    for _ in 1..LOCATE_ATTEMPTS {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(LOCATE_RETRY);
        result = locate(projects_dir, session_id, caller);
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

/// The byte offset of the line holding `path`'s pending `Bash` call that
/// `caller`'s ancestor shell is running, if its newest lines hold one. The
/// command text is read for this comparison only.
fn callers_pending_call(path: &Path, caller: &Caller) -> Option<u64> {
    let (base, tail) = read_tail(path, LOCATE_TAIL_BYTES)?;
    let mut pending: Vec<(String, String, u64)> = Vec::new();
    let mut at = 0_usize;
    while let Some(newline) = tail[at..].iter().position(|byte| *byte == b'\n') {
        let offset = base + at as u64;
        let line = &tail[at..at + newline];
        at += newline + 1;
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
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
                        pending.push((id.to_string(), command.to_string(), offset));
                    }
                }
                ("user", "tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        pending.retain(|(open, _, _)| open != id);
                    }
                }
                _ => {}
            }
        }
    }
    pending
        .iter()
        .filter(|(_, command, _)| caller.runs(command))
        .map(|(_, _, offset)| *offset)
        .max()
}

/// The last `max` bytes of `path`, starting at a line boundary, with the
/// byte offset they start at.
fn read_tail(path: &Path, max: u64) -> Option<(u64, Vec<u8>)> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buffer = Vec::new();
    file.take(max).read_to_end(&mut buffer).ok()?;
    if start == 0 {
        return Some((0, buffer));
    }
    let skip = buffer.iter().position(|byte| *byte == b'\n')? + 1;
    Some((start + skip as u64, buffer.split_off(skip)))
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
        Self::try_acquire_at(path, 2)
    }

    #[cfg_attr(not(unix), allow(unused_variables))]
    fn try_acquire_at(path: &Path, retries_left: u32) -> std::io::Result<Option<Self>> {
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
            // The path may have been removed (by a holder releasing it, or a
            // sweep) between the open and the lock. A lock on that unlinked
            // file excludes nobody, so take the one now at the path instead.
            if !same_file(&file, path) {
                drop(file);
                return if retries_left == 0 {
                    Ok(None)
                } else {
                    Self::try_acquire_at(path, retries_left - 1)
                };
            }
        }
        Ok(Some(StreamLock {
            path: path.to_path_buf(),
            _file: file,
        }))
    }

    /// [`Self::try_acquire`], waiting up to `wait` for the holder to hand the
    /// transcript over, unless `give_up` says to stop first.
    async fn acquire_within(
        path: &Path,
        wait: Duration,
        mut give_up: impl FnMut() -> bool,
    ) -> std::io::Result<Option<Self>> {
        let began = Instant::now();
        loop {
            if let Some(lock) = Self::try_acquire(path)? {
                return Ok(Some(lock));
            }
            if give_up() || began.elapsed() >= wait {
                return Ok(None);
            }
            tokio::time::sleep(HANDOVER_POLL).await;
        }
    }

    /// Remove the lock file while still holding it. A tailer that opens the
    /// path afterwards gets a fresh file, which is harmless: this one is only
    /// draining by then.
    fn release(self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether the open `file` is still the file at `path`.
#[cfg(unix)]
fn same_file(file: &std::fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(open), Ok(named)) => open.dev() == named.dev() && open.ino() == named.ino(),
        _ => false,
    }
}

/// How long a newer claim's tailer waits for the older one to finish its
/// segment and let go of the transcript. The older one notices the newer
/// claim on its next pass, so a few seconds suffice.
const HANDOVER_WAIT: Duration = Duration::from_secs(20);
const HANDOVER_POLL: Duration = Duration::from_millis(200);

/// One transcript's lock file: one tailer reads a transcript at a time.
fn lock_path(root: &Path, located: &Located) -> PathBuf {
    state_dir(root).join(format!("{}.lock", file_key(&located.stream_id)))
}

/// One transcript's claim file: which claim owns it now.
fn claim_path(root: &Path, located: &Located) -> PathBuf {
    state_dir(root).join(format!("{}.claim", file_key(&located.stream_id)))
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
    /// Byte offset of the claim line in `transcript`, where the run's own
    /// lines begin. Without it an explicit transcript is followed from its
    /// current end.
    pub from_offset: Option<u64>,
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
    /// A prompt from outside the agent (a coordinator's message, a person's
    /// prompt or interrupt) started its next task.
    NextTask,
    /// A newer claim took the transcript over at its own line.
    Superseded,
    /// The parent session recorded this foreground subagent's result.
    Returned,
}

impl EndReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::SessionExited => "the session process exited",
            EndReason::Idle => "the transcript went idle",
            EndReason::MaxAge => "the tailer reached its maximum age",
            EndReason::TranscriptGone => "the transcript disappeared",
            EndReason::NextTask => "a new prompt started the agent's next task",
            EndReason::Superseded => "a newer claim took the transcript over",
            EndReason::Returned => "the subagent returned its result to its parent",
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
    /// The caller is a top-level session whose turn no `/loom:<role>` command
    /// naming the issue opened (or whose transcript was named without a
    /// proven binding). The reason is recorded in the diagnostic (#10129).
    TopLevelSession(String),
    /// A tailer already follows this claim, or (for a tailer) the older one
    /// never let go of the transcript.
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
            Outcome::TopLevelSession(why) => format!(
                "issue #{issue}: not publishing live output, the caller is a top-level session \
                 and {why}; only a subagent, or a turn a /loom:<role> command naming the issue \
                 opened, is followed (rjwalters/loom#10129)"
            ),
            Outcome::AlreadyRunning => {
                format!("issue #{issue}: a live-output tailer already follows this claim")
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
/// network. The outcome is also recorded in `last-start.log` ([`upkeep`]).
#[must_use]
pub fn start(request: &StartRequest, env: &AttendEnv) -> Outcome {
    let projects = crate::transcript_tokens::claude_projects_dir();
    let outcome =
        start_with(request, env, projects.as_deref(), Caller::from_process, spawn_detached);
    upkeep::record_start(&request.workspace, request.issue, &outcome);
    outcome
}

/// [`start`] with the projects directory, the caller's process tree and the
/// spawn injected.
fn start_with(
    request: &StartRequest,
    env: &AttendEnv,
    projects_dir: Option<&Path>,
    caller: impl FnOnce() -> Result<Caller, String>,
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
    upkeep::sweep_stale(&state_dir(&root));
    let located = match &request.transcript {
        Some(path) => match Located::from_path(path) {
            // An explicit transcript is followed from the request's offset,
            // else from now: what it held before is not known to be this
            // issue's.
            Some(located) => Located {
                from: request
                    .from_offset
                    .unwrap_or_else(|| std::fs::metadata(path).map_or(0, |m| m.len())),
                ..located
            },
            None => return Outcome::NotLocated(format!("{} is not a transcript", path.display())),
        },
        None => {
            let Some(session_id) = env.session_id.as_deref() else {
                return Outcome::NoSession;
            };
            let Some(projects) = projects_dir else {
                return Outcome::NotLocated("no Claude projects directory".to_string());
            };
            let caller = match caller() {
                Ok(caller) => caller,
                Err(why) => return Outcome::NotLocated(why),
            };
            match locate_with_retry(projects, session_id, &caller) {
                Ok(located) => located,
                Err(why) => return Outcome::NotLocated(why),
            }
        }
    };
    if located.is_top_level() {
        // Naming a transcript carries no proof the caller is bound to it.
        if request.transcript.is_some() {
            return Outcome::TopLevelSession(
                "an explicit transcript is not bound to the caller through its parent shells"
                    .to_string(),
            );
        }
        if let Err(why) = turn::opened_by_role_command(&located.path, located.from, request.issue) {
            return Outcome::TopLevelSession(why);
        }
    }
    let claim = Claim {
        issue: request.issue,
        from: located.from,
    };
    let claim_file = claim_path(&root, &located);
    // A second start for the very same claim (a retried claim step) while its
    // tailer runs is a no-op. Any other claim is recorded as the newest, and
    // the tailer it starts takes the transcript over from the older one at
    // the new claim's line.
    //
    // The probe is held until the spawn is decided: a failed start removes the
    // lock file it created, and a started tailer takes the lock over within
    // one handover poll.
    let mut probe = None;
    if segment::read_claim(&claim_file) == Some(claim) {
        match StreamLock::try_acquire(&lock_path(&root, &located)) {
            Ok(Some(held)) => probe = Some(held),
            Ok(None) => return Outcome::AlreadyRunning,
            Err(error) => return Outcome::SpawnFailed(error.to_string()),
        }
    }
    let failed = |probe: Option<StreamLock>, why: String| {
        if let Some(probe) = probe {
            probe.release();
        }
        Outcome::SpawnFailed(why)
    };
    if let Err(error) = segment::write_claim(&claim_file, claim) {
        return failed(probe, format!("{}: {error}", claim_file.display()));
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return failed(probe, error.to_string()),
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
        .arg("--from-offset")
        .arg(located.from.to_string())
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
        Ok(pid) => {
            drop(probe);
            Outcome::Started {
                pid,
                sweep_id: located.sweep_id(),
            }
        }
        // The claim stays recorded even so: an older run on this transcript
        // still has to end at this line, because the agent has moved on.
        Err(error) => failed(probe, error.to_string()),
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

/// The stream an attended run numbers its status records on (#10136):
/// `<sweep_id>@<issue>:<claim offset>`.
///
/// `sweep_id` alone is a function of the transcript, and every run numbers its
/// status records from 0, so two runs on one transcript (a newer claim taking
/// it over, the same issue claimed again later) would reuse each other's
/// `event_id`s. The claim tells runs apart: one line holds one claim call, so
/// two runs share an offset only when they are the same claim, and the issue
/// separates claims that share an offset because none was located (an explicit
/// `--transcript` followed from its current end). It is still a pure function
/// of transcript and claim, so a restarted tailer for the same claim
/// reproduces the same ids, which de-duplication relies on.
fn status_stream(sweep_id: &str, claim: Claim) -> String {
    format!("{sweep_id}@{}:{}", claim.issue, claim.from)
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

/// What [`drive`] asks on every pass, besides the transcript itself.
struct Watch<A, C>
where
    A: FnMut() -> bool,
    C: FnMut() -> Option<u64>,
{
    /// Whether the watched session process is still running.
    session_alive: A,
    /// Where a newer claim on this transcript starts, once there is one.
    newer_claim: C,
}

/// Follow one attended run until it ends, publishing through `sink`. The
/// daemon producer's own [`super::tick`] does the reading, so an attended run
/// gets exactly the records, heartbeats and gaps a daemon run gets. The
/// [`Segment`] decides how far it may read: from the claim line to the
/// agent's next task or a newer claim.
async fn drive<A, C>(
    identity: RunIdentity,
    located: &Located,
    workspace_root: &Path,
    sink: &SessionOutputSink,
    live: ResolvedLiveOutput,
    limits: Limits,
    mut watch: Watch<A, C>,
) -> EndReason
where
    A: FnMut() -> bool,
    C: FnMut() -> Option<u64>,
{
    let started = Utc::now();
    let root_key = workspace_root.display().to_string();
    let issue = identity.issue.unwrap_or_default();
    let key = (root_key.clone(), issue);
    let status_stream = status_stream(
        identity.sweep_id.as_deref().unwrap_or(&located.sweep_id()),
        Claim {
            issue,
            from: located.from,
        },
    );
    let source = Source::Fixed {
        stream_id: located.stream_id.clone(),
        path: located.path.clone(),
        from: located.from,
        until: located.from,
    };
    let mut tracker = Tracker::default();
    let run = tracker.insert(
        &root_key,
        issue,
        Run::new(identity, workspace_root.to_path_buf(), source, status_stream, started),
    );
    sink.push(run.status(OutputCategory::Coverage, started, Coverage::Degraded, RunState::Running));

    let mut segment = Segment::starting_at(located.from);
    let mut returns = returned::ReturnWatch::attach(&located.path);
    let mut slug_cache: HashMap<String, String> = HashMap::new();
    let mut ticker = tokio::time::interval(live.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let reason = loop {
        ticker.tick().await;
        if let Some(at) = (watch.newer_claim)() {
            segment.supersede(at);
        }
        // Checked before the scan: everything the subagent wrote is on disk
        // by the time its parent holds the result, so the run reads to here.
        if returns.as_mut().is_some_and(returned::ReturnWatch::poll) {
            returns = None;
            if let Ok(meta) = std::fs::metadata(&located.path) {
                segment.returned(meta.len());
            }
        }
        // Check the new lines before the cursor may read them.
        segment.scan(&located.path);
        if let Some(Source::Fixed { until, .. }) =
            tracker.runs.get_mut(&key).map(|run| &mut run.source)
        {
            *until = segment.limit();
        }
        super::tick(&mut tracker, sink, &mut slug_cache, live).await;
        let read_to = tracker
            .runs
            .get(&key)
            .and_then(|run| run.cursors.get(&located.stream_id))
            .map(|(_, cursor)| cursor.offset());
        if let Some(reason) = read_to.and_then(|offset| segment.finished(offset)) {
            break reason;
        }
        let age = Utc::now()
            .signed_duration_since(started)
            .to_std()
            .unwrap_or_default();
        let alive = (watch.session_alive)();
        if let Some(reason) = end_reason(age, alive, idle_for(&located.path), limits) {
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
    let from = request
        .from_offset
        .unwrap_or_else(|| std::fs::metadata(&located.path).map_or(0, |m| m.len()));
    // The binding was proven by the start that spawned this tailer; the turn
    // is checked again here so no path into it follows an inline claim.
    if located.is_top_level() {
        if let Err(why) = turn::opened_by_role_command(&located.path, from, request.issue) {
            return Outcome::TopLevelSession(why);
        }
    }
    let located = Located { from, ..located };
    let claim = Claim {
        issue: request.issue,
        from,
    };
    let claim_file = claim_path(&root, &located);
    let sweep_id = located.sweep_id();
    // An older claim's tailer may still be reading this transcript. It ends
    // at this claim's line once it sees the claim file, then lets go.
    let superseded = || segment::newer_claim(&claim_file, claim).is_some();
    let lock =
        match StreamLock::acquire_within(&lock_path(&root, &located), HANDOVER_WAIT, superseded)
            .await
        {
            Ok(Some(lock)) => lock,
            Ok(None) if superseded() => {
                return Outcome::Ended {
                    sweep_id,
                    reason: EndReason::Superseded,
                }
            }
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
    // Queue files are this process's own, so a newer claim's tailer can take
    // the transcript over while this one is still draining.
    let stream_key = file_key(&located.stream_id);
    sweep_dead_queues(&state_dir(&root), &stream_key);
    let queue_key = format!("{stream_key}.{}", std::process::id());
    let Some((sink, queue_files)) = build_sink(&plan, &state_dir(&root), &queue_key, &ingest_key)
    else {
        lock.release();
        return Outcome::NotConfigured("no OTLP exporter could be constructed".to_string());
    };
    drop(ingest_key);
    let repo = crate::forge_etag_store::remote_identity(&root).map(|(_host, slug)| slug);
    let identity = identity(&located, request.issue, request.role.clone(), repo);
    let limits = Limits {
        max_age: Duration::from_secs(request.max_age_secs),
        idle_exit: Duration::from_secs(request.idle_exit_secs),
    };
    let watch_pid = request.watch_pid;
    let watch_since = Utc::now();
    let watch = Watch {
        session_alive: move || {
            watch_pid.is_none_or(|pid| {
                crate::sweep_registry::reaper::pid_identity::pid_alive_since(pid, watch_since)
            })
        },
        newer_claim: || segment::newer_claim(&claim_file, claim),
    };
    let reason = drive(identity, &located, &root, &sink, plan.live, limits, watch).await;
    lock.release();
    segment::clear_claim(&claim_file, claim);
    super::super::shutdown::flush_before_shutdown(FINAL_FLUSH).await;
    // A tailer has no next boot to drain a backlog on, so an undelivered tail
    // is dropped with its queue rather than left to accumulate.
    for file in queue_files {
        let _ = std::fs::remove_file(file);
    }
    Outcome::Ended { sweep_id, reason }
}

/// Remove the queue files that earlier tailers of this transcript left
/// behind when they died before their own cleanup. A file whose tailer is
/// still running (one that let go of the transcript and is draining) stays.
fn sweep_dead_queues(dir: &Path, stream_key: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{stream_key}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some((pid, rest)) = name
            .to_str()
            .and_then(|name| name.strip_prefix(&prefix))
            .and_then(|rest| rest.split_once('.'))
        else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if rest.starts_with("otlp-")
            && pid != std::process::id()
            && !crate::live_claim::pid_is_live_process(pid)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
