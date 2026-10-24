//! Backfill the owning workspace onto a sweep child's published event.
//!
//! A sweep child publishes `sweep.issue.{N}.phase` / `.blocker` over the IPC
//! socket, and the contract asks it to put its `LOOM_WORKSPACE` in the payload's
//! `repo`. When it does not, the narration sink has no repo to qualify the
//! `task_id` with and falls back to the bare issue number — so issue #N in two
//! managed repos lands in one Matrix thread, and the bare id can collide with
//! any other line that uses the same number.
//!
//! The daemon usually knows the answer: the child was dispatched into exactly
//! one managed workspace, whose registry tracks a live sweep for `N`. This
//! module looks that up across every provisioned registry and stamps the
//! workspace root when **exactly one** owns a live sweep for the issue. Zero
//! owners (an in-session `/loom:sweep` the daemon never dispatched) or two (the
//! same issue number live in two repos — the very case a guess would get wrong)
//! leave the event untouched, as does a payload that already names its repo.
use super::*;

/// Stamp `event`'s `repo` from the single registry in `registries` that has a
/// live sweep for its issue. A no-op for any event other than a child-published
/// [`Event::SweepPhase`] / [`Event::SweepBlocker`] with no `repo`, and when the
/// owner is not unique. See the module doc.
pub(crate) fn backfill_published_repo<'a>(
    event: &mut Event,
    registries: impl IntoIterator<Item = &'a Arc<Mutex<SweepRegistry>>>,
) {
    let issue = match event {
        Event::SweepPhase {
            issue, repo: None, ..
        }
        | Event::SweepBlocker {
            issue, repo: None, ..
        } => *issue,
        _ => return,
    };
    let mut owners = registries.into_iter().filter_map(|registry| {
        let registry = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry
            .live_issue_sweep_id(issue)
            .map(|_| registry.config.workspace_root.display().to_string())
    });
    // Same string `emit_event` stamps on the daemon's own events, so the sink
    // qualifies a child's phase line exactly like its dispatch line.
    if let (Some(root), None) = (owners.next(), owners.next()) {
        event.set_repo_if_absent(&root);
    }
}

#[cfg(test)]
impl SweepRegistry {
    /// Record a `Running` issue sweep for `issue`, as a dispatch would.
    pub(crate) fn seed_live_issue_sweep_for_test(&mut self, issue: u32) {
        let sweep_id = format!("sweep-issue-{issue}-test");
        let log_path = self.compute_log_path(issue);
        self.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                pgid: None,
                sweep_id,
                kind: SweepKind::Issue(issue),
                pid: 0,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path,
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
                overflow: false,
            },
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn registry(dir: &TempDir, name: &str) -> (Arc<Mutex<SweepRegistry>>, String) {
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        let reg = SweepRegistry::new(SweepRegistryConfig::new(root.clone()));
        (Arc::new(Mutex::new(reg)), root.display().to_string())
    }

    fn phase(issue: u32, repo: Option<&str>) -> Event {
        Event::SweepPhase {
            issue,
            phase: "builder".into(),
            pr_number: None,
            repo: repo.map(ToOwned::to_owned),
        }
    }

    fn repo_of(event: &Event) -> Option<&str> {
        match event {
            Event::SweepPhase { repo, .. } | Event::SweepBlocker { repo, .. } => repo.as_deref(),
            _ => None,
        }
    }

    /// The fleet case: the child omitted `repo`, but only one workspace has a
    /// live sweep for the issue — the daemon dispatched it there.
    #[test]
    fn stamps_the_one_workspace_running_the_issue() {
        let dir = tempfile::tempdir().unwrap();
        let (a, a_root) = registry(&dir, "alpha");
        let (b, _) = registry(&dir, "beta");
        a.lock().unwrap().seed_live_issue_sweep_for_test(42);

        let mut event = phase(42, None);
        backfill_published_repo(&mut event, [&a, &b]);
        assert_eq!(repo_of(&event), Some(a_root.as_str()));

        let mut blocker = Event::SweepBlocker {
            issue: 42,
            reason: "r".into(),
            label_added: "loom:blocked".into(),
            repo: None,
        };
        backfill_published_repo(&mut blocker, [&a, &b]);
        assert_eq!(repo_of(&blocker), Some(a_root.as_str()));
    }

    /// Issue #42 live in two repos is exactly the collision a guess would get
    /// wrong, so it is left unqualified rather than pinned to either.
    #[test]
    fn leaves_an_ambiguous_issue_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = registry(&dir, "alpha");
        let (b, _) = registry(&dir, "beta");
        a.lock().unwrap().seed_live_issue_sweep_for_test(42);
        b.lock().unwrap().seed_live_issue_sweep_for_test(42);

        let mut event = phase(42, None);
        backfill_published_repo(&mut event, [&a, &b]);
        assert_eq!(repo_of(&event), None);
    }

    /// No owner (an in-session sweep the daemon never dispatched) and a
    /// payload that already names its repo are both untouched.
    #[test]
    fn leaves_unowned_and_already_stamped_events_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = registry(&dir, "alpha");
        a.lock().unwrap().seed_live_issue_sweep_for_test(42);

        let mut unowned = phase(7, None);
        backfill_published_repo(&mut unowned, [&a]);
        assert_eq!(repo_of(&unowned), None);

        let mut stamped = phase(42, Some("/elsewhere/gamma"));
        backfill_published_repo(&mut stamped, [&a]);
        assert_eq!(repo_of(&stamped), Some("/elsewhere/gamma"));
    }
}
