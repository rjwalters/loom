//! Why an item missed its safe point (issue #11049).
//!
//! When the pause budget runs out, the daemon requeues every item that wrote
//! no safe-point record (`pause-budget-missed`). That reason alone cannot
//! tell an unwired hook from a busy agent, which is what left the first
//! fleet roll undiagnosable. [`diagnose`] reads the item's pause state dir
//! and names the cause, so the manifest answers it:
//!
//! | Cause | Evidence |
//! |---|---|
//! | `no-hook` | No trace of the hook ever running for this item: no `hook-seen` marker, no ledger, no parked call. The session's hook config does not run `roll-pause.sh`, or the script is not installed. |
//! | `no-tool-call` | The hook has run for this item, but not since the request: the agent was in a long model turn, or one tool call ran through the whole window (`detail` counts calls in flight). |
//! | `hook-refused` | The hook ran after the request but wrote no safe point: it parked calls while other leaf calls were still in flight, or it did not park at all. |

use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::{inflight_count, PauseRequest, INFLIGHT_DIR, PARKED_DIR};

/// Touched by the hook on every `PreToolUse` of a daemon item: its mtime is
/// the hook's last run.
pub const SEEN_FILE: &str = "hook-seen";

/// The cause of a missed safe point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissCause {
    NoHook,
    NoToolCall,
    HookRefused,
}

impl MissCause {
    /// The manifest spelling (`no-hook`, …).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MissCause::NoHook => "no-hook",
            MissCause::NoToolCall => "no-tool-call",
            MissCause::HookRefused => "hook-refused",
        }
    }
}

/// The manifest's record of a missed safe point. `cause` is a
/// [`MissCause`] spelling, kept a string so a reader older or newer than the
/// writer still loads the manifest (design §6 compatibility rules).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafePointMiss {
    pub cause: String,
    #[serde(default)]
    pub detail: String,
}

impl SafePointMiss {
    fn new(cause: MissCause, detail: impl Into<String>) -> Self {
        Self {
            cause: cause.as_str().to_string(),
            detail: detail.into(),
        }
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// How many entries in `dir` were written at or after `since`.
fn entries_since(dir: &Path, since: SystemTime) -> usize {
    std::fs::read_dir(dir).map_or(0, |rd| {
        rd.filter_map(Result::ok)
            .filter(|e| mtime(&e.path()).is_some_and(|t| t >= since))
            .count()
    })
}

/// When `request` was raised: the request file's mtime (written once, when
/// the daemon raised it), else its `requested_at`, else the epoch (every
/// hook trace then counts as in the window).
fn requested_at(item_dir: &Path, request: &PauseRequest) -> SystemTime {
    mtime(&item_dir.join(super::REQUEST_FILE))
        .or_else(|| {
            chrono::DateTime::parse_from_rfc3339(&request.requested_at)
                .ok()
                .map(SystemTime::from)
        })
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Name the cause of an item missing its safe point for `request`. `None`
/// for `item_dir` means the item had no pause state dir at all.
#[must_use]
pub fn diagnose(item_dir: Option<&Path>, request: &PauseRequest) -> SafePointMiss {
    let Some(dir) = item_dir else {
        return SafePointMiss::new(MissCause::NoHook, "the item has no pause state dir");
    };
    let since = requested_at(dir, request);
    let in_flight = inflight_count(dir, None);
    let parked = entries_since(&dir.join(PARKED_DIR), since);
    let seen = mtime(&dir.join(SEEN_FILE));
    if parked > 0 {
        return SafePointMiss::new(
            MissCause::HookRefused,
            format!(
                "the hook parked {parked} call(s), but {in_flight} leaf call(s) were still in flight"
            ),
        );
    }
    if seen.is_some_and(|t| t >= since) {
        return SafePointMiss::new(
            MissCause::HookRefused,
            "the hook ran after the request but did not park the call",
        );
    }
    let ever_ran =
        seen.is_some() || dir.join(INFLIGHT_DIR).is_dir() || dir.join(PARKED_DIR).is_dir();
    if ever_ran {
        let last = seen
            .and_then(|t| since.duration_since(t).ok())
            .map_or_else(String::new, |d| {
                format!("; its last run was {} s before the request", d.as_secs())
            });
        return SafePointMiss::new(
            MissCause::NoToolCall,
            format!(
                "no tool call started in the window ({in_flight} leaf call(s) in flight){last}"
            ),
        );
    }
    SafePointMiss::new(
        MissCause::NoHook,
        "roll-pause.sh never ran for this item: the session's hook config does not run it, \
         or it is not installed",
    )
}

#[cfg(test)]
#[path = "miss_tests.rs"]
mod tests;
