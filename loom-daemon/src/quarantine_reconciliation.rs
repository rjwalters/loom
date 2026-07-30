//! Daemon-startup reconciliation of stranded `loom:blocked` insta-crash
//! quarantines across every managed workspace (Issue #4110).
//!
//! The insta-crash quarantine (#3939) is memory-only:
//! [`crate::sweep_registry::SweepRegistry`]'s `quarantined` map never survives
//! a daemon restart. The forge label (`loom:blocked`) it applied, however,
//! does survive — so a restart drops the in-memory pause while leaving the
//! label behind, and nothing scans for that afterward. The `loom:blocked`
//! issue is then invisible to the work finder (which only polls open
//! `loom:issue`) with no quarantine left anywhere to clear, forever.
//!
//! This module is the startup pass that closes that gap: for every registered
//! workspace, list the open `loom:blocked` issues and release the ones that
//! carry a daemon-authored quarantine comment (identified by
//! [`crate::sweep_registry::QUARANTINE_COMMENT_MARKER`]) back to `loom:issue`.
//! An issue a human deliberately blocked by hand carries no such comment and
//! is never touched — this pass only ever undoes its own past mutation.
//!
//! Mirrors [`crate::claim_reconciliation`]'s conventions: a kill-switch env
//! var, a per-workspace issue cap, and a pure `decide`/`plan` split that keeps
//! the decision logic unit-testable without a forge. Unlike
//! `claim_reconciliation`, there is no liveness question to answer here — a
//! fresh daemon's quarantine memory is *always* empty at startup, so the sole
//! question is "did the daemon itself apply this `loom:blocked` label", which
//! the comment marker answers directly.
//!
//! ## Recency comparison (Issue #4206)
//!
//! The marker-comment-presence heuristic above is wrong once an issue has
//! ever been quarantined: the comment is permanent, so it stays on the
//! thread forever. That means EVERY later manual park an operator applies to
//! that same issue — including one applied long after the daemon released
//! its own quarantine — is misclassified as daemon-owned and auto-flipped
//! right back to `loom:issue`, silently overriding a deliberate human
//! decision. This is the "crash-loop on previously-quarantined issues"
//! defect the issue describes.
//!
//! [`decide`] now additionally compares two timestamps when both are
//! available: the most recent `labeled loom:blocked` timeline event, and the
//! most recent quarantine-marker comment. A `loom:blocked` label is only
//! treated as daemon-owned (and thus releasable) when its labeling event is
//! contemporaneous with — not meaningfully newer than — the daemon's own
//! comment (see [`RECENCY_TOLERANCE_SECS`]). When either timestamp is
//! unavailable this falls back to the pre-#4206 behavior (marker presence
//! alone), so a partial/older API response never strands a genuine daemon
//! quarantine.

use chrono::{DateTime, Utc};

/// Env var to disable the startup quarantine reconciliation pass entirely
/// (kill switch, not a feature gate — this is corrective crash recovery in
/// the same spirit as [`crate::claim_reconciliation::RECONCILE_ENABLED_ENV`],
/// so it defaults to ON). `0`/`false`/`no`/`off` disables; anything else
/// (including unset) leaves it enabled.
pub const RECONCILE_ENABLED_ENV: &str = "LOOM_QUARANTINE_RECONCILE";

/// Bound on how many `loom:blocked` issues one reconciliation pass inspects
/// per workspace (defense in depth against an unexpectedly huge backlog
/// turning startup into a `gh`-API storm), matching
/// [`crate::claim_reconciliation::MAX_ISSUES_PER_WORKSPACE`].
pub const MAX_ISSUES_PER_WORKSPACE: u32 = 100;

/// How close the most recent `labeled loom:blocked` timeline event must be
/// to the most recent quarantine-marker comment for the two to be treated as
/// the SAME daemon-applied mutation (Issue #4206). `gh`'s label-flip and
/// comment-post are two separate API calls a few hundred milliseconds apart
/// at most; a few seconds of slack absorbs API latency without opening a
/// window a human's manual re-park could land inside.
pub const RECENCY_TOLERANCE_SECS: i64 = 5;

