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
//!
//! Issue #10955 adds one more carve-out of the same shape: an issue that did
//! not carry `loom:issue` when it was claimed does not gain it on release.
//! See [`prior_label`].
//!
//! Issue #11852 adds the last one: an issue with a verified open linked PR
//! keeps `loom:building` while that PR is open — the release writes no label
//! at all. See [`open_pr_veto`].

use super::*;

mod open_pr_veto;
mod prior_label;
mod step_restore;

/// What a claim release did (#11743).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimRelease {
    /// The release edit ran.
    Released,
    /// The issue is `loom:frozen`: no label was touched.
    Frozen,
    /// The issue has this open linked PR (#11852): it keeps `loom:building`
    /// while the PR is open, so no label was touched.
    HeldByOpenPr(u32),
    /// A step's release wrote nothing (#11898): its claim label was already
    /// gone (the step finished and moved the item on, or a later role holds
    /// it), or the item could not be read and a Builder claim is not touched
    /// blind.
    Untouched,
}

impl SweepRegistry {
    /// The reaper's claim release for the reaped dispatch `sweep_id` (Issue
    /// #11438): [`restore_label_to_ready`](Self::restore_label_to_ready),
    /// except when that very dispatch self-reported a no-op release
    /// ([`noop_release_covers_dispatch`](Self::noop_release_covers_dispatch),
    /// #8912). Such a sweep removed `loom:building` and deliberately left
    /// `loom:issue` off to stop a claim/release loop; re-adding it ~25 s later
    /// re-armed the very loop (loom-ui#3508, #2727). So only the stale
    /// `loom:building` claim is removed — never a queue label re-added.
    pub(crate) fn restore_label_after_reap(&self, issue: u32, sweep_id: &str) -> Result<()> {
        if !self.noop_release_covers_dispatch(issue, sweep_id) {
            return self.restore_label_to_ready(issue);
        }
        log::info!(
            "sweep_registry: sweep {sweep_id} self-reported a no-op release for #{issue} — \
             removing the stale `loom:building` claim only, NOT re-adding `loom:issue` (#11438)"
        );
        self.run_claim_release_edit(
            issue,
            vec![
                "issue".to_string(),
                "edit".to_string(),
                issue.to_string(),
                "--remove-label".to_string(),
                "loom:building".to_string(),
            ],
        )
    }

    /// [`restore_label_to_ready`](Self::restore_label_to_ready) with the
    /// Issue #9463 closed-state carve-out applied. The label-mutation body
    /// lives here (a sibling module) so the size-ratcheted `guards.rs` only
    /// keeps the delegating entry point.
    pub(crate) fn restore_label_to_ready_with_state_check(&self, issue: u32) -> Result<()> {
        self.restore_label_to_ready_checked(issue).map(|_| ())
    }

