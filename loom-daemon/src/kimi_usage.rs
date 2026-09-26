//! Read-only per-model token accounting from Kimi Code CLI's own on-disk
//! session store (Issue #8564, extending #8507's [`crate::usage_source`] seam).
//!
//! # Why this module exists
//!
//! [`crate::transcript_tokens`] sums per-model token usage **only** from
//! Claude Code's JSONL transcripts, and [`crate::opencode_usage`] only from
//! OpenCode's SQLite session store. A sweep or role tick dispatched on the
//! Kimi runtime (#8561, `LOOM_RUNTIME=kimi`) therefore produced no
//! `tokens_by_model` at all — and downstream, no model badge and no provider
//! on the public fleet feed, the exact hole #8507 reported for the GLM-5.3
//! trial. This module is the third implementation behind that seam.
//!
//! # Where the numbers actually are (provenance)
//!
//! Established on **2026-09-22** by reading the shipped bundle of
//! `@moonshot-ai/kimi-code@2.0.2` (`npm pack`, `dist/main.mjs`) — the same
//! pinned CLI `defaults/runtimes/kimi.json` targets, and the same
//! credential-free method `docs/experiments/kimi-harness-probe-2026-09-22.json`
//! used. The findings are written up in `defaults/docs/runtime-model-trials.md`
//! next to the Pi `message_end` / OpenCode `step_finish` notes. In summary:
//!
//! - **The `--output-format stream-json` stdout stream carries NO token
//!   counters.** `PromptJsonWriter` emits only `{"role":"assistant",…}`,
//!   `{"role":"tool",…}` and `{"role":"meta","type":…}` lines. It *does* emit
//!   `{"role":"meta","type":"session.resume_hint","session_id":"…"}`, which is
//!   where a caller gets the exact session id this reader can filter on.
//! - **`state.json` carries NO token counters either.** It is session
//!   metadata (`id`/`version`/`cwd`/`title`/`titleKind`/`createdAt`/
//!   `updatedAt`/`archived`) — `normalizeSessionMeta` in the bundle.
//! - **The counters live in `wire.jsonl`**, the append-only durable event log
//!   at `<sessionDir>/agents/<agentId>/wire.jsonl`. Every durable event is
//!   serialized flat as `{"type":…, …payload, "time":<epoch ms>}`
//!   (`Event2.serialize()`). Two of those types matter here:
//!   - [`USAGE_RECORD_TYPE`] — `{"type":"usage.record","agentId":…,
//!     "model":<alias>,"usage":{"inputOther":N,"output":N,"inputCacheRead":N,
//!     "inputCacheCreation":N},"usageScope":"turn"|"session","time":…}`.
//!     `model` is the **model alias** (`request.modelAlias`), not necessarily
//!     the provider's own model id.
//!   - [`LLM_REQUEST_TYPE`] — `{"type":"llm.request","provider":<protocol>,
//!     "model":<provider model name>,"modelAlias":…,…}`. This is what resolves
//!     an alias to the real model id and its provider.
//!
//! # Session identification
//!
//! `$KIMI_CODE_HOME/session_index.jsonl` is an append-only index of
//! `{"sessionId","sessionDir","workDir"}` entries (plus `{"sessionId",
//! "deleted":true}` tombstones) — `appendSessionIndexEntry` in the bundle.
//! `sessionDir` is absolute, so this reader never has to reproduce Kimi's
//! `workDirKey` slug. Two filters are supported, in the order the issue asks
//! for: an **exact session id** when the caller captured one from the launch's
//! `session.resume_hint` line, otherwise the launch's **working directories**
//! (exact `workDir` match, the same key [`crate::opencode_usage`] uses) plus
//! the caller's wall-clock window.
//!
//! # Security: two event types, four numeric fields
//!
//! `wire.jsonl` holds the WHOLE conversation — user prompts, tool output, and
//! (when it differs from the bound profile) the system prompt on
//! `llm.request`. This reader must never surface any of it. It therefore
//! decodes exactly two `type`s and, from them, only the model/provider
//! strings and the four integer counters named above; every other record is
//! skipped without being retained, and nothing read here is ever logged.
//! `tests::a_wire_log_full_of_secrets_yields_only_counters` pins that.
//!
//! # Mapping to [`ModelUsageTotals`]
//!
//! Kimi has no analogue to Claude's prompt-caching `speed`/`service_tier`
//! axes, so every row uses the literal `"standard"` default for both — the
//! same default [`crate::script_helpers::transcript_usage`] applies to a
//! Claude record carrying neither field, and the same one
//! [`crate::opencode_usage`] uses, so the grouping key's vocabulary stays one
//! vocabulary across all three readers.
//!
//! `inputOther` maps to `input` (it is the non-cached remainder: the bundle's
//! own `inputTotal()` is `inputOther + inputCacheRead + inputCacheCreation`),
//! `inputCacheRead` to `cache_read`, and `output` to `output`. Kimi's `usage`
//! has no separate reasoning counter, so reasoning tokens arrive already
//! folded into `output` by the provider.
//!
//! `inputCacheCreation` is attributed to the **5-minute** bucket, NOT the
//! 1-hour one the Claude and OpenCode readers default to. That is not a
//! copy-paste divergence: Kimi's published billing note says "If no TTL is
//! specified, the 5min tier applies by default", and the CLI sends no TTL, so
//! 5m is the tier a Kimi cache write is actually billed at. Defaulting to 1h
//! here would over-report every K3 cache write 2x.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::script_helpers::sweep_experiment::ModelUsageTotals;

