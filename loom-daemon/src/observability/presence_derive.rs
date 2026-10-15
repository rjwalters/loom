//! Daemon-side derivation of `operator.presence` and in-session
//! `sweep.started` / `sweep.outcome` (#12121, part of #11821).
//!
//! Until #12121 these facts existed only if a Claude Code hook (or an agent)
//! ran `loom-daemon presence …`, which broke the #11821 principle that
//! telemetry never depends on an agent or a hook running a CLI command. The
//! collector now derives them from state the daemon host already has:
//!
//! - **Presence** comes from the operator's session transcripts
//!   (`~/.claude/projects/**/*.jsonl`): a session is *interactive* when its
//!   records carry `entrypoint: "cli"` (a dispatched `claude -p` is
//!   `sdk-cli`), and a human typed when a non-sidechain, non-meta `user`
//!   record has text rather than tool results. Only the session id and
//!   timestamps are kept — never prompt text. [`OperatorPresenceRecord`] has no
//!   field for it, and a test pins that a distinctive prompt cannot reach the
//!   serialized record.
//! - **In-session sweeps** come from the lease renewer's ownership records
//!   (`~/.loom/lease-renew/*.owner`, written by `sweep-lease-renew.sh start`
//!   through `lease renewer claim`): one per lease, carrying the lease's own
//!   sweep id. A live record is `sweep.started`; a record that is released,
//!   gone or whose owner died ends the sweep. The result is `success` only
//!   when the sweep's checkpoint (`.loom/sweep-checkpoint/issue-N.json`) was
//!   seen and has since been deleted (the merge path), carrying the PR it
//!   named; any other end is `failure`.
//! - **No double emission.** A sweep id the daemon's own registry tracks is a
//!   dispatched sweep with its own emitter and is skipped.
//!
//! Scope: this only covers hosts where the daemon runs and can read the
//! session's transcript directory. A daemon-less operator host emits
//! nothing (the old hook + one-shot flush path was removed, not kept as a
//! second writer). The privacy off-switch is `LOOM_OPERATOR_PRESENCE=0` or
//! `~/.loom/operator-presence.optout` ([`enabled`]).
//!
//! Limits worth knowing: the tick is [`TICK`] (60 s), so a sweep that starts
//! and ends between two ticks is seen only as an already-ended record and
//! reported as `failure` unless its checkpoint was caught; and
//! `started_at` for a sweep first seen live is the owner record's mtime.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::telemetry::kinds::operator_presence::{DrivenItem, OperatorPresenceRecord};
use crate::telemetry::{
    SweepResult, SweepStartFacts, SweepStartedRecord, TelemetryEnvelope, TelemetryRecord,
};

/// How often the collector runs [`Deriver::tick`].
pub const TICK: Duration = Duration::from_secs(60);

/// A session is "active" when its last record is this recent, and an active
/// session re-emits at most this often unless the human typed again.
const THROTTLE_SECS: i64 = 300;

/// Bytes read from the end of a transcript; a prompt older than this is
/// reported as unknown `last_input_at`, never guessed.
const TAIL_BYTES: u64 = 256 * 1024;

/// An unseen *ended* owner record older than this is history, not news.
const STALE_RECORD_SECS: i64 = 3600;

/// The origin every record produced here carries.
const OPERATOR_ORIGIN: &str = "operator";

/// Env switch: `0` / `false` turns derivation off.
pub const ENABLE_ENV: &str = "LOOM_OPERATOR_PRESENCE";
/// Overrides the opt-out file path.
pub const OPTOUT_FILE_ENV: &str = "LOOM_PRESENCE_OPTOUT_FILE";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

/// Derivation is on unless the operator opted out (env `0`/`false`, or the
/// opt-out file exists). Read every tick so an opt-out takes effect live.
#[must_use]
pub fn enabled() -> bool {
    let file = std::env::var(OPTOUT_FILE_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map_or_else(|| home().join(".loom/operator-presence.optout"), PathBuf::from);
    enabled_with(std::env::var(ENABLE_ENV).ok().as_deref(), file.exists())
}

fn enabled_with(env: Option<&str>, optout_file_exists: bool) -> bool {
    match env.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("0" | "false" | "off" | "no") => false,
        Some("1" | "true" | "on" | "yes") => true,
        _ => !optout_file_exists,
    }
}

