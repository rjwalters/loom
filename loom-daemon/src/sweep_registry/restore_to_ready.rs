//! Claim-restore state safety (Issue #9463): a claim release must never hand
//! a **closed** issue back to the build queue.
//!
//! # The measured bug
//!
//! One in three landed issues got `loom:issue` re-added a median **39 s**
//! after it closed (107 of 322 landed issues, 2026-08-15..09-29). The timing
//! matches a sweep's exit/abort cleanup restoring its claim right after the
//! sweep's own merge — `merge-pr.sh` closes the issue at merge (#6199 strips
//! the stale `loom:building`), and then the reaper's terminal transition
//! restores the claim unconditionally, re-adding the queue label to a closed
//! issue. Every queue/throughput query then has to filter `state:open` to be
//! correct, label timelines read as re-queues, and any path that trusts the
//! label without re-checking state can re-dispatch finished work.
//!
//! # The rule
//!
//! **Any claim release that would re-add `loom:issue` first verifies the
//! issue is still OPEN, and does nothing on a closed issue** — the stale
//! `loom:building` claim is still always removed. This is a carve-out in the
//! same family as #4206 (`loom:blocked`), #4887 (`loom:operator-only`) and
//! #4653 (a PR number): each skips only the re-add leg, and each probe is
//! best-effort with a fail-open fallback (an unverifiable state falls back
//! to the pre-#4206 unconditional restore, because a stranded claim is the
//! more common failure mode this path exists to fix — and the one closed
//! issues must not pay for with a stranded `loom:building`).
//!
//! The state probe is [`SweepRegistry::fetch_issue_signals`]'s `closed` field — one
//! REST `gh api` read on the independent pool, breaker-gated, the same cost
//! shape as the complexity read this terminal path already pays.

use super::forge_gh::loom_repo_flag;
use super::*;
use crate::gh_invocation::{AccessIntent, GhTarget};

impl SweepRegistry {
    /// [`restore_label_to_ready`](Self::restore_label_to_ready) with the
    /// Issue #9463 closed-state carve-out applied. The label-mutation body
    /// lives here (a sibling module) so the size-ratcheted `guards.rs` only
    /// keeps the delegating entry point.
    pub(crate) fn restore_label_to_ready_with_state_check(&self, issue: u32) -> Result<()> {
        let blocked = self.issue_has_blocked_label(issue);
        // Only probe `loom:operator-only` when `loom:blocked` doesn't already
        // decide the outcome — avoids a redundant `gh` call in the (more
        // common) blocked path.
        let operator_only = !blocked && self.issue_has_operator_only_label(issue);
        let parked = blocked || operator_only;
        // Only probe PR-ness when a park carve-out doesn't already decide the
        // outcome — avoids a redundant `gh` call on the parked path.
        let is_pr = !parked && self.issue_is_pull_request(issue).unwrap_or(false);
        // Issue #9463: the closed-state probe runs last (only when no other
        // carve-out already decided against the re-add), so the common
        // non-restore paths pay nothing and a parked/PR restore pays the
        // same one probe it always did.
        let closed = if parked || is_pr {
            None
        } else {
            // Reuses #9441's combined issue-signals read (state + labels off
            // one REST call) instead of a separate probe — same fail-open
            // contract: `None` falls back to the unconditional restore.
            self.fetch_issue_signals(issue).closed
        };
        // Scope the restore to the registry's workspace (`self.gh`) so the
        // crash-path label recovery resolves against the right repo in a
        // multi-workspace daemon (#3937). LOOM_REPO still overrides when set.
        let mut cmd = self
            .gh("issue.edit", AccessIntent::Write, GhTarget::None)
            .args([
                "issue",
                "edit",
                &issue.to_string(),
                "--remove-label",
                "loom:building",
            ]);
        if blocked {
            log::info!(
                "sweep_registry: restore_label_to_ready for #{issue} found `loom:blocked` \
                 already present — preserving the operator's park by removing the stale \
                 `loom:building` claim only, NOT re-adding `loom:issue` (#4206)"
            );
        } else if operator_only {
            log::info!(
                "sweep_registry: restore_label_to_ready for #{issue} found \
                 `loom:operator-only` already present — preserving the authoritative \
                 reroute by removing the stale `loom:building` claim only, NOT re-adding \
                 `loom:issue` (#4887)"
            );
        } else if is_pr {
            log::info!(
                "sweep_registry: restore_label_to_ready for #{issue} resolved to a pull \
                 request — removing the stale `loom:building` claim only, NOT re-adding \
                 `loom:issue` (#4653)"
            );
        } else if closed == Some(true) {
            log::info!(
                "sweep_registry: restore_label_to_ready for #{issue} found the issue \
                 CLOSED — the merge already disposed of it (#9463). Removing the stale \
                 `loom:building` claim only, NOT re-adding the `loom:issue` queue label \
                 to a closed issue."
            );
        } else {
            cmd = cmd.args(["--add-label", "loom:issue"]);
        }
        // Best-effort during reap, but bounded so a wedged `gh` on the
        // `ListSweeps` / `GetSweepStatus` read path cannot block the registry
        // read indefinitely (Issue #3973).
        let timeout = reap_gh_timeout();
        if cmd.args(loom_repo_flag()).output_bounded()?.is_none() {
            log::warn!(
                "sweep_registry: restore_label_to_ready gh for #{issue} exceeded {}s \
                 and was killed (#3973)",
                timeout.as_secs()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake `gh` satisfying every probe the restore path makes, with a
    /// parameterized `.state` answer, recording every invocation.
    fn fake_gh(dir: &Path, state: &str) -> (PathBuf, PathBuf) {
        let fake_gh = dir.join("fake-gh.sh");
        let gh_log = dir.join("gh-calls.log");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then
  echo "false"
  exit 0
fi
if [ "$1" = "api" ]; then
  case "$*" in
    *".pull_request != null"*) echo "false" ;;
    # #9441's combined issue-signals projection (body/state/closed_at/labels).
    *".state"*) printf '{{"body":"","state":"{state}","closed_at":null,"labels":[]}}' ;;
    *) echo "" ;;
  esac
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        (fake_gh, gh_log)
    }

