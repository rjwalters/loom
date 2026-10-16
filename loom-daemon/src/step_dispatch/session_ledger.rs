//! The step session ledger and soft host affinity (#11913; design
//! `docs/design/work-finder-step-dispatch.md` §"Sessions").
//!
//! Every step the item-first engine spawns starts a fresh session, so a Judge
//! re-review or a Doctor pass re-reads context the previous step on that item
//! already paid for. This ledger remembers, per `(item, role)`, the last
//! session that step ran, so a later step on the same item may `--resume` it
//! (the launch mode of `roll_pause::resume`).
//!
//! * **Host-local.** The ledger is one JSON file under the workspace's
//!   `.loom/`; a session store is only reachable from the host that ran it, so
//!   a fleet-wide ledger would add nothing. A host never learns of another
//!   host's sessions.
//! * **Written at the step end** ([`record_ended`], called from
//!   `step_end::settle`) and **read at dispatch** ([`decide`], called from
//!   `step_spawn::spawn_prepared_step`).
//! * **A preference, never eligibility.** [`decide`] only ever answers
//!   "resume this session" or "run fresh"; nothing here removes an item from
//!   the draw. A holder that is gone, a record past its TTL or over the
//!   context cap, or a store that is unreachable all mean a fresh step. A
//!   fresh step on an item that had a record carries a written handoff
//!   ([`handoff`]), a summary and never the session.
//!   [`affinity_wait`] is the grace window for a record held by another host
//!   (one a shared workspace mount could show): past it, the step is
//!   eligible.
//! * **Roles.** [`sources`] names which earlier roles' sessions a role may
//!   resume. A Judge resumes only a Judge: it never inherits the author's
//!   context ([`Refusal::JudgeMayNotResumeAuthor`]).
//! * **Default off** ([`Config::enabled`]): `LOOM_STEP_SESSION_RESUME` or
//!   `autonomous.stepSessionResume.enabled`. Flipping the default waits on
//!   the resume-vs-fresh experiment, which is tracked outside this repo.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::step_role::StepRole;

/// Env override of [`Config::enabled`].
pub const ENABLED_ENV: &str = "LOOM_STEP_SESSION_RESUME";
/// Default grace window a step waits for the host holding its session.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(10 * 60);
/// Default time a record stays resumable after its step ended.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);
/// Default cap on the session transcript a resume will carry (bytes).
pub const DEFAULT_CONTEXT_CAP: u64 = 2 * 1024 * 1024;
/// Records kept per ledger; the oldest are dropped past it.
const MAX_RECORDS: usize = 512;

/// The ledger's knobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    pub grace: Duration,
    pub ttl: Duration,
    pub context_cap: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            grace: DEFAULT_GRACE,
            ttl: DEFAULT_TTL,
            context_cap: DEFAULT_CONTEXT_CAP,
        }
    }
}

impl Config {
    /// The config for the workspace at `root`: env over
    /// `autonomous.stepSessionResume` in `.loom/config.json` over the default
    /// (off). Keys: `enabled`, `graceSecs`, `ttlSecs`, `contextCapBytes`.
    #[must_use]
    pub fn load(root: &Path) -> Self {
        let json = std::fs::read_to_string(root.join(".loom").join("config.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        let block = json
            .as_ref()
            .and_then(|j| j.pointer("/autonomous/stepSessionResume"));
        Self::from_parts(block, std::env::var(ENABLED_ENV).ok().as_deref())
    }

    /// [`Self::load`]'s rule over an already-read block and env value. Pure.
    #[must_use]
    pub fn from_parts(block: Option<&serde_json::Value>, env_enabled: Option<&str>) -> Self {
        let mut c = Self::default();
        let secs = |k: &str| {
            block
                .and_then(|b| b.get(k))
                .and_then(serde_json::Value::as_u64)
        };
        if let Some(v) = block
            .and_then(|b| b.get("enabled"))
            .and_then(serde_json::Value::as_bool)
        {
            c.enabled = v;
        }
        if let Some(v) = env_enabled.map(str::trim).filter(|v| !v.is_empty()) {
            c.enabled = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = secs("graceSecs") {
            c.grace = Duration::from_secs(v);
        }
        if let Some(v) = secs("ttlSecs") {
            c.ttl = Duration::from_secs(v);
        }
        if let Some(v) = secs("contextCapBytes") {
            c.context_cap = v;
        }
        c
    }
}

/// One step's session, as it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub item: u32,
    pub role: StepRole,
    pub host: String,
    pub runtime: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The session store (Codex: the account's `CODEX_HOME`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_store: Option<String>,
    pub ended_at: DateTime<Utc>,
    /// Bytes of the session transcript when the step ended.
    pub context_size: u64,
}

/// The records of one host's workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub records: Vec<SessionRecord>,
}

/// Why a role may not resume a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A Judge resumes only a Judge: never the author's (Builder's or
    /// Doctor's) session, which would carry the author's reasoning into the
    /// review.
    JudgeMayNotResumeAuthor,
    /// The role is not one that resumes anything.
    RoleDoesNotResume,
}

/// The earlier roles whose session a step of `role` prefers, in order. Pure.
///
/// Doctor: its own last session, else the Builder's. Builder retries its own;
/// Judge only its own. Curator and Champion steps are short and resume
/// nothing.
#[must_use]
pub fn sources(role: StepRole) -> &'static [StepRole] {
    match role {
        StepRole::Judge => &[StepRole::Judge],
        StepRole::Builder => &[StepRole::Builder],
        StepRole::Doctor => &[StepRole::Doctor, StepRole::Builder],
        StepRole::Curator | StepRole::ChampionPromote | StepRole::ChampionMerge => &[],
    }
}