    /// [`restore_label_to_ready_with_state_check`](Self::restore_label_to_ready_with_state_check),
    /// reporting whether the release found the issue `loom:frozen` and so
    /// wrote nothing (#11743) — for a caller with a write of its own to skip.
    pub(crate) fn restore_label_to_ready_checked(&self, issue: u32) -> Result<ClaimRelease> {
        // #11480: one REST read (state + labels) answers both the park
        // carve-outs and #9463's closed-state probe, and the shared
        // `restore_veto` decides the park side for every release path. It
        // replaces the #4206 (`loom:blocked`) and #4887 (`loom:operator-only`)
        // one-label GraphQL probes. Fail-open as before: a failed read yields
        // no labels and no state, so the restore goes ahead.
        let signals = self.release_issue_signals(issue);
        let veto = crate::work_finder::restore_veto(&signals.labels, &[]);
        if veto == Some(crate::work_finder::FROZEN_LABEL) {
            log::info!(
                "sweep_registry: #{issue} is `loom:frozen` — leaving every label as it is, \
                 even the stale `loom:building` claim (#11743)"
            );
            return Ok(ClaimRelease::Frozen);
        }
        // Only probe PR-ness when a veto doesn't already decide the outcome.
        let is_pr = veto.is_none() && self.issue_is_pull_request(issue).unwrap_or(false);
        let closed = if veto.is_some() || is_pr {
            None
        } else {
            signals.closed
        };
        // Issue #10955: last of all, and only when the re-add would otherwise
        // happen — did the claim take `loom:issue` away in the first place?
        // Only a positive "no" skips the re-add (see `prior_label`).
        let never_ready = !(veto.is_some() || is_pr || closed == Some(true))
            && self.claim_took_ready_label(issue) == Some(false);
        // Issue #11852: and, only when the re-add would STILL happen, is the
        // issue's PR open? Then the issue keeps `loom:building` while it is,
        // and nothing is written. A fresh memo is fine (here `Open` prevents a
        // write); anything else releases as before (see `open_pr_veto`). Asked
        // only once the signals read verified the issue OPEN: a forge that
        // could not answer that read (a wedged `gh`, #3973) is not asked more.
        if veto.is_none() && !is_pr && closed == Some(false) && !never_ready {
            if let Some(pr) = self.release_open_linked_pr(issue) {
                log::info!(
                    "sweep_registry: restore_label_to_ready for #{issue} found open linked PR \
                     #{pr} — keeping `loom:building` while the PR is open, no label write; \
                     claim reconciliation releases it once the PR closes unmerged (#11852)"
                );
                return Ok(ClaimRelease::HeldByOpenPr(pr));
            }
        }
        let mut args = vec![
            "issue".to_string(),
            "edit".to_string(),
            issue.to_string(),
            "--remove-label".to_string(),
            "loom:building".to_string(),
        ];
        if let Some(label) = veto {
            log::info!(
                "sweep_registry: restore_label_to_ready for #{issue} found `{label}` — \
                 preserving that hold by removing the stale `loom:building` claim only, NOT \
                 re-adding `loom:issue` (#11480; was #4206/#4887)"
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
        } else if never_ready {
            log::info!(
                "sweep_registry: restore_label_to_ready for #{issue} found the issue did \
                 not carry `loom:issue` when it was claimed — removing the stale \
                 `loom:building` claim only, NOT adding a queue label it never had (#10955)"
            );
        } else {
            args.extend(["--add-label".to_string(), "loom:issue".to_string()]);
        }
        self.run_claim_release_edit(issue, args)
            .map(|()| ClaimRelease::Released)
    }

    /// The issue-signals read for a release path's restore veto (#11480).
    ///
    /// Unlike [`fetch_issue_signals`](Self::fetch_issue_signals) it is NOT
    /// skipped while the rate-limit breaker suppresses polling: the one-label
    /// probes it replaced always ran, and a skipped read would let a release
    /// re-add `loom:issue` beside a veto label for the whole breaker window.
    /// One REST call, counted as `restore.issue_signals`. Fail-open: a failed
    /// read yields no labels and no state.
    pub(crate) fn release_issue_signals(
        &self,
        issue: u32,
    ) -> outcome_journal::complexity_signal::IssueSignals {
        use outcome_journal::complexity_signal::{
            parse_issue_signals, IssueSignals, ISSUE_SIGNALS_JQ,
        };
        if self.config.skip_label_flip {
            return IssueSignals::default();
        }
        let path = format!("repos/{{owner}}/{{repo}}/issues/{issue}");
        match self
            .gh_read("restore.issue_signals", ["api", path.as_str(), "--jq", ISSUE_SIGNALS_JQ])
        {
            Ok(Some(o)) if o.status.success() => {
                parse_issue_signals(&String::from_utf8_lossy(&o.stdout))
            }
            Ok(Some(o)) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                crate::rate_limit_breaker::global_observe_failure(&stderr, "restore_issue_signals");
                log::warn!(
                    "sweep_registry: release veto read for #{issue} failed ({}) — releasing \
                     without it (fail-open, #11480): {stderr}",
                    o.status
                );
                IssueSignals::default()
            }
            Ok(None) | Err(_) => {
                log::warn!(
                    "sweep_registry: release veto read for #{issue} timed out or could not \
                     run — releasing without it (fail-open, #11480)"
                );
                IssueSignals::default()
            }
        }
    }

