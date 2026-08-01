//! `depends_on` / block-the-subtree bookkeeping (issue #3729).

use super::*;

impl SweepRegistry {
    // ------------------------------------------------------------------------
    // Stacked-PR block-the-subtree (issue #3729, v1 item 4)
    // ------------------------------------------------------------------------

    /// Return the issue numbers of every still-live (`Running`/`Pending`)
    /// sweep whose `depends_on` names `parent`. Terminal children are
    /// excluded — they no longer need blocking. Because `depends_on` is a
    /// single optional parent, this only ever returns the *direct* children
    /// of `parent` (a linear chain hop, never a diamond).
    #[must_use]
    pub fn children_of(&self, parent: u32) -> Vec<u32> {
        self.entries
            .values()
            .filter(|info| {
                matches!(info.state, SweepState::Running | SweepState::Pending)
                    && info.depends_on == Some(parent)
            })
            .filter_map(|info| match &info.kind {
                SweepKind::Issue(n) => Some(*n),
                SweepKind::PrSet(_) => None,
            })
            .collect()
    }

    /// Block the subtree stacked on `parent` (issue #3729, v1 item 4).
    ///
    /// For each direct child of `parent` (see [`Self::children_of`]), emit a
    /// `sweep.issue.{child}.blocker` event on the existing frozen event-bus
    /// topic (#3453 — no new topic). This is the safety net that keeps a
    /// stacked child from auto-progressing (opening/merging its PR) when its
    /// parent ends in `loom:blocked`. Auto-detach (rebasing an orphaned child
    /// onto `main`) is explicitly out of v1 scope — block-the-subtree is the
    /// only cascade behavior.
    ///
    /// Returns the child issue numbers that were signalled. Emission is
    /// best-effort (no subscribers ⇒ debug log only), mirroring the rest of
    /// the reaper's event handling.
    pub fn block_children_of(&self, parent: u32, reason: &str) -> Vec<u32> {
        let children = self.children_of(parent);
        for child in &children {
            self.emit_event(Event::SweepBlocker {
                issue: *child,
                reason: reason.to_string(),
                label_added: "loom:blocked".to_string(),
                repo: None, // stamped by emit_event (#3929)
            });
        }
        children
    }

    /// Best-effort check of whether `issue` currently carries the
    /// `loom:blocked` label on the forge. Used by the reaper to decide
    /// whether a terminated parent ended blocked (in which case its stacked
    /// children must be blocked too) versus completing successfully.
    ///
    /// Returns `false` on any error, when label flips are skipped (test
    /// fixtures), or when `gh` is unavailable — a conservative default that
    /// never blocks a child on an unverifiable parent state.
    pub(crate) fn issue_has_blocked_label(&self, issue: u32) -> bool {
        self.issue_has_label_via_graphql(issue, "loom:blocked")
    }

    /// Best-effort check of whether `issue` currently carries the
    /// `loom:operator-only` label on the forge (Issue #4887). Used by
    /// [`restore_label_to_ready`](super::guards::SweepRegistry::restore_label_to_ready)
    /// alongside [`Self::issue_has_blocked_label`] so the crash-path claim
    /// restore never re-adds `loom:issue` on top of either park label.
    ///
    /// Returns `false` on any error, when label flips are skipped (test
    /// fixtures), or when `gh` is unavailable — the same fail-open default as
    /// [`Self::issue_has_blocked_label`].
    pub(crate) fn issue_has_operator_only_label(&self, issue: u32) -> bool {
        self.issue_has_label_via_graphql(issue, "loom:operator-only")
    }

