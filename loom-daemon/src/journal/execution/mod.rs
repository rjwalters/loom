//! J1, the execution journal: shadow appends from the daemon's execution
//! producers (Epic #9908, Phase 2b, #11346).
//!
//! `docs/design/persistence-inventory.md` §9.2 / §10 / §11. Every append here
//! is a **shadow** of a legacy write that has already succeeded:
//!
//! | Legacy store | Producer | Kinds |
//! |---|---|---|
//! | E1 `~/.loom/sweeps.json` | `sweep_journal::record_sweep_at` / `remove_sweep_at` | `claim.acquired`, `claim.released` |
//! | E2 `.loom/locks/{issue,pr}-N/owner.json` | `SweepRegistry` lock writes | `claim.acquired`, `claim.child_recorded`, `claim.released` |
//! | E8 `.loom/logs/sweep-outcomes.jsonl` | `sweep_outcomes::append_outcome` | `sweep.terminal` |
//! | E9 PR-less sidecars | `prless_retry::durable` | `prless.cleared`, `prless.counted` |
//! | E10 `.loom/metrics/recovery-events.json` | `validate_phase` | `recovery.event` |
//! | E16 `# LOOM_LAUNCH` credential attribution | `worker_spawn::run` | `claim.child_recorded` (`credential`) |
//!
//! | E3 `.loom/sweep-checkpoint/issue-N.json` | CLI `sweep-checkpoint write` | `checkpoint.completed` |
//!
//! # Contract
//!
//! - **Default off.** Nothing is written unless [`ENABLE_ENV`] is set to an
//!   enabling value (`shadow`, `1`, `true`, `on`, `yes`). Disabled is exactly
//!   the pre-2b behaviour: no file, no directory, no extra I/O.
//! - **Legacy first, legacy authoritative.** Callers append only after their
//!   legacy write succeeded. [`shadow`] never returns an error and never
//!   panics into the caller: a journal failure is logged (bounded) and
//!   counted, and the legacy operation's result is untouched.
//! - **Machine-level location (Q1).** `$LOOM_JOURNAL_DIR/execution/`, default
//!   `~/.loom/journal/execution/`. A root inside a git repository is refused
//!   (journals stay outside repositories).
//! - **Divergence detection (§10.2).** With [`VERIFY_ENV`] on (the default
//!   when enabled), every append is followed by a projection of the touched
//!   key, rebuilt from the journal itself, compared with the legacy value just
//!   written after normalisation. A mismatch increments
//!   [`divergence_count`] (`loom.persistence.divergence`), logs one WARN per
//!   `(store, item)` (bounded) and appends a durable `journal.divergence`
//!   record.
//! - **Never liveness.** Nothing here answers "is this sweep alive". Readers
//!   keep the PID + start-time probe and the forge lease (ADR-0025); a
//!   journal record is evidence of what was written, nothing more.
//! - **No credential values.** Producers pass account *names* only, and every
//!   record is scrubbed ([`scrub`]) before it is encoded.

mod concern;
mod producers;
mod projection;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests_producers;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};

use super::{Journal, JournalError, JournalOptions};

pub use concern::{verify_concern, ConcernReport, StoreSummary, UnexplainedDivergence};
pub use producers::*;
pub use projection::{diff_patch, merge_patch, normalize, Entry, Projection};

/// Enables the shadow journal (`shadow`, `1`, `true`, `on`, `yes`).
pub const ENABLE_ENV: &str = "LOOM_EXECUTION_JOURNAL";
/// The journal root override; the execution stream lives in `<root>/execution`.
pub const DIR_ENV: &str = "LOOM_JOURNAL_DIR";
/// Post-append divergence check (`persistence.shadowVerify`); on unless set
/// to `0`/`false`/`off`/`no`.
pub const VERIFY_ENV: &str = "LOOM_JOURNAL_SHADOW_VERIFY";
/// The J1 stream name.
pub const STREAM: &str = "execution";
/// The divergence counter's documented metric name.
pub const DIVERGENCE_METRIC: &str = "loom.persistence.divergence";
/// Distinct `(store, item)` pairs that get their own WARN before warnings
/// are suppressed (the bound on divergence logging).
pub const MAX_WARNED_KEYS: usize = 256;

/// Resolved shadow settings. `None` from [`Settings::resolve`] means disabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Journal root; records go to `<root>/execution/`.
    pub root: PathBuf,
    /// Run the post-append projection comparison.
    pub verify: bool,
    pub options: JournalOptions,
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "shadow" | "1" | "true" | "on" | "yes"
    )
}

fn falsy(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
}