/// Resolve whether the startup quarantine reconciliation pass is enabled.
#[must_use]
pub fn reconciliation_enabled() -> bool {
    match std::env::var(RECONCILE_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// A `loom:blocked` issue reported by the forge, trimmed to the fields the
/// reconciliation decision needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockedIssue {
    pub number: u32,
    /// Whether any comment on the issue contains
    /// [`crate::sweep_registry::QUARANTINE_COMMENT_MARKER`].
    pub has_quarantine_comment: bool,
    /// Timestamp of the most recent quarantine-marker comment on the issue
    /// thread (Issue #4206). `None` when there is no marker comment, or its
    /// timestamp could not be resolved.
    pub last_quarantine_comment_at: Option<DateTime<Utc>>,
    /// Timestamp of the most recent `labeled loom:blocked` timeline event
    /// (Issue #4206). `None` when unresolvable — callers must treat this as
    /// "no recency signal available", not "never labeled".
    pub last_blocked_labeled_at: Option<DateTime<Utc>>,
}

/// The reconciliation decision for one issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Leave the claim alone. This is the fail-safe branch: a human's
    /// manual `loom:blocked` — including a LATER manual re-park on an issue
    /// the daemon once quarantined (Issue #4206) — is never touched.
    Keep,
    /// Flip `loom:blocked` back to `loom:issue`: the label was applied by a
    /// daemon quarantine that could not have survived to this restart (or,
    /// for the TTL path, has now aged out).
    Release,
}

/// Pure decision function — no I/O, fully unit-testable. See the module docs
/// for the decision rule.
///
/// 1. No quarantine-marker comment anywhere on the thread ⇒ this `loom:blocked`
///    was never touched by the daemon ⇒ [`ReconcileAction::Keep`].
/// 2. A marker comment exists AND both recency timestamps are available ⇒
///    compare them: a `labeled loom:blocked` event more than
///    [`RECENCY_TOLERANCE_SECS`] newer than the marker comment is a later,
///    independent manual park ⇒ [`ReconcileAction::Keep`]. Otherwise the
///    labeling is either the daemon's own (contemporaneous) mutation or
///    predates it ⇒ [`ReconcileAction::Release`].
/// 3. A marker comment exists but either timestamp is unavailable (partial
///    API response) ⇒ fall back to the pre-#4206 behavior: marker presence
///    alone is enough ⇒ [`ReconcileAction::Release`]. This is the fail-open
///    branch — favoring the pre-existing release behavior over stranding a
///    genuine daemon quarantine on an unverifiable read.
#[must_use]
pub fn decide(issue: &BlockedIssue) -> ReconcileAction {
    if !issue.has_quarantine_comment {
        return ReconcileAction::Keep;
    }
    if let (Some(labeled_at), Some(comment_at)) =
        (issue.last_blocked_labeled_at, issue.last_quarantine_comment_at)
    {
        if (labeled_at - comment_at).num_seconds() > RECENCY_TOLERANCE_SECS {
            return ReconcileAction::Keep;
        }
    }
    ReconcileAction::Release
}

/// Plan reconciliation decisions for every issue in `issues`. Performs no
/// I/O; `issues` is expected to already be capped to
/// [`MAX_ISSUES_PER_WORKSPACE`] by the caller.
#[must_use]
pub fn plan(issues: &[BlockedIssue]) -> Vec<(u32, ReconcileAction)> {
    issues
        .iter()
        .map(|issue| (issue.number, decide(issue)))
        .collect()
}

/// `gh`/label-flip glue. Not unit-tested directly (mirrors
/// [`crate::claim_reconciliation::forge`] / [`crate::work_finder::forge`]) —
/// the decision logic above is the fully-covered surface; this module is a
/// thin, best-effort `Command` wrapper.
pub mod forge {
    use super::{decide, plan, BlockedIssue, ReconcileAction, MAX_ISSUES_PER_WORKSPACE};
    use crate::sweep_registry::{output_with_timeout, reap_gh_timeout, QUARANTINE_COMMENT_MARKER};
    use anyhow::{anyhow, Context, Result};
    use chrono::{DateTime, Utc};
    use serde::Deserialize;
    use std::path::Path;
    use std::process::{Command, Stdio};

