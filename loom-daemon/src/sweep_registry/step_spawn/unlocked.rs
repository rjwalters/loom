//! A step spawn that releases the registry lock around its slow parts
//! (#11909, the #11961 review): the work finder's step tick calls this.
//!
//! [`SweepRegistry::spawn_step`] holds `&mut self` from the role's resolvers
//! to the recorded child. Two of its parts can take seconds: the runtime
//! admission's private-clone containment (#8787, Docker) and a non-Builder
//! step's pre-claim (`lease pr claim`, forge reads and a write). Run under the
//! lock they would stall every other registry user: the reaper, IPC, the
//! status surface. So, as `private_dispatch::prepare` does for sweeps:
//!
//! 1. **locked**: the roll gate, and a copy of the configuration;
//! 2. **unlocked**: the role's resolvers, containment included;
//! 3. **locked**: the sweep id, the item's lock, the peer advertisement, and
//!    the pre-claim command;
//! 4. **unlocked**: the pre-claim (non-Builder roles);
//! 5. **locked**: a Builder's claim flip and lease comment (the same phase a
//!    sweep takes it in), the stagger and the spawn;
//! 6. **unlocked**: the account-selection poll;
//! 7. **locked**: [`SweepRegistry::finish_step_spawn`].
//!
//! The item's lock taken in phase 3 is what keeps another dispatcher off the
//! item while the lock is released.

use super::*;

fn locked(registry: &Arc<Mutex<SweepRegistry>>) -> std::sync::MutexGuard<'_, SweepRegistry> {
    registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run `f` off the async runtime's worker when called from one.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

impl SweepRegistry {
    /// Spawn one step with the lock released around containment and the
    /// pre-claim (module docs).
    ///
    /// # Errors
    /// A [`StepRefusal`] (downcastable) naming why nothing ran; every side
    /// effect taken so far is undone.
    pub fn spawn_step_unlocked(
        registry: &Arc<Mutex<Self>>,
        request: &StepSpawnRequest,
    ) -> Result<DispatchOutcome> {
        let refuse = |refusal: StepRefusal| -> anyhow::Error {
            let reg = locked(registry);
            reg.record_step_dispatch(request, dispatch_step::RESULT_REFUSED, refusal.reason, None);
            log::info!(
                "sweep_registry: {} step on #{} not spawned — {refusal}",
                request.role,
                request.item
            );
            anyhow::Error::new(refusal)
        };
        let (item, role, kind) = (request.item, request.role, request.kind());
        let config = {
            let reg = locked(registry);
            if let Err(e) = reg.roll_gate.admit(&kind) {
                drop(reg);
                return Err(refuse(StepRefusal::new("roll_paused", e)));
            }
            reg.config.clone()
        };
        let launch = match blocking(|| Self::resolve_step_launch(&config, role, &kind)) {
            Ok(launch) => launch,
            Err(refusal) => return Err(refuse(refusal)),
        };
        let sweep_id = generate_sweep_id(&kind);
        let claim_cmd = {
            let reg = locked(registry);
            let held = if role.acts_on_pr() {
                reg.acquire_pr_lock(item, &sweep_id)
            } else {
                reg.acquire_lock(item, &sweep_id)
            };
            if let Err(e) = held {
                drop(reg);
                return Err(refuse(StepRefusal::new("claimed", format!("{e:#}"))));
            }
            reg.publish_step_claim(peer_claims::ClaimKind::Advertise, item, role);
            (role != StepRole::Builder && !config.skip_label_flip)
                .then(|| reg.step_claim_command(item, role, &sweep_id))
        };
        if let Some(cmd) = claim_cmd {
            if let Err(refusal) = blocking(|| super::lease::run_step_claim(cmd)) {
                locked(registry).unwind_step_claim(item, role, &sweep_id, false);
                return Err(refuse(refusal));
            }
        }
        let mut prepared = {
            let mut reg = locked(registry);
            if role == StepRole::Builder {
                if let Err(refusal) = reg.pre_claim_step(item, role, &sweep_id) {
                    reg.unwind_step_claim(item, role, &sweep_id, false);
                    drop(reg);
                    return Err(refuse(refusal));
                }
            }
            match reg.spawn_prepared_step(request, &sweep_id, launch) {
                Ok(p) => p,
                Err(refusal) => {
                    drop(reg);
                    return Err(refuse(refusal));
                }
            }
        };
        let (token_name, runtime, death) = blocking(|| {
            poll_and_classify_spawned_child(
                &mut prepared.child,
                &prepared.log_path,
                &prepared.header_anchor,
            )
        });
        locked(registry).finish_step_spawn(prepared, token_name, runtime, death)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::{assert_child_wrote, fixture_registry};
    use serial_test::serial;
    use tempfile::tempdir;

    /// The unlocked path launches and tracks a step exactly as `spawn_step`
    /// does, and a held item is refused before anything runs.
    #[test]
    #[serial]
    fn the_unlocked_spawn_launches_and_refuses_like_the_locked_one() {
        let dir = tempdir().unwrap();
        let (reg, record) = fixture_registry(dir.path());
        crate::sweep_registry::test_support::write_lock_owner(
            &reg,
            9_201,
            "sweep-issue-9201-1",
            std::process::id(),
        );
        let registry = Arc::new(Mutex::new(reg));
        let outcome = SweepRegistry::spawn_step_unlocked(
            &registry,
            &StepSpawnRequest::new(9_200, StepRole::Doctor),
        )
        .unwrap();
        let recorded = assert_child_wrote(&record, &format!("LOOM_SWEEP_ID={}", outcome.sweep_id));
        assert!(recorded.contains("arg: /loom:doctor 9200"), "{recorded}");
        let refused = SweepRegistry::spawn_step_unlocked(
            &registry,
            &StepSpawnRequest::new(9_201, StepRole::Curator),
        )
        .unwrap_err()
        .downcast::<StepRefusal>()
        .unwrap();
        assert_eq!(refused.reason, "claimed");
        let mut reg = registry.lock().unwrap();
        assert!(matches!(
            reg.entries[&outcome.sweep_id].kind,
            SweepKind::Step {
                item: 9_200,
                role: StepRole::Doctor
            }
        ));
        let ids: Vec<String> = reg.entries.keys().cloned().collect();
        for id in ids {
            let _ = reg.cancel(&id, Duration::from_millis(50));
        }
    }
}
