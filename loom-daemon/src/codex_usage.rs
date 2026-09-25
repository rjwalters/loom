//! Read-only per-model token accounting from Codex's own rollout session
//! store (Issue #8594).
//!
//! # Why this module exists
//!
//! [`crate::transcript_tokens`] sums per-model token usage **only** from
//! Claude Code's on-disk JSONL transcripts, and [`crate::opencode_usage`] does
//! the same job for OpenCode's SQLite session store. A sweep or role tick
//! dispatched on Codex wrote neither, so it reached the public fleet feed with
//! no `tokens_by_model` at all. The numbers exist the whole time, on disk, in
//! Codex's own rollout store:
//! `${CODEX_HOME:-~/.codex}/sessions/<YYYY>/<MM>/<DD>/rollout-*.jsonl`. This
//! module locates those files and reads them — the same role
//! [`crate::opencode_usage`] plays for OpenCode, behind the runtime-dispatch
//! seam in [`crate::usage_source`].
//!
//! # Security: `sessions/**/rollout-*.jsonl` only, ever
//!
//! `$CODEX_HOME` is a **credential-bearing directory**. It holds `auth.json`
//! (OAuth tokens and API keys), `config.toml`, `history.jsonl` (every prompt
//! the operator ever typed) and several SQLite stores, all siblings of the
//! `sessions/` tree this reader wants. So the whole file-open surface of this
//! module is one function — [`read_rollout`] — reached only for a path that
//! [`is_rollout_path`] accepted, i.e. a file under the `sessions/` subtree
//! ([`SESSIONS_DIR`]) whose name starts with [`ROLLOUT_PREFIX`] and ends with
//! [`ROLLOUT_SUFFIX`]. Every forbidden basename in that directory fails at
//! least one of those three tests.
//!
//! [`tests::the_only_file_open_in_this_module_is_the_guarded_rollout_reader`]
//! pins that by scanning the module's OWN SOURCE for file-open idioms — not
//! merely by testing behavior — so a future edit that opens a second path
//! fails a test that says exactly why, the same discipline
//! [`crate::opencode_usage`]'s `SESSION_QUERY` scan establishes for its SQL
//! surface. A companion behavioral test drives a fixture `$CODEX_HOME` with a
//! planted secret in every credential-bearing sibling and asserts none of them
//! is opened and none of their bytes reaches the output.
//!
//! # Which `$CODEX_HOME`: every home a launch could have written to
//!
//! A daemon-dispatched Codex sweep does **not** write under the daemon's own
//! `$CODEX_HOME`. `spawn-codex.sh`'s managed headless path runs
//! `loom-daemon tokens select --provider codex --export`, which exports
//! `CODEX_HOME=<pool profile dir>` (`~/.loom/codex-profiles/<name>`) into the
//! **child's** env only, so the rollouts land in
//! `<profile>/sessions/<YYYY>/<MM>/<DD>/`. A reader that looked only at the
//! daemon's ambient home would miss every pool-selected launch.
//!
//! So [`codex_homes`] returns the ambient home (`CODEX_HOME`, else
//! `~/.codex`) **plus** every directory directly under the pooled-profile root
//! ([`crate::tokens_pool::paths::codex_profile_root`], `LOOM_CODEX_PROFILE_ROOT`
//! overridable). Scanning homes the launch did not use is safe: every row
//! must still match the caller's `session_meta.cwd` set and window (or exact
//! session ids), so another profile's sessions cannot be misattributed. A
//! provisioned profile symlinks its `sessions/` to the default account's
//! (#8694), so the same rollout can be reachable through two homes; each file
//! is read once, keyed by its canonical path. The explicit
//! [`CODEX_HOME_ENV`] (`LOOM_CODEX_HOME`) pin is exact — it replaces the whole
//! set rather than joining it. The [`is_rollout_path`] gate is applied per
//! home, so a profile's own `auth.json` is exactly as unreachable as the
//! default home's.
//!
//! # Session identification
//!
//! A sweep's or role tick's own Codex sessions are identified by the rollout's
//! `session_meta.cwd` (the working directory Codex was invoked from, matched
//! against an explicit caller-supplied set — see [`crate::usage_source`] for
//! how a sweep's set gains its worktree and a role tick's does not) plus the
//! caller's own wall-clock window. That is the same two-part key
//! [`crate::opencode_usage`] uses.
//!
//! Unlike OpenCode's store, Codex's store also carries the exact session id
//! (`session_meta.session_id`, which is also the rollout filename's own uuid),
//! so [`sessions`] reports it on every row and [`SessionFilter::ids`] accepts
//! an explicit id set — the strictly-more-precise attribution #8507's design
//! note asked for. It is offered rather than required: nothing in the daemon
//! captures a launch's Codex session ids today, so directory+window remains
//! the default, and an id set narrows it when a caller has one.
//!
//! # Mapping to [`ModelUsageTotals`]
//!
//! Codex has no analogue to Claude's prompt-caching `speed`/`service_tier`
//! axes, so every row uses the literal `"standard"` default for both — the
//! same default [`crate::opencode_usage`] and
//! [`crate::script_helpers::transcript_usage`] apply, so the grouping key's
//! vocabulary stays one vocabulary across all three readers.
//!
//! The four counters are mapped to keep Claude's **disjoint** vocabulary,
//! because Codex's are nested (verified below):
//!
//! | `ModelUsageTotals` | from Codex |
//! |---|---|
//! | `input` | `input_tokens - cached_input_tokens` |
//! | `cache_read` | `cached_input_tokens` |
//! | `output` | `output_tokens` (`reasoning_output_tokens` is a SUBSET — never added) |
//! | `cache_write_5m` / `cache_write_1h` | `0` — Codex reports no cache-write counter at all |
//!
//! # Schema provenance
//!
//! Verified live on 2026-09-22 against this host's own `~/.codex/sessions/`
//! tree: **140** `rollout-*.jsonl` files (and no non-rollout file anywhere in
//! that tree) written by `codex` 0.46.0 and 0.154.0, carrying **12,001**
//! `token_count` events of which **11,868** have a non-null `info`. What that
//! survey established, and which this module's decoding depends on:
//!
//! * `reasoning_output_tokens > output_tokens` in **0** of 11,868 samples ⇒
//!   reasoning is a subset of output, so adding them would double-count. (The
//!   opposite of OpenCode, whose `tokens_reasoning` IS a separate counter.)
//! * `cached_input_tokens > input_tokens` in **0** of 11,868 samples ⇒ cached
//!   input is a subset of input, so `input` must have it subtracted out.
//! * `total_tokens == input_tokens + output_tokens` in 11,840 of 11,868 ⇒
//!   `total_tokens` is redundant with the parts, so this module never reads it
//!   and the 28 older-format disagreements cost nothing.
//! * `total_token_usage` is **cumulative over the session** and every
//!   `token_count` event is emitted TWICE in a row with identical contents.
//!   Both facts are handled by one mechanism: [`fold_rollout`] accumulates
//!   per-field **deltas** against the previous event, so a duplicate
//!   contributes exactly `0` and a mid-session model switch attributes each
//!   turn's delta to the `turn_context.model` in force at that point.
//! * `session_meta` carries the session id under `id` in all 140 rollouts and
//!   additionally under `session_id` only in the 0.154.0 one, so
//!   [`fold_rollout`] reads `session_id` first and falls back to `id`.
//!   `model_provider` likewise exists only from 0.154.0 — hence
//!   [`CodexSessionUsage::provider`] being an `Option`, never a filled-in
//!   default.
//! * `turn_context` precedes the first usage-bearing `token_count` in **all
//!   140** rollouts: replaying the whole store through this module's own
//!   attribution rule drops **0** of 587,337,443 observed tokens for
//!   "no model named yet". The rule below is therefore a genuine safety net,
//!   not a silent tax on real usage. A rollout with no readable model id is
//!   still DROPPED, never given a guessed name.
//!
//! A future version that renames or retypes a field degrades to `None` here
//! (every decode failure is treated as "no data", never a panic and never a
//! fabricated reading).

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, NaiveDate, Utc};