/// The `speed`/`service_tier` bucket every Kimi row is grouped under — see the
/// module doc's "Mapping" section.
const DEFAULT_BUCKET: &str = "standard";

/// Kimi Code CLI's own data-root variable. When set and non-empty it replaces
/// `~/.kimi-code` wholesale (`defaultHomeDir`/`resolveKimiHome` in the 2.0.2
/// bundle) — config, credentials, logs and the session store all move with it.
pub const KIMI_CODE_HOME_ENV: &str = "KIMI_CODE_HOME";

/// Loom-side override naming an exact Kimi data root — for tests, and for an
/// operator whose host does not match [`discover_kimi_homes`]'s layout.
/// Mirrors [`crate::opencode_usage::OPENCODE_DB_ENV`]'s convention, and takes
/// precedence over [`KIMI_CODE_HOME_ENV`] so a Loom probe never has to mutate
/// the CLI's own variable.
pub const KIMI_HOME_ENV: &str = "LOOM_KIMI_CODE_HOME";

/// The data root's default location when neither override is set.
const DEFAULT_HOME_DIR: &str = ".kimi-code";

/// The session index, relative to the data root.
const SESSION_INDEX: &str = "session_index.jsonl";

/// The per-agent durable event log, relative to a session directory's
/// `agents/<agentId>/`.
const WIRE_LOG: &str = "wire.jsonl";

/// The durable event type carrying the token counters.
pub const USAGE_RECORD_TYPE: &str = "usage.record";

/// The durable event type that resolves a model alias to the provider's own
/// model id and provider/protocol name.
pub const LLM_REQUEST_TYPE: &str = "llm.request";

/// Longest `wire.jsonl` line this reader will hold in memory. Kimi records are
/// routinely 10k+ characters and a single tool result can be far larger; a
/// pathological line must degrade to "skipped" rather than to unbounded
/// allocation in a daemon that only wants four integers out of the file.
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;

/// Every Kimi data root this host may have session state under.
///
/// A `Vec` rather than a single path for the same reason
/// [`crate::opencode_usage::discover_opencode_dbs`] returns one: per-account
/// `KIMI_CODE_HOME` rotation is already the planned shape for the Kimi account
/// pool (#8563), and a launch may have run against any root that existed at
/// the time. Today at most one is resolved, in precedence order:
/// [`KIMI_HOME_ENV`], then [`KIMI_CODE_HOME_ENV`], then `<home>/.kimi-code`.
///
/// `home` is the caller's home **directory** (the parent of `.kimi-code`), and
/// is injectable for tests; production passes `None`, which resolves it via
/// `dirs::home_dir`. A named `home` **short-circuits the two environment
/// variables entirely** rather than merely supplying the last fallback: a
/// process-global `KIMI_CODE_HOME` set by one test would otherwise silently
/// redirect every *other* test that passed its own fixture root, which is a
/// cross-test data dependency no `#[serial]` attribute on the env-mutating
/// tests alone can remove. Production is unaffected — it never names one.
///
/// A root that does not exist is dropped, so the result is empty rather than a
/// path that cannot be read.
#[must_use]
pub fn discover_kimi_homes(home: Option<&Path>) -> Vec<PathBuf> {
    let home = match home {
        Some(home) => home.to_path_buf(),
        None => {
            for env in [KIMI_HOME_ENV, KIMI_CODE_HOME_ENV] {
                let Some(raw) = std::env::var_os(env) else {
                    continue;
                };
                if raw.is_empty() {
                    continue;
                }
                let path = PathBuf::from(&raw);
                return if path.is_dir() {
                    vec![path]
                } else {
                    Vec::new()
                };
            }
            let Some(home) = dirs::home_dir() else {
                return Vec::new();
            };
            home
        }
    };
    let root = home.join(DEFAULT_HOME_DIR);
    if root.is_dir() {
        vec![root]
    } else {
        Vec::new()
    }
}

