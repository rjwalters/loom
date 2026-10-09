//! Safe-point pause for a daemon roll (issue #10830; design
//! `docs/design/daemon-roll-pause-resume.md` §2).
//!
//! A roll stops every daemon-dispatched agent at a **safe point**, a moment
//! when none of its leaf tool calls is executing, and later resumes it from its
//! saved session. This module is the agent-side half of that: the pre-/post-
//! tool-use hook (`loom-daemon roll-pause hook`), the per-item state files the
//! daemon and the hook share, and (in [`resume`]) the session handles and the
//! resume prompt the spawn scripts use.
//!
//! H4 (#10831) writes pause requests and polls safe points; H5 (#10832)
//! resumes. [`suppress`] keeps restart recovery off paused agents in between.
//!
//! # Inert by construction
//!
//! The hook does nothing unless [`ITEM_ENV`] names the agent. Only the daemon's
//! dispatch sets it, so an in-session or attended agent is never parked and
//! never counted. With the item id set, the hook still only records a ledger
//! entry until a pause request exists for that item.
//!
//! # State layout
//!
//! Per item, under [`DIR_ENV`] (default `<root>/.loom/state/roll-pause`, which
//! is gitignored and, being under the workspace, is visible at the same path
//! inside a session container):
//!
//! | Path | Writer | Meaning |
//! |---|---|---|
//! | `request` | daemon | a pause is requested; removing it withdraws the request |
//! | `inflight/<key>` | hook | one file per executing leaf tool call (the ledger) |
//! | `parked/<key>` | hook | one file per parked call |
//! | `safe-point.json` | hook | written once per request, atomically, when a call parks with an empty ledger |
//! | `handle.json` | spawn script | the live-captured resume handle (Codex) |
//! | `claim.json` | hook | the claim label the agent took, if any ([`claim_breadcrumb`]) |
//!
//! A file per in-flight call, not a counter, so concurrent hooks never race on
//! a read-modify-write: the count is the directory listing.
//!
//! # A safe point answers one request (Judge finding 2 on #10974)
//!
//! A safe-point record says "this agent is parked, with nothing running, for
//! *this* pause request". It says nothing about a later request: by then the
//! parked call has been released and the agent is running again. So the
//! record carries the id of the request it answered
//! ([`SafePoint::request_id`], the request's `manifest_id`), and:
//!
//! - the daemon accepts a record only for its current request
//!   ([`read_safe_point_for`]);
//! - the hook removes the record when its parked call is released (the
//!   request was withdrawn), and replaces a record left by another request;
//! - a pause that stands down removes its own record with its request
//!   ([`stand_down`]), and a new request clears whatever record it finds
//!   ([`request_pause`]).
//!
//! A record with no id comes from a hook older than this rule (a session
//! container on an older image). It is accepted only when it was written at or
//! after the request was raised; `request_pause` has already removed any that
//! predates it.

pub mod claim_breadcrumb;
pub mod hold;
pub mod live_runs;
pub mod resume;
pub mod suppress;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The daemon-dispatched item this agent belongs to. Unset means "not the
/// daemon's agent": the hook is then a no-op.
pub const ITEM_ENV: &str = "LOOM_DAEMON_ITEM_ID";
/// Root of the per-item pause state.
pub const DIR_ENV: &str = "LOOM_ROLL_PAUSE_DIR";
/// `0` disables the in-flight ledger (Codex, whose managed hook entry is
/// pre-tool-use only; see `guard-codex-bridge.sh`).
pub const LEDGER_ENV: &str = "LOOM_ROLL_PAUSE_LEDGER";
/// How long one parked call is held before it is denied.
pub const PARK_SECS_ENV: &str = "LOOM_ROLL_PAUSE_PARK_SECS";
/// Runtime name recorded in the safe-point record (`claude` / `codex`).
pub const RUNTIME_ENV: &str = "LOOM_ROLL_PAUSE_RUNTIME";
/// Poll interval while parked, in milliseconds (tests shorten it).
pub const POLL_MS_ENV: &str = "LOOM_ROLL_PAUSE_POLL_MS";