    /// Run one claim-release `gh issue edit` (`args`) for `issue`, bounded.
    fn run_claim_release_edit(&self, issue: u32, mut args: Vec<String>) -> Result<()> {
        // Scoped to the registry's workspace (#3937; LOOM_REPO still overrides)
        // and counted as `restore.label` (#10089). Best-effort during reap, but
        // bounded so a wedged `gh` on the `ListSweeps` / `GetSweepStatus` read
        // path cannot block the registry read indefinitely (Issue #3973).
        args.extend(crate::claim_reconciliation::gh_call::loom_repo_flag());
        let timeout = reap_gh_timeout();
        match self.gh_write("restore.label", &args)? {
            None => {
                log::warn!(
                    "sweep_registry: restore_label_to_ready gh for #{issue} exceeded {}s \
                     and was killed (#3973)",
                    timeout.as_secs()
                );
                anyhow::bail!("label restore for #{issue} timed out after {}s", timeout.as_secs());
            }
            Some(out) if !out.status.success() => anyhow::bail!(
                "label restore for #{issue} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Some(_) => Ok(()),
        }
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
        fake_gh_labels(dir, state, "")
    }

    /// [`fake_gh`] whose issue-signals read also reports `labels` (a JSON
    /// list body, e.g. `"loom:needs-capability"` quoted).
    fn fake_gh_labels(dir: &Path, state: &str, labels: &str) -> (PathBuf, PathBuf) {
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
    *".state"*) printf '{{"body":"","state":"{state}","closed_at":null,"labels":[{labels}]}}' ;;
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

    /// #11480: a lane veto that is not a skip label (`loom:needs-capability`)
    /// keeps the claim release from re-arming `loom:issue`; the claim goes.
    #[test]
    #[serial_test::serial]
    fn a_veto_label_keeps_the_queue_label_off_on_claim_release() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, gh_log) =
            fake_gh_labels(dir.path(), "open", r#""loom:building","loom:needs-capability""#);
        let registry = registry_with(&fake_gh, dir.path());

        registry.restore_label_to_ready(11480).unwrap();

        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(calls.contains("issue edit 11480 --remove-label loom:building"), "{calls}");
        assert!(!calls.contains("--add-label loom:issue"), "{calls}");
    }

