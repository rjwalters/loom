//! Record builders for each touched producer. Every function here is called
//! **after** its legacy write succeeded, returns nothing a caller must
//! handle, and is a no-op (no I/O) when the shadow journal is disabled.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use super::{current_settings, shadow, Change, Kind, ShadowOutcome, ShadowRecord, Store, Subject};

/// The E1 item key: `<repo>#<issue>`.
#[must_use]
pub fn sweeps_item(repo: &str, issue: u32) -> String {
    format!("{repo}#{issue}")
}

/// The wall-clock start of `pid`, when the platform reports it. Recorded so a
/// Phase 3 liveness projection can pair PID with start time (§10.1); it is
/// never used here to decide liveness.
fn pid_start_time(pid: u32) -> Value {
    crate::sweep_registry::reaper::pid_identity::pid_start_wallclock(pid)
        .map_or(Value::Null, |t| Value::from(t.to_rfc3339()))
}

/// E1: `sweeps.json` upserted `(repo, issue) -> {pid, started_at}`.
pub fn sweeps_upserted(
    path: &Path,
    repo: &str,
    issue: u32,
    pid: u32,
    started_at: DateTime<Utc>,
) -> ShadowOutcome {
    if current_settings().is_none() {
        return ShadowOutcome::Disabled;
    }
    let mut fields = Map::new();
    fields.insert("lock".into(), Value::from("liveness"));
    fields.insert("owner_pid".into(), Value::from(pid));
    fields.insert("pid_start_time".into(), pid_start_time(pid));
    fields.insert("provisional".into(), Value::from(false));
    shadow(ShadowRecord {
        kind: Kind::ClaimAcquired,
        store: Store::Sweeps,
        path: path.to_path_buf(),
        item: sweeps_item(repo, issue),
        change: Change::Set(json!({"pid": pid, "started_at": started_at})),
        subject: Subject {
            workspace: repo.to_owned(),
            issue: Some(issue),
            ..Subject::default()
        },
        fields,
        key: None,
    })
}

/// E1: `sweeps.json` entry for `(repo, issue)` removed.
pub fn sweeps_removed(path: &Path, repo: &str, issue: u32) -> ShadowOutcome {
    let mut fields = Map::new();
    fields.insert("lock".into(), Value::from("liveness"));
    fields.insert("outcome".into(), Value::from("released"));
    shadow(ShadowRecord {
        kind: Kind::ClaimReleased,
        store: Store::Sweeps,
        path: path.to_path_buf(),
        item: sweeps_item(repo, issue),
        change: Change::Remove,
        subject: Subject {
            workspace: repo.to_owned(),
            issue: Some(issue),
            ..Subject::default()
        },
        fields,
        key: None,
    })
}

/// E8: one terminal outcome appended to `sweep-outcomes.jsonl`.
pub fn outcome_appended(
    path: &Path,
    record: &crate::sweep_outcomes::OutcomeRecord,
) -> ShadowOutcome {
    if current_settings().is_none() {
        return ShadowOutcome::Disabled;
    }
    let value = match serde_json::to_value(record) {
        Ok(value) => value,
        Err(e) => return ShadowOutcome::Failed(e.to_string()),
    };
    let mut fields = Map::new();
    fields.insert("outcome".into(), Value::from(record.outcome.clone()));
    shadow(ShadowRecord {
        kind: Kind::SweepTerminal,
        store: Store::Outcomes,
        path: path.to_path_buf(),
        item: record.sweep_id.clone(),
        change: Change::Set(value),
        subject: Subject {
            workspace: record.repo.clone(),
            issue: Some(record.issue),
            sweep_id: Some(record.sweep_id.clone()),
            ..Subject::default()
        },
        fields,
        key: Some(format!("terminal:{}", record.sweep_id)),
    })
}

/// E9: the PR-less clear mark for `issue` was persisted at `at`.
pub fn prless_cleared(
    path: &Path,
    workspace: &Path,
    issue: u32,
    at: DateTime<Utc>,
) -> ShadowOutcome {
    shadow(ShadowRecord {
        kind: Kind::PrlessCleared,
        store: Store::PrlessClears,
        path: path.to_path_buf(),
        item: issue.to_string(),
        change: Change::Set(json!(at)),
        subject: Subject {
            workspace: workspace.display().to_string(),
            issue: Some(issue),
            ..Subject::default()
        },
        fields: Map::new(),
        key: None,
    })
}

/// E9: the PR-less tally counted `sweep_id`'s outcome at `at`.
pub fn prless_counted(
    path: &Path,
    workspace: &Path,
    sweep_id: &str,
    at: DateTime<Utc>,
) -> ShadowOutcome {
    shadow(ShadowRecord {
        kind: Kind::PrlessCounted,
        store: Store::PrlessCounted,
        path: path.to_path_buf(),
        item: sweep_id.to_owned(),
        change: Change::Set(json!(at)),
        subject: Subject {
            workspace: workspace.display().to_string(),
            sweep_id: Some(sweep_id.to_owned()),
            ..Subject::default()
        },
        fields: Map::new(),
        key: Some(format!("counted:{sweep_id}")),
    })
}