impl Settings {
    /// Resolve from a variable lookup and a home directory. Pure (test seam).
    pub fn resolve(get: impl Fn(&str) -> Option<String>, home: Option<&Path>) -> Option<Settings> {
        if !get(ENABLE_ENV).is_some_and(|v| truthy(&v)) {
            return None;
        }
        let root = default_root(&get, home)?;
        if let Some(repo) = enclosing_repository(&root) {
            warn_once(
                "root-in-repo",
                &format!(
                    "execution journal: refusing root {} inside repository {} — journals are \
                     machine state and stay outside repositories; shadow writes disabled",
                    root.display(),
                    repo.display()
                ),
            );
            return None;
        }
        let verify = !get(VERIFY_ENV).is_some_and(|v| falsy(&v));
        Some(Settings {
            root,
            verify,
            options: JournalOptions::default(),
        })
    }

    /// Resolve from the process environment.
    #[must_use]
    pub fn from_env() -> Option<Settings> {
        Settings::resolve(|k| std::env::var(k).ok(), dirs::home_dir().as_deref())
    }

    /// The journal handle for this root.
    #[must_use]
    pub fn journal(&self) -> Journal {
        Journal::with_options(self.root.clone(), self.options)
    }
}

/// `$LOOM_JOURNAL_DIR` (non-empty), else `~/.loom/journal`.
pub fn default_root(get: &impl Fn(&str) -> Option<String>, home: Option<&Path>) -> Option<PathBuf> {
    match get(DIR_ENV).filter(|v| !v.trim().is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => home.map(|h| h.join(".loom").join("journal")),
    }
}

/// The nearest ancestor of `path` (itself included) holding a `.git` entry.
fn enclosing_repository(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
thread_local! {
    /// Per-thread override so unit tests never touch the process environment
    /// (and never see another test's journal). `Some(None)` forces disabled.
    static TEST_SETTINGS: std::cell::RefCell<Option<Option<Settings>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_test_settings(settings: Option<Option<Settings>>) {
    TEST_SETTINGS.with(|s| *s.borrow_mut() = settings);
}

/// The settings in force for this call: the env, or a unit test's override.
#[must_use]
pub fn current_settings() -> Option<Settings> {
    #[cfg(test)]
    {
        if let Some(forced) = TEST_SETTINGS.with(|s| s.borrow().clone()) {
            return forced;
        }
    }
    Settings::from_env()
}

/// J1 record kinds (§9.2), plus the divergence record (§10.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ClaimAcquired,
    ClaimChildRecorded,
    ClaimReleased,
    SweepTerminal,
    PrlessCleared,
    PrlessCounted,
    RecoveryEvent,
    CheckpointCompleted,
    Divergence,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::ClaimAcquired => "claim.acquired",
            Kind::ClaimChildRecorded => "claim.child_recorded",
            Kind::ClaimReleased => "claim.released",
            Kind::SweepTerminal => "sweep.terminal",
            Kind::PrlessCleared => "prless.cleared",
            Kind::PrlessCounted => "prless.counted",
            Kind::RecoveryEvent => "recovery.event",
            Kind::CheckpointCompleted => "checkpoint.completed",
            Kind::Divergence => "journal.divergence",
        }
    }
}

/// The legacy store a record shadows. Its tag scopes projection keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Store {
    /// E1 `sweeps.json`.
    Sweeps,
    /// E2 claim-lock `owner.json`.
    Owner,
    /// E8 `sweep-outcomes.jsonl`.
    Outcomes,
    /// E9 `.prless-clears.json`.
    PrlessClears,
    /// E9 `.prless-counted.json`.
    PrlessCounted,
    /// E10 `recovery-events.json`.
    Recovery,
    /// E16 `# LOOM_LAUNCH` credential attribution in a sweep log.
    Launch,
    /// E3 `sweep-checkpoint` `issue-N.json`.
    Checkpoint,
}

impl Store {
    pub const ALL: [Store; 8] = [
        Store::Sweeps,
        Store::Owner,
        Store::Outcomes,
        Store::PrlessClears,
        Store::PrlessCounted,
        Store::Recovery,
        Store::Launch,
        Store::Checkpoint,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Store::Sweeps => "sweeps_json",
            Store::Owner => "owner_json",
            Store::Outcomes => "sweep_outcomes",
            Store::PrlessClears => "prless_clears",
            Store::PrlessCounted => "prless_counted",
            Store::Recovery => "recovery_events",
            Store::Launch => "launch_record",
            Store::Checkpoint => "sweep_checkpoint",
        }
    }

    #[must_use]
    pub fn parse(tag: &str) -> Option<Store> {
        Store::ALL.into_iter().find(|s| s.as_str() == tag)
    }
}

/// What a record says about the touched key.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// The key now holds this whole value.
    Set(Value),
    /// RFC 7386 merge patch over the key's previous value; `expected` is the
    /// full legacy value just written (compared, not journaled).
    Patch { patch: Value, expected: Value },
    /// The key was removed from the legacy store.
    Remove,
}

