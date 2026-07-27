//! Durable operator watches on issue/PR terminal state (Issue #3971).
//!
//! ## Problem
//!
//! At the end of the 2026-07-26 canary session an operator armed a 4-hour
//! background poll watching two items (`vibesql#6193`, `loom#3964`) for terminal
//! state. The operator's Claude Code session then crashed, and the watch died
//! with it — background tasks are children of the session process, so the
//! terminal-state report was silently lost until the next session reconstructed
//! it from logs.
//!
//! The root cause is harness behaviour (session-scoped background tasks), but the
//! durable fix belongs on the daemon side: the `loom-daemon` is the long-lived
//! process that already outlives any one operator session. What was missing was a
//! way for an operator to register interest that survives their session dying.
//!
//! ## Fix
//!
//! An operator registers a [`WatchSpec`] against the daemon (over IPC —
//! `Request::RegisterWatch`, or the `loom-daemon watch add` CLI). The watch is
//! persisted to a machine-level file (`~/.loom/watches.json`, override via
//! [`WATCHES_PATH_ENV`]) so it survives **both** the registering session's death
//! **and** a daemon restart. A background monitor loop
//! ([`crate::watch_registry` runtime wiring in `main.rs`]) polls each watch's
//! terminal state on a cadence and, when a watch reaches a terminal state,
//! **durably appends** a [`WatchResult`] line to `~/.loom/logs/watch-results.log`
//! (override via [`RESULTS_LOG_PATH_ENV`]) — a file a later session can trivially
//! `cat`/`tail` — and drops the watch from the active set.
//!
//! ## Why forge polling (not the event bus)
//!
//! The daemon's in-memory event bus (`sweep.issue.{N}.*`, `sweep.global.*`) only
//! knows about sweeps **this daemon dispatched**. The motivating case is
//! explicitly cross-repo — a `vibesql` issue watched from the `loom` operator
//! session — which the daemon may never have dispatched a sweep for. The v0.10.0
//! event taxonomy is also frozen (new topics require a follow-up issue). So the
//! monitor **polls the forge directly** (`gh issue view` / `gh pr view`) for each
//! watch's terminal state, which works uniformly for any repo — sweep-backed or
//! not — without minting a new event topic. A watch carries either an explicit
//! forge slug ([`WatchSpec::repo`]) or a [`WatchSpec::workspace_root`] the `gh`
//! query runs in (mirroring how [`crate::work_finder::forge::GhWorkSource`]
//! addresses a specific managed workspace).
//!
//! All I/O here is best-effort by design: a missing/empty/corrupt watches file
//! degrades to an empty registry, and a single failing probe is logged and the
//! watch retained (retried next tick) rather than aborting the whole tick.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ============================================================================
// Constants
// ============================================================================

/// Environment override for the persisted watches file (mirrors
/// [`crate::sweep_journal::JOURNAL_PATH_ENV`]). Primarily a test seam.
pub const WATCHES_PATH_ENV: &str = "LOOM_WATCHES_PATH";

/// Environment override for the durable results log file.
pub const RESULTS_LOG_PATH_ENV: &str = "LOOM_WATCH_RESULTS_LOG";

/// Current on-disk schema version for the watches file.
pub const WATCHES_VERSION: u32 = 1;

/// Environment variable controlling the watch-monitor loop.
///
/// Like [`crate::token_ranking_refresh::TOKEN_RANKING_REFRESH_ENABLE_ENV`] (and
/// unlike the dispatch-affecting work-finder / main-health-gate loops), this loop
/// is **default-on**: it only ever *reads* forge state and appends to a
/// bookkeeping log, has no dispatch side effect, and makes **zero** forge calls
/// when no watches are registered. Set to `0`/`false`/`no`/`off`
/// (case-insensitive) to disable; `1`/`true`/`yes`/`on` to force-enable.
pub const WATCH_MONITOR_ENABLE_ENV: &str = "LOOM_WATCH_MONITOR";

/// Environment override for the monitor cadence (seconds).
pub const WATCH_MONITOR_INTERVAL_ENV: &str = "LOOM_WATCH_MONITOR_INTERVAL_SECS";

/// Environment override for the watch expiry window (seconds); `0` disables
/// expiry (watches poll until they resolve).
pub const WATCH_MONITOR_EXPIRY_ENV: &str = "LOOM_WATCH_MONITOR_EXPIRY_SECS";

/// Default monitor cadence — responsive without being chatty.
pub const DEFAULT_WATCH_MONITOR_INTERVAL_SECS: u64 = 120;

/// Default expiry window: a watch that has not resolved in 24h is recorded as
/// [`WatchOutcome::Expired`] and dropped, so `watches.json` and the forge-call
/// budget stay bounded on a long-lived daemon. `0` disables expiry.
pub const DEFAULT_WATCH_MONITOR_EXPIRY_SECS: u64 = 86_400;

/// How long to wait for a single `gh` probe before killing it.
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

// ============================================================================
// Core types
// ============================================================================

/// Whether a watch targets an issue or a pull request. Governs which `gh`
/// subcommand the probe uses and which terminal states are observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchKind {
    Issue,
    Pr,
}

impl WatchKind {
    /// Lower-case label used in ids and human summaries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Pr => "pr",
        }
    }
}

/// The recorded outcome of a resolved watch.
///
/// `Closed` / `Merged` / `Blocked` are forge-observable terminal states read by
/// the probe. `Expired` is monitor-generated — the daemon gave up after the
/// configured expiry window without observing a terminal state (never a forge
/// state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchOutcome {
    /// The issue or PR was closed (issue closed, or PR closed without merge).
    Closed,
    /// The PR was merged.
    Merged,
    /// The issue/PR carries the `loom:blocked` label (a pending-decision block).
    Blocked,
    /// Monitor gave up after the expiry window with no terminal state observed.
    Expired,
}

impl WatchOutcome {
    /// Lower-case label for human summaries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Merged => "merged",
            Self::Blocked => "blocked",
            Self::Expired => "expired",
        }
    }
}

