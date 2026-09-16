use super::*;
use serial_test::serial;

/// Restore both shard env vars to whatever they were, so a `#[serial]`
/// test that sets them cannot leak into the next one.
struct EnvGuard {
    index: Option<String>,
    count: Option<String>,
}

impl EnvGuard {
    fn capture() -> Self {
        let g = Self {
            index: std::env::var(SHARD_INDEX_ENV).ok(),
            count: std::env::var(SHARD_COUNT_ENV).ok(),
        };
        std::env::remove_var(SHARD_INDEX_ENV);
        std::env::remove_var(SHARD_COUNT_ENV);
        g
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.index {
            Some(v) => std::env::set_var(SHARD_INDEX_ENV, v),
            None => std::env::remove_var(SHARD_INDEX_ENV),
        }
        match &self.count {
            Some(v) => std::env::set_var(SHARD_COUNT_ENV, v),
            None => std::env::remove_var(SHARD_COUNT_ENV),
        }
    }
}

fn sharded(index: usize, count: usize) -> ShardPosture {
    ShardPosture::Sharded {
        index,
        count,
        index_source: ValueSource::Env,
        count_source: ValueSource::Config,
    }
}

/// The 27-workspace fleet from the issue's own incident report.
fn fleet_keys() -> Vec<String> {
    (0..27).map(|i| format!("2amlogic/repo-{i}")).collect()
}

// ---- Hashing ----

