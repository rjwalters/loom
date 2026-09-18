//! Classification of a failed role invocation's `role-<role>.log`, split out
//! of `role_runner.rs` so the over-threshold parent file shrinks rather than
//! grows (`.loom/docs/file-size-policy.md`).
//!
//! `defaults/scripts/claude-wrapper.sh` writes purpose-built, machine-readable
//! `# SENTINEL` lines to stderr immediately before a child terminates for a
//! known, classified reason — [`describe_role_failure`] prefers one of those
//! sentinels over an arbitrary tail-window fragment of stderr whenever one is
//! present, so `role_runner`'s `tick failed` summary names the real cause
//! instead of whatever happened to be printed last. Two different kinds of
//! terminal failure are covered:
//!
//! - Pre-flight rejections (issue #6757), where the wrapper aborts WITHOUT
//!   ever exec'ing the CLI: `AUTH_PREFLIGHT_FAILED` at `claude-wrapper.sh:2603`,
//!   `MCP_PREFLIGHT_FAILED` at `:2614`.
//! - Mid-run terminal failures (issue #8123), where the CLI DID run but the
//!   wrapper's own account-rotation/output-monitor loop killed it for a
//!   reason it already knows precisely: `ACCOUNT_POOL_EXHAUSTED` (rotation
//!   exhausted every account — `claude-wrapper.sh:2680,2697,2719,2733`) and
//!   `RATE_LIMIT_ABORT` (usage/plan-limit prompt or 100%-weekly-limit kill —
//!   `claude-wrapper.sh:2036,2050,2153,2160`). Before #8123 these two fell
//!   through to the generic tail-window fallback, which could surface an
//!   unrelated informational WARN (e.g. an MCP-config notice) instead of the
//!   real cause — see #8123 for the incident.

use super::*;

/// A [`FAILURE_SENTINELS`] entry: the exact literal text
/// `claude-wrapper.sh` writes to stderr, paired with the human-readable
/// classification [`describe_role_failure`] reports for it. Matched
/// literally (not a regex — these are fixed, purpose-built markers, not
/// free-form prose).
const FAILURE_SENTINELS: &[(&str, &str)] = &[
    ("# AUTH_PREFLIGHT_FAILED", "pre-flight rejected the session"),
    ("# MCP_PREFLIGHT_FAILED", "pre-flight rejected the session"),
    ("# ACCOUNT_POOL_EXHAUSTED", "token pool exhausted (bad or rate-limited)"),
    ("# RATE_LIMIT_ABORT", "rate-limit abort mid-run (CLI usage/plan limit reached)"),
];

/// Search `full_log` — the ENTIRE contents of a role's own `role-<role>.log`,
/// not just the retained [`MAX_OUTPUT_TAIL_BYTES`] tail — for one of the
/// [`FAILURE_SENTINELS`] (issues #6757, #8123). Returns the matched sentinel
/// text and its classification.
///
/// Full-file search is deliberate and free: every caller already has the
/// whole file in memory (`tail_of_file`/`read_role_log` read it all via
/// `std::fs::read_to_string` before truncating to a tail), and the sentinel
/// can land earlier in the file than the retained tail window if the child
/// wrote enough unrelated output afterward — the exact scenario #6757
/// reports (an `INFO` line from `lib/locate-daemon-bin.sh`'s ordinary
/// resolution logging pushing the sentinel out of the tail window).
#[must_use]
fn find_failure_sentinel(full_log: &str) -> Option<(&'static str, &'static str)> {
    FAILURE_SENTINELS
        .iter()
        .copied()
        .find(|(sentinel, _)| full_log.contains(sentinel))
}