    fn registry_with(fake_gh: &Path, dir: &Path) -> SweepRegistry {
        let mut config = SweepRegistryConfig::new(dir.to_path_buf());
        config.gh_bin = Some(fake_gh.to_path_buf());
        config.skip_label_flip = false; // exercise the real restore path
        SweepRegistry::new(config)
    }

    /// #9463 regression: merge closes the issue, then the sweep exits — the
    /// claim restore removes the stale `loom:building` but must NOT re-add
    /// the `loom:issue` queue label to a closed issue.
    #[test]
    #[serial_test::serial]
    fn a_closed_issue_never_gains_the_queue_label_on_claim_release() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, gh_log) = fake_gh(dir.path(), "closed");
        let registry = registry_with(&fake_gh, dir.path());

        registry.restore_label_to_ready(9463).unwrap();

        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            calls.contains("issue edit 9463 --remove-label loom:building"),
            "the stale claim is still removed: {calls}"
        );
        assert!(
            !calls.contains("--add-label loom:issue"),
            "a CLOSED issue must never re-gain the queue label (#9463): {calls}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn an_open_issue_still_gets_the_queue_label_back() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, gh_log) = fake_gh(dir.path(), "open");
        let registry = registry_with(&fake_gh, dir.path());

        registry.restore_label_to_ready(4206).unwrap();

        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            calls.contains("issue edit 4206 --remove-label loom:building --add-label loom:issue"),
            "the normal crash-recovery restore is unchanged: {calls}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn an_unverifiable_state_fails_open_to_the_restore() {
        // The pre-#9463 contract: a stranded claim is the more common failure
        // mode, so an unverifiable state probe keeps the unconditional
        // restore.
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, gh_log) = fake_gh(dir.path(), "");
        let registry = registry_with(&fake_gh, dir.path());

        registry.restore_label_to_ready(3937).unwrap();

        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            calls.contains("--add-label loom:issue"),
            "fail-open: an unverifiable state still restores the queue label: {calls}"
        );
    }
}