// ---------------------------------------------------------------------------
// Presence: session transcripts
// ---------------------------------------------------------------------------

/// What one interactive session's transcript tail says. Ids and timestamps
/// only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFacts {
    pub session_id: String,
    /// The last human prompt in the tail; `None` when the tail holds none.
    pub last_input_at: Option<DateTime<Utc>>,
    /// The last record of any kind.
    pub last_activity_at: DateTime<Utc>,
}

fn parse_ts(obj: &Value) -> Option<DateTime<Utc>> {
    obj.get("timestamp")
        .and_then(Value::as_str)
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// A `user` record a human typed: not meta, not a subagent turn, and text
/// rather than tool results.
fn is_human_prompt(obj: &Value) -> bool {
    if obj.get("type").and_then(Value::as_str) != Some("user")
        || obj.get("isMeta").and_then(Value::as_bool) == Some(true)
        || obj.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return false;
    }
    match obj.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => !blocks
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")),
        _ => false,
    }
}

/// Read the facts out of `path`'s tail. `None` for a non-interactive session
/// (no `entrypoint: "cli"` record, e.g. a dispatched `sdk-cli` run), an
/// unreadable file, or a tail with no timestamp.
#[must_use]
pub fn session_facts(path: &Path) -> Option<SessionFacts> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if start > 0 {
        lines.next(); // a partial first line
    }
    let (mut interactive, mut session, mut input, mut activity) = (false, None, None, None);
    for line in lines {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match obj.get("entrypoint").and_then(Value::as_str) {
            Some("cli") => interactive = true,
            Some(_) => return None,
            None => {}
        }
        if session.is_none() {
            session = obj
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        let Some(at) = parse_ts(&obj) else {
            continue;
        };
        activity = activity.max(Some(at));
        if is_human_prompt(&obj) {
            input = input.max(Some(at));
        }
    }
    if !interactive {
        return None;
    }
    let session_id = session.or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
    })?;
    Some(SessionFacts {
        session_id: sanitize_session(&session_id),
        last_input_at: input,
        last_activity_at: activity?,
    })
}

fn sanitize_session(raw: &str) -> String {
    let s: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(80)
        .collect();
    if s.is_empty() {
        "unknown".into()
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// In-session sweeps: the lease renewer's ownership records
// ---------------------------------------------------------------------------

/// The fields of `cli::lease_renewer::OwnerRecord` this module reads.
#[derive(Debug, Clone, Deserialize)]
pub struct Owner {
    pub repo: String,
    pub sweep: String,
    pub issue: u64,
    pub pid: u32,
    #[serde(default)]
    pub released: bool,
    /// The record file's mtime, filled by [`read_owners`].
    #[serde(skip)]
    pub mtime: Option<DateTime<Utc>>,
}

/// Where the renewer keeps its records (same override as the CLI).
#[must_use]
pub fn owner_dir() -> PathBuf {
    std::env::var("LOOM_LEASE_RENEW_STATE_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map_or_else(|| home().join(".loom/lease-renew"), PathBuf::from)
}

/// Every parseable `*.owner` record under `dir`.
#[must_use]
pub fn read_owners(dir: &Path) -> Vec<Owner> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "owner"))
        .filter_map(|p| {
            let mut owner: Owner = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
            owner.mtime = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .map(Into::into);
            Some(owner)
        })
        .collect()
}

/// Is `pid` a live process?
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only probes for existence.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// What the sweep checkpoint for an issue says right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    Absent,
    Present { pr: Option<u32> },
}

/// Read `<root>/.loom/sweep-checkpoint/issue-<n>.json`.
#[must_use]
pub fn read_checkpoint(root: &Path, issue: u32) -> Checkpoint {
    let path = root.join(format!(".loom/sweep-checkpoint/issue-{issue}.json"));
    let Ok(text) = std::fs::read_to_string(path) else {
        return Checkpoint::Absent;
    };
    let pr = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v["pr_number"].as_u64())
        .and_then(|n| u32::try_from(n).ok());
    Checkpoint::Present { pr }
}

/// One in-session sweep this daemon has reported `sweep.started` for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tracked {
    pub repo: String,
    pub issue: u32,
    pub sweep_id: String,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub last_pr: Option<u32>,
    #[serde(default)]
    pub checkpoint_seen: bool,
}

