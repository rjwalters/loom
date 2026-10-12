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
    let mut state = load_state(&ctx.root, &ctx.session_id);
    let started = state
        .driving
        .iter()
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
    state.driving.retain(|i| i.sweep_id != sweep_id);
    save_state(&ctx.root, &ctx.session_id, &state)
}