/// Who/what a record is about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subject {
    pub workspace: String,
    pub issue: Option<u32>,
    pub pr: Option<u32>,
    pub sweep_id: Option<String>,
}

impl Subject {
    fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("workspace".into(), Value::from(self.workspace.clone()));
        if let Some(issue) = self.issue {
            map.insert("issue".into(), Value::from(issue));
        }
        if let Some(pr) = self.pr {
            map.insert("pr".into(), Value::from(pr));
        }
        if let Some(id) = &self.sweep_id {
            map.insert("sweep_id".into(), Value::from(id.clone()));
        }
        Value::Object(map)
    }
}

/// One shadow append.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowRecord {
    pub kind: Kind,
    pub store: Store,
    /// The legacy store's path (a file, or the log for [`Store::Launch`]).
    pub path: PathBuf,
    /// The key within that store.
    pub item: String,
    pub change: Change,
    pub subject: Subject,
    /// Kind-specific §9.2 fields (`lock`, `owner_pid`, `child_pid`, …).
    pub fields: Map<String, Value>,
    /// Natural key, when the record has one.
    pub key: Option<String>,
}

/// Fields every record reserves; kind-specific `fields` never override them.
const RESERVED: [&str; 7] = ["subject", "store", "path", "item", "value", "patch", "op"];

impl ShadowRecord {
    fn data(&self) -> Value {
        let mut data = Map::new();
        for (k, v) in &self.fields {
            if !RESERVED.contains(&k.as_str()) {
                data.insert(k.clone(), v.clone());
            }
        }
        data.insert("subject".into(), self.subject.to_value());
        data.insert("store".into(), Value::from(self.store.as_str()));
        data.insert("path".into(), Value::from(self.path.display().to_string()));
        data.insert("item".into(), Value::from(self.item.clone()));
        match &self.change {
            Change::Set(value) => {
                data.insert("op".into(), Value::from("set"));
                data.insert("value".into(), value.clone());
            }
            Change::Patch { patch, .. } => {
                data.insert("op".into(), Value::from("patch"));
                data.insert("patch".into(), patch.clone());
            }
            Change::Remove => {
                data.insert("op".into(), Value::from("remove"));
            }
        }
        let mut data = Value::Object(data);
        scrub(&mut data);
        data
    }

    /// The legacy value the projection must reproduce (`None` = absent).
    fn expected(&self) -> Option<Value> {
        let mut value = match &self.change {
            Change::Set(v) => v.clone(),
            Change::Patch { expected, .. } => expected.clone(),
            Change::Remove => return None,
        };
        scrub(&mut value);
        Some(value)
    }
}

/// The post-append comparison's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Match,
    /// The projection differs; the differing top-level fields.
    Diverged(Vec<String>),
    /// A patch arrived for a key the journal has no base for (journaling was
    /// enabled while this key already existed). Explained, not a divergence.
    NoBase,
    /// The cache had already consumed past this record (a concurrent append
    /// was checked first); not comparable as-of this record.
    Skipped,
    /// Verification is off.
    Unchecked,
}

/// The result of one [`shadow`] call. Callers ignore it; tests read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowOutcome {
    Disabled,
    Appended { seq: u64, verdict: Verdict },
    Failed(String),
}

static DIVERGENCES: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);

/// Total `loom.persistence.divergence` increments in this process.
#[must_use]
pub fn divergence_count() -> u64 {
    DIVERGENCES.load(Ordering::Relaxed)
}

/// Total shadow appends that failed (busy, I/O, oversize) in this process.
#[must_use]
pub fn shadow_failure_count() -> u64 {
    FAILURES.load(Ordering::Relaxed)
}

/// Shadow one successful legacy write. Never fails the caller.
pub fn shadow(record: ShadowRecord) -> ShadowOutcome {
    match current_settings() {
        Some(settings) => shadow_with(&settings, record),
        None => ShadowOutcome::Disabled,
    }
}

/// [`shadow`] with explicit settings.
pub fn shadow_with(settings: &Settings, record: ShadowRecord) -> ShadowOutcome {
    let store = record.store;
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shadow_inner(settings, &record)));
    let error = match result {
        Ok(Ok(outcome)) => return outcome,
        Ok(Err(e)) => e.to_string(),
        Err(_) => "panic during shadow append".to_owned(),
    };
    FAILURES.fetch_add(1, Ordering::Relaxed);
    warn_once(
        &format!("failure:{}", store.as_str()),
        &format!(
            "execution journal: shadow append for {} failed ({error}); the legacy write \
             stands and stays authoritative (further failures for this store log at debug)",
            store.as_str()
        ),
    );
    log::debug!("execution journal: shadow append for {} failed: {error}", store.as_str());
    ShadowOutcome::Failed(error)
}