/// Default park window. Claude Code's default hook timeout is 60 s and the
/// installer's generated entries carry no `timeout`, so the park must end
/// first: a hook that times out is an error, not a decision. The daemon stops
/// the tree within seconds of the safe-point record, so this only bounds the
/// case where it does not.
pub const DEFAULT_PARK_SECS: u64 = 50;
/// The longest park [`PARK_SECS_ENV`] may ask for (#10831). Claude Code's hook
/// timeout is 60 s and the generated hook entries set none, so a longer park
/// would turn the pause into a hook-timeout error instead of a decision: the
/// env value is clamped here rather than trusted.
pub const MAX_PARK_SECS: u64 = 55;
/// An in-flight ledger entry older than this no longer counts as executing
/// (#10831). A call that another guard hook denied gets its pre-tool-use
/// entry here but never a post-tool-use event, so without an age limit it
/// would hold the ledger open and the agent could never reach a safe point.
/// The default is above Claude Code's longest Bash timeout (10 min), so a
/// genuinely running call is never aged out; a denied entry younger than this
/// still blocks the safe point, and the daemon then requeues the agent at the
/// pause budget (`pause-budget-missed`) rather than wait.
pub const DEFAULT_INFLIGHT_STALE_SECS: u64 = 660;
/// Env override for [`DEFAULT_INFLIGHT_STALE_SECS`].
pub const INFLIGHT_STALE_SECS_ENV: &str = "LOOM_ROLL_PAUSE_INFLIGHT_STALE_SECS";
const DEFAULT_POLL_MS: u64 = 500;

pub const REQUEST_FILE: &str = "request";
pub const INFLIGHT_DIR: &str = "inflight";
pub const PARKED_DIR: &str = "parked";
pub const SAFE_POINT_FILE: &str = "safe-point.json";
pub const HANDLE_FILE: &str = "handle.json";

/// The deny reason a parked call gets when the park window runs out.
pub const DENY_REASON: &str = "Loom: paused for a daemon roll. This tool call did not run. \
Do not retry it now: end your turn. The session will be resumed after the roll, and you will \
re-run it then.";

/// Whether `id` is usable as an item directory name.
#[must_use]
pub fn valid_item_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id.len() <= 200
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The default pause root for a workspace: `<root>/.loom/state/roll-pause`.
#[must_use]
pub fn default_pause_root(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".loom")
        .join("state")
        .join("roll-pause")
}

/// The state directory of one item.
#[must_use]
pub fn item_dir(pause_root: &Path, item: &str) -> PathBuf {
    pause_root.join(item)
}

/// A pause request, as the daemon writes it. Its presence is the signal; the
/// fields are for the audit trail and the resume prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseRequest {
    pub requested_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_id: Option<String>,
}

/// The record the hook writes the first time a call parks while no other leaf
/// call is executing. The daemon polls for it before stopping the tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafePoint {
    pub reached_at: String,
    pub parked_tool: String,
    pub parked_summary: String,
    pub parked_tool_use_id: String,
    #[serde(default)]
    pub harness_pid: Option<u32>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub runtime: String,
    /// The `manifest_id` of the pause request this record answers. `None`
    /// from a hook that predates the field, or for a request with no id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Write `bytes` to `path` atomically: a temp file in the same directory,
/// `fsync`, then `rename`. A reader sees the old file or the new one, never a
/// partial write.
///
/// # Errors
/// Any I/O failure creating, writing, syncing or renaming the file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    // #10831: make the rename itself durable, not only the file's bytes.
    // Best-effort where a directory cannot be opened for sync.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Raise a pause request for an item (daemon side). A safe-point record
/// already in the item dir answered an earlier request and is removed first:
/// no call can be parked for a request that does not exist yet.
///
/// # Errors
/// When the request cannot be written.
pub fn request_pause(item_dir: &Path, request: &PauseRequest) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(request).map_err(std::io::Error::other)?;
    clear_safe_point(item_dir);
    write_atomic(&item_dir.join(REQUEST_FILE), &body)
}

