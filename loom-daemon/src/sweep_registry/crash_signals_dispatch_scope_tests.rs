//! Regression coverage for `dispatch_scoped_tail` (Issue #8716): failure
//! classification and account-exhaustion evidence must be scoped to the
//! CURRENT dispatch's log region, not the whole accumulated per-issue log
//! file.
//!
//! Lives in its own sibling module rather than `crash_signals.rs`'s `mod
//! tests`: that file sits close to the file-size ratchet's 1000-line
//! threshold (see `.loom/docs/file-size-policy.md`). Mirrors the existing
//! `crash_signals_empty_pool_tests.rs` precedent.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn write_log(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("sweep.log");
    std::fs::write(&path, body).unwrap();
    path
}

/// Issue #8716's own reported incident: a per-issue log whose EARLIER
/// dispatch (a Claude attempt) died on a genuine account-exhaustion
/// signature, reused for a brand-new dispatch (an OpenCode attempt) that died
/// on a distinct, unrelated native-model/config failure before ever reaching
/// a launch record. The old exhaustion banner sits well within the last
/// `EXHAUSTION_LOG_TAIL_LINES` lines of the WHOLE file, which is exactly what
/// let it bleed into classification before this fix.
fn stale_exhaustion_then_new_distinct_failure() -> String {
    "==== loom-daemon dispatch: 2026-09-23T02:30:00Z sweep_id=sweep-issue-8716-0 issue=8716 ====\n\
     spawn-claude: using OAuth account 'agent-old' (mode=ranking)\n\
     # CLAUDE_CLI_START\n\
     Claude: You've hit your weekly limit — try again later\n\
     ==== loom-daemon dispatch: 2026-09-23T02:41:00Z sweep_id=sweep-issue-8716-1 issue=8716 ====\n\
     spawn-opencode: using account 'agent-new'\n\
     bare model differs from the selected profile; use provider/model or define a model profile\n"
        .to_string()
}

#[test]
fn scopes_to_the_region_after_the_newest_dispatch_header() {
    let dir = tempdir().unwrap();
    let path = write_log(dir.path(), &stale_exhaustion_then_new_distinct_failure());

    let tail = dispatch_scoped_tail(&path, EXHAUSTION_LOG_TAIL_LINES).unwrap();

    assert!(
        tail.starts_with(
            "==== loom-daemon dispatch: 2026-09-23T02:41:00Z sweep_id=sweep-issue-8716-1"
        ),
        "must start at the NEWEST dispatch header, not the file's first line: {tail:?}"
    );
    assert!(
        !tail.contains("agent-old"),
        "must not carry any text from the OLDER dispatch region: {tail:?}"
    );
}

#[test]
fn reports_the_new_failure_without_inheriting_the_old_exhaustion_signature() {
    let dir = tempdir().unwrap();
    let path = write_log(dir.path(), &stale_exhaustion_then_new_distinct_failure());

    let tail = dispatch_scoped_tail(&path, EXHAUSTION_LOG_TAIL_LINES).unwrap();

    assert_eq!(
        classify_account_exhaustion(&tail),
        None,
        "the OLDER dispatch's exhaustion banner must not be visible to the scoped tail"
    );
    assert_ne!(
        classify_crash(&tail, None).as_deref(),
        Some("account-exhausted:rate-limited"),
        "the new, distinct failure must not be mis-reported as account exhaustion"
    );
}

/// Regression guard: scoping must narrow, not disable, exhaustion detection.
#[test]
fn genuine_exhaustion_within_the_current_dispatch_still_classifies() {
    let dir = tempdir().unwrap();
    let body = "==== loom-daemon dispatch: sweep-issue-9001-0 ====\n\
                spawn-claude: using OAuth account 'agent-a' (mode=ranking)\n\
                # CLAUDE_CLI_START\n\
                Claude: You've hit your weekly limit — try again later\n";
    let path = write_log(dir.path(), body);

    let tail = dispatch_scoped_tail(&path, EXHAUSTION_LOG_TAIL_LINES).unwrap();

    assert_eq!(classify_account_exhaustion(&tail), Some("rate-limited"));
    assert_eq!(classify_crash(&tail, None).as_deref(), Some("account-exhausted:rate-limited"));
}

#[test]
fn errors_on_a_missing_log_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("does-not-exist.log");

    assert!(dispatch_scoped_tail(&path, EXHAUSTION_LOG_TAIL_LINES).is_err());
}

#[test]
fn returns_the_whole_region_when_shorter_than_the_bound() {
    let dir = tempdir().unwrap();
    let body = "==== loom-daemon dispatch: sweep-issue-42-0 ====\nline one\nline two\n";
    let path = write_log(dir.path(), body);

    let tail = dispatch_scoped_tail(&path, EXHAUSTION_LOG_TAIL_LINES).unwrap();

    assert_eq!(tail, body.trim_end_matches('\n'));
}