/// One live `session_index.jsonl` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiSessionEntry {
    /// `sessionId` — the same id the launch's
    /// `{"role":"meta","type":"session.resume_hint",…}` stdout line carries.
    pub session_id: String,
    /// `sessionDir`, absolute as the CLI writes it.
    pub session_dir: PathBuf,
    /// `workDir` verbatim — the working directory the session was opened in.
    pub work_dir: String,
}

/// Parse `<root>/session_index.jsonl` into its live entries, newest record per
/// id winning and `{"deleted":true}` tombstones removing the id entirely.
///
/// Returns an empty `Vec` for an absent or unreadable index — this is a
/// "nothing attributable" signal, and the caller ([`usage_records`]) collapses
/// the distinction into the single `None`/empty contract the seam specifies.
/// Garbage lines are skipped, exactly as the CLI's own
/// `classifySessionIndexLine` does.
#[must_use]
pub fn session_index(root: &Path) -> Vec<KimiSessionEntry> {
    let path = root.join(SESSION_INDEX);
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    // Insertion-ordered by first sighting, value replaced on re-append, so a
    // re-indexed session keeps its position but gains its newest directory.
    let mut live: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, KimiSessionEntry> = HashMap::new();
    let mut deleted: BTreeSet<String> = BTreeSet::new();

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(session_id) = string_field(&value, "sessionId") else {
            continue;
        };
        if value.get("deleted").and_then(serde_json::Value::as_bool) == Some(true) {
            deleted.insert(session_id);
            continue;
        }
        let (Some(session_dir), Some(work_dir)) =
            (string_field(&value, "sessionDir"), string_field(&value, "workDir"))
        else {
            continue;
        };
        if !by_id.contains_key(&session_id) {
            live.push(session_id.clone());
        }
        by_id.insert(
            session_id.clone(),
            KimiSessionEntry {
                session_id,
                session_dir: PathBuf::from(session_dir),
                work_dir,
            },
        );
    }

    live.into_iter()
        .filter(|id| !deleted.contains(id))
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

/// One decoded `usage.record`, already resolved against its session's
/// `llm.request` records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiUsageRecord {
    /// The provider's own model id when an `llm.request` in the same wire log
    /// mapped this alias to one (e.g. `kimi-k2.7-code`); otherwise the alias
    /// verbatim. NEVER a synthesised or guessed name — both forms were read
    /// off disk.
    pub model: String,
    /// The `llm.request.provider` (protocol) that served this alias, when one
    /// was recorded. `None` — never a guess — otherwise.
    pub provider: Option<String>,
    /// The alias the `usage.record` itself carried, kept so a caller can tell
    /// a resolved id from an unresolved one.
    pub alias: String,
    /// The record's own `time`, decoded from epoch milliseconds.
    pub recorded_at: DateTime<Utc>,
    /// `usage.inputOther` — input tokens that neither hit nor wrote the cache.
    pub input: i64,
    /// `usage.output`, reasoning tokens already folded in by the provider.
    pub output: i64,
    /// `usage.inputCacheRead`.
    pub cache_read: i64,
    /// `usage.inputCacheCreation`.
    pub cache_creation: i64,
}

impl KimiUsageRecord {
    /// Whether this record carries any usage at all. Kimi writes a
    /// `usage.record` for every request including ones that failed before the
    /// provider billed anything (`usage ?? emptyUsage()` at the call site), so
    /// an all-zero record is a real artifact but not usage — folding it in
    /// would publish a fabricated 0-token badge for a model that was never
    /// actually charged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_creation == 0
    }
}