/// One registered watch. Persisted verbatim to the watches file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchSpec {
    /// Stable opaque id assigned at registration (`watch-<kind>-<number>-<rand>`).
    pub id: String,
    /// Whether this targets an issue or a PR.
    pub kind: WatchKind,
    /// The issue/PR number.
    pub number: u32,
    /// Forge slug `owner/name` (preferred — works cross-repo for a repo this
    /// machine may not manage). `None` ⇒ the probe runs in [`Self::workspace_root`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Workspace root the `gh` query runs in when [`Self::repo`] is absent
    /// (mirrors [`crate::work_finder::forge::GhWorkSource::for_root`]). `None` and
    /// no `repo` ⇒ the daemon's own cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    /// Optional operator note surfaced in the result line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Registration timestamp (drives expiry).
    pub registered_at: DateTime<Utc>,
}

impl WatchSpec {
    /// Human target label, e.g. `issue #6193 in rjwalters/vibesql`.
    #[must_use]
    pub fn target_label(&self) -> String {
        let where_ = self
            .repo
            .clone()
            .or_else(|| self.workspace_root.clone())
            .unwrap_or_else(|| "<default workspace>".to_string());
        format!("{} #{} in {}", self.kind.as_str(), self.number, where_)
    }

    /// The dedup key: two registrations of the same `(target, kind, number)`
    /// collapse to one watch. `target` is the forge slug, else the workspace
    /// root, else the empty string (the daemon's default workspace).
    fn dedup_key(&self) -> (String, WatchKind, u32) {
        let target = self
            .repo
            .clone()
            .or_else(|| self.workspace_root.clone())
            .unwrap_or_default();
        (target, self.kind, self.number)
    }
}

/// A resolved watch, appended as one JSON line to the durable results log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchResult {
    pub id: String,
    pub kind: WatchKind,
    pub number: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub registered_at: DateTime<Utc>,
    pub resolved_at: DateTime<Utc>,
    pub outcome: WatchOutcome,
    /// Pre-rendered human summary line (also embedded so a plain `tail` of the
    /// JSONL log is readable without a parser).
    pub summary: String,
}

/// The full on-disk watches file: a flat list of active [`WatchSpec`]s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchRegistry {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub watches: Vec<WatchSpec>,
}

fn default_version() -> u32 {
    WATCHES_VERSION
}

impl Default for WatchRegistry {
    fn default() -> Self {
        Self {
            version: WATCHES_VERSION,
            watches: Vec::new(),
        }
    }
}

impl WatchRegistry {
    /// Insert a watch, deduplicating on `(target, kind, number)`. Returns the
    /// stored watch (the pre-existing one on a dedup hit) and whether it was new.
    pub fn add(&mut self, spec: WatchSpec) -> (WatchSpec, bool) {
        let key = spec.dedup_key();
        if let Some(existing) = self.watches.iter().find(|w| w.dedup_key() == key) {
            return (existing.clone(), false);
        }
        self.watches.push(spec.clone());
        (spec, true)
    }

    /// Remove a watch by id. Returns `true` if one was removed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.watches.len();
        self.watches.retain(|w| w.id != id);
        before != self.watches.len()
    }
}

// ============================================================================
// Persistence (watches.json)
// ============================================================================

/// Resolve the watches file path: [`WATCHES_PATH_ENV`] override (non-empty), else
/// `~/.loom/watches.json`.
pub fn default_watches_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var(WATCHES_PATH_ENV) {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    Ok(home.join(".loom").join("watches.json"))
}

/// Resolve the results log path: [`RESULTS_LOG_PATH_ENV`] override (non-empty),
/// else `~/.loom/logs/watch-results.log`.
pub fn default_results_log_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var(RESULTS_LOG_PATH_ENV) {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    Ok(home.join(".loom").join("logs").join("watch-results.log"))
}

/// Load the registry from `path`. Tolerant by design: a missing/empty/corrupt
/// file yields an empty registry (a garbled watches file must never wedge the
/// monitor loop), mirroring [`crate::sweep_journal::load`].
#[must_use]
pub fn load(path: &Path) -> WatchRegistry {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return WatchRegistry::default();
            }
            serde_json::from_str(&contents).unwrap_or_else(|e| {
                log::warn!(
                    "watch_registry: corrupt watches file at {} ({e}) — treating as empty",
                    path.display()
                );
                WatchRegistry::default()
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => WatchRegistry::default(),
        Err(e) => {
            log::warn!(
                "watch_registry: failed to read {} ({e}) — treating as empty",
                path.display()
            );
            WatchRegistry::default()
        }
    }
}

