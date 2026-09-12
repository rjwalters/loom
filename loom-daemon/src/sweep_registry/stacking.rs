//! `depends_on` / block-the-subtree bookkeeping (issue #3729).

use super::*;

/// Outcome of probing a single label's presence on the forge (Issue
/// #7553). Distinguishes a probe that ran to completion and found the
/// label absent (`Absent`) from one that could not be run/completed at
/// all (`Unknown`, e.g. a `gh` timeout or non-zero exit) — both of which
/// previously collapsed into a bare `false`, making a forge-probe failure
/// indistinguishable from a confirmed absence.
enum LabelProbe {
    Present,
    Absent,
    Unknown,
}

/// Outcome of probing whether an issue carries one of
/// [`crate::hard_exclusion::HARD_EXCLUSION_LABELS`] (Issue #7528, tri-state
/// split #7553).
///
/// The reaper's discriminator for "this sweep declined on a label rule"
/// versus "this sweep self-skipped / found no work" — both exit 0 with no
/// checkpoint, and nothing in the exit status can tell them apart, but the
/// label is a fact on the forge that needs no cooperation from the agent
/// session that declined.
///
/// `Unknown` is distinct from `NotExcluded`: `decline_cooldown.rs`
/// requires *positive evidence* the rule no longer applies before
/// clearing an armed cooldown record, and a probe that could not
/// complete (forge outage, `gh` timeout, non-zero exit) is the opposite
/// of positive evidence. Only `NotExcluded` — every configured label
/// confirmed absent via a successful probe — authorizes a clear; the
/// caller must treat `Unknown` as "leave existing cooldown state alone".
pub(crate) enum HardExclusionProbe {
    /// One of `HARD_EXCLUSION_LABELS` was confirmed present.
    Excluded(&'static str),
    /// Every configured label was confirmed absent.
    NotExcluded,
    /// At least one label's probe could not be completed before a
    /// confirmed verdict was reached; the true state is unverifiable.
    Unknown,
}

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

    /// The [`crate::hard_exclusion::HARD_EXCLUSION_LABELS`] entry `issue`
    /// currently carries on the forge, if any is confirmed present — else
    /// whether absence was confirmed or the probe was inconclusive (Issue
    /// #7528, tri-state split #7553). See [`HardExclusionProbe`].
    ///
    /// Costs one `gh` round trip per hard-exclusion label (one today), so
    /// callers gate it on a clean exit and on `!skip_label_flip` exactly like
    /// every other forge probe in the reap path.
    pub(crate) fn issue_hard_exclusion_label(&self, issue: u32) -> HardExclusionProbe {
        let mut saw_unknown = false;
        for label in crate::hard_exclusion::HARD_EXCLUSION_LABELS.iter().copied() {
            match self.label_probe_via_graphql(issue, label) {
                LabelProbe::Present => return HardExclusionProbe::Excluded(label),
                LabelProbe::Unknown => saw_unknown = true,
                LabelProbe::Absent => {}
            }
        }
        if saw_unknown {
            HardExclusionProbe::Unknown
        } else {
            HardExclusionProbe::NotExcluded
        }
    }

    /// Best-effort check of whether `issue` currently carries `label` on the
    /// forge, collapsing [`LabelProbe`]'s tri-state down to a bare bool for
    /// [`Self::issue_has_blocked_label`] / [`Self::issue_has_operator_only_label`]
    /// (#4887), which only ever need "is it present" and already fail closed
    /// on anything else.
    ///
    /// Returns `false` on any error, when label flips are skipped (test
    /// fixtures), or when `gh` is unavailable — a conservative default that
    /// never blocks a cascade, or claims a park label is present, on an
    /// unverifiable read. Byte-identical to the pre-#7553 behavior of this
    /// function for these two callers.
    fn issue_has_label_via_graphql(&self, issue: u32, label: &str) -> bool {
        matches!(self.label_probe_via_graphql(issue, label), LabelProbe::Present)
    }

    /// Shared GraphQL-backed (`gh issue view --json labels`) probe for a single
    /// label's presence on `issue`, factored out so
    /// [`Self::issue_has_blocked_label`], [`Self::issue_has_operator_only_label`]
    /// (#4887), and [`Self::issue_hard_exclusion_label`] (#7528/#7553) share
    /// the command-building/timeout plumbing.
    ///
    /// Returns [`LabelProbe::Unknown`] — never a silent `Absent` — on a `gh`
    /// timeout or non-zero exit (Issue #7553): a command that failed to run
    /// to completion tells us nothing about the label's actual state, and
    /// conflating that with a confirmed absence is exactly the bug this
    /// tri-state exists to close.
    fn label_probe_via_graphql(&self, issue: u32, label: &str) -> LabelProbe {
        if self.config.skip_label_flip {
            return LabelProbe::Absent;
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
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        // Bounded so a wedged `gh` on the `ListSweeps` / `GetSweepStatus` read
        // path (this runs inside `reap_liveness`) cannot block the registry read
        // indefinitely (Issue #3973).
        let timeout = reap_gh_timeout();
        match output_with_timeout(cmd, timeout) {
            Ok(Some(out)) if out.status.success() => {
                if String::from_utf8_lossy(&out.stdout).trim() == "true" {
                    LabelProbe::Present
                } else {
                    LabelProbe::Absent
                }
            }
            Ok(None) => {
                log::warn!(
                    "sweep_registry: label_probe_via_graphql({label}) gh for #{issue} \
                     exceeded {}s and was killed; treating as unknown (#3973, #7553)",
                    timeout.as_secs()
                );
                LabelProbe::Unknown
            }
            Ok(Some(out)) => {
                log::warn!(
                    "sweep_registry: label_probe_via_graphql({label}) gh for #{issue} \
                     exited non-zero ({:?}); treating as unknown (#7553)",
                    out.status.code()
                );
                LabelProbe::Unknown
            }
            Err(e) => {
                log::warn!(
                    "sweep_registry: label_probe_via_graphql({label}) gh for #{issue} \
                     failed to spawn: {e}; treating as unknown (#7553)"
                );
                LabelProbe::Unknown
            }
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
                    pgid: None,
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
                pgid: None,
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

    /// Issue #5431: `issue_has_label_via_graphql` (backing both
    /// `issue_has_blocked_label` and `issue_has_operator_only_label`, which
    /// `guards::restore_label_to_ready` depends on) must thread a registered
    /// cross-owner workspace's installation-token `GH_CONFIG_DIR` through to
    /// the real `gh issue view` child.
    #[test]
    #[serial]
    fn issue_has_blocked_label_applies_registered_gh_config_dir() {
        crate::credential_preflight::clear_owner_root_registry();
        let dir = tempdir().unwrap();
        let gh_log = dir.path().join("gh.log");
        let owner_dir = dir.path().join(".loom/gh-config-by-owner/2AMLogic");
        crate::credential_preflight::register_root_gh_config_dir(dir.path(), &owner_dir);

        let fake_gh = install_fake_gh_env_logger(dir.path(), &gh_log, "false", 0);
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false;
        let registry = SweepRegistry::new(config);

        registry.issue_has_blocked_label(9701);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains(&format!("GH_CONFIG_DIR={}", owner_dir.display())),
            "expected the registered owner's GH_CONFIG_DIR on the gh child; got: {gh_calls:?}"
        );

        crate::credential_preflight::clear_owner_root_registry();
    }
}