use crate::script_helpers::sweep_experiment::ModelUsageTotals;

/// The `speed`/`service_tier` bucket every Codex row is grouped under — see
/// the module doc's "Mapping" section for why.
const DEFAULT_BUCKET: &str = "standard";

/// The ONLY subdirectory of `$CODEX_HOME` this module ever descends into (see
/// the module doc's "Security" section). Its credential-bearing siblings —
/// `auth.json`, `config.toml`, `history.jsonl`, `sqlite/`, `*.sqlite` — are
/// outside it by construction.
pub const SESSIONS_DIR: &str = "sessions";

/// The ONLY filename prefix this module ever opens.
pub const ROLLOUT_PREFIX: &str = "rollout-";

/// The ONLY filename suffix this module ever opens.
pub const ROLLOUT_SUFFIX: &str = ".jsonl";

/// Env override naming an exact `$CODEX_HOME` — for tests, and for an operator
/// whose host does not match [`codex_home`]'s default. Mirrors
/// [`crate::opencode_usage::OPENCODE_DB_ENV`]'s convention: a Loom-namespaced
/// override that takes precedence over discovery.
pub const CODEX_HOME_ENV: &str = "LOOM_CODEX_HOME";

/// Codex's own home override, honored second so an operator who already
/// relocated Codex does not have to restate it for Loom.
pub const CODEX_NATIVE_HOME_ENV: &str = "CODEX_HOME";