/// The item's pause request, if one is raised and readable.
#[must_use]
pub fn read_request(item_dir: &Path) -> Option<PauseRequest> {
    let raw = std::fs::read_to_string(item_dir.join(REQUEST_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Remove the item's safe-point record, whatever request it answered.
pub fn clear_safe_point(item_dir: &Path) {
    let _ = std::fs::remove_file(item_dir.join(SAFE_POINT_FILE));
}

/// Whether the request on disk is the one `manifest_id` raised. A request
/// that cannot be read is nobody's.
fn request_is(item_dir: &Path, manifest_id: &str) -> bool {
    read_request(item_dir).is_some_and(|r| r.manifest_id.as_deref() == Some(manifest_id))
}

/// Withdraw the pause request `manifest_id` raised, and only that one: a
/// request another pause has raised since is left alone. The safe-point
/// record stays (the manifest of a completed pause refers to it).
pub fn withdraw_for(item_dir: &Path, manifest_id: &str) {
    if request_is(item_dir, manifest_id) {
        let _ = withdraw(item_dir);
    }
}

/// Undo `manifest_id`'s pause of one item: withdraw its request and remove
/// the safe-point record that answered it. A request or record belonging to
/// another pause is left alone.
pub fn stand_down(item_dir: &Path, manifest_id: &str) {
    withdraw_for(item_dir, manifest_id);
    if read_safe_point(item_dir).is_some_and(|sp| sp.request_id.as_deref() == Some(manifest_id)) {
        clear_safe_point(item_dir);
    }
}

/// Withdraw a pause request. Parked calls are then released (allowed).
///
/// # Errors
/// Any failure other than the request already being absent.
pub fn withdraw(item_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(item_dir.join(REQUEST_FILE)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Whether a pause request exists for the item.
#[must_use]
pub fn is_requested(item_dir: &Path) -> bool {
    item_dir.join(REQUEST_FILE).is_file()
}

/// The item's safe-point record, if the hook has written one. It may answer
/// any request: a caller acting on it wants [`read_safe_point_for`].
#[must_use]
pub fn read_safe_point(item_dir: &Path) -> Option<SafePoint> {
    let raw = std::fs::read_to_string(item_dir.join(SAFE_POINT_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The item's safe-point record **for `request`**, or `None` when there is no
/// record or it answered another request (see the module doc).
#[must_use]
pub fn read_safe_point_for(item_dir: &Path, request: &PauseRequest) -> Option<SafePoint> {
    let sp = read_safe_point(item_dir)?;
    let answers = match (&sp.request_id, &request.manifest_id) {
        (Some(have), Some(want)) => have == want,
        // A hook that predates the id: only a record no older than the request.
        (None, _) => {
            let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
            matches!((at(&sp.reached_at), at(&request.requested_at)), (Some(r), Some(q)) if r >= q)
        }
        (Some(_), None) => false,
    };
    answers.then_some(sp)
}

/// Number of leaf tool calls currently executing, excluding `except`. Entries
/// older than the in-flight stale limit ([`INFLIGHT_STALE_SECS_ENV`]) are not
/// counted: they are calls that never got a post-tool-use event (#10831).
#[must_use]
pub fn inflight_count(item_dir: &Path, except: Option<&str>) -> usize {
    let stale = Duration::from_secs(
        std::env::var(INFLIGHT_STALE_SECS_ENV)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_INFLIGHT_STALE_SECS),
    );
    inflight_count_with(item_dir, except, stale)
}

/// [`inflight_count`] with an explicit stale limit.
#[must_use]
pub fn inflight_count_with(item_dir: &Path, except: Option<&str>, stale: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(item_dir.join(INFLIGHT_DIR)) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    entries
        .filter_map(Result::ok)
        .filter(|e| except.is_none_or(|k| e.file_name().to_string_lossy() != k))
        .filter(|e| {
            // An unreadable mtime counts as executing: never age out a call
            // we cannot date.
            e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_none_or(|age| age < stale)
        })
        .count()
}

/// Everything the hook reads from its environment, resolved once.
#[derive(Debug, Clone)]
pub struct HookEnv {
    /// [`ITEM_ENV`]; `None` makes the hook inert.
    pub item: Option<String>,
    /// The resolved pause root.
    pub pause_root: PathBuf,
    pub ledger: bool,
    pub park: Duration,
    pub poll: Duration,
    pub runtime: String,
    pub harness_pid: Option<u32>,
}

impl HookEnv {
    /// Resolve from the process environment. The pause root is [`DIR_ENV`],
    /// else `LOOM_PROJECT_ROOT` or the git main checkout of the cwd.
    #[must_use]
    pub fn from_env(harness_pid: Option<u32>) -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let item = var(ITEM_ENV).filter(|i| valid_item_id(i));
        let pause_root = match (var(DIR_ENV), &item) {
            (Some(dir), _) => PathBuf::from(dir),
            (None, Some(_)) => default_pause_root(&project_root()),
            (None, None) => PathBuf::new(),
        };
        let secs = |k: &str, d: u64| var(k).and_then(|v| v.trim().parse().ok()).unwrap_or(d);
        HookEnv {
            item,
            pause_root,
            ledger: var(LEDGER_ENV).is_none_or(|v| v.trim() != "0"),
            park: Duration::from_secs(secs(PARK_SECS_ENV, DEFAULT_PARK_SECS).min(MAX_PARK_SECS)),
            poll: Duration::from_millis(secs(POLL_MS_ENV, DEFAULT_POLL_MS).max(10)),
            runtime: var(RUNTIME_ENV).unwrap_or_else(|| "claude".to_string()),
            harness_pid,
        }
    }
}

fn project_root() -> PathBuf {
    if let Ok(root) = std::env::var("LOOM_PROJECT_ROOT") {
        if !root.trim().is_empty() {
            return PathBuf::from(root);
        }
    }
    let common = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    match common.as_deref().and_then(Path::parent) {
        Some(root) => root.to_path_buf(),
        None => std::env::current_dir().unwrap_or_default(),
    }
}

/// The hook's decision for one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// No output: other hooks decide.
    Allow,
    /// A deny decision with this reason.
    Deny(String),
}

impl HookOutcome {
    /// The Claude-shaped `PreToolUse` decision JSON, or `None` for allow.
    /// `guard-codex-bridge.sh` reads the same shape.
    #[must_use]
    pub fn to_json(&self) -> Option<String> {
        match self {
            HookOutcome::Allow => None,
            HookOutcome::Deny(reason) => Some(
                serde_json::json!({"hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }})
                .to_string(),
            ),
        }
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Subagent containers stay "executing" for as long as their subagent runs, so
/// they would hold the ledger open for the whole subagent: never count them.
fn is_container_call(tool: &str) -> bool {
    matches!(tool, "Task" | "Agent")
}

fn ledger_key(payload: &serde_json::Value) -> String {
    let raw = match payload.get("tool_use_id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id.to_string(),
        // No id: hash the call so the pre and post events still pair up.
        _ => {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            payload
                .get("tool_name")
                .map(ToString::to_string)
                .hash(&mut h);
            payload
                .get("tool_input")
                .map(ToString::to_string)
                .hash(&mut h);
            format!("h{:016x}", h.finish())
        }
    };
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn summary(payload: &serde_json::Value) -> String {
    let input = payload.get("tool_input");
    let pick = ["command", "cmd", "file_path", "description"]
        .iter()
        .find_map(|k| {
            input
                .and_then(|i| i.get(*k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let s = pick.unwrap_or_else(|| input.map(ToString::to_string).unwrap_or_default());
    s.chars().take(300).collect()
}

fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, b"");
}

/// Write the safe-point record unless one already exists for the same
/// request. Exactly one parked call wins: the record is linked into place,
/// which fails if it exists. A record that answered another request is stale
/// and is removed first.
fn write_safe_point(dir: &Path, sp: &SafePoint) {
    let target = dir.join(SAFE_POINT_FILE);
    if target.exists() {
        if read_safe_point(dir).is_some_and(|have| have.request_id == sp.request_id) {
            return;
        }
        let _ = std::fs::remove_file(&target);
    }
    let Ok(body) = serde_json::to_vec(sp) else {
        return;
    };
    let Ok(mut tmp) = tempfile::NamedTempFile::new_in(dir) else {
        return;
    };
    if tmp.write_all(&body).is_err() || tmp.as_file().sync_all().is_err() {
        return;
    }
    let _ = std::fs::hard_link(tmp.path(), &target);
}

/// Run the hook for one event payload. Never fails: every unexpected input is
/// an allow, because a pause hook that errors must not block the agent.
#[must_use]
pub fn run_hook(env: &HookEnv, payload: &str) -> HookOutcome {
    let Some(item) = env.item.as_deref() else {
        return HookOutcome::Allow;
    };
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(payload) else {
        return HookOutcome::Allow;
    };
    let field = |k: &str| payload.get(k).and_then(|v| v.as_str()).unwrap_or_default();
    let tool = field("tool_name");
    if tool.is_empty() {
        return HookOutcome::Allow;
    }
    let dir = item_dir(&env.pause_root, item);
    let key = ledger_key(&payload);
    let count_it = env.ledger && !is_container_call(tool);
    // #10832: note a claim label the agent takes or releases, so a requeue can
    // release exactly that claim.
    claim_breadcrumb::observe(&dir, field("hook_event_name"), &payload, env.ledger);
    match field("hook_event_name") {
        "PostToolUse" | "PostToolUseFailure" => {
            let _ = std::fs::remove_file(dir.join(INFLIGHT_DIR).join(&key));
            return HookOutcome::Allow;
        }
        "PreToolUse" => {}
        _ => return HookOutcome::Allow,
    }
    if !is_requested(&dir) {
        if count_it {
            touch(&dir.join(INFLIGHT_DIR).join(&key));
        }
        return HookOutcome::Allow;
    }

    // A pause is requested: park this call.
    let summary = summary(&payload);
    let parked = dir.join(PARKED_DIR).join(&key);
    if let Some(parent) = parked.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &parked,
        serde_json::json!({"tool": tool, "summary": summary, "parked_at": now_rfc3339()})
            .to_string(),
    );
    let deadline = Instant::now() + env.park;
    // The request this call is parked for. Re-read on every poll: a pause
    // that stood down and a new one raised between two polls is a new request.
    let mut request_id = read_request(&dir).and_then(|r| r.manifest_id);
    loop {
        if !is_requested(&dir) {
            // Withdrawn (an aborted pause): release the call. The agent is
            // about to run again, so a safe point recorded for that request
            // no longer holds.
            let _ = std::fs::remove_file(&parked);
            if read_safe_point(&dir).is_some_and(|sp| sp.request_id == request_id) {
                clear_safe_point(&dir);
            }
            if count_it {
                touch(&dir.join(INFLIGHT_DIR).join(&key));
            }
            return HookOutcome::Allow;
        }
        if let Some(request) = read_request(&dir) {
            request_id = request.manifest_id;
        }
        if !env.ledger || inflight_count(&dir, Some(&key)) == 0 {
            let session = field("session_id");
            write_safe_point(
                &dir,
                &SafePoint {
                    reached_at: now_rfc3339(),
                    parked_tool: tool.to_string(),
                    parked_summary: summary.clone(),
                    parked_tool_use_id: key.clone(),
                    harness_pid: env.harness_pid,
                    session_id: (!session.is_empty()).then(|| session.to_string()),
                    runtime: env.runtime.clone(),
                    request_id: request_id.clone(),
                },
            );
        }
        if Instant::now() >= deadline {
            return HookOutcome::Deny(DENY_REASON.to_string());
        }
        std::thread::sleep(env.poll);
    }
}

#[cfg(test)]
mod tests;
