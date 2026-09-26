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
pub(super) fn reap_worktrees_generic(
    repo_root: &Path,
    parse_name: &dyn Fn(&str) -> Option<u32>,
    classify: &dyn Fn(&Path, u32) -> WorktreeDecision,
    quarantine: &dyn Fn(&Path, u32) -> Option<String>,
    remove: &dyn Fn(&Path, u32) -> bool,
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
        );
        assert_eq!(report.scanned, 1);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.removed.is_empty());
        assert_eq!(std::fs::read_to_string(worktree.join("keep")).unwrap(), "host work");
    }
}