/// How far outside the caller's UTC window the date-partitioned directory scan
/// reaches. Codex names `sessions/<YYYY>/<MM>/<DD>/` by the host's LOCAL date
/// while the window is UTC, and the largest real UTC offset is under 24h, so
/// one day of pad on each side cannot miss a rollout — and bounds the scan to
/// a handful of directories instead of the whole multi-year tree.
const DATE_PAD: i64 = 1;

/// `$CODEX_HOME`: [`CODEX_HOME_ENV`], else [`CODEX_NATIVE_HOME_ENV`], else
/// `<home>/.codex`.
///
/// `home` is injectable for tests; production passes `None` (resolves via
/// `dirs::home_dir`).
#[must_use]
pub fn codex_home(home: Option<&Path>) -> Option<PathBuf> {
    for key in [CODEX_HOME_ENV, CODEX_NATIVE_HOME_ENV] {
        if let Some(value) = std::env::var_os(key).map(PathBuf::from) {
            if !value.as_os_str().is_empty() {
                return Some(value);
            }
        }
    }
    Some(
        home.map(Path::to_path_buf)
            .or_else(dirs::home_dir)?
            .join(".codex"),
    )
}

/// Every `$CODEX_HOME` a Codex launch on this host could have written its
/// rollouts under — see the module doc's "Which `$CODEX_HOME`" section.
///
/// An explicit [`CODEX_HOME_ENV`] pin is exact and returns only itself.
/// Otherwise: [`codex_home`]'s ambient home first, then every directory
/// directly under the pooled-profile root, sorted, de-duplicated by canonical
/// path. Homes that do not exist are kept (they contribute nothing), so the
/// result names what was consulted.
///
/// `home` is injectable for tests: with it given and `LOOM_CODEX_PROFILE_ROOT`
/// unset, the profile root is `<home>/.loom/codex-profiles`, so a test never
/// reaches the operator's real pool.
#[must_use]
pub fn codex_homes(home: Option<&Path>) -> Vec<PathBuf> {
    if let Some(pinned) = std::env::var_os(CODEX_HOME_ENV).map(PathBuf::from) {
        if !pinned.as_os_str().is_empty() {
            return vec![pinned];
        }
    }
    let mut homes: Vec<PathBuf> = codex_home(home).into_iter().collect();
    if let Some(root) = profile_root(home) {
        if let Ok(entries) = std::fs::read_dir(&root) {
            let mut profiles: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect();
            profiles.sort();
            homes.extend(profiles);
        }
    }
    let mut seen = HashSet::new();
    homes.retain(|h| seen.insert(h.canonicalize().unwrap_or_else(|_| h.clone())));
    homes
}