/// Whether a step of `into` may resume a session that `from` ran. Pure.
///
/// # Errors
/// [`Refusal`] when it may not.
pub fn may_resume(from: StepRole, into: StepRole) -> Result<(), Refusal> {
    if into == StepRole::Judge && from != StepRole::Judge {
        return Err(Refusal::JudgeMayNotResumeAuthor);
    }
    if sources(into).contains(&from) {
        Ok(())
    } else {
        Err(Refusal::RoleDoesNotResume)
    }
}

/// Why a step runs fresh although the ledger was consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshReason {
    Disabled,
    NoRecord,
    OtherHost,
    Expired,
    OverContextCap,
    StoreUnreachable,
}

impl FreshReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoRecord => "no_record",
            Self::OtherHost => "other_host",
            Self::Expired => "expired",
            Self::OverContextCap => "over_context_cap",
            Self::StoreUnreachable => "store_unreachable",
        }
    }
}

/// What a dispatch does with the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// `--resume` this session.
    Resume(SessionRecord),
    /// Run fresh; `handoff` summarises the earlier session when one existed.
    Fresh {
        reason: FreshReason,
        handoff: Option<String>,
    },
}

/// Decide how a step of `role` on `item` launches. `store_reachable` answers
/// whether a record's session store can be read (`session_store_reachable`);
/// it is only called for a record that passed every other check. Pure.
#[must_use]
pub fn decide(
    ledger: &Ledger,
    item: u32,
    role: StepRole,
    now: DateTime<Utc>,
    local_host: &str,
    cfg: &Config,
    store_reachable: &dyn Fn(&SessionRecord) -> bool,
) -> Decision {
    let fresh = |reason, held: Option<&SessionRecord>| Decision::Fresh {
        reason,
        handoff: held.map(|r| handoff(r, role)),
    };
    if !cfg.enabled {
        return fresh(FreshReason::Disabled, None);
    }
    let Some(record) = newest(ledger, item, role) else {
        return fresh(FreshReason::NoRecord, None);
    };
    if record.host != local_host {
        return fresh(FreshReason::OtherHost, Some(record));
    }
    let age = (now - record.ended_at).to_std().unwrap_or(Duration::ZERO);
    if age > cfg.ttl {
        return fresh(FreshReason::Expired, Some(record));
    }
    if record.context_size > cfg.context_cap {
        return fresh(FreshReason::OverContextCap, Some(record));
    }
    if !store_reachable(record) {
        return fresh(FreshReason::StoreUnreachable, Some(record));
    }
    Decision::Resume(record.clone())
}

/// The newest record on `item` that `role` may resume: the first of
/// [`sources`] with a record wins, so a Doctor prefers its own last session.
fn newest(ledger: &Ledger, item: u32, role: StepRole) -> Option<&SessionRecord> {
    sources(role).iter().find_map(|from| {
        ledger
            .records
            .iter()
            .filter(|r| r.item == item && r.role == *from && may_resume(r.role, role).is_ok())
            .max_by_key(|r| r.ended_at)
    })
}

/// How long a step must still wait for the host holding its session: nonzero
/// only for a record another host holds, inside the grace window. Past it the
/// step is eligible and runs fresh, so an unavailable holder never blocks an
/// item. Pure.
#[must_use]
pub fn affinity_wait(
    record: &SessionRecord,
    now: DateTime<Utc>,
    local_host: &str,
    cfg: &Config,
) -> Duration {
    if !cfg.enabled || record.host == local_host {
        return Duration::ZERO;
    }
    let age = (now - record.ended_at).to_std().unwrap_or(Duration::ZERO);
    cfg.grace.saturating_sub(age)
}

/// The written handoff a fresh step receives in place of a session. A
/// summary of what ran, never the session itself.
#[must_use]
pub fn handoff(record: &SessionRecord, into: StepRole) -> String {
    format!(
        "Loom handoff: an earlier {} step on #{} ended at {} (session not resumed). \
You are a fresh {into} step: re-read the item's current state from the forge rather than \
trusting memory of the earlier pass.",
        record.role,
        record.item,
        record.ended_at.to_rfc3339()
    )
}

/// The prompt a resumed step receives (the roll prompt would be wrong: no
/// roll paused it).
#[must_use]
pub fn resume_prompt(record: &SessionRecord, role: StepRole) -> String {
    format!(
        "Loom: you are resumed as the next {role} step on #{} (your earlier {} step ended at {}). \
The item may have changed since: re-read its labels, comments and diff from the forge, then \
do this step's work.",
        record.item,
        record.role,
        record.ended_at.to_rfc3339()
    )
}