/// Legacy logs predate the dispatch-header marker (or come from an adapter
/// that never writes it) — scoping must fall back to the pre-#8716 whole-file
/// tail behavior, not narrow it further.
#[test]
fn falls_back_to_the_whole_file_tail_when_no_dispatch_header_exists() {
    let dir = tempdir().unwrap();
    let lines: Vec<String> = (0..250).map(|i| format!("legacy line {i}")).collect();
    let body = lines.join("\n") + "\n";
    let path = write_log(dir.path(), &body);

    let tail = dispatch_scoped_tail(&path, EXHAUSTION_LOG_TAIL_LINES).unwrap();

    let expected = lines[lines.len() - EXHAUSTION_LOG_TAIL_LINES..].join("\n");
    assert_eq!(
        tail, expected,
        "no header present -> fall back to the last n lines of the WHOLE file, unchanged from \
         the pre-#8716 tail_lines behavior"
    );
}

/// The bound must never straddle two dispatches: even when the CURRENT
/// dispatch's own region is longer than `n`, only the tail of THAT region is
/// returned — older-region lines must never fill the remainder.
#[test]
fn the_line_bound_never_reaches_into_an_older_dispatch() {
    let dir = tempdir().unwrap();
    let old = "==== loom-daemon dispatch: sweep-issue-77-0 ====\n\
               old line a\nold line b\nold line c\n";
    let new_lines: Vec<String> = (0..5).map(|i| format!("new line {i}")).collect();
    let body = format!(
        "{old}==== loom-daemon dispatch: sweep-issue-77-1 ====\n{}\n",
        new_lines.join("\n")
    );
    let path = write_log(dir.path(), &body);

    let tail = dispatch_scoped_tail(&path, 3).unwrap();

    assert!(
        !tail.contains("old line"),
        "must never pull lines from the older region: {tail:?}"
    );
    assert_eq!(tail, new_lines[new_lines.len() - 3..].join("\n"));
}

/// Issue #8749: `poll_and_classify_spawned_child`'s immediate-preflight-death
/// check reads the log through `dispatch_scoped_tail`, so a PRIOR dispatch's
/// signature in the same reused per-issue log can neither classify nor clear
/// the CURRENT dispatch's death.
fn poll_classify_dead_child(body: &str) -> Option<&'static str> {
    let dir = tempdir().unwrap();
    let path = write_log(dir.path(), body);
    let mut child = std::process::Command::new("true").spawn().unwrap();
    child.wait().unwrap();

    let (token_name, _runtime, death) =
        poll_and_classify_spawned_child(&mut child, &path, "sweep_id=current");
    assert_eq!(token_name, UNKNOWN_TOKEN_NAME);
    death
}

#[test]
fn spawned_child_preflight_death_ignores_an_older_dispatch_signature() {
    let body = "==== loom-daemon dispatch: sweep_id=old ====\n\
                spawn-claude: dispatching\n\
                # MCP_PREFLIGHT_FAILED\n\
                ==== loom-daemon dispatch: sweep_id=current ====\n\
                spawn-opencode: bare model differs from the selected profile\n";

    assert_eq!(
        poll_classify_dead_child(body),
        Some("preflight-no-cli-start"),
        "the OLDER dispatch's MCP preflight marker must not label the current death"
    );
}

#[test]
fn spawned_child_preflight_death_is_not_cleared_by_an_older_cli_start() {
    let body = "==== loom-daemon dispatch: sweep_id=old ====\n\
                # CLAUDE_CLI_START\n\
                Claude: working on it\n\
                ==== loom-daemon dispatch: sweep_id=current ====\n\
                spawn-claude: dispatching\n\
                # MCP_PREFLIGHT_FAILED\n";

    assert_eq!(poll_classify_dead_child(body), Some("preflight-mcp-failed"));
}

#[test]
fn spawned_child_that_reached_cli_start_in_the_current_dispatch_is_not_preflight() {
    let body = "==== loom-daemon dispatch: sweep_id=old ====\n\
                # MCP_PREFLIGHT_FAILED\n\
                ==== loom-daemon dispatch: sweep_id=current ====\n\
                spawn-claude: dispatching\n\
                # CLAUDE_CLI_START\n";

    assert_eq!(poll_classify_dead_child(body), None);
}

/// #8599: the `# LOOM_RUNTIME_PREFERENCE` marker `worker_spawn::launch` now
/// writes into the CURRENT dispatch's log region is launch-record preamble, not
/// progress. Without the exclusion in `log_has_progress`, EVERY hung
/// preference-resolved sweep would read as alive to stall detection. (Here
/// rather than in `crash_signals.rs`'s own `mod tests` for the same file-size
/// reason as the rest of this module.)
#[test]
fn log_has_progress_ignores_the_runtime_preference_marker() {
    let log = "==== loom-daemon dispatch: t sweep_id=s issue=7 ====\n\
# LOOM_RUNTIME_RESOLVED runtime=opencode\n\
# LOOM_RUNTIME_PREFERENCE order=claude,codex,opencode:zai-metered tier=2 \
tap=opencode:zai-metered skipped=claude:unavailable(claude_tokens: 0/21 spawnable) \
source=preference\n\
[ts] spawn-worker: runtime=opencode\n";
    assert!(!log_has_progress(log));
    assert!(log_has_progress(&format!("{log}Stage 0: resolving backend...\n")));
}