    #[derive(Debug, Deserialize)]
    struct GhComment {
        #[serde(default)]
        body: String,
        #[serde(default, rename = "createdAt")]
        created_at: Option<DateTime<Utc>>,
    }

    #[derive(Debug, Deserialize)]
    struct GhBlockedIssue {
        number: u32,
        #[serde(default)]
        comments: Vec<GhComment>,
    }

    fn list_blocked_issues(gh_bin: &Path, root: &Path) -> Result<Vec<BlockedIssue>> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("issue")
            .arg("list")
            .arg("--label")
            .arg("loom:blocked")
            .arg("--state")
            .arg("open")
            .arg("--limit")
            .arg(MAX_ISSUES_PER_WORKSPACE.to_string())
            .arg("--json")
            .arg("number,comments");
        cmd.current_dir(root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh issue list --label loom:blocked failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let rows: Vec<GhBlockedIssue> =
            serde_json::from_slice(&out.stdout).context("parse gh issue list JSON")?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let marker_comments: Vec<&GhComment> = r
                    .comments
                    .iter()
                    .filter(|c| c.body.contains(QUARANTINE_COMMENT_MARKER))
                    .collect();
                BlockedIssue {
                    number: r.number,
                    has_quarantine_comment: !marker_comments.is_empty(),
                    last_quarantine_comment_at: marker_comments
                        .iter()
                        .filter_map(|c| c.created_at)
                        .max(),
                    // Filled in by the caller (Issue #4206) only for
                    // candidates that could actually be released — an issue
                    // with no marker comment is `Keep` unconditionally and
                    // never needs the extra `gh` round-trip.
                    last_blocked_labeled_at: None,
                }
            })
            .collect())
    }

    /// Best-effort fetch of the most recent `labeled loom:blocked` timeline
    /// event for `issue` (Issue #4206), used to distinguish the daemon's own
    /// (contemporaneous) label-flip from a later, independent manual park.
    /// Returns `None` on any failure/timeout/unparseable-output — callers
    /// must treat that as "no recency signal", which [`decide`] resolves by
    /// falling back to the pre-#4206 marker-presence-only rule.
    fn fetch_last_blocked_labeled_at(
        gh_bin: &Path,
        root: &Path,
        issue: u32,
    ) -> Option<DateTime<Utc>> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("api")
            .arg(format!("repos/{{owner}}/{{repo}}/issues/{issue}/timeline"))
            .arg("--paginate")
            .arg("--jq")
            .arg(
                r#"[.[] | select(.event == "labeled" and .label.name == "loom:blocked") | .created_at] | max // empty"#,
            );
        cmd.current_dir(root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        let out = output_with_timeout(cmd, reap_gh_timeout()).ok()??;
        if !out.status.success() {
            return None;
        }
        parse_timestamp(&out.stdout)
    }

    /// Best-effort fetch of the most recent quarantine-marker comment's
    /// timestamp for `issue` (Issue #4206), independent of
    /// [`list_blocked_issues`]'s bulk fetch — used by [`probe_manual_repark`],
    /// which is called per-issue outside the startup listing pass (from the
    /// TTL-expiry path). Returns `None` on any failure, OR when no comment on
    /// the thread contains [`QUARANTINE_COMMENT_MARKER`].
    fn fetch_last_quarantine_comment_at(
        gh_bin: &Path,
        root: &Path,
        issue: u32,
    ) -> Option<DateTime<Utc>> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("issue")
            .arg("view")
            .arg(issue.to_string())
            .arg("--json")
            .arg("comments")
            .arg("--jq")
            .arg(format!(
                r#"[.comments[] | select(.body | contains("{QUARANTINE_COMMENT_MARKER}")) | .createdAt] | max // empty"#
            ));
        cmd.current_dir(root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        let out = output_with_timeout(cmd, reap_gh_timeout()).ok()??;
        if !out.status.success() {
            return None;
        }
        parse_timestamp(&out.stdout)
    }

    /// Parse a `gh --jq` scalar result that is either a bare RFC-3339
    /// timestamp or a JSON-quoted one, tolerating an empty/`null` result
    /// (no match). Also tolerates multi-line output: `gh api --paginate`
    /// re-invokes `--jq` once per page and concatenates the per-page
    /// results, so a filter like `max // empty` run against a timeline
    /// spanning more than one page (>100 events, as
    /// [`fetch_last_blocked_labeled_at`] queries) yields one `max`-per-page
    /// line rather than a single overall max (Issue #4637). Each
    /// non-empty/non-`null` line is parsed independently and the maximum
    /// across all lines is returned; a single-line result (e.g. from
    /// [`fetch_last_quarantine_comment_at`]'s non-paginated `gh issue view`)
    /// is handled identically to before.
    pub(crate) fn parse_timestamp(stdout: &[u8]) -> Option<DateTime<Utc>> {
        let raw = String::from_utf8_lossy(stdout);
        raw.lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed == "null" {
                    return None;
                }
                let unquoted = trimmed.trim_matches('"');
                DateTime::parse_from_rfc3339(unquoted)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            })
            .max()
    }

    /// Best-effort, single-issue check of whether `issue`'s current
    /// `loom:blocked` label is a manual park applied well AFTER the daemon's
    /// own quarantine comment (Issue #4206) — used by
    /// [`crate::sweep_registry::SweepRegistry`]'s TTL-expiry path (outside
    /// the startup listing pass) to decide whether releasing would clobber a
    /// deliberate later operator decision.
    ///
    /// Returns `true` only when [`decide`] resolves to
    /// [`ReconcileAction::Keep`] on freshly-fetched data — i.e. a marker
    /// comment DOES exist (this issue was genuinely quarantined at some
    /// point) and a `loom:blocked` labeling event is confirmed to postdate
    /// it by more than [`super::RECENCY_TOLERANCE_SECS`]. Any unresolvable
    /// signal (no marker comment found, a `gh` failure/timeout, or missing
    /// timeline data) returns `false` — fail-open, so a forge hiccup can
    /// never permanently strand a genuine daemon quarantine at `loom:blocked`.
    #[must_use]
    pub fn probe_manual_repark(gh_bin: &Path, root: &Path, issue: u32) -> bool {
        let Some(last_quarantine_comment_at) =
            fetch_last_quarantine_comment_at(gh_bin, root, issue)
        else {
            return false;
        };
        let last_blocked_labeled_at = fetch_last_blocked_labeled_at(gh_bin, root, issue);
        let state = BlockedIssue {
            number: issue,
            has_quarantine_comment: true,
            last_quarantine_comment_at: Some(last_quarantine_comment_at),
            last_blocked_labeled_at,
        };
        decide(&state) == ReconcileAction::Keep
    }

    fn release(gh_bin: &Path, root: &Path, issue: u32) -> Result<()> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--remove-label")
            .arg("loom:blocked")
            .arg("--add-label")
            .arg("loom:issue");
        cmd.current_dir(root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh issue edit failed for #{issue} in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Reconcile stranded `loom:blocked` quarantines for one registered
    /// workspace `root`. Best-effort and bounded: any `gh` failure is logged
    /// at `warn` and this workspace's pass returns `(0, 0)` rather than
    /// propagating an error (one repo's forge hiccup must never block the
    /// daemon's startup, nor the other registered workspaces).
    ///
    /// Returns `(checked, released)` — the number of `loom:blocked` issues
    /// inspected and the number actually released, for the caller's summary
    /// log line.
    pub fn reconcile_workspace(gh_bin: &Path, root: &Path) -> (usize, usize) {
        // Shared GitHub rate limit exhausted (#4429): the listing (and the
        // per-candidate timeline probes below) would all fail — skip.
        if crate::rate_limit_breaker::global_is_suppressed() {
            log::info!(
                "quarantine_reconciliation: {} skipped — shared GitHub API rate limit \
                 exhausted (#4429)",
                root.display()
            );
            return (0, 0);
        }
        let issues = match list_blocked_issues(gh_bin, root) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("quarantine_reconciliation: {}: {e}", root.display());
                crate::rate_limit_breaker::global_observe_failure(
                    &e.to_string(),
                    "quarantine_reconciliation",
                );
                return (0, 0);
            }
        };
        if issues.is_empty() {
            return (0, 0);
        }

        // Issue #4206: augment with the timeline recency signal, but only
        // for candidates that might actually be released (a marker comment
        // exists) — an issue with no marker comment is `Keep` unconditionally
        // and never needs the extra `gh` round-trip.
        let issues: Vec<BlockedIssue> = issues
            .into_iter()
            .map(|mut issue| {
                if issue.has_quarantine_comment {
                    issue.last_blocked_labeled_at =
                        fetch_last_blocked_labeled_at(gh_bin, root, issue.number);
                }
                issue
            })
            .collect();

        let decisions = plan(&issues);
        let checked = decisions.len();

        let mut released = 0usize;
        for (issue_number, action) in decisions {
            let ReconcileAction::Release = action else {
                continue;
            };
            match release(gh_bin, root, issue_number) {
                Ok(()) => {
                    released += 1;
                    log::info!(
                        "quarantine_reconciliation: released stranded quarantine \
                         loom:blocked -> loom:issue for #{issue_number} in {} (#4110)",
                        root.display()
                    );
                }
                Err(e) => {
                    log::warn!(
                        "quarantine_reconciliation: failed to release #{issue_number} in {}: {e}",
                        root.display()
                    );
                }
            }
        }

        (checked, released)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn issue(number: u32, has_comment: bool) -> BlockedIssue {
        BlockedIssue {
            number,
            has_quarantine_comment: has_comment,
            last_quarantine_comment_at: None,
            last_blocked_labeled_at: None,
        }
    }

    #[test]
    fn decide_releases_when_quarantine_comment_present() {
        assert_eq!(decide(&issue(42, true)), ReconcileAction::Release);
    }

    #[test]
    fn decide_keeps_manual_block_with_no_quarantine_comment() {
        assert_eq!(
            decide(&issue(42, false)),
            ReconcileAction::Keep,
            "a human's manual loom:blocked (no daemon comment) must never be auto-flipped"
        );
    }

    #[test]
    fn plan_maps_each_issue_independently() {
        let issues = vec![issue(1, true), issue(2, false), issue(3, true)];
        let decisions = plan(&issues);
        assert_eq!(
            decisions,
            vec![
                (1, ReconcileAction::Release),
                (2, ReconcileAction::Keep),
                (3, ReconcileAction::Release),
            ]
        );
    }

    /// Issue #4206 (Option 1, recency comparison): when both timestamps are
    /// available and the `loom:blocked` labeling event is contemporaneous
    /// with the daemon's own quarantine-marker comment (within
    /// [`RECENCY_TOLERANCE_SECS`]), the label is still recognized as
    /// daemon-owned and released — this is the normal quarantine-apply
    /// sequence (label flip, then comment), not a manual park.
    #[test]
    fn decide_releases_when_labeled_event_contemporaneous_with_marker_comment() {
        let comment_at = Utc::now();
        let labeled_at = comment_at + chrono::Duration::seconds(2);
        let issue = BlockedIssue {
            number: 42,
            has_quarantine_comment: true,
            last_quarantine_comment_at: Some(comment_at),
            last_blocked_labeled_at: Some(labeled_at),
        };
        assert_eq!(decide(&issue), ReconcileAction::Release);
    }

    /// Issue #4206 (Option 1, recency comparison) — the load-bearing
    /// regression case: an issue the daemon quarantined LONG ago (its
    /// marker comment is old), but whose `loom:blocked` label was applied by
    /// a human much more recently (a deliberate later park unrelated to the
    /// original daemon quarantine). The comment-presence-only heuristic
    /// (pre-#4206) would misclassify this as daemon-owned and release it —
    /// silently overriding the operator's park. The recency-aware decision
    /// must Keep it.
    #[test]
    fn decide_keeps_manual_repark_long_after_daemon_quarantine_comment() {
        let comment_at = Utc::now() - chrono::Duration::days(3);
        let labeled_at = Utc::now(); // re-blocked by a human just now
        let issue = BlockedIssue {
            number: 4206,
            has_quarantine_comment: true,
            last_quarantine_comment_at: Some(comment_at),
            last_blocked_labeled_at: Some(labeled_at),
        };
        assert_eq!(
            decide(&issue),
            ReconcileAction::Keep,
            "a loom:blocked label applied well after the daemon's quarantine comment is a \
             later, independent manual park and must never be auto-released"
        );
    }

    /// Issue #4206: when a marker comment exists but the timeline recency
    /// signal could not be resolved (partial/older API response, or the
    /// timeline fetch failed), `decide` falls back to the pre-#4206
    /// marker-presence-only rule rather than stranding a genuine daemon
    /// quarantine on an unverifiable read.
    #[test]
    fn decide_releases_on_missing_timeline_signal_with_marker_comment_present() {
        let issue = BlockedIssue {
            number: 7,
            has_quarantine_comment: true,
            last_quarantine_comment_at: Some(Utc::now()),
            last_blocked_labeled_at: None,
        };
        assert_eq!(decide(&issue), ReconcileAction::Release);
    }

    #[test]
    #[serial]
    fn reconciliation_enabled_resolves_env_precedence() {
        std::env::remove_var(RECONCILE_ENABLED_ENV);
        assert!(reconciliation_enabled(), "defaults to enabled");

        for off in ["0", "false", "no", "off", "OFF", "False"] {
            std::env::set_var(RECONCILE_ENABLED_ENV, off);
            assert!(!reconciliation_enabled(), "{off} should disable");
        }

        std::env::set_var(RECONCILE_ENABLED_ENV, "1");
        assert!(reconciliation_enabled());

        std::env::remove_var(RECONCILE_ENABLED_ENV);
    }

    fn write_fake_gh(dir: &std::path::Path, script: &str) -> std::path::PathBuf {
        let fake_gh = dir.join("fake-gh.sh");
        std::fs::write(&fake_gh, script).unwrap();
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_gh, perms).unwrap();
        }
        fake_gh
    }

    /// A `loom:blocked` issue carrying the daemon's quarantine comment is
    /// released, and the recorded `gh` argv proves the real label-flip
    /// command ran (not just the in-memory decision).
    ///
    /// `#[serial]` is load-bearing (#4547): the fake-`gh` script below spawns
    /// a subprocess whose `bash` resolution depends on the inherited `PATH`,
    /// which `disk_headroom.rs`'s tests mutate process-globally.
    #[test]
    #[serial]
    fn reconcile_workspace_releases_issue_with_quarantine_comment() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let script = format!(
            r#"#!/bin/bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "issue" ] && [ "$2" = "list" ]; then
  echo '[{{"number":99,"comments":[{{"body":"Auto-quarantined by loom-daemon (#3939): insta-crashed 3 times"}}]}}]'
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        let fake_gh = write_fake_gh(dir.path(), &script);

        let (checked, released) = forge::reconcile_workspace(&fake_gh, &repo_root);
        assert_eq!(checked, 1);
        assert_eq!(released, 1);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 99 --remove-label loom:blocked --add-label loom:issue"),
            "expected the real label-flip argv; got: {gh_calls:?}"
        );
    }

    /// A `loom:blocked` issue with NO daemon quarantine comment (an
    /// operator's manual block) is left untouched — no `gh issue edit` call
    /// is ever made for it.
    ///
    /// `#[serial]` is load-bearing (#4547) for the same reason as
    /// `reconcile_workspace_releases_issue_with_quarantine_comment` above.
    #[test]
    #[serial]
    fn reconcile_workspace_never_touches_manual_block() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let script = format!(
            r#"#!/bin/bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "issue" ] && [ "$2" = "list" ]; then
  echo '[{{"number":77,"comments":[{{"body":"Blocked manually pending a human decision"}}]}}]'
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        let fake_gh = write_fake_gh(dir.path(), &script);

        let (checked, released) = forge::reconcile_workspace(&fake_gh, &repo_root);
        assert_eq!(checked, 1);
        assert_eq!(released, 0, "no quarantine comment => never released");

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !gh_calls.contains("issue edit"),
            "a manually-blocked issue must never be flipped; got: {gh_calls:?}"
        );
    }

    /// The per-workspace cap is passed through to `gh issue list --limit`, so
    /// an unexpectedly huge backlog cannot turn startup into an API storm.
    ///
    /// `#[serial]` is load-bearing (#4547): `std::env::set_var`/`remove_var`
    /// (used by `disk_headroom.rs`'s PATH-mutating tests) race any
    /// concurrently-running thread that reads the process environment —
    /// including the implicit env read inside `std::process::Command`'s
    /// child-spawn plumbing below, not just literal `PATH` lookups.
    #[test]
    #[serial]
    fn reconcile_workspace_passes_issue_cap_to_gh_list() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let script = format!(
            r#"#!/bin/bash
printf '%s\n' "$*" >> "{log}"
echo '[]'
exit 0
"#,
            log = gh_log.display(),
        );
        let fake_gh = write_fake_gh(dir.path(), &script);

        let (checked, released) = forge::reconcile_workspace(&fake_gh, &repo_root);
        assert_eq!(checked, 0);
        assert_eq!(released, 0);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains(&format!("--limit {MAX_ISSUES_PER_WORKSPACE}")),
            "expected the issue-cap limit in the gh invocation; got: {gh_calls:?}"
        );
    }

    /// A `gh issue list` failure is absorbed (logged, not propagated) so one
    /// repo's forge hiccup never blocks startup.
    ///
    /// `#[serial]` is load-bearing (#4547) for the same reason as
    /// `reconcile_workspace_passes_issue_cap_to_gh_list` above.
    #[test]
    #[serial]
    fn reconcile_workspace_absorbs_gh_list_failure() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let fake_gh = write_fake_gh(dir.path(), "#!/bin/bash\necho 'boom' >&2\nexit 1\n");

        let (checked, released) = forge::reconcile_workspace(&fake_gh, &repo_root);
        assert_eq!(checked, 0);
        assert_eq!(released, 0);
    }

    // Issue #4637: `gh api --paginate --jq` re-invokes the `--jq` filter once
    // per page and concatenates the per-page results, so a `max // empty`
    // filter against `fetch_last_blocked_labeled_at`'s paginated timeline
    // query yields one line per page (>100 events) rather than a single
    // overall max. `parse_timestamp` must resolve the true max across every
    // line, while still handling the single-line result
    // `fetch_last_quarantine_comment_at`'s non-paginated query always
    // produces.

    #[test]
    fn parse_timestamp_single_line_bare() {
        let parsed = forge::parse_timestamp(b"2026-01-01T00:00:00Z\n").unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-01-01T00:00:00+00:00");
    }

    #[test]
    fn parse_timestamp_multi_page_picks_max_out_of_order() {
        let stdout = b"2026-01-01T00:00:00Z\n2026-03-15T12:30:00Z\n2026-02-01T00:00:00Z\n";
        let parsed = forge::parse_timestamp(stdout).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-03-15T12:30:00+00:00");
    }

    #[test]
    fn parse_timestamp_multi_page_skips_empty_and_null_lines() {
        let stdout = b"\n2026-05-05T05:05:05Z\nnull\n";
        let parsed = forge::parse_timestamp(stdout).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-05-05T05:05:05+00:00");
    }

    #[test]
    fn parse_timestamp_returns_none_for_empty_output() {
        assert!(forge::parse_timestamp(b"").is_none());
        assert!(forge::parse_timestamp(b"\n\n").is_none());
        assert!(forge::parse_timestamp(b"null\n").is_none());
        assert!(forge::parse_timestamp(b"null\nnull\n").is_none());
    }

    #[test]
    fn parse_timestamp_returns_none_for_garbage() {
        assert!(forge::parse_timestamp(b"not-a-timestamp\n").is_none());
        assert!(forge::parse_timestamp(b"not-a-timestamp\nalso-not-one\n").is_none());
    }

    #[test]
    fn parse_timestamp_handles_json_quoted_lines() {
        let stdout = b"\"2026-01-01T00:00:00Z\"\n\"2026-06-06T06:06:06Z\"\n";
        let parsed = forge::parse_timestamp(stdout).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-06-06T06:06:06+00:00");
    }
}