#[test]
fn fnv1a64_matches_the_published_vectors() {
    // The canonical FNV-1a 64 test vectors. These pin the algorithm so a
    // future "optimization" cannot silently change every host's shard
    // assignment (which would be invisible on any single host and would
    // break the fleet only once the hosts disagreed).
    assert_eq!(hash_key(""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(hash_key("a"), 0xaf63_dc4c_8601_ec8c);
    // NB: 0x8506_7b17_8119_5929 is FNV-**1** (multiply-then-xor) for the
    // same input — a near-miss that is easy to copy by mistake. This is
    // the FNV-**1a** (xor-then-multiply) vector, which is what
    // [`hash_key`] computes.
    assert_eq!(hash_key("foobar"), 0x8594_4171_f739_67e8);
}

#[test]
fn owning_shard_is_none_for_a_zero_count() {
    assert_eq!(owning_shard("rjwalters/loom", 0), None);
}

#[test]
fn owning_shard_is_always_within_range() {
    for key in fleet_keys() {
        for count in 1..=8 {
            let owner = owning_shard(&key, count).expect("count > 0");
            assert!(owner < count, "{key} -> {owner} out of 0..{count}");
        }
    }
}

// ---- AC1: exactly one owner per workspace per interval, fleet-wide ----

#[test]
fn every_workspace_is_owned_by_exactly_one_host_in_a_two_host_fleet() {
    // The issue's headline acceptance criterion, at its stated size.
    for key in fleet_keys() {
        let owners: Vec<usize> = (0..2).filter(|i| sharded(*i, 2).owns(&key)).collect();
        assert_eq!(owners.len(), 1, "{key} owned by {owners:?}, expected exactly one host");
    }
}

#[test]
fn every_workspace_is_owned_by_exactly_one_host_across_fleet_sizes() {
    // The invariant is arithmetic, so it must hold at every fleet size,
    // not just the one the incident happened at.
    for count in 2..=8 {
        for key in fleet_keys() {
            let owners: Vec<usize> = (0..count)
                .filter(|i| sharded(*i, count).owns(&key))
                .collect();
            assert_eq!(
                owners.len(),
                1,
                "{key} owned by {owners:?} in a {count}-host fleet, expected exactly one"
            );
        }
    }
}

// ---- AC2: token draw scales with workspaces, not workspaces x hosts ----

#[test]
fn total_role_ticks_per_interval_equal_the_workspace_count_not_workspaces_times_hosts() {
    let keys = fleet_keys();
    for count in 2..=8 {
        let ticks: usize = (0..count)
            .map(|i| keys.iter().filter(|k| sharded(i, count).owns(k)).count())
            .sum();
        assert_eq!(
            ticks,
            keys.len(),
            "a {count}-host fleet drew {ticks} role ticks for {} workspaces; sharding must \
                 make the draw scale with workspaces alone",
            keys.len()
        );
        // And the pre-#6374 behavior it replaces, for contrast.
        let unsharded: usize = (0..count)
            .map(|_| {
                keys.iter()
                    .filter(|k| ShardPosture::Unsharded(UnshardedReason::NotConfigured).owns(k))
                    .count()
            })
            .sum();
        assert_eq!(unsharded, keys.len() * count);
    }
}

#[test]
fn the_assignment_is_not_degenerate_across_a_four_host_fleet() {
    // A hash that mapped everything to one shard would satisfy "exactly
    // one owner" while delivering none of the point. Assert every shard
    // gets a real share of the 27-workspace fleet.
    let keys = fleet_keys();
    let loads: Vec<usize> = (0..4)
        .map(|i| keys.iter().filter(|k| sharded(i, 4).owns(k)).count())
        .collect();
    for (shard, load) in loads.iter().enumerate() {
        assert!(*load > 0, "shard {shard} owns nothing; loads={loads:?}");
        assert!(
            *load <= keys.len() / 2,
            "shard {shard} owns {load} of {} workspaces; loads={loads:?}",
            keys.len()
        );
    }
}

#[test]
fn assignment_is_stable_across_repeated_resolution() {
    // Two "hosts" resolving independently must agree. This is the whole
    // cross-host correctness argument, expressed locally.
    let key = "rjwalters/loom";
    let first = sharded(0, 4).owns(key);
    for _ in 0..100 {
        assert_eq!(sharded(0, 4).owns(key), first);
    }
}

// ---- Unsharded fallbacks own everything (fail-safe direction) ----

#[test]
fn an_unsharded_posture_owns_every_workspace() {
    let posture = ShardPosture::Unsharded(UnshardedReason::NotConfigured);
    for key in fleet_keys() {
        assert!(posture.owns(&key));
    }
    assert!(!posture.is_sharded());
    assert_eq!(posture.index(), None);
    assert_eq!(posture.count(), None);
}

#[test]
#[serial]
fn an_out_of_range_index_falls_back_to_unsharded_rather_than_owning_nothing() {
    let _env = EnvGuard::capture();
    std::env::set_var(SHARD_INDEX_ENV, "4");
    std::env::set_var(SHARD_COUNT_ENV, "4");
    let posture = resolve_posture_from(None, Path::new("/repos/loom"));
    assert_eq!(
        posture,
        ShardPosture::Unsharded(UnshardedReason::IndexOutOfRange { index: 4, count: 4 })
    );
    assert!(posture.owns("rjwalters/loom"), "must not silently rotate nothing");
}

#[test]
#[serial]
fn a_zero_count_falls_back_to_unsharded() {
    let _env = EnvGuard::capture();
    std::env::set_var(SHARD_INDEX_ENV, "0");
    std::env::set_var(SHARD_COUNT_ENV, "0");
    assert_eq!(
        resolve_posture_from(None, Path::new("/repos/loom")),
        ShardPosture::Unsharded(UnshardedReason::ZeroCount)
    );
}

#[test]
#[serial]
fn a_single_shard_fleet_is_unsharded() {
    let _env = EnvGuard::capture();
    std::env::set_var(SHARD_INDEX_ENV, "0");
    std::env::set_var(SHARD_COUNT_ENV, "1");
    assert_eq!(
        resolve_posture_from(None, Path::new("/repos/loom")),
        ShardPosture::Unsharded(UnshardedReason::SingleShard)
    );
}

#[test]
#[serial]
fn an_index_without_a_count_falls_back_to_unsharded() {
    let _env = EnvGuard::capture();
    std::env::set_var(SHARD_INDEX_ENV, "1");
    assert_eq!(
        resolve_posture_from(None, Path::new("/repos/loom")),
        ShardPosture::Unsharded(UnshardedReason::Incomplete {
            have_index: true,
            have_count: false,
        })
    );
}

#[test]
#[serial]
fn a_count_without_an_index_falls_back_to_unsharded() {
    let _env = EnvGuard::capture();
    std::env::set_var(SHARD_COUNT_ENV, "4");
    assert_eq!(
        resolve_posture_from(None, Path::new("/repos/loom")),
        ShardPosture::Unsharded(UnshardedReason::Incomplete {
            have_index: false,
            have_count: true,
        })
    );
}

#[test]
#[serial]
fn a_malformed_knob_falls_back_to_unsharded_and_names_the_field() {
    let _env = EnvGuard::capture();
    std::env::set_var(SHARD_INDEX_ENV, "two");
    std::env::set_var(SHARD_COUNT_ENV, "4");
    let posture = resolve_posture_from(None, Path::new("/repos/loom"));
    let ShardPosture::Unsharded(UnshardedReason::Malformed { field, raw }) = &posture else {
        panic!("expected Malformed, got {posture:?}");
    };
    assert_eq!(field, SHARD_INDEX_ENV);
    assert_eq!(raw, "two");
    assert!(posture.owns("rjwalters/loom"));
}

#[test]
#[serial]
fn nothing_configured_resolves_to_not_configured() {
    let _env = EnvGuard::capture();
    assert_eq!(
        resolve_posture_from(None, Path::new("/repos/loom")),
        ShardPosture::Unsharded(UnshardedReason::NotConfigured)
    );
}

// ---- Precedence ----

#[test]
#[serial]
fn env_overrides_config_for_both_knobs() {
    let _env = EnvGuard::capture();
    let block = serde_json::json!({ "shardIndex": 3, "shardCount": 4 });
    std::env::set_var(SHARD_INDEX_ENV, "1");
    std::env::set_var(SHARD_COUNT_ENV, "2");
    assert_eq!(
        resolve_posture_from(Some(&block), Path::new("/repos/loom")),
        ShardPosture::Sharded {
            index: 1,
            count: 2,
            index_source: ValueSource::Env,
            count_source: ValueSource::Env,
        }
    );
}

#[test]
#[serial]
fn the_count_may_come_from_config_while_the_index_comes_from_env() {
    // The intended deployment shape: a fleet-wide count committed to the
    // repo, a per-host index in the service unit.
    let _env = EnvGuard::capture();
    let block = serde_json::json!({ "shardCount": 4 });
    std::env::set_var(SHARD_INDEX_ENV, "2");
    assert_eq!(
        resolve_posture_from(Some(&block), Path::new("/repos/loom")),
        ShardPosture::Sharded {
            index: 2,
            count: 4,
            index_source: ValueSource::Env,
            count_source: ValueSource::Config,
        }
    );
}

// ---- The fleet-breaking misconfiguration ----

#[test]
#[serial]
fn a_tracked_config_shard_index_is_refused_rather_than_honored() {
    let _env = EnvGuard::capture();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join(".loom")).expect("mkdir .loom");
    std::fs::write(
        root.join(crate::config_resolver::LEGACY_CONFIG_REL),
        r#"{"autonomous":{"roleRunner":{"shardIndex":1,"shardCount":4}}}"#,
    )
    .expect("write config");

    let block = serde_json::json!({ "shardIndex": 1, "shardCount": 4 });
    let posture = resolve_posture_from(Some(&block), root);
    assert_eq!(
        posture,
        ShardPosture::Unsharded(UnshardedReason::IndexFromTrackedConfig { index: 1 })
    );
    // Fail-safe: refusing must not stop role rotation, only un-shard it.
    assert!(posture.owns("rjwalters/loom"));
    assert!(posture.describe().contains("REFUSED"));
}

#[test]
#[serial]
fn an_env_index_still_shards_even_when_the_tracked_config_also_declares_one() {
    // The env value is per-host by construction, so it is legitimate and
    // overrides the (ignored) tracked one rather than tripping the refusal.
    let _env = EnvGuard::capture();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join(".loom")).expect("mkdir .loom");
    std::fs::write(
        root.join(crate::config_resolver::LEGACY_CONFIG_REL),
        r#"{"autonomous":{"roleRunner":{"shardIndex":1,"shardCount":4}}}"#,
    )
    .expect("write config");

    std::env::set_var(SHARD_INDEX_ENV, "2");
    let block = serde_json::json!({ "shardIndex": 1, "shardCount": 4 });
    assert_eq!(
        resolve_posture_from(Some(&block), root),
        ShardPosture::Sharded {
            index: 2,
            count: 4,
            index_source: ValueSource::Env,
            count_source: ValueSource::Config,
        }
    );
}

