//! Tests for the per-host backstop ceiling (Issue #8555).
//!
//! Every test that touches the lease store points [`LEASE_DIR_ENV`] at a
//! tempdir and is `#[serial]`: the real store is machine-wide by design, so a
//! parallel test that forgot the override would count a sibling's leases (and,
//! worse, a developer's real ones).

use super::*;

/// A PID that is certainly not a live process. `u32::MAX` is above every
/// platform's `pid_max`, so it can never be recycled onto a real process
/// mid-test.
const DEAD_PID: u32 = u32::MAX;

/// Points the machine-wide lease dir at a tempdir for the scope of a test and
/// restores whatever was there before, including across a panic.
///
/// It also clears [`LEASE_STALE_SECS_ENV`] for the same scope: the age
/// thresholds are `env > default`, so an operator who had shortened them on
/// this host would otherwise reap fixtures the age tests expect to still be
/// live. Hermetic by construction, not by luck.
struct Store {
    dir: tempfile::TempDir,
    prior_dir: Option<std::ffi::OsString>,
    prior_stale: Option<std::ffi::OsString>,
}

impl Store {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let prior_dir = std::env::var_os(LEASE_DIR_ENV);
        let prior_stale = std::env::var_os(LEASE_STALE_SECS_ENV);
        std::env::set_var(LEASE_DIR_ENV, dir.path().join("backstop"));
        std::env::remove_var(LEASE_STALE_SECS_ENV);
        Self {
            dir,
            prior_dir,
            prior_stale,
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("backstop")
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        match self.prior_dir.take() {
            Some(value) => std::env::set_var(LEASE_DIR_ENV, value),
            None => std::env::remove_var(LEASE_DIR_ENV),
        }
        match self.prior_stale.take() {
            Some(value) => std::env::set_var(LEASE_STALE_SECS_ENV, value),
            None => std::env::remove_var(LEASE_STALE_SECS_ENV),
        }
    }
}

fn bound(max: Option<u32>) -> BackstopCeiling {
    BackstopCeiling {
        max_concurrent: max,
        applies_from: DEFAULT_APPLIES_FROM,
        min_complexity: None,
    }
}