    /// #11743: the reaper makes no label write at all to a frozen item, not
    /// even removing the stale `loom:building` claim.
    #[test]
    #[serial_test::serial]
    fn a_frozen_item_gets_no_label_write_on_claim_release() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, gh_log) =
            fake_gh_labels(dir.path(), "open", r#""loom:building","loom:frozen""#);
        let registry = registry_with(&fake_gh, dir.path());

        registry.restore_label_to_ready(11743).unwrap();

        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(!calls.contains("issue edit"), "{calls}");
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

    /// #11852: an issue keeps `loom:building` while its PR is open, so a
    /// release with a verified open linked PR writes no label at all. The
    /// verified answer here is a fresh #6788 memo, which this veto may use
    /// because `Open` prevents a write rather than permitting a dispatch.
    #[test]
    #[serial_test::serial]
    fn an_open_linked_pr_keeps_the_claim_label_on_release() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, gh_log) = fake_gh(dir.path(), "open");
        let registry = registry_with(&fake_gh, dir.path());
        registry.seed_open_pr_memo(11852, 11853, chrono::Utc::now());

        let released = registry.restore_label_to_ready_checked(11852).unwrap();

        assert_eq!(released, ClaimRelease::HeldByOpenPr(11853));
        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            !calls.contains("issue edit"),
            "an issue keeps `loom:building` while its PR is open — no label write (#11852): \
             {calls}"
        );
    }

    /// #11852 off-switch: `LOOM_RELEASE_OPEN_PR_VETO=0` restores the old
    /// release even with a verified open PR, and asks no probe.
    #[test]
    #[serial_test::serial]
    fn the_open_pr_hold_has_an_off_switch() {
        std::env::set_var(crate::release_veto::OPEN_PR_VETO_ENV, "0");
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, gh_log) = fake_gh(dir.path(), "open");
        let registry = registry_with(&fake_gh, dir.path());
        registry.seed_open_pr_memo(11858, 11859, chrono::Utc::now());

        registry.restore_label_to_ready(11858).unwrap();
        std::env::remove_var(crate::release_veto::OPEN_PR_VETO_ENV);

        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            calls.contains("issue edit 11858 --remove-label loom:building --add-label loom:issue"),
            "switched off, the release re-adds the queue label as before: {calls}"
        );
    }

    /// #11852 fail-open: a probe that cannot answer (no resolvable repo, and
    /// a stale memo whose one-PR recheck does not say "open") restores the
    /// queue label exactly as before.
    #[test]
    #[serial_test::serial]
    fn a_failed_open_pr_probe_still_restores_the_queue_label() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, gh_log) = fake_gh(dir.path(), "open");
        let registry = registry_with(&fake_gh, dir.path());
        registry.seed_open_pr_memo(11854, 11855, chrono::Utc::now() - chrono::Duration::days(1));

        registry.restore_label_to_ready(11854).unwrap();

        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            calls.contains("issue edit 11854 --remove-label loom:building --add-label loom:issue"),
            "an unverifiable open-PR answer fails open to the restore (#11852): {calls}"
        );
    }

    /// #11852: the open-PR probe is asked only when the re-add would otherwise
    /// happen — a closed issue is decided without it, even with a memo saying
    /// a PR is open.
    #[test]
    #[serial_test::serial]
    fn the_open_pr_probe_is_not_consulted_once_another_veto_decides() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, gh_log) = fake_gh(dir.path(), "closed");
        let registry = registry_with(&fake_gh, dir.path());

        registry.restore_label_to_ready(11856).unwrap();

        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(!calls.contains("graphql"), "no open-PR probe on a closed issue: {calls}");
        assert!(!calls.contains("pulls"), "no open-PR listing on a closed issue: {calls}");
        assert!(!calls.contains("--add-label loom:issue"), "{calls}");
    }

    /// Issue #11438: when the reaped dispatch itself self-reported a no-op
    /// release (#8912's dispatch-scoped record), the release removes only the
    /// stale `loom:building` claim. Re-adding `loom:issue` ~25 s later re-armed
    /// the claim/release loop (loom-ui#3508: claim released 09:07:29Z,
    /// `loom:issue` restored 09:07:55Z, re-claimed 09:10:00Z).
    #[test]
    #[serial_test::serial]
    fn a_self_reported_noop_releases_only_the_stale_claim() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, gh_log) = fake_gh(dir.path(), "open");
        let mut registry = registry_with(&fake_gh, dir.path());

        let sweep_id = crate::sweep_registry::test_support::insert_clean_exit_running(
            &mut registry,
            114_380,
            0,
        );
        registry.record_noop_release(114_380, Some("no build work".into()));
        assert!(registry.noop_release_covers_dispatch(114_380, &sweep_id));

        registry
            .restore_label_after_reap(114_380, &sweep_id)
            .unwrap();

        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            calls.contains("issue edit 114380 --remove-label loom:building"),
            "the stale claim is still removed: {calls}"
        );
        assert!(
            !calls.contains("--add-label loom:issue"),
            "a self-reported no-op deliberately left `loom:issue` off — re-adding it \
             re-arms the loop (#11438): {calls}"
        );
    }

    /// The guard on the fix: without a self-report the crash-recovery restore
    /// is byte-identical to `restore_label_to_ready`.
    #[test]
    #[serial_test::serial]
    fn a_plain_reap_restore_is_unchanged_without_a_noop_report() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, gh_log) = fake_gh(dir.path(), "open");
        let mut registry = registry_with(&fake_gh, dir.path());

        let sweep_id = crate::sweep_registry::test_support::insert_clean_exit_running(
            &mut registry,
            114_381,
            0,
        );
        assert!(!registry.noop_release_covers_dispatch(114_381, &sweep_id));

        registry
            .restore_label_after_reap(114_381, &sweep_id)
            .unwrap();

        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            calls.contains("issue edit 114381 --remove-label loom:building --add-label loom:issue"),
            "an ordinary crash-recovery restore still re-queues the issue: {calls}"
        );
    }
}