/// The persisted set of reported-started, not-yet-ended sweeps, so a daemon
/// restart neither re-reports a start nor loses an end.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub sweeps: Vec<Tracked>,
}

/// Ambient inputs to [`step`].
pub struct Inputs<'a> {
    pub owners: &'a [Owner],
    /// Sweep ids the daemon's registry tracks (dispatched; not ours).
    pub dispatched: &'a HashSet<String>,
    pub now: DateTime<Utc>,
    pub is_live: &'a dyn Fn(u32) -> bool,
    /// The checkpoint for `(repo, issue)`; `Absent` when it cannot be read.
    pub checkpoint: &'a dyn Fn(&str, u32) -> Checkpoint,
}

fn outcome_record(t: &Tracked, result: SweepResult, now: DateTime<Utc>) -> Option<TelemetryRecord> {
    let mut value = serde_json::json!({
        "issue": t.issue,
        "sweep_id": t.sweep_id,
        "repo": t.repo,
        "total_duration_sec": (now - t.started_at).num_seconds().max(0),
        "result": result,
        "dispatch_origin": OPERATOR_ORIGIN,
    });
    if let (Some(map), Some(pr)) = (value.as_object_mut(), t.last_pr) {
        map.insert("pr_number".into(), pr.into());
        map.insert("disposition".into(), "landed".into());
    }
    serde_json::from_value(value)
        .ok()
        .map(TelemetryRecord::SweepOutcome)
}

/// Advance the ledger one tick, returning the `sweep.started` /
/// `sweep.outcome` records it produced.
pub fn step(ledger: &mut Ledger, inp: &Inputs<'_>) -> Vec<TelemetryRecord> {
    let mut out = Vec::new();
    let live = |o: &Owner| !o.released && (inp.is_live)(o.pid);
    for o in inp.owners {
        let Ok(issue) = u32::try_from(o.issue) else {
            continue;
        };
        if o.sweep.is_empty()
            || o.sweep.starts_with("daemon-")
            || inp.dispatched.contains(&o.sweep)
            || ledger
                .sweeps
                .iter()
                .any(|t| t.sweep_id == o.sweep && t.issue == issue)
        {
            continue;
        }
        let age = o.mtime.map_or(0, |m| (inp.now - m).num_seconds());
        if !live(o) && age > STALE_RECORD_SECS {
            continue;
        }
        let started_at = o.mtime.unwrap_or(inp.now);
        out.push(TelemetryRecord::SweepStarted(SweepStartedRecord {
            repo: o.repo.clone(),
            visibility: crate::telemetry::RepoVisibility::default(),
            issue,
            sweep_id: o.sweep.clone(),
            started_at,
            facts: SweepStartFacts {
                dispatch_origin: Some(OPERATOR_ORIGIN.to_string()),
                ..SweepStartFacts::default()
            },
            story_points: None,
        }));
        ledger.sweeps.push(Tracked {
            repo: o.repo.clone(),
            issue,
            sweep_id: o.sweep.clone(),
            started_at,
            last_pr: None,
            checkpoint_seen: false,
        });
    }
    let mut keep = Vec::new();
    for mut t in std::mem::take(&mut ledger.sweeps) {
        let owner = inp
            .owners
            .iter()
            .find(|o| o.sweep == t.sweep_id && o.issue == u64::from(t.issue));
        let cp = (inp.checkpoint)(&t.repo, t.issue);
        if let Checkpoint::Present { pr } = cp {
            t.checkpoint_seen = true;
            t.last_pr = pr.or(t.last_pr);
        }
        if owner.is_some_and(live) {
            keep.push(t);
            continue;
        }
        let result = if cp == Checkpoint::Absent && t.checkpoint_seen {
            SweepResult::Success
        } else {
            SweepResult::Failure
        };
        out.extend(outcome_record(&t, result, inp.now));
    }
    ledger.sweeps = keep;
    out
}

// ---------------------------------------------------------------------------
// The per-host deriver the collector ticks
// ---------------------------------------------------------------------------

/// Everything one collector tick needs besides time.
pub struct Deriver {
    pub host: String,
    pub operator: Option<String>,
    pub projects_dir: PathBuf,
    pub owner_dir: PathBuf,
    pub ledger_path: PathBuf,
    ledger: Ledger,
    /// Per session: `(last emit, last_input_at it carried)`.
    emitted: HashMap<String, (DateTime<Utc>, Option<DateTime<Utc>>)>,
}

