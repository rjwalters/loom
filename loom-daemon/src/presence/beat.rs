//! Producing operator-session facts (#11563): the presence beat and the
//! in-session `sweep.started` / `sweep.outcome` records.
//!
//! All state is a small per-session file under `<root>/.loom/presence/`. It
//! holds timestamps and the leases this session is driving, never any hook
//! payload text.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::journal;
use crate::telemetry::kinds::operator_presence::{DrivenItem, OperatorPresenceRecord};
use crate::telemetry::{
    SweepResult, SweepStartFacts, SweepStartedRecord, TelemetryEnvelope, TelemetryRecord,
};

/// Non-prompt events emit at most once per this many seconds.
pub const THROTTLE_SECS: i64 = 300;

/// The origin every record produced here carries.
pub const OPERATOR_ORIGIN: &str = "operator";

/// The session id used when none is known.
pub const UNKNOWN_SESSION: &str = "unknown";

/// The hook event that means a human typed.
pub const PROMPT_EVENT: &str = "UserPromptSubmit";

/// What a hook payload is allowed to tell us. Everything else in the payload
/// (the prompt, tool input, the transcript path) is discarded unread: the
/// struct has no field for it, so serde drops it on parse.
#[derive(Debug, Default, Deserialize)]
pub struct HookFields {
    /// The Claude Code session id.
    #[serde(default)]
    pub session_id: Option<String>,
    /// `UserPromptSubmit`, `PostToolUse`, `Stop`, ...
    #[serde(default)]
    pub hook_event_name: Option<String>,
}

/// Parse the two allowed fields out of a hook payload; an unparseable payload
/// yields none (the caller falls back to flags and env).
#[must_use]
pub fn parse_hook_fields(stdin: &str) -> HookFields {
    serde_json::from_str(stdin).unwrap_or_default()
}

/// One lease this session drives, as kept in the state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateItem {
    /// `issue` or `pr`.
    pub kind: String,
    /// Issue or PR number.
    pub number: u32,
    /// The in-session lease's sweep id.
    pub sweep_id: String,
    /// `owner/repo`.
    pub repo: String,
    /// When the in-session sweep started.
    pub started_at: DateTime<Utc>,
}

/// The per-session state file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    /// Last `UserPromptSubmit`.
    #[serde(default)]
    pub last_input_at: Option<DateTime<Utc>>,
    /// Last event of any kind.
    #[serde(default)]
    pub last_activity_at: Option<DateTime<Utc>>,
    /// Last time a presence record was journaled.
    #[serde(default)]
    pub last_emit_at: Option<DateTime<Utc>>,
    /// Leases this session holds.
    #[serde(default)]
    pub driving: Vec<StateItem>,
}

/// Ambient inputs, injected so the logic is testable.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// Workspace root (holds `.loom/`).
    pub root: PathBuf,
    /// The journal to append to.
    pub journal: PathBuf,
    /// The session id (already sanitized by [`sanitize_session`]).
    pub session_id: String,
    /// This host's telemetry identity.
    pub host: String,
    /// The operator's forge login, when known.
    pub operator: Option<String>,
    /// The current time.
    pub now: DateTime<Utc>,
}

/// Reduce a session id to filename-safe characters.
#[must_use]
pub fn sanitize_session(raw: &str) -> String {
    let s: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(80)
        .collect();
    if s.is_empty() {
        UNKNOWN_SESSION.to_string()
    } else {
        s
    }
}

fn state_path(root: &Path, session_id: &str) -> PathBuf {
    root.join(".loom/presence")
        .join(format!("{session_id}.json"))
}

/// Load a session's state; a missing or unreadable file is a fresh session.
#[must_use]
pub fn load_state(root: &Path, session_id: &str) -> SessionState {
    std::fs::read_to_string(state_path(root, session_id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(root: &Path, session_id: &str, state: &SessionState) -> Result<()> {
    let path = state_path(root, session_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("creating presence state dir")?;
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(state)?).context("writing presence state")?;
    std::fs::rename(&tmp, &path).context("replacing presence state")
}

/// An exclusive advisory lock over every state file under
/// `<root>/.loom/presence/`, held across each load-modify-save so concurrent
/// hook processes (several sessions, or a beat racing `lease ensure`) cannot
/// lose a `last_emit_at` or `driving` update. One host-local lock for all
/// sessions: `beat` and `sweep_outcome` touch the `unknown` session's file as
/// well as their own, and every critical section is a few small file writes.
/// Released when dropped (the descriptor closes). Same pattern as
/// `fleet_captain::ShellArmLock`.
struct StateLock {
    _file: std::fs::File,
}

impl StateLock {
    fn acquire(root: &Path) -> Result<Self> {
        let dir = root.join(".loom/presence");
        std::fs::create_dir_all(&dir).context("creating presence state dir")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(".lock"))
            .context("opening presence state lock")?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: `flock` on a descriptor we own for the duration of the
            // call; no memory is shared with the kernel.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error()).context("locking presence state");
            }
        }
        Ok(Self { _file: file })
    }
}

