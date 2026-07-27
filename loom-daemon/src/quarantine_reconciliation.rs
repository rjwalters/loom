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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedIssue {
    pub number: u32,
    /// Whether any comment on the issue contains
    /// [`crate::sweep_registry::QUARANTINE_COMMENT_MARKER`].
    pub has_quarantine_comment: bool,
}

/// The reconciliation decision for one issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileAction {
    /// No daemon quarantine comment found — leave the claim alone. This is
    /// the fail-safe branch: a human's manual `loom:blocked` is never
    /// touched.
    Keep,
    /// Flip `loom:blocked` back to `loom:issue`: the label was applied by a
    /// daemon quarantine that could not have survived to this restart.
    Release,
}

/// Pure decision function — no I/O, fully unit-testable. See the module docs
/// for the decision rule.
#[must_use]
pub fn decide(issue: &BlockedIssue) -> ReconcileAction {
    if issue.has_quarantine_comment {
        ReconcileAction::Release
    } else {
        ReconcileAction::Keep
    }
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
    use super::{plan, BlockedIssue, ReconcileAction, MAX_ISSUES_PER_WORKSPACE};
    use crate::sweep_registry::QUARANTINE_COMMENT_MARKER;
    use anyhow::{anyhow, Context, Result};
    use serde::Deserialize;
    use std::path::Path;
    use std::process::{Command, Stdio};

    #[derive(Debug, Deserialize)]
    struct GhComment {
        #[serde(default)]
        body: String,
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
            .map(|r| BlockedIssue {
                number: r.number,
                has_quarantine_comment: r
                    .comments
                    .iter()
                    .any(|c| c.body.contains(QUARANTINE_COMMENT_MARKER)),
            })
            .collect())
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
        let issues = match list_blocked_issues(gh_bin, root) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("quarantine_reconciliation: {}: {e}", root.display());
                return (0, 0);
            }
        };
        if issues.is_empty() {
            return (0, 0);
        }

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
    #[test]
    fn reconcile_workspace_releases_issue_with_quarantine_comment() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let script = format!(
            r#"#!/usr/bin/env bash
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
    #[test]
    fn reconcile_workspace_never_touches_manual_block() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let script = format!(
            r#"#!/usr/bin/env bash
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
    #[test]
    fn reconcile_workspace_passes_issue_cap_to_gh_list() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let gh_log = dir.path().join("gh-invocations.log");
        let script = format!(
            r#"#!/usr/bin/env bash
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
    #[test]
    fn reconcile_workspace_absorbs_gh_list_failure() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let fake_gh = write_fake_gh(dir.path(), "#!/usr/bin/env bash\necho 'boom' >&2\nexit 1\n");

        let (checked, released) = forge::reconcile_workspace(&fake_gh, &repo_root);
        assert_eq!(checked, 0);
        assert_eq!(released, 0);
    }
}
