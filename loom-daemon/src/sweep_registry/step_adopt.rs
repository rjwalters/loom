//! Startup adoption of steps (Issue #11898).
//!
//! A restarted daemon rebuilds its registry from the claim locks it finds
//! (`reconstruct_issues`). A step holds one of two locks: an issue step the
//! `issue-<N>` lock an issue sweep holds, a PR step the `pr-<N>` lock a PR set
//! holds. The lock's owner records the holder's sweep id, which for a step is
//! `step-<role>-<item>-…` ([`StepRole::parse_sweep_id`]), so the kind is
//! recovered from the lock alone:
//!
//! - a live step is adopted as `SweepKind::Step { item, role }`, never as an
//!   issue sweep, so none of the issue-sweep paths (the `/loom:sweep`
//!   watchdogs, the `loom:building` → `loom:issue` restore, checkpoint resume)
//!   ever act on it;
//! - a dead step's lock is dropped, its process group reaped, and its claim
//!   released per role ([`SweepRegistry::restore_step_claim`]), which writes
//!   nothing when the step had already moved its item on.
//!
//! The issue-lock half lives in `reconstruct_issues` beside the sweep case;
//! this module adds the `pr-<N>` scan. A `pr-<N>` lock owned by a PR set is
//! left exactly as before (PR sets are not reconstructed).

use super::journal_shadow::LockNs;
use super::*;
use crate::step_role::StepRole;

impl SweepRegistry {
    /// Release the claim of a step whose lock outlived its process across a
    /// restart, unless another claimant holds it now (`started_at` is the
    /// lock's `acquired_at`). Best-effort: a failed write only logs.
    pub(crate) fn restore_dead_step_at_startup(
        &mut self,
        item: u32,
        role: StepRole,
        sweep_id: &str,
        started_at: DateTime<Utc>,
    ) {
        if self.config.skip_label_flip {
            return;
        }
        // The daemon was down: a peer may have reclaimed the item since, and
        // restoring over its live claim would stomp it (#11898).
        if self.step_claim_held_elsewhere(item, role, sweep_id, started_at) {
            log::info!(
                "reconstruct: {role} step {sweep_id} on #{item} died while no daemon ran, but \
                 its claim is held elsewhere now — leaving the labels alone (#11898)"
            );
            self.publish_step_claim(peer_claims::ClaimKind::Retract, item, role);
            return;
        }
        match self.restore_step_claim(item, role, sweep_id) {
            Ok(release) => log::info!(
                "reconstruct: {role} step {sweep_id} on #{item} died while no daemon ran — \
                 claim release: {release:?} (#11898)"
            ),
            Err(e) => log::warn!(
                "reconstruct: {role} step {sweep_id} on #{item}: claim release failed: {e:#}"
            ),
        }
        self.publish_step_claim(peer_claims::ClaimKind::Retract, item, role);
    }