/// Build the `RoleTickOutcome::Failure` detail for a role invocation that
/// exited non-zero (issues #6757, #8123). `full_log` is the role's own log
/// file's complete contents; `log_path` is that file's path.
///
/// When `full_log` carries a [`find_failure_sentinel`] match, the detail
/// names the sentinel's classification and points directly at `log_path` —
/// where the full log lives — instead of an arbitrary tail-window fragment
/// of stderr that varies run to run and frequently has nothing to do with
/// the real cause (see #8123: a mid-run pool-exhaustion or rate-limit abort
/// could previously be masked by a trailing informational WARN). Otherwise
/// falls back to the pre-existing cleaned/capped byte tail.
#[must_use]
pub(super) fn describe_role_failure(full_log: &str, log_path: &Path) -> String {
    match find_failure_sentinel(full_log) {
        Some((sentinel, classification)) => {
            format!("{classification} ({sentinel}) — see the full log at {}", log_path.display())
        }
        None => clean_and_cap_detail(&truncate_tail(full_log)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn find_failure_sentinel_detects_auth_failure() {
        let log = "some INFO noise\n# AUTH_PREFLIGHT_FAILED\nmore noise after\n";
        assert_eq!(
            find_failure_sentinel(log),
            Some(("# AUTH_PREFLIGHT_FAILED", "pre-flight rejected the session"))
        );
    }

    #[test]
    fn find_failure_sentinel_detects_mcp_failure() {
        let log = "some INFO noise\n# MCP_PREFLIGHT_FAILED\nmore noise after\n";
        assert_eq!(
            find_failure_sentinel(log),
            Some(("# MCP_PREFLIGHT_FAILED", "pre-flight rejected the session"))
        );
    }

    #[test]
    fn find_failure_sentinel_absent_returns_none() {
        let log = "just ordinary output, no sentinel here\n";
        assert_eq!(find_failure_sentinel(log), None);
    }

    #[test]
    fn find_failure_sentinel_found_even_outside_retained_tail_window() {
        // Reproduces the issue's exact scenario: the sentinel occurs early
        // in the log, followed by enough unrelated INFO noise to push it
        // outside the MAX_OUTPUT_TAIL_BYTES tail window that `tail_of_file`
        // alone would retain.
        let noise =
            "resolved /path/to/loom-daemon via $PATH (mtime: 2026-01-01T00:00:00Z)\n".repeat(100);
        let log = format!("# MCP_PREFLIGHT_FAILED\n{noise}");
        assert!(log.len() > MAX_OUTPUT_TAIL_BYTES);
        // The raw tail window alone no longer contains the sentinel...
        assert!(!truncate_tail(&log).contains("MCP_PREFLIGHT_FAILED"));
        // ...but full-file detection still finds it.
        assert_eq!(
            find_failure_sentinel(&log),
            Some(("# MCP_PREFLIGHT_FAILED", "pre-flight rejected the session"))
        );
    }

    #[test]
    fn find_failure_sentinel_detects_account_pool_exhausted() {
        // Issue #8123: mirrors `defaults/scripts/claude-wrapper.sh`'s
        // `echo "# ACCOUNT_POOL_EXHAUSTED" >&2` sentinel, written after
        // account rotation finds every account bad-marked or rate-limited.
        let log = "INFO: rotating account\n# ACCOUNT_POOL_EXHAUSTED\n";
        let (sentinel, classification) = find_failure_sentinel(log).expect("sentinel should match");
        assert_eq!(sentinel, "# ACCOUNT_POOL_EXHAUSTED");
        assert!(classification.contains("token pool exhausted"), "{classification:?}");
    }

    #[test]
    fn find_failure_sentinel_detects_rate_limit_abort() {
        // Issue #8123: mirrors `claude-wrapper.sh`'s
        // `echo "# RATE_LIMIT_ABORT" >&2` sentinel, written when the CLI's
        // usage/plan-limit prompt or 100%-weekly-limit banner is detected.
        let log = "INFO: output monitor watching\n# RATE_LIMIT_ABORT\n";
        let (sentinel, classification) = find_failure_sentinel(log).expect("sentinel should match");
        assert_eq!(sentinel, "# RATE_LIMIT_ABORT");
        assert!(classification.contains("rate-limit abort"), "{classification:?}");
    }

    #[test]
    fn describe_role_failure_names_sentinel_and_log_path_when_present() {
        let log =
            "INFO: starting up\n# AUTH_PREFLIGHT_FAILED\nINFO: resolved something unrelated\n";
        let log_path = Path::new("/tmp/some-workspace/.loom/logs/role-champion.log");
        let detail = describe_role_failure(log, log_path);
        assert!(detail.contains("AUTH_PREFLIGHT_FAILED"), "{detail:?}");
        assert!(
            detail.contains("/tmp/some-workspace/.loom/logs/role-champion.log"),
            "{detail:?}"
        );
        // Must NOT be the raw trailing noise line.
        assert!(!detail.contains("resolved something unrelated"), "{detail:?}");
    }

    #[test]
    fn describe_role_failure_falls_back_to_tail_when_no_sentinel() {
        let log = "ordinary error: connection refused\n";
        let log_path = Path::new("/tmp/some-workspace/.loom/logs/role-judge.log");
        let detail = describe_role_failure(log, log_path);
        assert_eq!(detail, "ordinary error: connection refused");
    }

    #[test]
    fn describe_role_failure_names_pool_exhaustion_reason_not_a_trailing_warn() {
        // Issue #8123's exact reported scenario: an informational MCP-config
        // WARN is the physically-last line in the log, but the real cause
        // (account-pool exhaustion) is the `# ACCOUNT_POOL_EXHAUSTED`
        // sentinel written earlier. The classified reason must win over the
        // raw tail.
        let log = "INFO: starting session\n\
                   log_error: Whole account pool exhausted — every account is marked bad or \
                   rate-limited.\n\
                   # ACCOUNT_POOL_EXHAUSTED\n\
                   WARN: MCP config not found at /repo/.mcp.json (or the git common dir) - \
                   skipping MCP pre-flight (expected under user-scope loom, #4230)\n";
        let log_path = Path::new("/tmp/some-workspace/.loom/logs/role-guide.log");
        let detail = describe_role_failure(log, log_path);
        assert!(detail.contains("ACCOUNT_POOL_EXHAUSTED"), "{detail:?}");
        assert!(detail.contains("token pool exhausted"), "{detail:?}");
        assert!(
            !detail.contains("MCP config not found"),
            "must not surface the misleading trailing WARN instead of the real cause: {detail:?}"
        );
    }

    #[test]
    fn describe_role_failure_names_rate_limit_reason_when_present_outside_tail_window() {
        // Mirrors `find_failure_sentinel_found_even_outside_retained_tail_window`
        // at the `describe_role_failure` level (issue #8123): the sentinel
        // is pushed out of the retained tail by later unrelated noise, but
        // full-file search still finds it and the summary still classifies
        // it correctly instead of falling back to a raw tail fragment.
        let noise = "INFO: idle poll, nothing to do\n".repeat(200);
        let log = format!("# RATE_LIMIT_ABORT\n{noise}");
        assert!(log.len() > MAX_OUTPUT_TAIL_BYTES);
        assert!(!truncate_tail(&log).contains("RATE_LIMIT_ABORT"));
        let log_path = Path::new("/tmp/some-workspace/.loom/logs/role-doctor.log");
        let detail = describe_role_failure(&log, log_path);
        assert!(detail.contains("RATE_LIMIT_ABORT"), "{detail:?}");
        assert!(detail.contains("rate-limit abort"), "{detail:?}");
    }
}