// ---- Shard key ----

#[test]
#[serial]
fn an_explicit_config_key_wins_over_every_derived_source() {
    clear_nwo_cache();
    let resolved = resolve_shard_key(Path::new("/repos/loom"), Some("  rjwalters/loom  "));
    assert_eq!(resolved.key, "rjwalters/loom");
    assert_eq!(resolved.source, KeySource::ConfigExplicit);
    assert!(resolved.source.is_cross_host_stable());
}

#[test]
#[serial]
fn a_blank_explicit_key_is_ignored_and_falls_through() {
    clear_nwo_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("my-workspace");
    std::fs::create_dir_all(&root).expect("mkdir");
    let resolved = resolve_shard_key(&root, Some("   "));
    assert_eq!(resolved.key, "my-workspace");
    assert_eq!(resolved.source, KeySource::Basename);
}

#[test]
#[serial]
fn a_workspace_with_no_git_remote_falls_back_to_its_basename() {
    clear_nwo_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("lean-genius");
    std::fs::create_dir_all(&root).expect("mkdir");
    let resolved = resolve_shard_key(&root, None);
    assert_eq!(resolved.key, "lean-genius");
    assert_eq!(resolved.source, KeySource::Basename);
    assert!(
        !resolved.source.is_cross_host_stable(),
        "the basename fallback must advertise that it can diverge across hosts"
    );
}