fn admit_one(ceiling: &BackstopCeiling) -> Verdict {
    admit(ceiling, "builder", &Tap::runtime("opencode"), Some("complex"))
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The edge case that keeps every pre-#8555 fleet byte-identical: with no key
/// configured there is no ceiling at all, which is what makes the fast path in
/// `resolve_runtime_for` skip the store entirely.
#[test]
#[serial_test::serial]
fn an_unconfigured_ceiling_is_unbounded() {
    let prior = std::env::var_os(MAX_CONCURRENT_ENV);
    std::env::remove_var(MAX_CONCURRENT_ENV);
    for config in [
        serde_json::json!({}),
        serde_json::json!({"runtimes": {"preference": ["claude", "opencode"]}}),
        serde_json::json!({"runtimes": {"backstopCeiling": {}}}),
        serde_json::json!({"runtimes": {"backstopCeiling": null}}),
        // `appliesFrom` alone bounds nothing — there is no count and no
        // filter, so it is inert rather than a ceiling of zero.
        serde_json::json!({"runtimes": {"backstopCeiling": {"appliesFrom": 2}}}),
    ] {
        assert_eq!(configured(&config).unwrap(), None, "{config}");
        assert!(check_config(&config).is_empty(), "{config}");
    }
    if let Some(value) = prior {
        std::env::set_var(MAX_CONCURRENT_ENV, value);
    }
}

#[test]
#[serial_test::serial]
fn a_configured_ceiling_parses_every_key() {
    let prior = std::env::var_os(MAX_CONCURRENT_ENV);
    std::env::remove_var(MAX_CONCURRENT_ENV);
    let config = serde_json::json!({
        "runtimes": {
            "backstopCeiling": {"maxConcurrent": 3, "appliesFrom": 2, "minComplexity": "complex"}
        }
    });
    assert_eq!(
        configured(&config).unwrap(),
        Some(BackstopCeiling {
            max_concurrent: Some(3),
            applies_from: 2,
            min_complexity: Some(ComplexityTier::Complex),
        })
    );
    // `appliesFrom` is what keeps a flat-rate tier 1 (a Codex seat) out of the
    // ceiling's reach while tier 2 (the metered tap) stays governed.
    let ceiling = configured(&config).unwrap().unwrap();
    assert!(!ceiling.governs(0));
    assert!(!ceiling.governs(1));
    assert!(ceiling.governs(2));
    if let Some(value) = prior {
        std::env::set_var(MAX_CONCURRENT_ENV, value);
    }
}

/// `env > config > default`, the daemon's standing precedence.
#[test]
#[serial_test::serial]
fn the_env_override_outranks_the_configured_count_and_can_create_one() {
    let prior = std::env::var_os(MAX_CONCURRENT_ENV);
    std::env::set_var(MAX_CONCURRENT_ENV, "1");
    let configured_two = serde_json::json!({"runtimes": {"backstopCeiling": {"maxConcurrent": 9}}});
    assert_eq!(configured(&configured_two).unwrap().unwrap().max_concurrent, Some(1));
    // With nothing configured at all, the env var is the whole ceiling.
    assert_eq!(
        configured(&serde_json::json!({})).unwrap().unwrap(),
        BackstopCeiling {
            max_concurrent: Some(1),
            applies_from: DEFAULT_APPLIES_FROM,
            min_complexity: None,
        }
    );
    // Junk is ignored in favour of the configured value rather than silently
    // removing the bound.
    std::env::set_var(MAX_CONCURRENT_ENV, "lots");
    assert_eq!(configured(&configured_two).unwrap().unwrap().max_concurrent, Some(9));
    match prior {
        Some(value) => std::env::set_var(MAX_CONCURRENT_ENV, value),
        None => std::env::remove_var(MAX_CONCURRENT_ENV),
    }
}

/// A spend ceiling that degraded silently to "unbounded" on a typo would fail
/// in exactly the direction the feature exists to prevent.
#[test]
#[serial_test::serial]
fn malformed_ceiling_config_fails_closed() {
    let prior = std::env::var_os(MAX_CONCURRENT_ENV);
    std::env::remove_var(MAX_CONCURRENT_ENV);
    let cases: [(serde_json::Value, &str); 6] = [
        (serde_json::json!({"runtimes": {"backstopCeiling": 2}}), "must be an object"),
        (
            serde_json::json!({"runtimes": {"backstopCeiling": {"maxConcurent": 2}}}),
            "unknown key(s)",
        ),
        (
            serde_json::json!({"runtimes": {"backstopCeiling": {"maxConcurrent": -1}}}),
            "non-negative integer",
        ),
        (
            serde_json::json!({"runtimes": {"backstopCeiling": {"maxConcurrent": 2, "appliesFrom": 0}}}),
            "must be >= 1",
        ),
        (
            serde_json::json!({"runtimes": {"backstopCeiling": {"minComplexity": "hard"}}}),
            "one of mechanical, routine, complex",
        ),
        (
            serde_json::json!({"runtimes": {"backstopCeiling": {"minComplexity": 3}}}),
            "must be a string",
        ),
    ];
    for (config, expected) in cases {
        let error = configured(&config).unwrap_err();
        assert!(error.contains(expected), "{config} -> {error}");
        assert_eq!(check_config(&config).len(), 1, "{config}");
    }
    if let Some(value) = prior {
        std::env::set_var(MAX_CONCURRENT_ENV, value);
    }
}

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

/// The core bound: the N+1th concurrent dispatch is passed over while the
/// first N hold slots, and capacity returns the moment one is released.
#[test]
#[serial_test::serial]
fn the_n_plus_first_concurrent_dispatch_is_refused() {
    let store = Store::new();
    let ceiling = bound(Some(2));
    let first = match admit_one(&ceiling) {
        Verdict::Admitted(slot) => slot,
        other => panic!("first dispatch must be admitted, got {other:?}"),
    };
    let second = match admit_one(&ceiling) {
        Verdict::Admitted(slot) => slot,
        other => panic!("second dispatch must be admitted, got {other:?}"),
    };
    assert_eq!(second.summary(), "2/2");
    assert_eq!(live_count(&store.path()).unwrap(), 2);

    let Verdict::Refused(reason) = admit_one(&ceiling) else {
        panic!("the third concurrent dispatch must be refused at a ceiling of 2");
    };
    assert_eq!(reason.kind(), "ceiling-at-capacity");
    assert!(reason.summary().contains("2/2"), "{}", reason.summary());
    assert!(matches!(
        reason,
        SkipReason::Ceiling {
            skip: CeilingSkip::AtCapacity { live: 2, limit: 2 },
            ..
        }
    ));
    // A refusal writes nothing: the count is unchanged.
    assert_eq!(live_count(&store.path()).unwrap(), 2);

    // Releasing one frees exactly one slot.
    first.release();
    assert_eq!(live_count(&store.path()).unwrap(), 1);
    assert!(matches!(admit_one(&ceiling), Verdict::Admitted(_)));
    drop(second);
}

/// The race the `mkdir` control lock exists to close, and the one a green suite
/// most easily misses: without it, peers that all read `live == limit - 1`
/// before any of them writes would *all* admit, and the ceiling would be
/// breached by exactly the concurrency it was configured to bound. Sequential
/// admission cannot detect that — only contention can.
///
/// Threads rather than processes because the lock is filesystem-level, so it
/// serialises either equally, and the count is file-based rather than
/// in-memory: nothing here shares state through the process except the store
/// the lock guards.
#[test]
#[serial_test::serial]
fn concurrent_admissions_never_exceed_the_ceiling() {
    const LIMIT: u32 = 3;
    const PEERS: usize = 12;

    let store = Store::new();
    let ceiling = bound(Some(LIMIT));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(PEERS));

    let admitted: Vec<Verdict> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..PEERS)
            .map(|_| {
                let barrier = std::sync::Arc::clone(&barrier);
                let ceiling = ceiling.clone();
                // Every peer counts-and-reserves in the same instant, which is
                // precisely when a check-then-act without a lock over-admits.
                scope.spawn(move || {
                    barrier.wait();
                    admit_one(&ceiling)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let (granted, refused): (Vec<_>, Vec<_>) = admitted
        .into_iter()
        .partition(|verdict| matches!(verdict, Verdict::Admitted(_)));
    assert_eq!(
        granted.len(),
        LIMIT as usize,
        "exactly {LIMIT} of {PEERS} contending peers may hold a metered slot"
    );
    assert_eq!(refused.len(), PEERS - LIMIT as usize);
    for verdict in &refused {
        let Verdict::Refused(reason) = verdict else {
            unreachable!()
        };
        assert_eq!(reason.kind(), "ceiling-at-capacity", "{reason:?}");
    }
    // The store agrees with the verdicts: no peer wrote a lease it was then
    // refused for, and none of the granted slots was lost to a racing reap.
    assert_eq!(live_count(&store.path()).unwrap(), LIMIT);

    // `summary()` numbers the slots 1..=LIMIT with no duplicates — each peer
    // saw its own reservation counted, so none of them read a stale total.
    let mut seen: Vec<u32> = granted
        .iter()
        .map(|verdict| match verdict {
            Verdict::Admitted(slot) => slot.usage().0,
            _ => unreachable!(),
        })
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, (1..=LIMIT).collect::<Vec<_>>());

    drop(granted);
    assert_eq!(live_count(&store.path()).unwrap(), 0);
}

/// A ceiling of zero switches the metered tier off without the operator having
/// to edit the preference list — still a skip, never a stall.
#[test]
#[serial_test::serial]
fn a_zero_ceiling_refuses_everything_without_touching_the_store() {
    let store = Store::new();
    let Verdict::Refused(reason) = admit_one(&bound(Some(0))) else {
        panic!("a ceiling of 0 must refuse");
    };
    assert_eq!(reason.kind(), "ceiling-at-capacity");
    assert!(reason.summary().contains("switched off"), "{}", reason.summary());
    assert!(!store.path().exists(), "a zero ceiling must not create the store");
}

/// An eligibility filter, not a count: low-value work never reaches the
/// metered tap at all, and an unmarked issue counts as `routine` (the same
/// default the rest of the daemon applies).
#[test]
#[serial_test::serial]
fn the_complexity_filter_excludes_work_below_the_configured_tier() {
    let _store = Store::new();
    let ceiling = BackstopCeiling {
        max_concurrent: Some(4),
        applies_from: DEFAULT_APPLIES_FROM,
        min_complexity: Some(ComplexityTier::Complex),
    };
    let tap = Tap::runtime("opencode");
    for low in [Some("routine"), Some("mechanical"), None, Some("bogus")] {
        let Verdict::Refused(reason) = admit(&ceiling, "builder", &tap, low) else {
            panic!("{low:?} must not reach the metered tap under minComplexity=complex");
        };
        assert_eq!(reason.kind(), "ceiling-ineligible");
        assert!(reason.summary().contains("complex"), "{}", reason.summary());
    }
    assert!(matches!(
        admit(&ceiling, "builder", &tap, Some("complex")),
        Verdict::Admitted(_)
    ));
    // An unmarked dispatch is `routine`, which a `routine` minimum admits.
    let routine_minimum = BackstopCeiling {
        min_complexity: Some(ComplexityTier::Routine),
        ..ceiling
    };
    assert!(matches!(admit(&routine_minimum, "builder", &tap, None), Verdict::Admitted(_)));
}

/// A filter with no count bounds eligibility only — no slot is taken and the
/// store is never created.
#[test]
#[serial_test::serial]
fn an_eligibility_only_ceiling_takes_no_slot() {
    let store = Store::new();
    let ceiling = BackstopCeiling {
        max_concurrent: None,
        applies_from: DEFAULT_APPLIES_FROM,
        min_complexity: Some(ComplexityTier::Routine),
    };
    assert!(matches!(
        admit(&ceiling, "builder", &Tap::runtime("opencode"), Some("complex")),
        Verdict::Unbounded
    ));
    assert!(!store.path().exists());
}

// ---------------------------------------------------------------------------
// The lease lifecycle
// ---------------------------------------------------------------------------

/// The RAII property dispatch depends on: a reservation that is never attached
/// is released when it drops, so resolving without launching — or panicking
/// between the two — cannot strand a metered slot.
#[test]
#[serial_test::serial]
fn an_unattached_reservation_is_released_on_drop() {
    let store = Store::new();
    let path = {
        let Verdict::Admitted(slot) = admit_one(&bound(Some(1))) else {
            panic!("expected admission");
        };
        slot.path().to_path_buf()
    };
    assert!(!path.exists(), "drop must release an unattached reservation");
    assert_eq!(live_count(&store.path()).unwrap(), 0);
}

/// Attaching hands the slot to the spawned worker: the lease survives the
/// resolving scope and is then governed by that PID's liveness, which is what
/// makes the count mean "dispatches currently running" rather than "dispatches
/// this process is still holding a handle for".
#[test]
#[serial_test::serial]
fn attaching_hands_the_slot_to_the_spawned_worker() {
    let store = Store::new();
    let path = {
        let Verdict::Admitted(slot) = admit_one(&bound(Some(1))) else {
            panic!("expected admission");
        };
        let path = slot.path().to_path_buf();
        slot.attach(std::process::id()).unwrap();
        path
    };
    assert!(path.exists(), "attach must keep the lease past the reservation scope");
    assert_eq!(live_count(&store.path()).unwrap(), 1);

    // A lease attached to a PID that is gone is not counted, and is reaped —
    // no release step is needed when a sweep dies.
    std::fs::remove_file(&path).unwrap();
    let Verdict::Admitted(slot) = admit_one(&bound(Some(1))) else {
        panic!("expected admission");
    };
    let dead = slot.path().to_path_buf();
    slot.attach(DEAD_PID).unwrap();
    assert_eq!(live_count(&store.path()).unwrap(), 0);
    assert!(!dead.exists(), "a dead owner's lease was not reaped");
}

/// A reservation whose launch died between resolve and spawn ages out on the
/// short threshold — a stranded slot must not throttle the host for hours.
#[test]
#[serial_test::serial]
fn an_unattached_reservation_ages_out_far_sooner_than_an_attached_lease() {
    let store = Store::new();
    for (attached, age, expected) in [
        (false, DEFAULT_RESERVATION_STALE_SECS + 1, 0),
        (false, DEFAULT_RESERVATION_STALE_SECS - 60, 1),
        (true, DEFAULT_RESERVATION_STALE_SECS + 1, 1),
        (true, DEFAULT_LEASE_STALE_SECS + 1, 0),
    ] {
        std::fs::create_dir_all(store.path()).unwrap();
        let path = store.path().join("fixture.json");
        let holder = serde_json::json!({
            "pid": std::process::id(),
            "startedAt": epoch_now() - age,
            "attached": attached,
            "tap": "opencode",
            "role": "builder",
        });
        std::fs::write(&path, holder.to_string()).unwrap();
        assert_eq!(live_count(&store.path()).unwrap(), expected, "attached={attached} age={age}");
        let _ = std::fs::remove_file(&path);
    }
}

/// Over-counting is the safe direction for a spend ceiling: a lease file
/// nothing can parse still proves a dispatch took a slot, so it counts until
/// it ages out.
#[test]
#[serial_test::serial]
fn an_unparsable_lease_counts_until_it_ages_out() {
    let store = Store::new();
    std::fs::create_dir_all(store.path()).unwrap();
    std::fs::write(store.path().join("torn.json"), "{not json").unwrap();
    assert_eq!(live_count(&store.path()).unwrap(), 1);
}

/// The deliberate inversion of `api_keys_pool::inflight`'s degrade-open: an
/// unknown count must never read as "there is room", because guessing wrong
/// costs real money. The fallback — prefer another tap, or hold — costs
/// throughput only, and is never a human-in-the-loop step.
#[test]
#[serial_test::serial]
fn an_unusable_store_fails_closed_rather_than_admitting() {
    let store = Store::new();
    // A file where the lease directory belongs: `create_dir_all` fails.
    std::fs::write(store.path(), "not a directory").unwrap();
    let Verdict::Refused(reason) = admit_one(&bound(Some(4))) else {
        panic!("an unusable lease store must not admit a metered dispatch");
    };
    assert_eq!(reason.kind(), "ceiling-unknown");
    assert!(matches!(
        reason,
        SkipReason::Ceiling {
            skip: CeilingSkip::Unknown,
            ..
        }
    ));
}

#[test]
#[serial_test::serial]
fn the_lease_dir_honours_the_env_override_and_falls_back_to_home() {
    let prior = std::env::var_os(LEASE_DIR_ENV);
    std::env::set_var(LEASE_DIR_ENV, "/tmp/loom-backstop-test");
    assert_eq!(lease_dir(), Some(PathBuf::from("/tmp/loom-backstop-test")));
    std::env::remove_var(LEASE_DIR_ENV);
    if let Some(home) = dirs::home_dir() {
        // Machine-wide, like the build slot — one ceiling governs every
        // workspace this host runs.
        assert_eq!(lease_dir(), Some(home.join(".loom").join("leases").join("backstop")));
    }
    if let Some(value) = prior {
        std::env::set_var(LEASE_DIR_ENV, value);
    }
}