/// What a beat did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeatOutcome {
    /// One `operator.presence` record was journaled.
    Emitted,
    /// Inside the throttle window for a non-prompt event.
    Throttled,
}

/// Apply one hook event: update the session state, and journal one
/// `operator.presence` record unless it is a non-prompt event inside the
/// throttle window.
///
/// # Errors
/// When the state or journal cannot be written.
pub fn beat(ctx: &Ctx, event: &str) -> Result<BeatOutcome> {
    let _lock = StateLock::acquire(&ctx.root)?;
    let mut state = load_state(&ctx.root, &ctx.session_id);
    // A lease taken where the session id was unknowable is filed under
    // `unknown`; it belongs to whichever session beats on this host.
    let unattributed = if ctx.session_id == UNKNOWN_SESSION {
        Vec::new()
    } else {
        load_state(&ctx.root, UNKNOWN_SESSION).driving
    };
    let is_prompt = event == PROMPT_EVENT;
    if is_prompt {
        state.last_input_at = Some(ctx.now);
    }
    state.last_activity_at = Some(ctx.now);
    let due = is_prompt
        || state
            .last_emit_at
            .is_none_or(|t| (ctx.now - t).num_seconds() >= THROTTLE_SECS);
    if due {
        let record = OperatorPresenceRecord {
            captured_at: ctx.now,
            host: ctx.host.clone(),
            session_id: ctx.session_id.clone(),
            operator: ctx.operator.clone(),
            last_input_at: state.last_input_at,
            last_activity_at: ctx.now,
            driving: state
                .driving
                .iter()
                .chain(unattributed.iter())
                .map(|i| DrivenItem {
                    kind: i.kind.clone(),
                    number: i.number,
                    sweep_id: Some(i.sweep_id.clone()),
                })
                .collect(),
        };
        journal::append(&ctx.journal, &envelope(ctx, TelemetryRecord::OperatorPresence(record)))?;
        state.last_emit_at = Some(ctx.now);
    }
    save_state(&ctx.root, &ctx.session_id, &state)?;
    Ok(if due {
        BeatOutcome::Emitted
    } else {
        BeatOutcome::Throttled
    })
}

fn envelope(ctx: &Ctx, record: TelemetryRecord) -> TelemetryEnvelope {
    let mut e = TelemetryEnvelope::new(ctx.host.clone(), record);
    e.emitted_at = ctx.now;
    e
}

/// Journal `sweep.started` (`dispatch_origin=operator`, the lease's sweep id)
/// and record the lease in the session's `driving` set.
///
/// # Errors
/// When the state or journal cannot be written.
pub fn sweep_started(ctx: &Ctx, repo: &str, kind: &str, number: u32, sweep_id: &str) -> Result<()> {
    let record = SweepStartedRecord {
        repo: repo.to_string(),
        visibility: crate::telemetry::RepoVisibility::default(),
        issue: number,
        sweep_id: sweep_id.to_string(),
        started_at: ctx.now,
        facts: SweepStartFacts {
            dispatch_origin: Some(OPERATOR_ORIGIN.to_string()),
            ..SweepStartFacts::default()
        },
        story_points: None,
    };
    journal::append(&ctx.journal, &envelope(ctx, TelemetryRecord::SweepStarted(record)))?;
    let _lock = StateLock::acquire(&ctx.root)?;
    let mut state = load_state(&ctx.root, &ctx.session_id);
    state.driving.retain(|i| i.sweep_id != sweep_id);
    state.driving.push(StateItem {
        kind: kind.to_string(),
        number,
        sweep_id: sweep_id.to_string(),
        repo: repo.to_string(),
        started_at: ctx.now,
    });
    save_state(&ctx.root, &ctx.session_id, &state)
}