/// Decode every attributable `usage.record` under one session directory.
///
/// Reads `<session_dir>/agents/<agentId>/wire.jsonl` for every agent (the main
/// agent plus any subagents, each of which has its own log and its own billed
/// tokens). Within a log, `llm.request` records supply the alias -> (model id,
/// provider) mapping; a `usage.record` whose alias never appears in one keeps
/// the alias as its model id.
///
/// `window`, when given, filters on each record's own `time` — a far tighter
/// key than session creation time, because one long-lived Kimi session can
/// span several launches.
#[must_use]
pub fn usage_records_in_session(
    session_dir: &Path,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<KimiUsageRecord> {
    let mut out = Vec::new();
    let Ok(agents) = std::fs::read_dir(session_dir.join("agents")) else {
        return out;
    };
    let mut logs: Vec<PathBuf> = agents
        .flatten()
        .map(|entry| entry.path().join(WIRE_LOG))
        .filter(|log| log.is_file())
        .collect();
    logs.sort();
    for log in logs {
        out.extend(usage_records_in_wire_log(&log, window));
    }
    out
}

/// [`usage_records_in_session`] for a single `wire.jsonl`.
///
/// Two passes over the file rather than one: `llm.request` for an alias is
/// written before the `usage.record` that uses it today, but the wire log is
/// an append-only event journal with no ordering contract this reader is
/// entitled to assume, and resolving after the fact costs one extra sequential
/// read of a file we are already streaming line-by-line.
#[must_use]
pub fn usage_records_in_wire_log(
    path: &Path,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<KimiUsageRecord> {
    let aliases = alias_map(path);
    let mut out = Vec::new();
    for value in wire_records(path, USAGE_RECORD_TYPE) {
        let Some(alias) = string_field(&value, "model") else {
            continue;
        };
        // `time` is written by `Event2.serialize()` on every durable record; a
        // record without a decodable one cannot be placed in a window, so it
        // is dropped rather than assumed to be inside it.
        let Some(recorded_at) = value
            .get("time")
            .and_then(serde_json::Value::as_i64)
            .and_then(DateTime::<Utc>::from_timestamp_millis)
        else {
            continue;
        };
        if let Some((start, end)) = window {
            if recorded_at < start || recorded_at > end {
                continue;
            }
        }
        let usage = value.get("usage");
        let counter = |key: &str| {
            usage
                .and_then(|u| u.get(key))
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
        };
        let resolved = aliases.get(&alias);
        out.push(KimiUsageRecord {
            model: resolved
                .and_then(|(model, _)| model.clone())
                .unwrap_or_else(|| alias.clone()),
            provider: resolved.and_then(|(_, provider)| provider.clone()),
            alias,
            recorded_at,
            input: counter("inputOther"),
            output: counter("output"),
            cache_read: counter("inputCacheRead"),
            cache_creation: counter("inputCacheCreation"),
        });
    }
    out
}

/// `modelAlias` -> (provider model id, provider) as recorded by this wire
/// log's own `llm.request` events. A later record wins, so an alias rebound
/// mid-session resolves to whatever served it last.
fn alias_map(path: &Path) -> HashMap<String, (Option<String>, Option<String>)> {
    let mut map = HashMap::new();
    for value in wire_records(path, LLM_REQUEST_TYPE) {
        let Some(alias) = string_field(&value, "modelAlias") else {
            continue;
        };
        map.insert(alias, (string_field(&value, "model"), string_field(&value, "provider")));
    }
    map
}

/// Stream one `wire.jsonl`, yielding only the records whose `type` is exactly
/// `wanted`.
///
/// The `contains` pre-filter is an optimisation only (a conversation record
/// can be megabytes and there is no reason to parse it to learn it is not a
/// `usage.record`); correctness comes from the `type` equality check after
/// parsing, so a tool output that happens to quote the literal string costs a
/// parse and is then discarded.
fn wire_records(path: &Path, wanted: &str) -> Vec<serde_json::Value> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let needle = format!("\"type\":\"{wanted}\"");
    let mut out = Vec::new();
    let mut reader = BufReader::new(file);
    loop {
        let mut line = String::new();
        // `take` caps ONE line, so an over-long record is truncated and
        // discarded instead of being read into memory whole.
        let read = (&mut reader)
            .take(MAX_LINE_BYTES)
            .read_line(&mut line)
            .unwrap_or(0);
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        if !trimmed.starts_with('{') || !trimmed.contains(&needle) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some(wanted) {
            out.push(value);
        }
    }
    out
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Every attributable `usage.record` across every discovered Kimi data root,
/// sorted oldest-first.
///
/// `session_id`, when given, is the exact filter (the launch captured its own
/// id from the `session.resume_hint` stdout line); `directories` is the
/// fallback, matching `session_index.jsonl`'s `workDir` exactly the way
/// [`crate::opencode_usage`] matches `session.directory`. When BOTH are given
/// the session id wins outright — it is the precise key, and a resumed session
/// can legitimately carry a `workDir` outside the caller's set.
///
/// `home` is injectable for tests (see [`discover_kimi_homes`]); production
/// callers pass `None`.
#[must_use]
pub fn usage_records(
    directories: &[PathBuf],
    session_id: Option<&str>,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    home: Option<&Path>,
) -> Vec<KimiUsageRecord> {
    let wanted: Vec<&str> = directories
        .iter()
        .filter_map(|d| d.to_str())
        .filter(|d| !d.is_empty())
        .collect();
    let mut all: Vec<KimiUsageRecord> = discover_kimi_homes(home)
        .into_iter()
        .flat_map(|root| session_index(&root))
        .filter(|entry| match session_id {
            Some(id) => entry.session_id == id,
            None => wanted.iter().any(|w| *w == entry.work_dir),
        })
        .flat_map(|entry| usage_records_in_session(&entry.session_dir, window))
        .filter(|record| !record.is_empty())
        .collect();
    all.sort_by(|a, b| {
        a.recorded_at
            .cmp(&b.recorded_at)
            .then_with(|| a.model.cmp(&b.model))
    });
    all
}

/// Per-`(model, speed, service_tier)` token totals for a Kimi launch (Issue
/// #8564) — the Kimi counterpart of
/// [`crate::transcript_tokens::sum_sweep_tokens_by_model`] and
/// [`crate::opencode_usage::tokens_by_model`], reached through the
/// runtime-dispatch seam in [`crate::usage_source`].
///
/// `None` — never `Some(vec![])` — when no Kimi data root was found, when no
/// session matched, or when every matched session's records were counter-free:
/// **unknown is not zero**, the contract every reader behind the seam keeps.
#[must_use]
pub fn tokens_by_model(
    directories: &[PathBuf],
    session_id: Option<&str>,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    home: Option<&Path>,
) -> Option<Vec<ModelUsageTotals>> {
    fold_records(usage_records(directories, session_id, window, home))
}

/// Fold already-selected records into per-model totals — the whole of
/// [`tokens_by_model`]'s arithmetic, split out so the mapping documented in
/// the module doc has exactly one implementation (and so a test can drive it
/// from a fixture directly, without the process-global discovery override).
///
/// Counter-free records are dropped here rather than by the caller: see
/// [`KimiUsageRecord::is_empty`].
#[must_use]
pub fn fold_records(
    records: impl IntoIterator<Item = KimiUsageRecord>,
) -> Option<Vec<ModelUsageTotals>> {
    let mut totals: BTreeMap<String, ModelUsageTotals> = BTreeMap::new();
    for record in records.into_iter().filter(|r| !r.is_empty()) {
        let entry = totals
            .entry(record.model.clone())
            .or_insert_with(|| ModelUsageTotals {
                model: record.model.clone(),
                speed: DEFAULT_BUCKET.to_string(),
                service_tier: DEFAULT_BUCKET.to_string(),
                ..ModelUsageTotals::default()
            });
        entry.input = entry.input.saturating_add(record.input);
        entry.output = entry.output.saturating_add(record.output);
        entry.cache_read = entry.cache_read.saturating_add(record.cache_read);
        // 5-minute bucket, not 1-hour — see the module doc's "Mapping" section.
        entry.cache_write_5m = entry.cache_write_5m.saturating_add(record.cache_creation);
    }
    (!totals.is_empty()).then(|| totals.into_values().collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
