//! Host reaping never treats a private job as authority over a host worktree.
use super::*;
/// The shared removal loop behind [`reap_worktrees`] and [`reap_pr_worktrees`]
/// (issue #5939).
///
/// `parse_name` turns a directory name into the number that identifies it
/// (issue number or PR number) or `None` to skip the entry entirely;
/// `classify` applies that class's safety gates. `quarantine` handles
/// [`WorktreeDecision::RemoveWithQuarantine`] (issue #6653) — a dirty
/// worktree whose grace period already elapsed — by pushing its uncommitted
/// changes into a `loom-quarantine:` stash and returning the stash's commit
/// sha, or `None` to signal the push failed / found nothing to stash (in
/// which case the worktree is preserved, never removed, exactly like any
/// other skip). Everything else — the enumeration, `scanned` accounting, the
/// skip/remove/fail bookkeeping — is identical for both classes by
/// construction, which is the point: the `pr-<N>` pass cannot drift from the
/// `issue-<N>` pass's gate handling because there is only one copy of it.
///
/// `confirm` is the pre-removal fresh read (W6 PR2,
/// [`crate::worktree_ops::hygiene_pass`]): it runs once for every worktree
/// the gates decided to remove, immediately before the quarantine or the
/// removal, and `Some(reason)` keeps the worktree. It is never asked about a
/// worktree a gate already preserved.
pub(super) fn reap_worktrees_generic(
    repo_root: &Path,
    parse_name: &dyn Fn(&str) -> Option<u32>,
    classify: &dyn Fn(&Path, u32) -> WorktreeDecision,
    quarantine: &dyn Fn(&Path, u32) -> Option<String>,
    remove: &dyn Fn(&Path, u32) -> bool,
    confirm: &dyn Fn(u32) -> Option<String>,
) -> ReapReport {
    let mut report = ReapReport::default();

    for entry in enumerate_worktree_dirs(repo_root) {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(num) = parse_name(&name) else {
            continue;
        };
        report.scanned += 1;
        if name.starts_with("issue-")
            && crate::tokens_pool::private_workspace::export::has_issue(repo_root, num)
        {
            report
                .skipped
                .push((num, "private workspace ownership; host worktree is unrelated".into()));
            continue;
        }

        let worktree_path = entry.path().canonicalize().unwrap_or_else(|_| entry.path());
        // #8413: `classify` only sees registry-visible liveness (claim-lock,
        // `.loom-in-use`, a process whose cwd is inside). An in-session builder
        // mid-`cargo` has none of those, so its worktree stayed removable while
        // a compile was running. `removal_veto` adds the registry-INDEPENDENT
        // signals — a live `loom-daemon inflight` claim on this tree, or a write
        // inside the activity window — downgrading a removal to `SkipInUse`.
        let decision = removal_veto(&worktree_path, classify(&worktree_path, num));

        // W6 PR2: what `classify` read may have been held for the pass,
        // remembered across passes or served from a `304`. Before anything is
        // quarantined or removed, one fresh unconditional read must agree.
        // The stuck-removal record is left alone: a confirm that could not
        // be made (forge unreachable) says nothing about eligibility.
        if skip_reason(&decision).is_none() {
            if let Some(reason) = confirm(num) {
                report.skipped.push((num, reason));
                continue;
            }
        }

        if matches!(decision, WorktreeDecision::RemoveWithQuarantine) {
            match quarantine(&worktree_path, num) {
                Some(stash_sha) => {
                    log::warn!(
                        "worktree_reaper: {} quarantined uncommitted/untracked changes in \
                         {name} before reclaim (stash {stash_sha}) — recover with `git stash \
                         apply {stash_sha}`",
                        repo_root.display()
                    );
                }
                None => {
                    // #7939: this worktree is not going to be removed this
                    // tick — clear any stuck-removal record for it now,
                    // rather than only on a successful `remove()` call this
                    // route never reaches.
                    clear_stuck_record(&worktree_path);
                    report.skipped.push((
                        num,
                        "uncommitted changes (quarantine-stash failed or nothing to stash)"
                            .to_string(),
                    ));
                    continue;
                }
            }
        } else if let Some(reason) = skip_reason(&decision) {
            // #7939: every `Skip*`/`ConfirmClosedIssue` route below makes this
            // worktree ineligible for removal — via `remove_with_backoff`'s
            // `Ok(())` arm — for as long as the condition holds. If it was
            // previously stuck (backed off after repeated failures) and the
            // condition that made it stuck is what changed (an issue
            // reopened, new uncommitted work landing, the `.loom-managed`
            // sentinel removed, …), the reaper is no longer even attempting a
            // removal here, so `remove_with_backoff` never runs to clear the
            // old record. Clear it directly so `loom-daemon health` reflects
            // reality the moment eligibility changes, not at the next daemon
            // restart.
            clear_stuck_record(&worktree_path);
            report.skipped.push((num, reason));
            continue;
        }

        if remove(&worktree_path, num) {
            report.removed.push(num);
        } else {
            report.failed.push(num);
        }
    }

    report
}