/// Journal `sweep.outcome` (`dispatch_origin=operator`, the lease's sweep id)
/// and drop the lease from the `driving` set. The duration comes from the
/// recorded start when this session saw it.
///
/// A lease started where the session id was unknowable is filed under the
/// `unknown` session, and [`beat`] reports it for every session on the host.
/// So the lease is also dropped from (and its start looked up in) `unknown`'s
/// state, or it would be reported as driven forever.
///
/// # Errors
/// When the state or journal cannot be written, or the record cannot be built.
pub fn sweep_outcome(
    ctx: &Ctx,
    repo: Option<&str>,
    number: u32,
    sweep_id: &str,
    result: SweepResult,
    pr_number: Option<u32>,
) -> Result<()> {
    let _lock = StateLock::acquire(&ctx.root)?;
    outcome_locked(ctx, repo, number, sweep_id, result, pr_number)
}

/// [`sweep_outcome`]'s body; the caller holds the [`StateLock`].
fn outcome_locked(
    ctx: &Ctx,
    repo: Option<&str>,
    number: u32,
    sweep_id: &str,
    result: SweepResult,
    pr_number: Option<u32>,
) -> Result<()> {
    let mut state = load_state(&ctx.root, &ctx.session_id);
    let mut unattributed =
        (ctx.session_id != UNKNOWN_SESSION).then(|| load_state(&ctx.root, UNKNOWN_SESSION));
    let started = state
        .driving
        .iter()
        .chain(unattributed.iter().flat_map(|u| u.driving.iter()))
        .find(|i| i.sweep_id == sweep_id)
        .cloned();
    let repo = repo
        .map(str::to_string)
        .or_else(|| started.as_ref().map(|s| s.repo.clone()));
    let duration = started.map_or(0, |s| (ctx.now - s.started_at).num_seconds().max(0));
    let mut value = serde_json::json!({
        "issue": number,
        "sweep_id": sweep_id,
        "total_duration_sec": duration,
        "result": result,
        "dispatch_origin": OPERATOR_ORIGIN,
    });
    if let Some(map) = value.as_object_mut() {
        match repo {
            Some(r) => {
                map.insert("repo".into(), r.into());
            }
            None => {
                map.insert("repo_unresolved".into(), true.into());
            }
        }
        if let Some(pr) = pr_number {
            map.insert("pr_number".into(), pr.into());
            map.insert("disposition".into(), "landed".into());
        }
    }
    let record: crate::telemetry::SweepOutcomeRecord =
        serde_json::from_value(value).context("building sweep.outcome record")?;
    journal::append(&ctx.journal, &envelope(ctx, TelemetryRecord::SweepOutcome(record)))?;
    if let Some(u) = unattributed.as_mut() {
        let before = u.driving.len();
        u.driving.retain(|i| i.sweep_id != sweep_id);
        if u.driving.len() != before {
            save_state(&ctx.root, UNKNOWN_SESSION, u)?;
        }
    }
    state.driving.retain(|i| i.sweep_id != sweep_id);
    save_state(&ctx.root, &ctx.session_id, &state)
}

/// The automatic end of an in-session sweep: journal `sweep.outcome` for every
/// lease on issue `number` that any session on this host still lists as
/// driven, and stop listing it. Returns the sweep ids it finished.
///
/// Called from the code paths a sweep already runs when it ends (the
/// checkpoint delete after a merge, the renewal loop's release or its
/// issue-closed stop), so no prompt has to remember an extra step. Only a
/// sweep this host recorded as started is finished, which makes a second
/// end signal for the same sweep a no-op and keeps daemon-dispatched sweeps
/// (never in the `driving` set) out of it. `ctx.session_id` is ignored: each
/// lease is finished under the session that holds it.
///
/// # Errors
/// When the state or journal cannot be written.
pub fn finish_driven(
    ctx: &Ctx,
    number: u32,
    result: SweepResult,
    pr_number: Option<u32>,
) -> Result<Vec<String>> {
    let _lock = StateLock::acquire(&ctx.root)?;
    let mut held: Vec<(String, String)> = Vec::new();
    let dir = ctx.root.join(".loom/presence");
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(session) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        for item in load_state(&ctx.root, session).driving {
            let seen = held.iter().any(|(_, id)| *id == item.sweep_id);
            if item.kind == "issue" && item.number == number && !seen {
                held.push((session.to_string(), item.sweep_id));
            }
        }
    }
    // A named session first, so the `unknown` fallback inside
    // `outcome_locked` prunes both copies under the session that owns it.
    held.sort_by_key(|(session, _)| session == UNKNOWN_SESSION);
    for (session, sweep_id) in &held {
        let owner = Ctx {
            session_id: session.clone(),
            ..ctx.clone()
        };
        outcome_locked(&owner, None, number, sweep_id, result, pr_number)?;
    }
    Ok(held.into_iter().map(|(_, id)| id).collect())
}