impl Deriver {
    /// Production wiring: this host's identity, the user's transcripts and
    /// renewer records, and a ledger under the workspace's `.loom/`.
    #[must_use]
    pub fn new(host: String, workspace_root: &Path) -> Self {
        let ledger_path = workspace_root.join(".loom/presence/derived-sweeps.json");
        Self {
            host,
            operator: std::env::var("LOOM_OPERATOR")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            projects_dir: crate::transcript_tokens::claude_projects_dir()
                .unwrap_or_else(|| PathBuf::from("projects")),
            owner_dir: owner_dir(),
            ledger: std::fs::read_to_string(&ledger_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
            ledger_path,
            emitted: HashMap::new(),
        }
    }

    fn save_ledger(&self) {
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.ledger_path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self
                .ledger_path
                .with_extension(format!("tmp{}", std::process::id()));
            std::fs::write(&tmp, serde_json::to_vec(&self.ledger)?)?;
            std::fs::rename(&tmp, &self.ledger_path)
        };
        if let Err(e) = write() {
            log::warn!("presence-derive: could not persist the sweep ledger: {e}");
        }
    }

    fn sessions(&self, now: DateTime<Utc>) -> Vec<SessionFacts> {
        let cutoff = now - chrono::Duration::seconds(THROTTLE_SECS);
        crate::activity::transcript_ingest::collect_transcripts(&self.projects_dir, None)
            .into_iter()
            .filter(|p| !p.components().any(|c| c.as_os_str() == "subagents"))
            .filter(|p| {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .is_ok_and(|m| DateTime::<Utc>::from(m) >= cutoff)
            })
            .filter_map(|p| session_facts(&p))
            .collect()
    }

    /// One tick: the records to enqueue. `workspace` is this workspace's
    /// `(slug, root)`, the only checkpoints the daemon can read.
    pub fn tick(
        &mut self,
        now: DateTime<Utc>,
        dispatched: &HashSet<String>,
        workspace: Option<(&str, &Path)>,
    ) -> Vec<TelemetryEnvelope> {
        let owners = read_owners(&self.owner_dir);
        let checkpoint = |repo: &str, issue: u32| match workspace {
            Some((slug, root)) if slug.eq_ignore_ascii_case(repo) => read_checkpoint(root, issue),
            _ => Checkpoint::Absent,
        };
        let before = self.ledger.clone();
        let mut records = step(
            &mut self.ledger,
            &Inputs {
                owners: &owners,
                dispatched,
                now,
                is_live: &pid_alive,
                checkpoint: &checkpoint,
            },
        );
        if self.ledger != before {
            self.save_ledger();
        }
        let mut sessions = self.sessions(now);
        sessions.sort_by_key(|s| std::cmp::Reverse(s.last_activity_at));
        let driving: Vec<DrivenItem> = self
            .ledger
            .sweeps
            .iter()
            .map(|t| DrivenItem {
                kind: "issue".into(),
                number: t.issue,
                sweep_id: Some(t.sweep_id.clone()),
            })
            .collect();
        for (i, s) in sessions.iter().enumerate() {
            let due = self.emitted.get(&s.session_id).is_none_or(|(at, input)| {
                (now - *at).num_seconds() >= THROTTLE_SECS || s.last_input_at > *input
            });
            if !due {
                continue;
            }
            self.emitted
                .insert(s.session_id.clone(), (now, s.last_input_at));
            records.push(TelemetryRecord::OperatorPresence(OperatorPresenceRecord {
                captured_at: now,
                host: self.host.clone(),
                session_id: s.session_id.clone(),
                operator: self.operator.clone(),
                last_input_at: s.last_input_at,
                last_activity_at: s.last_activity_at,
                // The renewer records carry no session id, so the leases
                // ride on the most recently active session.
                driving: if i == 0 { driving.clone() } else { Vec::new() },
            }));
        }
        self.emitted
            .retain(|_, (at, _)| (now - *at).num_seconds() < 4 * THROTTLE_SECS);
        records
            .into_iter()
            .map(|r| {
                let mut e = TelemetryEnvelope::new(self.host.clone(), r);
                e.emitted_at = now;
                e
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