/// The pooled Codex profile root: the tokens-pool resolver when
/// `LOOM_CODEX_PROFILE_ROOT` is set or no `home` was injected, else
/// `<home>/.loom/codex-profiles` (the resolver's own default shape).
fn profile_root(home: Option<&Path>) -> Option<PathBuf> {
    match home {
        Some(home)
            if std::env::var_os(crate::tokens_pool::paths::CODEX_PROFILE_ROOT_ENV).is_none() =>
        {
            Some(home.join(".loom").join("codex-profiles"))
        }
        _ => crate::tokens_pool::paths::codex_profile_root(),
    }
}

/// Whether `path` is one this module is allowed to open: a file directly under
/// the `<codex_home>/sessions/` subtree whose name is `rollout-*.jsonl`.
///
/// The whole authorization check, in one place, so
/// [`tests::the_only_file_open_in_this_module_is_the_guarded_rollout_reader`]
/// has one predicate to pin rather than a scattered set of conditions.
#[must_use]
pub fn is_rollout_path(codex_home: &Path, path: &Path) -> bool {
    let sessions = codex_home.join(SESSIONS_DIR);
    if !path.starts_with(&sessions) {
        return false;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(ROLLOUT_PREFIX) && n.ends_with(ROLLOUT_SUFFIX))
}

/// Every rollout file that could hold a session inside `window`, across every
/// home [`codex_homes`] names.
///
/// Each file appears once even when two homes reach it (a provisioned
/// profile's `sessions/` symlinks to the default account's, #8694).
#[must_use]
pub fn discover_rollouts(
    home: Option<&Path>,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<PathBuf> {
    discover_home_rollouts(home, window)
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

/// [`discover_rollouts`], paired with the home each path was found under —
/// the home [`read_rollout`]'s gate must check it against.
fn discover_home_rollouts(
    home: Option<&Path>,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<(PathBuf, PathBuf)> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for codex_home in codex_homes(home) {
        for path in rollouts_under(&codex_home, window) {
            if seen.insert(path.canonicalize().unwrap_or_else(|_| path.clone())) {
                found.push((codex_home.clone(), path));
            }
        }
    }
    found
}

/// Every rollout file that could hold a session inside `window`, under one
/// `$CODEX_HOME`.
///
/// Only `<sessions>/<YYYY>/<MM>/<DD>/` directories whose date falls inside the
/// window padded by [`DATE_PAD`] are descended into; `window: None` scans the
/// whole tree. Every returned path has passed [`is_rollout_path`] — that is
/// the authorization gate, and this is the only producer of paths
/// [`read_rollout`] is ever given.
///
/// Sorted, so output is deterministic for tests and operator diffs.
fn rollouts_under(
    codex_home: &Path,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<PathBuf> {
    let sessions = codex_home.join(SESSIONS_DIR);
    let allowed_dates = window.map(|(start, end)| {
        (
            (start - Duration::days(DATE_PAD)).date_naive(),
            (end + Duration::days(DATE_PAD)).date_naive(),
        )
    });
    let mut found = Vec::new();
    // sessions/<YYYY>/<MM>/<DD>/rollout-*.jsonl — three fixed levels, walked
    // explicitly rather than recursively so no sibling tree can ever be
    // entered by a symlink or an unexpected nesting depth.
    for year in numeric_children(&sessions) {
        for month in numeric_children(&year.0) {
            for day in numeric_children(&month.0) {
                if let Some((first, last)) = allowed_dates {
                    let Some(date) = NaiveDate::from_ymd_opt(year.1, month.1 as u32, day.1 as u32)
                    else {
                        continue;
                    };
                    if date < first || date > last {
                        continue;
                    }
                }
                let Ok(entries) = std::fs::read_dir(&day.0) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && is_rollout_path(codex_home, &path) {
                        found.push(path);
                    }
                }
            }
        }
    }
    found.sort();
    found
}

/// The numerically-named child directories of `dir`, as `(path, value)`.
///
/// Non-numeric names are skipped rather than descended: that is what keeps the
/// walk to the `<YYYY>/<MM>/<DD>` shape the store documents, and it is why a
/// stray `sessions/auth.json` could never be reached even before
/// [`is_rollout_path`] gets a say.
fn numeric_children(dir: &Path) -> Vec<(PathBuf, i32)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, i32)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let value: i32 = path.file_name()?.to_str()?.parse().ok()?;
            Some((path, value))
        })
        .collect();
    out.sort();
    out
}

