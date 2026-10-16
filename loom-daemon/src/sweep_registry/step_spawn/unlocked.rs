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
//! 2. **unlocked**: a Builder step's landed check ([`landed_at_spawn`],
//!    #12251), then the role's resolvers, containment included;
//! 3. **locked**: the sweep id, the item's lock, the peer advertisement, and
//!    the pre-claim command;
//! 4. **unlocked**: the pre-claim (non-Builder roles);
//! 5. **locked**: the roll gate again (a roll may have paused spawns while the
//!    lock was released), a Builder's claim flip and lease comment (the same
//!    phase a sweep takes it in), the stagger and the spawn;
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

/// Run `f` off the async runtime's worker when called from a multi-thread
/// one; `block_in_place` panics on a current-thread runtime, where `f` just
/// runs inline (B8).
pub(crate) fn blocking<T>(f: impl FnOnce() -> T) -> T {
    let multi = tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if multi {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// Where one step spawn's time went (#12298), for the tick span's
/// `loom.dispatch.phase.spawn_*_ms`. A refused spawn stops adding at the
/// phase that refused it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StepSpawnTimings {
    /// Phase 2: a Builder step's landed check, then the role's resolvers,
    /// containment included.
    pub resolve: Duration,
    /// Phases 3 and 4, and a Builder's claim flip and lease comment.
    pub claim: Duration,
    /// Phase 5 but the Builder's claim: the roll gate, the stagger, the
    /// spawn.
    pub launch: Duration,
    /// Phases 6 and 7: the account-selection poll and the record.
    pub account: Duration,
}

impl StepSpawnTimings {
    /// Add `other` to `self`.
    pub fn add(&mut self, other: &Self) {
        self.resolve += other.resolve;
        self.claim += other.claim;
        self.launch += other.launch;
        self.account += other.account;
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
        Self::spawn_step_unlocked_timed(registry, request, &mut StepSpawnTimings::default(), false)
    }

    /// [`Self::spawn_step_unlocked`], adding where its time went to
    /// `timings` (#12298). `host_gate`: this spawn may run beside spawns in
    /// other repos (a concurrent tick), so its launch also keeps the
    /// host-wide stagger gap ([`super::launch_gate`]).
    ///
    /// # Errors
    /// As [`Self::spawn_step_unlocked`].
    #[allow(clippy::too_many_lines)] // the seven phases, kept in order.
    pub fn spawn_step_unlocked_timed(
        registry: &Arc<Mutex<Self>>,
        request: &StepSpawnRequest,
        timings: &mut StepSpawnTimings,
        host_gate: bool,
    ) -> Result<DispatchOutcome> {
        let mut mark = Instant::now();
        let mut lap = |slot: &mut Duration| {
            let now = Instant::now();
            *slot += now.saturating_duration_since(mark);
            mark = now;
        };
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
        let (config, target) = {
            let reg = locked(registry);
            if let Err(e) = reg.roll_gate.admit(&kind) {
                drop(reg);
                return Err(refuse(StepRefusal::new("roll_paused", e)));
            }
            let target = (role == StepRole::Builder)
                .then(|| reg.open_pr_listing_target())
                .flatten();
            (reg.config.clone(), target)
        };
        if let Some(pr) = target.and_then(|t| blocking(|| landed_at_spawn(&t, item))) {
            lap(&mut timings.resolve);
            locked(registry).hold_landed_builder(item);
            return Err(refuse(landed_refusal(item, pr)));
        }
        let resolved = blocking(|| Self::resolve_step_launch(&config, role, &kind));
        lap(&mut timings.resolve);
        let launch = match resolved {
            Ok(launch) => launch,
            Err(refusal) => {
                // The draw excludes a role whose pool is exhausted only from
                // this registry's `RoleTick` entries (B4): record the
                // preflight's refusals there, as a token-selection death is.
                if matches!(refusal.reason, "pool_exhausted" | "no_token_pool") {
                    locked(registry).record_role_tick_pool_exhausted(role.role_name());
                }
                return Err(refuse(refusal));
            }
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
            let claimed = blocking(|| super::lease::run_step_claim(cmd));
            lap(&mut timings.claim);
            if let Err(refusal) = claimed {
                locked(registry).unwind_step_claim(item, role, &sweep_id, false);
                return Err(refuse(refusal));
            }
        }
        lap(&mut timings.claim);
        let mut prepared = {
            let mut reg = locked(registry);
            // A roll may have paused spawns while the lock was released (B5):
            // admit again before the child, under the same lock the spawn's
            // `MidSpawn` is entered under.
            if let Err(e) = reg.roll_gate.admit(&kind) {
                reg.unwind_step_claim(item, role, &sweep_id, role != StepRole::Builder);
                drop(reg);
                return Err(refuse(StepRefusal::new("roll_paused", e)));
            }
            if role == StepRole::Builder {
                lap(&mut timings.launch);
                let claimed = reg.pre_claim_step(item, role, &sweep_id);
                lap(&mut timings.claim);
                if let Err(refusal) = claimed {
                    reg.unwind_step_claim(item, role, &sweep_id, false);
                    drop(reg);
                    return Err(refuse(refusal));
                }
            }
            let spawned = reg.spawn_prepared_step(request, &sweep_id, launch, host_gate);
            lap(&mut timings.launch);
            match spawned {
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
        let finished = locked(registry).finish_step_spawn(prepared, token_name, runtime, death);
        lap(&mut timings.account);
        finished
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

    /// #12298: concurrent spawns in two repos' registries launch through the
    /// host-wide gate, at least the stagger apart; a serial spawn does not
    /// touch it.
    #[test]
    #[serial]
    fn concurrent_spawns_in_two_registries_launch_the_stagger_apart() {
        use super::super::launch_gate::HOST_LAUNCH_GATE;
        let gap = Duration::from_millis(400);
        let dirs = [tempdir().unwrap(), tempdir().unwrap(), tempdir().unwrap()];
        let registries: Vec<Arc<Mutex<SweepRegistry>>> = dirs
            .iter()
            .map(|d| {
                let (mut reg, _) = fixture_registry(d.path());
                reg.set_dispatch_stagger(gap);
                Arc::new(Mutex::new(reg))
            })
            .collect();

        // Serial, in a third registry (so neither concurrent registry has a
        // stagger of its own to wait out): the gate is not used.
        let before = HOST_LAUNCH_GATE.last_launch();
        let serial = SweepRegistry::spawn_step_unlocked(
            &registries[2],
            &StepSpawnRequest::new(9_300, StepRole::Doctor),
        )
        .unwrap();
        assert_eq!(HOST_LAUNCH_GATE.last_launch(), before);

        // Concurrent: two launches at once, one per registry. Each is
        // recorded by the gate as it starts; the second waits out the gap.
        let started = std::time::Instant::now();
        let launches: Vec<std::time::Instant> = std::thread::scope(|s| {
            let handles: Vec<_> = registries[..2]
                .iter()
                .zip([9_301u32, 9_302])
                .map(|(registry, item)| {
                    s.spawn(move || {
                        let mut timings = StepSpawnTimings::default();
                        SweepRegistry::spawn_step_unlocked_timed(
                            registry,
                            &StepSpawnRequest::new(item, StepRole::Curator),
                            &mut timings,
                            true,
                        )
                        .unwrap();
                        timings
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            vec![HOST_LAUNCH_GATE.last_launch().unwrap()]
        });
        // The last launch through the gate started at least one gap after
        // the first (which started no earlier than `started`).
        assert!(
            launches[0].duration_since(started) >= gap,
            "second launch {:?} after the round began",
            launches[0].duration_since(started)
        );

        for registry in &registries {
            let mut reg = registry.lock().unwrap();
            let ids: Vec<String> = reg.entries.keys().cloned().collect();
            for id in ids {
                let _ = reg.cancel(&id, Duration::from_millis(50));
            }
        }
        drop(serial);
    }
}