fn shadow_inner(settings: &Settings, record: &ShadowRecord) -> Result<ShadowOutcome, JournalError> {
    let stream = settings.journal().stream(STREAM)?;
    let appended = stream.append(record.kind.as_str(), record.key.as_deref(), record.data())?;
    if !settings.verify {
        return Ok(ShadowOutcome::Appended {
            seq: appended.seq,
            verdict: Verdict::Unchecked,
        });
    }
    let projected = projection::check(&settings.root, &stream, appended.seq, record)?;
    let verdict = projection::compare(projected, record.expected().as_ref());
    if let Verdict::Diverged(fields) = &verdict {
        on_divergence(&stream, record, appended.seq, fields);
    }
    Ok(ShadowOutcome::Appended {
        seq: appended.seq,
        verdict,
    })
}

fn on_divergence(stream: &super::Stream, record: &ShadowRecord, seq: u64, fields: &[String]) {
    DIVERGENCES.fetch_add(1, Ordering::Relaxed);
    warn_once(
        &format!("divergence:{}:{}:{}", record.store.as_str(), record.path.display(), record.item),
        &format!(
            "execution journal: {DIVERGENCE_METRIC}{{concern=execution, store={}, field={}}} — \
             projection of {} [{}] differs from the legacy value after seq {seq}",
            record.store.as_str(),
            fields.join(","),
            record.path.display(),
            record.item
        ),
    );
    let mut data = Map::new();
    data.insert("concern".into(), Value::from("execution"));
    data.insert("store".into(), Value::from(record.store.as_str()));
    data.insert("path".into(), Value::from(record.path.display().to_string()));
    data.insert("item".into(), Value::from(record.item.clone()));
    data.insert("fields".into(), Value::from(fields.to_vec()));
    data.insert("seq".into(), Value::from(seq));
    if let Err(e) = stream.append(Kind::Divergence.as_str(), None, Value::Object(data)) {
        log::debug!("execution journal: could not record journal.divergence: {e}");
    }
}

/// Log `message` at WARN once per `key`, for at most [`MAX_WARNED_KEYS`]
/// keys per process; later keys log at debug.
fn warn_once(key: &str, message: &str) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut set) = warned.lock() else {
        log::debug!("{message}");
        return;
    };
    if set.contains(key) {
        log::debug!("{message}");
    } else if set.len() < MAX_WARNED_KEYS {
        set.insert(key.to_owned());
        log::warn!("{message}");
    } else {
        log::debug!("{message}");
    }
}

/// Key-name suffixes whose string values are never journaled (`gh_token`,
/// `api_key`, …). A suffix, so `token_name` (an account name) and
/// `input_tokens` (a count) are kept.
const SENSITIVE_KEYS: [&str; 9] = [
    "token",
    "secret",
    "password",
    "passwd",
    "api_key",
    "apikey",
    "authorization",
    "cookie",
    "private_key",
];

/// Value prefixes of well-known credential formats.
const SECRET_PREFIXES: [&str; 7] = ["sk-", "ghp_", "gho_", "ghs_", "ghu_", "github_pat_", "xox"];

/// The redaction marker.
pub const REDACTED: &str = "[redacted]";

/// Replace every string that looks like a credential value — under a
/// sensitive key name (camelCase keys are matched case-insensitively), or
/// with a well-known secret prefix — by [`REDACTED`].
/// Numbers and booleans are kept (`input_tokens` is a count, not a secret).
pub fn scrub(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                let lower = key.to_ascii_lowercase().replace('-', "_");
                if v.is_string() && SENSITIVE_KEYS.iter().any(|s| lower.ends_with(s)) {
                    *v = Value::from(REDACTED);
                } else {
                    scrub(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(scrub),
        Value::String(s) if SECRET_PREFIXES.iter().any(|p| s.starts_with(p)) => {
            *s = REDACTED.to_owned();
        }
        _ => {}
    }
}

/// Run concern verification at daemon startup (§10.2 step 4), off-thread
/// and logged. A no-op when the shadow journal is disabled.
pub fn log_startup_verification() {
    let Some(settings) = current_settings() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("journal-verify".into())
        .spawn(move || {
            let report = verify_concern(&settings.root);
            if report.records == 0 && report.unexplained.is_empty() {
                log::debug!(
                    "execution journal: nothing journaled yet at {}",
                    report.root.display()
                );
            } else if report.ok {
                log::info!(
                    "execution journal: startup verify ok — {} records, {} stores compared",
                    report.records,
                    report.stores.len()
                );
            } else {
                log::warn!(
                    "execution journal: startup verify found {} unexplained divergence(s) \
                     ({} records); run `loom-daemon journal verify --concern execution`",
                    report.unexplained.len(),
                    report.records
                );
            }
        });
    if let Err(e) = spawned {
        log::debug!("execution journal: could not start startup verification: {e}");
    }
}