impl Ledger {
    /// Add `record`, replacing the same `(item, role)`'s earlier one.
    pub fn upsert(&mut self, record: SessionRecord) {
        self.records
            .retain(|r| !(r.item == record.item && r.role == record.role));
        self.records.push(record);
        if self.records.len() > MAX_RECORDS {
            let drop = self.records.len() - MAX_RECORDS;
            self.records.sort_by_key(|r| r.ended_at);
            self.records.drain(..drop);
        }
    }

    /// Drop records older than `ttl` at `now`.
    pub fn prune(&mut self, now: DateTime<Utc>, ttl: Duration) {
        self.records
            .retain(|r| (now - r.ended_at).to_std().map_or(true, |age| age <= ttl));
    }
}

/// Where the workspace's ledger lives.
#[must_use]
pub fn path_for(root: &Path) -> PathBuf {
    root.join(".loom").join("step-sessions.json")
}

/// Read the ledger; an absent or unreadable file is an empty one.
#[must_use]
pub fn load(root: &Path) -> Ledger {
    std::fs::read_to_string(path_for(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Write the ledger atomically (temp file, then rename).
///
/// # Errors
/// The filesystem's.
pub fn save(root: &Path, ledger: &Ledger) -> std::io::Result<()> {
    let path = path_for(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(ledger).map_err(std::io::Error::other)?)?;
    std::fs::rename(&tmp, &path)
}

/// The Claude session a spawned step runs, noted at spawn and taken at its
/// end. Only Claude sessions are pinned by id at launch, so only they are
/// ledgered; a Codex id appears after launch, in a handle file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotedSession {
    pub session_id: String,
}

type Noted = std::collections::HashMap<(PathBuf, String), NotedSession>;

fn noted() -> &'static std::sync::Mutex<Noted> {
    static NOTED: std::sync::OnceLock<std::sync::Mutex<Noted>> = std::sync::OnceLock::new();
    NOTED.get_or_init(Default::default)
}

/// Note the session `sweep_id` runs (called at spawn).
pub fn note_session(root: &Path, sweep_id: &str, session: NotedSession) {
    let mut n = noted()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if n.len() >= 1024 {
        n.clear();
    }
    n.insert((root.to_path_buf(), sweep_id.to_string()), session);
}

/// The session noted for `sweep_id`, removed.
#[must_use]
pub fn take_session(root: &Path, sweep_id: &str) -> Option<NotedSession> {
    noted()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(root.to_path_buf(), sweep_id.to_string()))
}

/// Bytes of `session_id`'s Claude transcript under `config_dir`'s `projects`.
#[must_use]
pub fn claude_transcript_size(config_dir: &Path, session_id: &str) -> u64 {
    let name = format!("{session_id}.jsonl");
    std::fs::read_dir(config_dir.join("projects"))
        .map(|dirs| {
            dirs.filter_map(Result::ok)
                .filter_map(|d| std::fs::metadata(d.path().join(&name)).ok())
                .map(|m| m.len())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// Record the session of a step that ended (`step_end::settle`). A no-op when
/// the feature is off or the role resumes nothing. Best effort: a failed
/// write only costs a fresh step later.
pub fn record_ended(
    root: &Path,
    item: u32,
    role: StepRole,
    noted: &NotedSession,
    ended_at: DateTime<Utc>,
) {
    let cfg = Config::load(root);
    if !cfg.enabled || sources(role).is_empty() {
        return;
    }
    let session_id = noted.session_id.clone();
    if !crate::roll_pause::resume::valid_session_id(&session_id) {
        return;
    }
    let context_size = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
        .map_or(0, |d| claude_transcript_size(&d, &session_id));
    let mut ledger = load(root);
    ledger.prune(ended_at, cfg.ttl);
    ledger.upsert(SessionRecord {
        item,
        role,
        host: crate::host_identity::lease_host_identity(),
        runtime: "claude".to_string(),
        session_id,
        account: None,
        session_store: None,
        ended_at,
        context_size,
    });
    if let Err(e) = save(root, &ledger) {
        log::warn!("step_dispatch: session ledger not written: {e}");
    }
}

/// [`decide`] for the workspace at `root`, against the real ledger, host and
/// session stores.
#[must_use]
pub fn decide_for(root: &Path, item: u32, role: StepRole) -> Decision {
    let cfg = Config::load(root);
    if !cfg.enabled || sources(role).is_empty() {
        return Decision::Fresh {
            reason: FreshReason::Disabled,
            handoff: None,
        };
    }
    decide(
        &load(root),
        item,
        role,
        Utc::now(),
        &crate::host_identity::lease_host_identity(),
        &cfg,
        &|r| {
            crate::roll_pause::resume::session_store_reachable(
                &r.runtime,
                &r.session_id,
                r.session_store.as_deref(),
            )
            .is_ok()
        },
    )
}

#[cfg(test)]
#[path = "session_ledger_tests.rs"]
mod tests;