/// Persist the registry to `path` atomically (temp file + rename), creating the
/// parent directory as needed. Mirrors [`crate::sweep_journal::save`].
pub fn save(path: &Path, registry: &WatchRegistry) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating watches dir {}", parent.display()))?;
    }
    let mut json = serde_json::to_string_pretty(registry)?;
    json.push('\n');
    // Unique-per-writer temp name: all in-process writers (the monitor task and
    // every IPC handler task) share `std::process::id()`, so a PID-only suffix
    // would collide under intra-daemon concurrency and defeat the atomic-rename
    // guarantee. Appending a fresh uuid makes each writer's temp path unique
    // (belt-and-suspenders alongside the watches-file lock below).
    let tmp = path.with_extension(format!(
        "json.tmp.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, json.as_bytes())
        .with_context(|| format!("writing temp watches file {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Append one [`WatchResult`] as a JSON line to the durable results log at
/// `path`, creating the parent directory as needed. Append-only so concurrent
/// results never clobber one another and the file is a permanent record a later
/// session can read.
pub fn append_result(path: &Path, result: &WatchResult) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating results-log dir {}", parent.display()))?;
    }
    let mut line = serde_json::to_string(result)?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening results log {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("appending to results log {}", path.display()))?;
    Ok(())
}

// ============================================================================
// Concurrency guard (mkdir-based lock on watches.json)
// ============================================================================
//
// `watches.json` has **two independent concurrent writers** inside one daemon
// process: the background monitor loop and the IPC `RegisterWatch`/`RemoveWatch`
// handlers (each IPC connection is its own `tokio::spawn`). Nothing else
// serializes them, so a naive load→modify→save from each has a lost-update
// window (a watch registered while the monitor is mid-tick could be silently
// dropped when the monitor saves). We serialize the whole load→modify→save
// critical section with a `mkdir`-based lock — POSIX-atomic and macOS-safe
// (`flock` is deliberately avoided; not available on stock macOS), matching the
// convention CLAUDE.md documents for `.bad_tokens`/`.allowlist`.

/// How long a writer waits to acquire the watches-file lock before giving up.
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// A held lock older than this is presumed abandoned by a crashed writer and is
/// stolen (an mkdir lock has no OS-level auto-release on process death).
const STALE_LOCK_AGE: Duration = Duration::from_secs(60);

/// Sibling lock-directory path for a watches file (`<path>.lock`).
fn lock_dir_for(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".lock");
    PathBuf::from(os)
}

/// RAII guard holding the mkdir-based watches-file lock; releases (removes the
/// lock directory) on drop.
pub struct WatchLock {
    dir: PathBuf,
}

impl Drop for WatchLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Acquire the mkdir-based lock guarding `watches_path`, retrying until
/// `timeout`. A lock directory older than [`STALE_LOCK_AGE`] is treated as
/// abandoned (a writer crashed before releasing) and stolen.
fn acquire_watch_lock(watches_path: &Path, timeout: Duration) -> Result<WatchLock> {
    let dir = lock_dir_for(watches_path);
    if let Some(parent) = dir.parent() {
        // Best-effort: the save path also creates this, but the lock must exist
        // first, so ensure the parent is present up front.
        let _ = std::fs::create_dir_all(parent);
    }
    let start = Instant::now();
    loop {
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(WatchLock { dir }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Stale-lock breaker: steal a lock whose dir is older than the
                // presumed-crash window so a crashed writer can't wedge us.
                if let Ok(meta) = std::fs::metadata(&dir) {
                    if let Ok(modified) = meta.modified() {
                        if modified
                            .elapsed()
                            .map(|age| age > STALE_LOCK_AGE)
                            .unwrap_or(false)
                        {
                            log::warn!(
                                "watch_registry: stealing stale lock at {} (older than {}s)",
                                dir.display(),
                                STALE_LOCK_AGE.as_secs()
                            );
                            let _ = std::fs::remove_dir(&dir);
                            continue;
                        }
                    }
                }
                if start.elapsed() >= timeout {
                    return Err(anyhow::anyhow!(
                        "timed out after {}s acquiring watches lock at {}",
                        timeout.as_secs(),
                        dir.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("creating lock dir {}", dir.display()))
            }
        }
    }
}

/// Run `f` while holding the watches-file lock (acquired with the default
/// timeout), releasing it on return. The critical section MUST be short — never
/// hold this across a `gh` probe (see [`run_one_tick`], which probes lock-free
/// and only takes the lock for the fast reload-merge-save).
pub fn with_watches_lock<T, F>(watches_path: &Path, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let _guard = acquire_watch_lock(watches_path, DEFAULT_LOCK_TIMEOUT)?;
    f()
}

// ============================================================================
// Probe (forge polling, behind a trait for testability)
// ============================================================================

/// Reads a watch's current terminal state from the forge. Abstracted behind a
/// trait so [`tick`] is unit-testable with a scripted fake, mirroring
/// [`crate::work_finder::WorkSource`] / [`crate::token_ranking_refresh::RankingRefreshRunner`].
pub trait WatchProbe {
    /// Probe `spec`'s current state.
    ///
    /// - `Ok(Some(outcome))` — a terminal state was observed (`Closed` / `Merged`
    ///   / `Blocked`; never `Expired` — that is monitor-generated in [`tick`]).
    /// - `Ok(None)` — still open / non-terminal; keep watching.
    /// - `Err(_)` — the probe failed (network, `gh` missing, transient forge
    ///   error). The caller logs it and retains the watch for the next tick.
    fn probe(&self, spec: &WatchSpec) -> Result<Option<WatchOutcome>>;
}

/// The concrete [`WatchProbe`]: shells out to `gh issue view` / `gh pr view`.
pub struct GhWatchProbe {
    gh_bin: PathBuf,
    timeout: Duration,
}

impl GhWatchProbe {
    /// Construct a probe using `gh` from `PATH`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            gh_bin: PathBuf::from("gh"),
            timeout: DEFAULT_PROBE_TIMEOUT,
        }
    }

    /// Override the `gh` binary (tests / non-standard installs).
    #[must_use]
    pub fn with_gh_bin(mut self, bin: PathBuf) -> Self {
        self.gh_bin = bin;
        self
    }

    /// Build the `gh <issue|pr> view` command for `spec`.
    fn build_command(&self, spec: &WatchSpec) -> Command {
        let mut cmd = Command::new(&self.gh_bin);
        match spec.kind {
            WatchKind::Issue => {
                cmd.arg("issue").arg("view").arg(spec.number.to_string());
                cmd.arg("--json").arg("state,labels");
            }
            WatchKind::Pr => {
                cmd.arg("pr").arg("view").arg(spec.number.to_string());
                cmd.arg("--json").arg("state,labels");
            }
        }
        if let Some(ref repo) = spec.repo {
            cmd.arg("--repo").arg(repo);
        }
        if let Some(ref root) = spec.workspace_root {
            cmd.current_dir(root);
        }
        cmd.stdin(Stdio::null()).stderr(Stdio::piped());
        cmd
    }
}

impl Default for GhWatchProbe {
    fn default() -> Self {
        Self::new()
    }
}