/// [`reap_worktrees`] with the pre-removal confirm (see
/// [`reap_worktrees_generic`]); `reap_worktrees` itself confirms nothing.
pub(super) fn reap_worktrees_confirmed(
    repo_root: &Path,
    opts: &CleanOptions,
    probes: &WorktreeProbes<'_>,
    quarantine: &dyn Fn(&Path, u32) -> Option<String>,
    remove: &dyn Fn(&Path, u32) -> bool,
    confirm: &dyn Fn(u32) -> Option<String>,
) -> ReapReport {
    reap_worktrees_generic(
        repo_root,
        &crate::worktree_ops::naming::issue_from_worktree,
        &|path, issue_num| clean::classify_worktree(path, issue_num, opts, probes),
        quarantine,
        remove,
        confirm,
    )
}

/// [`reap_pr_worktrees`] with the pre-removal confirm.
pub(super) fn reap_pr_worktrees_confirmed(
    repo_root: &Path,
    opts: &CleanOptions,
    probes: &clean::PrWorktreeProbes<'_>,
    remove: &dyn Fn(&Path, u32) -> bool,
    confirm: &dyn Fn(u32) -> Option<String>,
) -> ReapReport {
    reap_worktrees_generic(
        repo_root,
        &crate::worktree_ops::naming::pr_from_worktree,
        &|path, pr_num| clean::classify_pr_worktree(path, pr_num, opts, probes),
        // `classify_pr_worktree` never returns `RemoveWithQuarantine` (issue
        // #6653's quarantine-then-reclaim path is scoped to issue-<N>
        // worktrees only, so far) — this closure is unreachable for the
        // `pr-<N>` pass.
        &|_: &Path, _: u32| None,
        remove,
        confirm,
    )
}

#[cfg(test)]
#[path = "confirm_tests.rs"]
mod confirm_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_issue_never_classifies_quarantines_or_removes_same_named_host_worktree() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join(".loom/worktrees/issue-7");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join("keep"), "host work").unwrap();
        std::fs::create_dir_all(root.path().join(".loom/private-jobs")).unwrap();
        std::fs::write(root.path().join(".loom/private-jobs/issue-7.json"), "{}").unwrap();
        let report = reap_worktrees_generic(
            root.path(),
            &crate::worktree_ops::naming::issue_from_worktree,
            &|_, _| panic!("host worktree classification must not run"),
            &|_, _| panic!("host quarantine must not run"),
            &|_, _| panic!("host removal must not run"),
            &|_| panic!("the confirm must not run"),
        );
        assert_eq!(report.scanned, 1);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.removed.is_empty());
        assert_eq!(std::fs::read_to_string(worktree.join("keep")).unwrap(), "host work");
    }
}
