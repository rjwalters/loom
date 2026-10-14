//! The PR a scheduled Judge or Doctor tick worked, as `loom.pr_number` on the
//! tick's `loom.role_attempt` span (#11855).
//!
//! Sweep-owned role spans already carry `loom.pr_number`; the role runner's
//! owned-boundary spans did not, so a PR re-judged hundreds of times could not
//! be named from telemetry. Two sources, first wins:
//!
//! 1. **The prompt.** A lane-assigned run is dispatched as `/loom:judge <PR>`
//!    (or `/loom:doctor <PR>`); the launch notes that PR here
//!    ([`note_prompt`]).
//! 2. **The claim.** An unassigned run picks its PR itself. The launch hands
//!    the child a file path in [`PR_FILE_ENV`]; `loom-daemon lease pr claim`
//!    writes the PR it won there ([`record_claim`]), and the close reads it
//!    back ([`take_handoff`]). The CLI exports nothing itself — the daemon
//!    stays the only producer of the span.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The span attribute (already on the daemon's export allowlist).
pub const PR_NUMBER: &str = "loom.pr_number";

/// The child's env var naming the file a won PR claim is written to.
pub const PR_FILE_ENV: &str = "LOOM_ROLE_PR_FILE";

thread_local! {
    static NOTED: RefCell<Option<u64>> = const { RefCell::new(None) };
}

/// The PR an explicit `/loom:judge <PR>` / `/loom:doctor <PR>` prompt names.
#[must_use]
pub fn explicit_pr(role: &str, prompt: &str) -> Option<u64> {
    if !matches!(role, "judge" | "doctor") {
        return None;
    }
    let mut words = prompt.split_whitespace();
    words.next()?.strip_prefix("/loom:")?;
    words.next()?.trim_start_matches('#').parse().ok()
}

/// Note the PR `prompt` names, for this thread's in-flight role tick.
pub fn note_prompt(role: &str, prompt: &str) {
    NOTED.with(|slot| *slot.borrow_mut() = explicit_pr(role, prompt));
}

/// Drop any note left behind, so a tick starts clean.
pub(super) fn clear() {
    NOTED.with(|slot| slot.borrow_mut().take());
}

/// Take this tick's prompt note, if any.
pub(super) fn take() -> Option<u64> {
    NOTED.with(|slot| slot.borrow_mut().take())
}

/// Where tick `execution` of `root` has its claimed PR handed back.
#[must_use]
pub fn handoff_path(root: &Path, execution: &str) -> PathBuf {
    let name: String = execution
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    root.join(".loom/state/role-target")
        .join(format!("{name}.pr"))
}

/// Give the launching child its handoff path.
pub fn export(cmd: &mut Command, root: &Path, execution: &str) {
    cmd.env(PR_FILE_ENV, handoff_path(root, execution));
}

/// `lease pr claim`, on a won claim: write `pr` to the handoff file, when the
/// launching tick gave one. Best-effort; telemetry never fails a claim.
pub fn record_claim(pr: u64) {
    let Some(path) = std::env::var_os(PR_FILE_ENV).filter(|p| !p.is_empty()) else {
        return;
    };
    let path = PathBuf::from(path);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, pr.to_string());
}

/// Read and remove tick `execution`'s handoff file.
pub(super) fn take_handoff(root: &Path, execution: &str) -> Option<u64> {
    let path = handoff_path(root, execution);
    let text = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    text?.trim().parse().ok()
}

#[cfg(test)]
#[path = "role_target/tests.rs"]
mod tests;