/// Which sessions a caller wants attributed.
///
/// `directories` + the caller's window is the default key (see the module
/// doc); `ids` narrows to an exact `session_meta.session_id` set when the
/// caller has one, and is the strictly-more-precise attribution #8507's design
/// note asked for.
#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    /// Working directories to attribute, matched exactly against
    /// `session_meta.cwd`. Empty ⇒ no directory constraint (only meaningful
    /// together with [`Self::ids`], which is then the whole key).
    pub directories: Vec<PathBuf>,
    /// Exact `session_meta.session_id` values to attribute. Empty ⇒ no id
    /// constraint.
    pub ids: Vec<String>,
}

impl SessionFilter {
    /// The directory+window key every consumer in the daemon uses today.
    #[must_use]
    pub fn directories(directories: &[PathBuf]) -> Self {
        Self {
            directories: directories.to_vec(),
            ids: Vec::new(),
        }
    }

    /// Whether a rollout's `cwd` / `session_id` pass this filter.
    ///
    /// An EMPTY filter (no directories, no ids) matches nothing: "attribute
    /// every session on this host" is never what a caller means, and silently
    /// treating it as such would fold an unrelated sweep's spend into a
    /// completion.
    #[must_use]
    fn matches(&self, cwd: &str, session_id: Option<&str>) -> bool {
        if self.directories.is_empty() && self.ids.is_empty() {
            return false;
        }
        if !self.directories.is_empty()
            && !self
                .directories
                .iter()
                .any(|d| d.to_str().is_some_and(|d| d == cwd))
        {
            return false;
        }
        if !self.ids.is_empty()
            && !session_id.is_some_and(|id| self.ids.iter().any(|want| want == id))
        {
            return false;
        }
        true
    }
}

/// One attributable Codex session, already decoded and folded — the shape both
/// [`tokens_by_model`] and [`sessions`] produce.
///
/// One row **per model** within a session, not one per session: a session that
/// switched models mid-run contributes one row each, because
/// [`fold_rollout`]'s delta accumulation can attribute each turn's usage to the
/// model that served it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionUsage {
    /// `turn_context.model`, e.g. `gpt-5.6-sol`.
    pub model: String,
    /// `session_meta.model_provider`, e.g. `openai`. `None` for a rollout
    /// written before that field existed (pre-0.154.0).
    pub provider: Option<String>,
    /// `session_meta.cwd` verbatim.
    pub directory: String,
    /// `session_meta.session_id` — also the rollout filename's uuid.
    pub session_id: Option<String>,
    /// `session_meta.timestamp`, the session's creation instant.
    pub created_at: DateTime<Utc>,
    /// `input_tokens - cached_input_tokens` (see the module doc's mapping).
    pub input: i64,
    /// `cached_input_tokens`.
    pub cache_read: i64,
    /// `output_tokens` — INCLUSIVE of `reasoning_output_tokens`.
    pub output: i64,
}

impl CodexSessionUsage {
    /// Whether this row recorded any token usage at all. Codex writes a
    /// `token_count` event with `info: null` on a turn that produced no usage,
    /// and a rollout can exist with no usage at all — a real artifact, but not
    /// usage, and folding it in would publish a fabricated `0`-token model
    /// badge for a model that was never billed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input == 0 && self.cache_read == 0 && self.output == 0
    }
}

/// The four cumulative counters one `token_count` event carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counters {
    input: i64,
    cached_input: i64,
    output: i64,
}

impl Counters {
    /// Read the three counters this module uses off a `total_token_usage`
    /// object. `reasoning_output_tokens` is deliberately NOT read: it is a
    /// subset of `output_tokens` (see the module doc's provenance survey), so
    /// reading it could only invite double-counting. `total_tokens` is not read
    /// either — it is redundant with the parts.
    fn from_total_usage(value: &serde_json::Value) -> Option<Self> {
        let count = |key: &str| value.get(key).and_then(serde_json::Value::as_i64);
        Some(Self {
            input: count("input_tokens")?,
            cached_input: count("cached_input_tokens").unwrap_or(0),
            output: count("output_tokens").unwrap_or(0),
        })
    }