/// The E10 item key of one recovery event: `<timestamp>|<issue>|<type>`.
#[must_use]
pub fn recovery_item(event: &Value) -> String {
    let field = |k: &str| match event.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    };
    format!("{}|{}|{}", field("timestamp"), field("issue"), field("recovery_type"))
}

/// E10: `event` was appended to `recovery-events.json` at `path`.
pub fn recovery_event(repo_root: &Path, issue: i64, path: &Path, event: &Value) -> ShadowOutcome {
    shadow(ShadowRecord {
        kind: Kind::RecoveryEvent,
        store: Store::Recovery,
        path: path.to_path_buf(),
        item: recovery_item(event),
        change: Change::Set(event.clone()),
        subject: Subject {
            workspace: repo_root.display().to_string(),
            issue: u32::try_from(issue).ok(),
            ..Subject::default()
        },
        fields: Map::new(),
        key: None,
    })
}

/// The E16 item: the newest launch record of a log.
pub const LAUNCH_ITEM: &str = "last";

/// E16: a `# LOOM_LAUNCH {record_json}` line was written to `log`. The
/// attribution is parsed from the exact JSON written, by the same parser
/// the terminal-time reader uses, so only `{source, provider, account}`
/// (an account NAME, never key material) is journaled.
pub fn launch_recorded(workspace: &Path, log: &Path, record_json: &str) -> ShadowOutcome {
    if current_settings().is_none() {
        return ShadowOutcome::Disabled;
    }
    let credential = crate::launch_record::parse_launch_credential(record_json);
    let value = match credential.as_ref().map(serde_json::to_value).transpose() {
        Ok(value) => value.unwrap_or(Value::Null),
        Err(e) => return ShadowOutcome::Failed(e.to_string()),
    };
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut fields = Map::new();
    let pid = std::process::id();
    fields.insert("child_pid".into(), Value::from(pid));
    fields.insert("pid_start_time".into(), pid_start_time(pid));
    fields.insert("credential".into(), value.clone());
    shadow(ShadowRecord {
        kind: Kind::ClaimChildRecorded,
        store: Store::Launch,
        path: log.to_path_buf(),
        item: LAUNCH_ITEM.to_owned(),
        change: Change::Set(value),
        subject: Subject {
            workspace: workspace.display().to_string(),
            issue: env("LOOM_ISSUE_NUMBER").and_then(|v| v.parse().ok()),
            sweep_id: env("LOOM_SWEEP_ID"),
            ..Subject::default()
        },
        fields,
        key: None,
    })
}

/// The E3 checkpoint fields journaled: an allow-list, so the escalation state
/// and any future key never reach the journal unreviewed.
const CHECKPOINT_FIELDS: [&str; 6] = [
    "phase",
    "attempt",
    "model",
    "pr_number",
    "task_id",
    "timestamp",
];

/// The allow-listed view of a legacy checkpoint record. The same view is read
/// back from the file by concern verification.
#[must_use]
pub fn checkpoint_projection(record: &Value) -> Value {
    let mut view = Map::new();
    for key in CHECKPOINT_FIELDS {
        if let Some(v) = record.get(key).filter(|v| !v.is_null()) {
            view.insert(key.to_owned(), v.clone());
        }
    }
    Value::Object(view)
}

/// The E3 item key for a checkpoint file: its stem (`issue-<N>`).
#[must_use]
pub fn checkpoint_item(path: &Path) -> String {
    path.file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
}

/// E3: `issue-<issue>.json` was durably written with `record`.
pub fn checkpoint_written(
    path: &Path,
    workspace: &Path,
    issue: u32,
    record: &Value,
) -> ShadowOutcome {
    if current_settings().is_none() {
        return ShadowOutcome::Disabled;
    }
    let view = checkpoint_projection(record);
    let mut fields = Map::new();
    for key in ["phase", "attempt", "model", "pr_number", "task_id"] {
        if let Some(v) = view.get(key) {
            fields.insert(key.to_owned(), v.clone());
        }
    }
    shadow(ShadowRecord {
        kind: Kind::CheckpointCompleted,
        store: Store::Checkpoint,
        path: path.to_path_buf(),
        item: checkpoint_item(path),
        change: Change::Set(view),
        subject: Subject {
            workspace: workspace.display().to_string(),
            issue: Some(issue),
            pr: record["pr_number"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok()),
            sweep_id: std::env::var("LOOM_SWEEP_ID")
                .ok()
                .filter(|v| !v.is_empty()),
        },
        fields,
        key: None,
    })
}