#[test]
fn key_source_labels_are_stable() {
    assert_eq!(KeySource::ConfigExplicit.label(), "config");
    assert_eq!(KeySource::GitRemote.label(), "git-remote");
    assert_eq!(KeySource::Basename.label(), "basename");
    assert_eq!(ValueSource::Env.label(), "env");
    assert_eq!(ValueSource::Config.label(), "config");
}

// ---- Decision ----

#[test]
#[serial]
fn decide_with_reports_the_owning_shard_and_this_hosts_verdict() {
    clear_nwo_cache();
    let key = "rjwalters/loom";
    let owner = owning_shard(key, 4).expect("count > 0");
    let root = Path::new("/repos/loom");

    let mine = decide_with(sharded(owner, 4), root, Some(key));
    assert!(mine.owned);
    assert_eq!(mine.owning_shard, Some(owner));
    assert!(mine.describe(root).contains("OWNED here"));

    let theirs = decide_with(sharded((owner + 1) % 4, 4), root, Some(key));
    assert!(!theirs.owned);
    assert_eq!(theirs.owning_shard, Some(owner));
    assert!(theirs.describe(root).contains("not owned here"));
}

#[test]
#[serial]
fn decide_with_owns_everything_when_unsharded() {
    clear_nwo_cache();
    let root = Path::new("/repos/loom");
    let decision = decide_with(
        ShardPosture::Unsharded(UnshardedReason::NotConfigured),
        root,
        Some("rjwalters/loom"),
    );
    assert!(decision.owned);
    assert_eq!(decision.owning_shard, None);
    assert!(decision.describe(root).contains("OWNED here"));
}