    /// The per-field increase from `self` to `next`, clamped at zero.
    ///
    /// Clamped rather than signed because these are cumulative counters that
    /// must only ever grow; a decrease (a format change, a compaction that
    /// rebases them) is unreadable, and treating it as negative usage would
    /// silently subtract real spend from a sibling model's row.
    fn delta(self, next: Self) -> Self {
        Self {
            input: (next.input - self.input).max(0),
            cached_input: (next.cached_input - self.cached_input).max(0),
            output: (next.output - self.output).max(0),
        }
    }
}

/// Read one rollout file's lines. **The whole file-open surface of this
/// module** (see the module doc's "Security" section): it refuses any path
/// [`is_rollout_path`] does not accept, so no credential-bearing sibling of
/// the `sessions/` tree can be opened even by a caller that constructed a path
/// by hand.
///
/// `None` when the path is not an authorized rollout or cannot be opened.
fn read_rollout(codex_home: &Path, path: &Path) -> Option<Vec<String>> {
    if !is_rollout_path(codex_home, path) {
        return None;
    }
    let reader = BufReader::new(File::open(path).ok()?);
    Some(reader.lines().map_while(Result::ok).collect())
}

/// Fold one rollout's lines into per-model rows.
///
/// The whole of this module's arithmetic, split out so it can be driven
/// directly from literal lines in a test without a filesystem — and so the
/// mapping documented at the module level has exactly one implementation.
///
/// Returns an empty `Vec` for a rollout that is unattributable, carries no
/// usage, or names no model: the caller collapses that to `None` (see
/// [`tokens_by_model`]'s "unknown != zero" contract).
#[must_use]
pub fn fold_rollout(
    lines: &[String],
    filter: &SessionFilter,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<CodexSessionUsage> {
    let mut directory: Option<String> = None;
    let mut provider: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut created_at: Option<DateTime<Utc>> = None;
    let mut model: Option<String> = None;
    let mut previous = Counters::default();
    let mut by_model: BTreeMap<String, Counters> = BTreeMap::new();

    for line in lines {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let payload = value.get("payload");
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("session_meta") => {
                let Some(payload) = payload else { continue };
                directory = string_field(payload, "cwd");
                provider = string_field(payload, "model_provider");
                session_id =
                    string_field(payload, "session_id").or_else(|| string_field(payload, "id"));
                created_at = string_field(payload, "timestamp")
                    .or_else(|| string_field(&value, "timestamp"))
                    .and_then(|ts| parse_instant(&ts));
            }
            // The model in force for the turns that follow. A rollout that
            // switches models writes a new `turn_context`, which is what makes
            // the per-model split below honest rather than a whole-session
            // guess.
            Some("turn_context") => {
                if let Some(next) = payload.and_then(|p| string_field(p, "model")) {
                    model = Some(next);
                }
            }
            Some("event_msg") => {
                let Some(payload) = payload else { continue };
                if payload.get("type").and_then(serde_json::Value::as_str) != Some("token_count") {
                    continue;
                }
                // `info: null` is Codex's own "no usage on this turn" — a
                // reading of silence, so it advances nothing.
                let Some(current) = payload
                    .get("info")
                    .and_then(|info| info.get("total_token_usage"))
                    .and_then(Counters::from_total_usage)
                else {
                    continue;
                };
                let delta = previous.delta(current);
                previous = current;
                // Never guess a model name: usage observed before any
                // `turn_context` named a model is unattributable and dropped.
                let Some(model) = model.as_ref() else {
                    continue;
                };
                let entry = by_model.entry(model.clone()).or_default();
                entry.input += delta.input;
                entry.cached_input += delta.cached_input;
                entry.output += delta.output;
            }
            _ => {}
        }
    }

    let Some(directory) = directory else {
        return Vec::new();
    };
    if !filter.matches(&directory, session_id.as_deref()) {
        return Vec::new();
    }
    // `session_meta.timestamp` is present in every observed rollout; one
    // without a decodable instant is unattributable to a window, so it is
    // dropped rather than assumed to be inside it.
    let Some(created_at) = created_at else {
        return Vec::new();
    };
    if let Some((start, end)) = window {
        if created_at < start || created_at > end {
            return Vec::new();
        }
    }
    by_model
        .into_iter()
        .map(|(model, counters)| CodexSessionUsage {
            model,
            provider: provider.clone(),
            directory: directory.clone(),
            session_id: session_id.clone(),
            created_at,
            // Keep Claude's DISJOINT vocabulary: Codex's `input_tokens`
            // includes its `cached_input_tokens`, so the cached part is moved
            // to `cache_read` rather than counted twice.
            input: (counters.input - counters.cached_input).max(0),
            cache_read: counters.cached_input,
            output: counters.output,
        })
        .filter(|row| !row.is_empty())
        .collect()
}