    /// Adopt every live PR step from its `pr-<N>` lock, and release every dead
    /// one. `only` restricts the scan, as for issues. Returns the number
    /// adopted.
    pub(crate) fn adopt_pr_steps(&mut self, only: Option<&HashSet<u32>>) -> usize {
        let Ok(entries) = std::fs::read_dir(self.config.locks_dir()) else {
            return 0;
        };
        let mut admitted = 0;
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            let Some(pr) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("pr-"))
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            if only.is_some_and(|set| !set.contains(&pr)) {
                continue;
            }
            let Some(owner) = std::fs::read_to_string(path.join("owner.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<LockOwner>(&s).ok())
            else {
                continue;
            };
            let Some((role, item)) = StepRole::parse_sweep_id(&owner.sweep_id)
                .filter(|(role, item)| *item == pr && role.acts_on_pr())
            else {
                continue; // a PR set's lock, or not a step: untouched
            };
            if self.entries.contains_key(&owner.sweep_id) {
                continue;
            }
            let alive =
                reaper::pid_identity::owner_pid_alive_since(owner.owner_pid, &owner.acquired_at);
            if !alive
                && crate::roll_pause::suppress::held_item(
                    &self.config.workspace_root,
                    &owner.sweep_id,
                )
                .is_some()
            {
                log::info!(
                    "reconstruct: {role} step on PR #{pr} was paused for a daemon roll; leaving \
                     its lock for the resume (#11898)"
                );
                continue;
            }
            let orphaned_group = !alive
                && owner
                    .pgid
                    .is_some_and(|pgid| self.group_still_populated(&owner.sweep_id, pgid));
            if !alive && !orphaned_group {
                if let Some(pgid) = owner.pgid {
                    self.reap_orphaned_group(&owner.sweep_id, Some(pr), pgid);
                }
                if std::fs::remove_dir_all(&path).is_ok() {
                    self.shadow_owner_released(LockNs::Pr, pr, Some(&owner.sweep_id));
                }
                let started_at = chrono::DateTime::parse_from_rfc3339(&owner.acquired_at)
                    .map_or_else(|_| Utc::now(), |t| t.with_timezone(&Utc));
                self.restore_dead_step_at_startup(item, role, &owner.sweep_id, started_at);
                continue;
            }
            let pgid = owner.pgid.filter(|&recorded| {
                orphaned_group || process_group_of(owner.owner_pid) == Some(recorded)
            });
            let log_path = self.compute_step_log_path(item, role);
            let started_at = chrono::DateTime::parse_from_rfc3339(&owner.acquired_at)
                .map_or_else(|_| Utc::now(), |t| t.with_timezone(&Utc));
            self.entries.insert(
                owner.sweep_id.clone(),
                SweepInfo {
                    sweep_id: owner.sweep_id.clone(),
                    kind: SweepKind::Step { item, role },
                    pid: owner.owner_pid,
                    pgid,
                    token_name: recover_adopted_token_name(&log_path, &owner.sweep_id),
                    runtime: recover_adopted_runtime(&log_path, &owner.sweep_id),
                    runtime_source: None,
                    log_path,
                    idempotency_key: None,
                    started_at,
                    state: SweepState::Running,
                    latest_phase: None,
                    pr_number: None,
                    model: owner.model.clone(),
                    effort: owner.effort.clone(),
                    depends_on: None,
                    repo: Some(self.config.workspace_root.display().to_string()),
                    overflow: owner.overflow,
                },
            );
            crate::observability::lifecycle::execution_adopted(
                &self.config.workspace_root,
                &owner.sweep_id,
            );
            admitted += 1;
        }
        admitted
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::fixture_registry;

    fn write_owner(reg: &SweepRegistry, dir: &str, number: u32, sweep_id: &str, pid: u32) {
        let lock = reg.config.locks_dir().join(dir);
        std::fs::create_dir_all(&lock).unwrap();
        let owner = LockOwner::new(number, pid, sweep_id);
        std::fs::write(lock.join("owner.json"), serde_json::to_string(&owner).unwrap()).unwrap();
    }

    /// A live issue step and a live PR step survive a restart as the steps
    /// they are, keeping role and item.
    #[test]
    fn live_steps_are_adopted_as_steps() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reg, _) = fixture_registry(dir.path());
        let me = std::process::id();
        write_owner(&reg, "issue-40", 40, "step-curator-40-1700000000", me);
        write_owner(&reg, "pr-41", 41, "step-doctor-41-1700000000", me);
        write_owner(&reg, "issue-42", 42, "sweep-issue-42-1700000000", me);

        assert_eq!(reg.reconstruct().unwrap(), 3);
        let kind = |id: &str| reg.entries[id].kind.clone();
        assert_eq!(
            kind("step-curator-40-1700000000"),
            SweepKind::Step {
                item: 40,
                role: StepRole::Curator
            }
        );
        assert_eq!(
            kind("step-doctor-41-1700000000"),
            SweepKind::Step {
                item: 41,
                role: StepRole::Doctor
            }
        );
        assert_eq!(kind("sweep-issue-42-1700000000"), SweepKind::Issue(42));
    }

    /// A dead step's lock is dropped without a `Crashed` sweep entry; a PR
    /// set's `pr-<N>` lock is left alone.
    #[test]
    fn dead_steps_release_their_locks_and_pr_set_locks_are_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reg, _) = fixture_registry(dir.path());
        let dead = {
            let mut c = std::process::Command::new("true").spawn().unwrap();
            let pid = c.id();
            c.wait().unwrap();
            pid
        };
        write_owner(&reg, "issue-50", 50, "step-builder-50-1700000000", dead);
        write_owner(&reg, "pr-51", 51, "step-judge-51-1700000000", dead);
        write_owner(&reg, "pr-52", 52, "sweep-prs-52-1700000000", std::process::id());
        // A builder checkpoint must not turn a dead step into a crashed sweep.
        crate::sweep_registry::test_support::write_checkpoint(&reg, 50, "builder");

        assert_eq!(reg.reconstruct().unwrap(), 0);
        let locks = reg.config.locks_dir();
        assert!(!locks.join("issue-50").exists());
        assert!(!locks.join("pr-51").exists());
        assert!(locks.join("pr-52").exists(), "a PR set's lock is not a step's");
        assert!(reg.entries.values().all(|i| i.kind != SweepKind::Issue(50)));
    }

    #[test]
    fn the_sweep_id_names_the_step() {
        assert_eq!(
            StepRole::parse_sweep_id("step-champion_promote-12-1700000000-r1"),
            Some((StepRole::ChampionPromote, 12))
        );
        assert_eq!(StepRole::parse_sweep_id("sweep-issue-12-1700000000"), None);
        assert_eq!(StepRole::parse_sweep_id("step-nobody-12-1"), None);
    }
}