    /// Shared GraphQL-backed (`gh issue view --json labels`) probe for a single
    /// label's presence on `issue`, factored out of
    /// [`Self::issue_has_blocked_label`] so [`Self::issue_has_operator_only_label`]
    /// (#4887) does not duplicate the command-building/timeout plumbing.
    ///
    /// Fails closed (`false`) on any read failure, exactly like the two
    /// callers it backs — never block a cascade, or claim a park label is
    /// present, on an unverifiable read.
    fn issue_has_label_via_graphql(&self, issue: u32, label: &str) -> bool {
        if self.config.skip_label_flip {
            return false;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("issue")
            .arg("view")
            .arg(issue.to_string())
            .arg("--json")
            .arg("labels")
            .arg("--jq")
            .arg(format!(r#"[.labels[].name] | index("{label}") != null"#));
        // Scope the label probe to the registry's workspace so it resolves
        // against the right repo in a multi-workspace daemon (#3937).
        cmd.current_dir(&self.config.workspace_root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        // Bounded so a wedged `gh` on the `ListSweeps` / `GetSweepStatus` read
        // path (this runs inside `reap_liveness`) cannot block the registry read
        // indefinitely (Issue #3973). A timeout is treated as absent — the same
        // conservative default as any other `gh` failure here.
        let timeout = reap_gh_timeout();
        match output_with_timeout(cmd, timeout) {
            Ok(Some(out)) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim() == "true"
            }
            Ok(None) => {
                log::warn!(
                    "sweep_registry: issue_has_label_via_graphql({label}) gh for #{issue} \
                     exceeded {}s and was killed; treating as absent (#3973)",
                    timeout.as_secs()
                );
                false
            }
            _ => false,
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
    use crate::sweep_registry::test_support::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;
    use tempfile::tempdir;

    /// Issue #3729 (v1 item 4, block-the-subtree): `block_children_of` emits a
    /// `sweep.issue.{child}.blocker` event for every live child whose
    /// `depends_on` names the given parent — and nothing for unrelated sweeps.
    #[tokio::test]
    async fn block_children_of_emits_blocker_for_dependents_only() {
        use crate::event_bus::EventBus;

        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let bus = Arc::new(EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        // Parent #60, a stacked child #61 (depends_on=60), and an unrelated
        // independent sweep #62 (depends_on=None).
        for (sid, issue, dep) in [
            ("sweep-issue-60", 60u32, None),
            ("sweep-issue-61", 61u32, Some(60u32)),
            ("sweep-issue-62", 62u32, None),
        ] {
            registry.entries.insert(
                sid.to_string(),
                SweepInfo {
                    sweep_id: sid.to_string(),
                    kind: SweepKind::Issue(issue),
                    pid: 2_147_483_640,
                    token_name: "unknown".into(),
                    runtime: "unknown".into(),
                    runtime_source: None,
                    log_path: registry.compute_log_path(issue),
                    idempotency_key: None,
                    started_at: Utc::now(),
                    state: SweepState::Running,
                    latest_phase: None,
                    pr_number: None,
                    model: None,
                    effort: None,
                    depends_on: dep,
                    repo: None,
                },
            );
        }

        let blocked = registry.block_children_of(60, "parent #60 blocked");
        assert_eq!(blocked, vec![61], "only #61 depends on #60");

        // Exactly one blocker event, for issue 61 on its .blocker topic.
        let ev = sub.recv().await.unwrap();
        match ev {
            Event::SweepBlocker {
                issue, label_added, ..
            } => {
                assert_eq!(issue, 61);
                assert_eq!(label_added, "loom:blocked");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Issue #3729: `children_of` only returns *live* direct children, and a
    /// terminal child is excluded (it no longer needs blocking).
    #[test]
    #[serial]
    fn children_of_returns_live_direct_children_only() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        fn mk(issue: u32, dep: Option<u32>, state: SweepState) -> SweepInfo {
            SweepInfo {
                sweep_id: format!("s{issue}"),
                kind: SweepKind::Issue(issue),
                pid: 2_147_483_640,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path: PathBuf::from(format!(".loom/logs/sweep-issue-{issue}.log")),
                idempotency_key: None,
                started_at: Utc::now(),
                state,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: dep,
                repo: None,
            }
        }
        registry
            .entries
            .insert("s70".into(), mk(70, None, SweepState::Running));
        registry
            .entries
            .insert("s71".into(), mk(71, Some(70), SweepState::Running));
        // Terminal child — excluded.
        registry.entries.insert(
            "s72".into(),
            mk(
                72,
                Some(70),
                SweepState::Exited {
                    code: None,
                    at: Utc::now(),
                },
            ),
        );

        let mut kids = registry.children_of(70);
        kids.sort_unstable();
        assert_eq!(kids, vec![71], "only the live child #71 is returned");
    }
}