/// A non-empty, trimmed string field, or `None` — the "omit rather than guess"
/// contract every reader in this tree follows.
fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse an RFC3339 instant into UTC. `None` — never "now" — for anything that
/// does not parse.
fn parse_instant(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Every attributable session row across every [`codex_homes`] rollout store, sorted
/// oldest-first.
///
/// The listing counterpart of [`tokens_by_model`], for the `codex-usage` CLI's
/// backfill path: an operator reconciling a past window needs the per-session
/// rows (and their session ids and providers), not only the folded totals.
///
/// `home` is injectable for tests (see [`codex_home`]); production passes
/// `None`.
#[must_use]
pub fn sessions(
    filter: &SessionFilter,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    home: Option<&Path>,
) -> Vec<CodexSessionUsage> {
    let mut all: Vec<CodexSessionUsage> = discover_home_rollouts(home, window)
        .into_iter()
        .filter_map(|(root, path)| read_rollout(&root, &path))
        .flat_map(|lines| fold_rollout(&lines, filter, window))
        .collect();
    all.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.model.cmp(&b.model))
    });
    all
}

/// Per-`(model, speed, service_tier)` token totals for `filter` across
/// every [`codex_homes`] rollout store (Issue #8594) — the Codex counterpart of
/// [`crate::opencode_usage::tokens_by_model`], reached through the
/// runtime-dispatch seam in [`crate::usage_source`].
///
/// `window`, when given, is used exactly as passed with no internal slack —
/// the caller (a sweep's whole-run window vs. a role tick's few minutes)
/// already knows what slack its own cadence needs. It additionally bounds the
/// directory scan (see [`discover_rollouts`]).
///
/// `None` — never `Some(vec![])` — when no rollout store was found at all, or
/// when nothing attributable was found in it: "unknown != zero", the same
/// contract the Claude and OpenCode readers follow.
#[must_use]
pub fn tokens_by_model(
    filter: &SessionFilter,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    home: Option<&Path>,
) -> Option<Vec<ModelUsageTotals>> {
    fold_sessions(sessions(filter, window, home))
}

/// Fold already-selected session rows into per-model totals.
///
/// Split out so a test can drive it from literal rows, and so the `codex-usage`
/// CLI can list the rows AND print their totals from one read.
#[must_use]
pub fn fold_sessions(
    sessions: impl IntoIterator<Item = CodexSessionUsage>,
) -> Option<Vec<ModelUsageTotals>> {
    let mut totals: BTreeMap<String, ModelUsageTotals> = BTreeMap::new();
    for session in sessions.into_iter().filter(|s| !s.is_empty()) {
        let entry = totals
            .entry(session.model.clone())
            .or_insert_with(|| ModelUsageTotals {
                model: session.model.clone(),
                speed: DEFAULT_BUCKET.to_string(),
                service_tier: DEFAULT_BUCKET.to_string(),
                ..ModelUsageTotals::default()
            });
        entry.input = entry.input.saturating_add(session.input);
        entry.cache_read = entry.cache_read.saturating_add(session.cache_read);
        entry.output = entry.output.saturating_add(session.output);
        // `cache_write_5m` / `cache_write_1h` stay at their `0` default:
        // Codex reports no cache-write counter at all, and a fabricated
        // split would be priced as real spend downstream.
    }
    (!totals.is_empty()).then(|| totals.into_values().collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