/// Minimal `gh <issue|pr> view --json state,labels` row.
#[derive(Debug, Deserialize)]
struct GhView {
    state: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

/// Classify a `gh view` JSON payload into a terminal outcome, or `None` when the
/// item is still open/non-terminal. Pure so it is exhaustively unit-testable
/// without shelling out. `_kind` is accepted for symmetry / future issue-vs-PR
/// divergence but does not currently affect classification (both surfaces expose
/// `state` + `labels`).
#[must_use]
pub fn classify_view(_kind: WatchKind, json: &[u8]) -> Option<WatchOutcome> {
    let view: GhView = serde_json::from_slice(json).ok()?;
    let blocked = view.labels.iter().any(|l| l.name == "loom:blocked");
    match view.state.to_ascii_uppercase().as_str() {
        "MERGED" => Some(WatchOutcome::Merged),
        "CLOSED" => Some(WatchOutcome::Closed),
        // Still open: a `loom:blocked` label is a terminal "pending decision"
        // state the operator asked to be told about. (Only meaningful while
        // OPEN — a closed item is already reported as Closed above.)
        _ if blocked => Some(WatchOutcome::Blocked),
        _ => None,
    }
}

impl WatchProbe for GhWatchProbe {
    fn probe(&self, spec: &WatchSpec) -> Result<Option<WatchOutcome>> {
        let mut cmd = self.build_command(spec);
        // Wall-clock timeout via a spawned child + poll loop (no extra deps).
        let mut child = cmd
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to invoke {}", self.gh_bin.display()))?;
        let start = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                let output = child.wait_with_output()?;
                if !status.success() {
                    return Err(anyhow::anyhow!(
                        "gh view for {} failed: {}",
                        spec.target_label(),
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
                return Ok(classify_view(spec.kind, &output.stdout));
            }
            if start.elapsed() >= self.timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!(
                    "gh view for {} timed out after {}s",
                    spec.target_label(),
                    self.timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

// ============================================================================
// Tick (pure logic, testable with a fake probe)
// ============================================================================

/// Run one monitor pass over `registry`, resolving every watch that has reached
/// a terminal state (or expired). Returns the resolved [`WatchResult`]s (the
/// caller appends each to the durable log) and **removes** them from `registry`.
///
/// Never panics: a probe error is logged and the watch retained for the next
/// tick. `now` and `expiry` are injected so expiry is deterministically testable
/// (`expiry == 0` disables expiry — watches poll until they resolve).
pub fn tick<P: WatchProbe>(
    registry: &mut WatchRegistry,
    probe: &P,
    now: DateTime<Utc>,
    expiry: Duration,
) -> Vec<WatchResult> {
    let mut resolved: Vec<WatchResult> = Vec::new();
    let mut resolved_ids: Vec<String> = Vec::new();

    for spec in &registry.watches {
        // Terminal-state probe first.
        match probe.probe(spec) {
            Ok(Some(outcome)) => {
                resolved.push(make_result(spec, outcome, now));
                resolved_ids.push(spec.id.clone());
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!(
                    "watch_registry: probe for {} failed (retained, retried next tick): {e}",
                    spec.target_label()
                );
                continue;
            }
        }
        // Not terminal — has it expired?
        if !expiry.is_zero() {
            let age = now.signed_duration_since(spec.registered_at);
            if age.num_seconds() >= 0 && (age.num_seconds() as u64) >= expiry.as_secs() {
                resolved.push(make_result(spec, WatchOutcome::Expired, now));
                resolved_ids.push(spec.id.clone());
            }
        }
    }

    registry.watches.retain(|w| !resolved_ids.contains(&w.id));
    resolved
}

/// Build a [`WatchResult`] with a pre-rendered summary line.
fn make_result(spec: &WatchSpec, outcome: WatchOutcome, now: DateTime<Utc>) -> WatchResult {
    let mut summary = format!("{} {}", spec.target_label(), outcome.as_str());
    if let Some(ref note) = spec.note {
        summary.push_str(&format!(" — {note}"));
    }
    WatchResult {
        id: spec.id.clone(),
        kind: spec.kind,
        number: spec.number,
        repo: spec.repo.clone(),
        workspace_root: spec.workspace_root.clone(),
        note: spec.note.clone(),
        registered_at: spec.registered_at,
        resolved_at: now,
        outcome,
        summary,
    }
}

// ============================================================================
// Registration helper
// ============================================================================

/// Build a fresh [`WatchSpec`] with a generated id and `registered_at = now`.
/// A blank `repo`/`workspace_root`/`note` is normalized to `None`.
#[must_use]
pub fn new_watch(
    kind: WatchKind,
    number: u32,
    repo: Option<String>,
    workspace_root: Option<String>,
    note: Option<String>,
) -> WatchSpec {
    let norm = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    let short = uuid::Uuid::new_v4().simple().to_string();
    let short = &short[..8];
    WatchSpec {
        id: format!("watch-{}-{}-{}", kind.as_str(), number, short),
        kind,
        number,
        repo: norm(repo),
        workspace_root: norm(workspace_root),
        note: norm(note),
        registered_at: Utc::now(),
    }
}

// ============================================================================
// Config (.loom/config.json → autonomous.watchMonitor)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.watchMonitor` this module
/// consumes. Each field is `Option` so an absent key falls through to the
/// env-var / built-in default — precedence **env > config > default** for every
/// knob, matching every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchMonitorConfig {
    /// `autonomous.watchMonitor.enabled` — default **true** (default-on loop).
    pub enabled: Option<bool>,
    /// `autonomous.watchMonitor.intervalSecs` — poll cadence.
    pub interval_secs: Option<u64>,
    /// `autonomous.watchMonitor.expirySecs` — give-up window (`0` disables).
    pub expiry_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.watchMonitor`, soft-failing every field
/// to `None` on any of: missing file, malformed JSON, or a missing
/// `autonomous`/`watchMonitor` block (mirrors
/// [`crate::token_ranking_refresh::read_token_ranking_refresh_config`]).
#[must_use]
pub fn read_watch_monitor_config(repo_root: &Path) -> WatchMonitorConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.watchMonitor")
    else {
        return WatchMonitorConfig::default();
    };
    WatchMonitorConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        // expirySecs 0 is a *meaningful* value (disable expiry), so it is NOT
        // filtered out like intervalSecs.
        expiry_secs: block.get("expirySecs").and_then(serde_json::Value::as_u64),
    }
}

/// Resolve whether the loop is enabled: **env > config > default(true)**.
#[must_use]
pub fn resolve_enabled(config: &WatchMonitorConfig) -> bool {
    if let Ok(v) = std::env::var(WATCH_MONITOR_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the poll cadence: **env > config > default**. A zero/unparseable env
/// value falls through rather than busy-looping.
#[must_use]
pub fn resolve_interval(config: &WatchMonitorConfig) -> Duration {
    std::env::var(WATCH_MONITOR_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.interval_secs)
        .map_or_else(
            || Duration::from_secs(DEFAULT_WATCH_MONITOR_INTERVAL_SECS),
            Duration::from_secs,
        )
}

/// Resolve the expiry window: **env > config > default**. `0` (env or config) is
/// honored as "expiry disabled".
#[must_use]
pub fn resolve_expiry(config: &WatchMonitorConfig) -> Duration {
    let secs = std::env::var(WATCH_MONITOR_EXPIRY_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .or(config.expiry_secs)
        .unwrap_or(DEFAULT_WATCH_MONITOR_EXPIRY_SECS);
    Duration::from_secs(secs)
}

// ============================================================================
// Runtime wiring
// ============================================================================

/// Spawn the watch-monitor loop on the shared daemon runtime. Each tick loads the
/// persisted registry, runs one [`tick`] with the given `probe`, appends every
/// resolved [`WatchResult`] to the durable results log, and re-saves the registry
/// (only when something changed). The probe runs on a blocking thread (it shells
/// out to `gh`) so it never parks a runtime worker.
///
/// A completely empty registry short-circuits to a no-op (no forge calls), so the
/// default-on loop costs a single file read per tick until an operator registers
/// a watch.
pub fn spawn_watch_monitor_task<P>(
    probe: P,
    interval: Duration,
    expiry: Duration,
) -> tokio::task::JoinHandle<()>
where
    P: WatchProbe + Send + Sync + 'static,
{
    log::info!(
        "watch_monitor: starting loop (interval={}s, expiry={}s)",
        interval.as_secs(),
        expiry.as_secs()
    );
    tokio::spawn(async move {
        let probe = std::sync::Arc::new(probe);
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let probe = probe.clone();
            let joined = tokio::task::spawn_blocking(move || run_one_tick(&*probe, expiry)).await;
            if let Err(e) = joined {
                log::error!("watch_monitor: tick task panicked ({e}); stopping loop");
                return;
            }
        }
    })
}

/// Execute a single monitor tick against the default (env-overridable) paths.
/// Best-effort throughout — an I/O error is logged and the tick returns.
pub fn run_one_tick<P: WatchProbe>(probe: &P, expiry: Duration) {
    let watches_path = match default_watches_path() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("watch_monitor: cannot resolve watches path ({e}); skipping tick");
            return;
        }
    };
    // Snapshot lock-free: probing shells out to `gh` (seconds, up to the 30s
    // timeout per watch), and holding the watches lock across that would block
    // every IPC register/remove for the whole tick. We probe against a snapshot
    // and only take the lock for the fast reload-merge-save below.
    let mut registry = load(&watches_path);
    if registry.watches.is_empty() {
        return; // no watches → no forge calls
    }
    let resolved = tick(&mut registry, probe, Utc::now(), expiry);
    if resolved.is_empty() {
        return; // nothing changed → no writes
    }
    // Append to the durable (append-only, O_APPEND) results log first; it is
    // concurrency-safe on its own and never needs the watches lock.
    if let Ok(log_path) = default_results_log_path() {
        for result in &resolved {
            if let Err(e) = append_result(&log_path, result) {
                log::error!("watch_monitor: failed to append result for {}: {e}", result.summary);
            } else {
                log::info!("watch_monitor: {}", result.summary);
            }
        }
    }
    // Persist under the lock, but re-load a FRESH copy from disk first and drop
    // only the ids we actually resolved — never blindly overwrite with our stale
    // in-memory snapshot. An IPC `RegisterWatch` that landed while we were
    // probing is merged in (it is present in the fresh load and is not one of
    // the resolved ids), so a concurrent registration is never lost.
    let resolved_ids: std::collections::HashSet<&str> =
        resolved.iter().map(|r| r.id.as_str()).collect();
    let persist = with_watches_lock(&watches_path, || {
        let mut fresh = load(&watches_path);
        fresh
            .watches
            .retain(|w| !resolved_ids.contains(w.id.as_str()));
        save(&watches_path, &fresh)
    });
    if let Err(e) = persist {
        log::error!("watch_monitor: failed to persist watches after resolution: {e}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::Mutex;
    use tempfile::tempdir;

    fn spec(kind: WatchKind, number: u32, repo: Option<&str>) -> WatchSpec {
        WatchSpec {
            id: format!("watch-{}-{number}", kind.as_str()),
            kind,
            number,
            repo: repo.map(String::from),
            workspace_root: None,
            note: None,
            registered_at: Utc::now(),
        }
    }

    // ---- classify_view ----

    #[test]
    fn classify_closed_issue() {
        let json = br#"{"state":"CLOSED","labels":[]}"#;
        assert_eq!(classify_view(WatchKind::Issue, json), Some(WatchOutcome::Closed));
    }

    #[test]
    fn classify_merged_pr() {
        let json = br#"{"state":"MERGED","labels":[]}"#;
        assert_eq!(classify_view(WatchKind::Pr, json), Some(WatchOutcome::Merged));
    }

    #[test]
    fn classify_open_is_none() {
        let json = br#"{"state":"OPEN","labels":[{"name":"loom:building"}]}"#;
        assert_eq!(classify_view(WatchKind::Issue, json), None);
    }

    #[test]
    fn classify_open_but_blocked() {
        let json = br#"{"state":"OPEN","labels":[{"name":"loom:blocked"}]}"#;
        assert_eq!(classify_view(WatchKind::Issue, json), Some(WatchOutcome::Blocked));
    }

    #[test]
    fn classify_closed_takes_precedence_over_blocked_label() {
        // A closed item that also carries loom:blocked reports Closed (its real
        // terminal state), not Blocked.
        let json = br#"{"state":"CLOSED","labels":[{"name":"loom:blocked"}]}"#;
        assert_eq!(classify_view(WatchKind::Issue, json), Some(WatchOutcome::Closed));
    }

    #[test]
    fn classify_garbage_is_none() {
        assert_eq!(classify_view(WatchKind::Issue, b"not json"), None);
    }

    // ---- persistence ----

    #[test]
    fn load_missing_is_empty() {
        let dir = tempdir().unwrap();
        let reg = load(&dir.path().join("watches.json"));
        assert!(reg.watches.is_empty());
        assert_eq!(reg.version, WATCHES_VERSION);
    }

    #[test]
    fn load_corrupt_is_empty_not_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("watches.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load(&path).watches.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("watches.json");
        let mut reg = WatchRegistry::default();
        reg.watches
            .push(spec(WatchKind::Issue, 6193, Some("rjwalters/vibesql")));
        save(&path, &reg).unwrap();
        let back = load(&path);
        assert_eq!(back.watches.len(), 1);
        assert_eq!(back.watches[0].number, 6193);
        assert_eq!(back.watches[0].repo.as_deref(), Some("rjwalters/vibesql"));
    }

    #[test]
    fn append_result_is_jsonl() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("logs").join("watch-results.log");
        let r1 =
            make_result(&spec(WatchKind::Issue, 1, Some("a/b")), WatchOutcome::Closed, Utc::now());
        let r2 =
            make_result(&spec(WatchKind::Pr, 2, Some("a/b")), WatchOutcome::Merged, Utc::now());
        append_result(&path, &r1).unwrap();
        append_result(&path, &r2).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: WatchResult = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed.outcome, WatchOutcome::Closed);
        assert!(lines[1].contains("merged"));
    }

    // ---- dedup ----

    #[test]
    fn add_dedups_on_target_kind_number() {
        let mut reg = WatchRegistry::default();
        let (_, new1) = reg.add(new_watch(WatchKind::Issue, 42, Some("a/b".into()), None, None));
        assert!(new1);
        let (existing, new2) =
            reg.add(new_watch(WatchKind::Issue, 42, Some("a/b".into()), None, None));
        assert!(!new2, "same target/kind/number should dedup");
        assert_eq!(reg.watches.len(), 1);
        assert_eq!(existing.number, 42);
        // Different repo is a distinct watch.
        let (_, new3) = reg.add(new_watch(WatchKind::Issue, 42, Some("c/d".into()), None, None));
        assert!(new3);
        assert_eq!(reg.watches.len(), 2);
        // Different kind is distinct.
        let (_, new4) = reg.add(new_watch(WatchKind::Pr, 42, Some("a/b".into()), None, None));
        assert!(new4);
        assert_eq!(reg.watches.len(), 3);
    }

    #[test]
    fn remove_by_id() {
        let mut reg = WatchRegistry::default();
        let (s, _) = reg.add(new_watch(WatchKind::Issue, 1, None, None, None));
        assert!(reg.remove(&s.id));
        assert!(!reg.remove(&s.id));
        assert!(reg.watches.is_empty());
    }

    // ---- tick ----

    /// Fake probe returning scripted outcomes keyed by watch number.
    struct FakeProbe {
        by_number: std::collections::HashMap<u32, Result<Option<WatchOutcome>, ()>>,
        calls: Mutex<Vec<u32>>,
    }
    impl WatchProbe for FakeProbe {
        fn probe(&self, spec: &WatchSpec) -> Result<Option<WatchOutcome>> {
            self.calls.lock().unwrap().push(spec.number);
            match self.by_number.get(&spec.number) {
                Some(Ok(o)) => Ok(*o),
                Some(Err(())) => Err(anyhow::anyhow!("boom")),
                None => Ok(None),
            }
        }
    }

    #[test]
    fn tick_resolves_terminal_and_retains_open() {
        let mut reg = WatchRegistry::default();
        reg.add(new_watch(
            WatchKind::Issue,
            1,
            Some("a/b".into()),
            None,
            Some("done soon".into()),
        ));
        reg.add(new_watch(WatchKind::Issue, 2, Some("a/b".into()), None, None));
        reg.add(new_watch(WatchKind::Pr, 3, Some("a/b".into()), None, None));

        let mut by_number = std::collections::HashMap::new();
        by_number.insert(1, Ok(Some(WatchOutcome::Closed)));
        by_number.insert(2, Ok(None)); // still open
        by_number.insert(3, Ok(Some(WatchOutcome::Merged)));
        let probe = FakeProbe {
            by_number,
            calls: Mutex::new(Vec::new()),
        };

        let resolved = tick(&mut reg, &probe, Utc::now(), Duration::from_secs(0));
        assert_eq!(resolved.len(), 2);
        // The still-open watch (#2) remains.
        assert_eq!(reg.watches.len(), 1);
        assert_eq!(reg.watches[0].number, 2);
        // Summary embeds the note.
        let closed = resolved.iter().find(|r| r.number == 1).unwrap();
        assert_eq!(closed.outcome, WatchOutcome::Closed);
        assert!(closed.summary.contains("done soon"), "{}", closed.summary);
    }

    #[test]
    fn tick_retains_watch_on_probe_error() {
        let mut reg = WatchRegistry::default();
        reg.add(new_watch(WatchKind::Issue, 9, Some("a/b".into()), None, None));
        let mut by_number = std::collections::HashMap::new();
        by_number.insert(9u32, Err(()));
        let probe = FakeProbe {
            by_number,
            calls: Mutex::new(Vec::new()),
        };
        let resolved = tick(&mut reg, &probe, Utc::now(), Duration::from_secs(0));
        assert!(resolved.is_empty());
        assert_eq!(reg.watches.len(), 1, "a failing probe retains the watch");
    }

    #[test]
    fn tick_expires_stale_watch() {
        let mut reg = WatchRegistry::default();
        let mut s = new_watch(WatchKind::Issue, 5, Some("a/b".into()), None, None);
        s.registered_at = Utc::now() - chrono::Duration::seconds(1000);
        reg.watches.push(s);
        let probe = FakeProbe {
            by_number: std::collections::HashMap::new(), // always Ok(None)
            calls: Mutex::new(Vec::new()),
        };
        // expiry 500s, watch is 1000s old → expired.
        let resolved = tick(&mut reg, &probe, Utc::now(), Duration::from_secs(500));
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].outcome, WatchOutcome::Expired);
        assert!(reg.watches.is_empty());
    }

    #[test]
    fn tick_expiry_zero_never_expires() {
        let mut reg = WatchRegistry::default();
        let mut s = new_watch(WatchKind::Issue, 5, Some("a/b".into()), None, None);
        s.registered_at = Utc::now() - chrono::Duration::seconds(1_000_000);
        reg.watches.push(s);
        let probe = FakeProbe {
            by_number: std::collections::HashMap::new(),
            calls: Mutex::new(Vec::new()),
        };
        let resolved = tick(&mut reg, &probe, Utc::now(), Duration::from_secs(0));
        assert!(resolved.is_empty());
        assert_eq!(reg.watches.len(), 1, "expiry=0 disables expiry");
    }

    // ---- config precedence ----

    fn write_config(root: &Path, contents: &str) {
        std::fs::create_dir_all(root.join(".loom")).unwrap();
        std::fs::write(root.join(".loom").join("config.json"), contents).unwrap();
    }

    #[test]
    fn config_missing_is_default() {
        let dir = tempdir().unwrap();
        assert_eq!(read_watch_monitor_config(dir.path()), WatchMonitorConfig::default());
    }

    #[test]
    fn config_reads_all_fields() {
        let dir = tempdir().unwrap();
        write_config(
            dir.path(),
            r#"{"autonomous":{"watchMonitor":{"enabled":false,"intervalSecs":60,"expirySecs":0}}}"#,
        );
        assert_eq!(
            read_watch_monitor_config(dir.path()),
            WatchMonitorConfig {
                enabled: Some(false),
                interval_secs: Some(60),
                expiry_secs: Some(0),
            }
        );
    }

    // ===================================================================
    // config_resolver migration (#4058) — tier precedence
    // ===================================================================

    fn write_project_config(root: &Path, contents: &str) {
        let full = root.join(crate::config_resolver::PROJECT_CONFIG_REL);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    fn write_local_config(root: &Path, contents: &str) {
        let full = root.join(crate::config_resolver::LOCAL_CONFIG_REL);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    #[test]
    #[serial(loom_config_env)]
    fn config_project_tier_only_is_honored_like_legacy() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        write_project_config(
            dir.path(),
            r#"{"autonomous":{"watchMonitor":{"enabled":false,"intervalSecs":60,"expirySecs":0}}}"#,
        );
        let cfg = read_watch_monitor_config(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(
            cfg,
            WatchMonitorConfig {
                enabled: Some(false),
                interval_secs: Some(60),
                expiry_secs: Some(0),
            }
        );
    }

    #[test]
    #[serial(loom_config_env)]
    fn config_project_tier_overrides_legacy_overlap_and_supplies_non_overlap() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        write_config(
            dir.path(),
            r#"{"autonomous":{"watchMonitor":{"enabled":true,"intervalSecs":60}}}"#,
        );
        write_project_config(dir.path(), r#"{"autonomous":{"watchMonitor":{"intervalSecs":15}}}"#);
        let cfg = read_watch_monitor_config(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        // Overlapping `intervalSecs` -> project tier wins.
        assert_eq!(cfg.interval_secs, Some(15));
        // Non-overlapping `enabled` still supplied by legacy tier.
        assert_eq!(cfg.enabled, Some(true));
    }

    #[test]
    #[serial(loom_config_env)]
    fn config_local_tier_overrides_legacy_and_project() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"autonomous":{"watchMonitor":{"intervalSecs":60}}}"#);
        write_project_config(dir.path(), r#"{"autonomous":{"watchMonitor":{"intervalSecs":30}}}"#);
        write_local_config(dir.path(), r#"{"autonomous":{"watchMonitor":{"intervalSecs":5}}}"#);
        let cfg = read_watch_monitor_config(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.interval_secs, Some(5));
    }

    /// Regression (#4058): `expirySecs: 0` set only at the project tier must
    /// still be read as `Some(0)` ("disable expiry"), not dropped to `None`
    /// like a zero `intervalSecs` would be. `resolve_expiry` itself (which
    /// turns `Some(0)` into a zero `Duration`) is unmodified by this
    /// migration and already covered by `resolve_expiry_zero_is_honored`
    /// above — this test isolates the tier-migrated read path and
    /// deliberately avoids touching `WATCH_MONITOR_EXPIRY_ENV` (that test
    /// mutates it under the crate-wide unkeyed `#[serial]` lock, which does
    /// not mutually exclude against this test's keyed
    /// `#[serial(loom_config_env)]` lock).
    #[test]
    #[serial(loom_config_env)]
    fn config_project_tier_expiry_secs_zero_is_meaningful() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        write_project_config(dir.path(), r#"{"autonomous":{"watchMonitor":{"expirySecs":0}}}"#);
        let cfg = read_watch_monitor_config(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.expiry_secs, Some(0));
    }

    #[test]
    #[serial]
    fn resolve_enabled_default_true() {
        std::env::remove_var(WATCH_MONITOR_ENABLE_ENV);
        assert!(resolve_enabled(&WatchMonitorConfig::default()));
    }

    #[test]
    #[serial]
    fn resolve_enabled_env_overrides_config() {
        std::env::set_var(WATCH_MONITOR_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&WatchMonitorConfig {
            enabled: Some(true),
            ..Default::default()
        }));
        std::env::set_var(WATCH_MONITOR_ENABLE_ENV, "1");
        assert!(resolve_enabled(&WatchMonitorConfig {
            enabled: Some(false),
            ..Default::default()
        }));
        std::env::remove_var(WATCH_MONITOR_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn resolve_interval_precedence() {
        std::env::remove_var(WATCH_MONITOR_INTERVAL_ENV);
        assert_eq!(
            resolve_interval(&WatchMonitorConfig::default()),
            Duration::from_secs(DEFAULT_WATCH_MONITOR_INTERVAL_SECS)
        );
        assert_eq!(
            resolve_interval(&WatchMonitorConfig {
                interval_secs: Some(300),
                ..Default::default()
            }),
            Duration::from_secs(300)
        );
        std::env::set_var(WATCH_MONITOR_INTERVAL_ENV, "45");
        assert_eq!(
            resolve_interval(&WatchMonitorConfig {
                interval_secs: Some(300),
                ..Default::default()
            }),
            Duration::from_secs(45)
        );
        std::env::remove_var(WATCH_MONITOR_INTERVAL_ENV);
    }

    #[test]
    #[serial]
    fn resolve_expiry_zero_is_honored() {
        std::env::remove_var(WATCH_MONITOR_EXPIRY_ENV);
        // Config 0 → disabled (not treated as "unset").
        assert_eq!(
            resolve_expiry(&WatchMonitorConfig {
                expiry_secs: Some(0),
                ..Default::default()
            }),
            Duration::from_secs(0)
        );
        // Absent everywhere → default.
        assert_eq!(
            resolve_expiry(&WatchMonitorConfig::default()),
            Duration::from_secs(DEFAULT_WATCH_MONITOR_EXPIRY_SECS)
        );
        // Env overrides config.
        std::env::set_var(WATCH_MONITOR_EXPIRY_ENV, "10");
        assert_eq!(
            resolve_expiry(&WatchMonitorConfig {
                expiry_secs: Some(999),
                ..Default::default()
            }),
            Duration::from_secs(10)
        );
        std::env::remove_var(WATCH_MONITOR_EXPIRY_ENV);
    }

    // ---- run_one_tick integration (env-driven paths) ----

    #[test]
    #[serial]
    fn run_one_tick_appends_and_persists() {
        let dir = tempdir().unwrap();
        let watches = dir.path().join("watches.json");
        let results = dir.path().join("watch-results.log");
        std::env::set_var(WATCHES_PATH_ENV, &watches);
        std::env::set_var(RESULTS_LOG_PATH_ENV, &results);

        let mut reg = WatchRegistry::default();
        reg.add(new_watch(WatchKind::Issue, 6193, Some("rjwalters/vibesql".into()), None, None));
        reg.add(new_watch(WatchKind::Issue, 3964, Some("rjwalters/loom".into()), None, None));
        save(&watches, &reg).unwrap();

        let mut by_number = std::collections::HashMap::new();
        by_number.insert(6193u32, Ok(Some(WatchOutcome::Closed)));
        by_number.insert(3964u32, Ok(None)); // still open
        let probe = FakeProbe {
            by_number,
            calls: Mutex::new(Vec::new()),
        };

        run_one_tick(&probe, Duration::from_secs(0));

        // One result appended; the still-open watch persists.
        let log = std::fs::read_to_string(&results).unwrap();
        assert_eq!(log.lines().count(), 1);
        assert!(log.contains("6193"));
        let after = load(&watches);
        assert_eq!(after.watches.len(), 1);
        assert_eq!(after.watches[0].number, 3964);

        std::env::remove_var(WATCHES_PATH_ENV);
        std::env::remove_var(RESULTS_LOG_PATH_ENV);
    }

    #[test]
    #[serial]
    fn run_one_tick_empty_registry_is_noop() {
        let dir = tempdir().unwrap();
        let watches = dir.path().join("watches.json");
        let results = dir.path().join("watch-results.log");
        std::env::set_var(WATCHES_PATH_ENV, &watches);
        std::env::set_var(RESULTS_LOG_PATH_ENV, &results);

        // No watches file at all.
        let probe = FakeProbe {
            by_number: std::collections::HashMap::new(),
            calls: Mutex::new(Vec::new()),
        };
        run_one_tick(&probe, Duration::from_secs(0));
        assert!(!results.exists(), "no results log written for an empty registry");
        assert!(probe.calls.lock().unwrap().is_empty(), "no probe calls for empty registry");

        std::env::remove_var(WATCHES_PATH_ENV);
        std::env::remove_var(RESULTS_LOG_PATH_ENV);
    }

    // ---- concurrency: register-during-resolving-tick (Issue #3971 durability) ----

    /// Regression for the lost-update window the Judge flagged: a
    /// `RegisterWatch` that lands while the monitor is mid-tick (blocked on a
    /// slow `gh` probe) must survive the tick's subsequent save. Without the
    /// reload-merge under the watches-file lock, the monitor would persist its
    /// stale in-memory snapshot and silently drop the just-registered watch.
    ///
    /// The `RegisteringProbe` stands in for the racing IPC handler: while
    /// probing watch A (which resolves `Closed`), it performs exactly the
    /// lock→load→add→save that `handle_register_watch` does, registering B. B
    /// must still be present after the tick completes.
    #[test]
    #[serial]
    fn concurrent_register_during_resolving_tick_is_not_lost() {
        let dir = tempdir().unwrap();
        let watches = dir.path().join("watches.json");
        let results = dir.path().join("watch-results.log");
        std::env::set_var(WATCHES_PATH_ENV, &watches);
        std::env::set_var(RESULTS_LOG_PATH_ENV, &results);

        // Registry starts with A (#1), which resolves Closed this tick.
        let mut reg = WatchRegistry::default();
        reg.add(new_watch(WatchKind::Issue, 1, Some("a/b".into()), None, None));
        save(&watches, &reg).unwrap();

        struct RegisteringProbe {
            watches: PathBuf,
        }
        impl WatchProbe for RegisteringProbe {
            fn probe(&self, _spec: &WatchSpec) -> Result<Option<WatchOutcome>> {
                // Simulate a concurrent RegisterWatch(B, #2) landing mid-probe,
                // via the same guarded critical section the IPC handler uses.
                with_watches_lock(&self.watches, || {
                    let mut r = load(&self.watches);
                    r.add(new_watch(WatchKind::Issue, 2, Some("a/b".into()), None, None));
                    save(&self.watches, &r)
                })
                .unwrap();
                Ok(Some(WatchOutcome::Closed))
            }
        }

        run_one_tick(
            &RegisteringProbe {
                watches: watches.clone(),
            },
            Duration::from_secs(0),
        );

        // A (#1) resolved and was dropped; B (#2), registered mid-tick, survives.
        let after = load(&watches);
        let numbers: Vec<u32> = after.watches.iter().map(|w| w.number).collect();
        assert_eq!(numbers, vec![2], "watch registered during a resolving tick must not be lost");

        std::env::remove_var(WATCHES_PATH_ENV);
        std::env::remove_var(RESULTS_LOG_PATH_ENV);
    }
}