#[test]
fn every_unsharded_reason_describes_the_fallback_direction() {
    // Whatever went wrong, the operator must be able to read off that
    // this host is still rotating everything.
    let reasons = [
        UnshardedReason::NotConfigured,
        UnshardedReason::SingleShard,
        UnshardedReason::Incomplete {
            have_index: true,
            have_count: false,
        },
        UnshardedReason::Incomplete {
            have_index: false,
            have_count: true,
        },
        UnshardedReason::ZeroCount,
        UnshardedReason::IndexOutOfRange { index: 9, count: 4 },
        UnshardedReason::Malformed {
            field: SHARD_COUNT_ENV.to_string(),
            raw: "four".to_string(),
        },
        UnshardedReason::IndexFromTrackedConfig { index: 1 },
    ];
    for reason in reasons {
        let text = ShardPosture::Unsharded(reason.clone()).describe();
        assert!(
            text.contains("EVERY registered workspace"),
            "{reason:?} described as {text:?} without naming the fallback direction"
        );
    }
}

#[test]
fn a_sharded_posture_describes_both_sources() {
    let text = sharded(2, 4).describe();
    assert!(text.contains("shard 2 of 4"), "{text}");
    assert!(text.contains("index from env"), "{text}");
    assert!(text.contains("count from config"), "{text}");
}

#[test]
#[serial]
fn log_decision_once_is_idempotent_for_an_unchanged_decision() {
    clear_decision_log();
    clear_nwo_cache();
    let root = Path::new("/repos/loom");
    let decision = decide_with(sharded(0, 4), root, Some("rjwalters/loom"));
    log_decision_once(root, &decision);
    let first = decision_logged()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(root)
        .cloned();
    log_decision_once(root, &decision);
    let second = decision_logged()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(root)
        .cloned();
    assert_eq!(first, second);
    assert_eq!(first, Some(decision.describe(root)));
}

#[test]
#[serial]
fn log_decision_once_re_records_a_changed_decision() {
    clear_decision_log();
    clear_nwo_cache();
    let root = Path::new("/repos/loom");
    let owned = decide_with(sharded(0, 4), root, Some("rjwalters/loom"));
    log_decision_once(root, &owned);
    let unsharded = decide_with(
        ShardPosture::Unsharded(UnshardedReason::NotConfigured),
        root,
        Some("rjwalters/loom"),
    );
    log_decision_once(root, &unsharded);
    assert_eq!(
        decision_logged()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(root)
            .cloned(),
        Some(unsharded.describe(root))
    );
}

// ---- The static ring is untouched by an enabled roster (#7690 Phase A's
// invariant, which #7691 preserves: the static env pair OUTRANKS the
// roster, so a host that sets it keeps #6374 verbatim) ----

#[test]
#[serial]
fn decide_verdict_is_unchanged_whether_the_roster_is_enabled_or_not() {
    clear_nwo_cache();
    let root = Path::new("/repos/loom");
    let posture = sharded(1, 4);

    let baseline = decide_with(posture.clone(), root, Some("rjwalters/loom"));

    // Enabling the roster (even with a fully valid issue) must not change
    // `decide`'s verdict at all when a static shard index is in effect.
    std::env::set_var(roster::ROSTER_ENABLED_ENV, "1");
    std::env::set_var(roster::ROSTER_ISSUE_ENV, "rjwalters/loom#1234");
    let with_roster = decide_with(posture.clone(), root, Some("rjwalters/loom"));
    std::env::remove_var(roster::ROSTER_ENABLED_ENV);
    std::env::remove_var(roster::ROSTER_ISSUE_ENV);

    assert_eq!(baseline, with_roster);

    // Same check for the misconfigured (enabled, no issue) roster state.
    std::env::set_var(roster::ROSTER_ENABLED_ENV, "1");
    let with_misconfigured_roster = decide_with(posture, root, Some("rjwalters/loom"));
    std::env::remove_var(roster::ROSTER_ENABLED_ENV);

    assert_eq!(baseline, with_misconfigured_roster);
}

// ========================================================================
// Roster-driven ring (Issue #7691, Phase B of #6704)
// ========================================================================

mod roster_mode;
