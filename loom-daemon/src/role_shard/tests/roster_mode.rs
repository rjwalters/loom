use super::*;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::collections::BTreeSet;

const NOW: &str = "2026-01-01T10:00:00Z";
const FLEET_CREATED: &str = "2026-01-01T00:00:00Z";
const TTL: u64 = 900;
const SETTLE: u64 = 900;
const KEY: &str = "rjwalters/loom";

fn dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn issue() -> roster::RosterIssueRef {
    roster::RosterIssueRef::parse("rjwalters/loom#1234").expect("valid ref")
}

fn active_config() -> roster::RosterConfig {
    roster::RosterConfig {
        state: roster::RosterState::Active(issue()),
        heartbeat_secs: 300,
        ttl_secs: TTL,
        settle_secs: SETTLE,
    }
}

fn disabled_config() -> roster::RosterConfig {
    roster::RosterConfig {
        state: roster::RosterState::Disabled,
        heartbeat_secs: 300,
        ttl_secs: TTL,
        settle_secs: SETTLE,
    }
}

/// One live roster record: created at `created`, last beat at `beat`,
/// serving every key in `keys`.
fn record(
    id: u64,
    host: &str,
    keys: &[&str],
    created: DateTime<Utc>,
    beat: DateTime<Utc>,
) -> roster::RosterComment {
    roster::RosterComment {
        id,
        host: host.to_string(),
        serves: keys.iter().map(|k| hash_key(k)).collect::<BTreeSet<u64>>(),
        created_at: created,
        updated_at: beat,
    }
}

/// A live three-host fleet at `now`: all created long ago, all still
/// beating (last beat 60s back), all serving `keys`.
fn live_fleet(now: DateTime<Utc>, keys: &[&str]) -> Vec<roster::RosterComment> {
    ["host-a", "host-b", "host-c"]
        .iter()
        .enumerate()
        .map(|(i, h)| {
            record(
                u64::try_from(i).unwrap() + 1,
                h,
                keys,
                dt(FLEET_CREATED),
                now - ChronoDuration::seconds(60),
            )
        })
        .collect()
}

fn snapshot(
    host: &str,
    comments: Vec<roster::RosterComment>,
    now: DateTime<Utc>,
) -> roster::RosterSnapshot {
    roster::RosterSnapshot {
        issue: issue(),
        host: host.to_string(),
        comments,
        ttl_secs: TTL,
        settle_secs: SETTLE,
        fetched_at: now,
    }
}

fn unsharded() -> ShardPosture {
    ShardPosture::Unsharded(UnshardedReason::NotConfigured)
}

// ---- Rank and size come from the roster, and say so ----

#[test]
#[serial]
fn the_ring_rank_and_size_are_derived_from_the_live_roster() {
    clear_nwo_cache();
    roster::clear_generation_fence_for_tests();
    let now = dt(NOW);
    let root = Path::new("/repos/loom");
    let snap = snapshot("host-b", live_fleet(now, &[KEY]), now);

    let decision =
        decide_with_roster(unsharded(), root, Some(KEY), &active_config(), Some(&snap), now);

    assert_eq!(
        decision.posture,
        ShardPosture::Sharded {
            // host-b is second in the id-sorted ring of three.
            index: 1,
            count: 3,
            index_source: ValueSource::Roster,
            count_source: ValueSource::Roster,
        },
        "with no static index and a settled roster, (index, count) must come from the ring"
    );
    // `status` must be able to say where the numbers came from (AC).
    let summary = decision.posture.describe();
    assert!(summary.contains("index from roster"), "{summary}");
    assert!(summary.contains("count from roster"), "{summary}");
    // Ownership is still the ordinary arithmetic over the same hash.
    assert_eq!(decision.owning_shard, owning_shard(KEY, 3));
    assert_eq!(decision.owned, decision.owning_shard == Some(1));
    assert_eq!(decision.owned, decision.admits_role_tick());
    roster::clear_generation_fence_for_tests();
}

#[test]
#[serial]
fn exactly_one_live_member_owns_each_key_under_a_settled_ring() {
    clear_nwo_cache();
    let now = dt(NOW);
    let keys: Vec<String> = (0..27).map(|i| format!("2amlogic/repo-{i}")).collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let comments = live_fleet(now, &key_refs);
    for key in &keys {
        let owners: Vec<&str> = ["host-a", "host-b", "host-c"]
            .into_iter()
            .filter(|h| roster_owns(&comments, h, key, now, TTL, SETTLE))
            .collect();
        assert_eq!(owners.len(), 1, "{key} owned by {owners:?} under a settled ring");
    }
}

// ---- Escape-hatch precedence (AC3) ----

#[test]
#[serial]
fn a_static_shard_index_beats_an_enabled_roster() {
    clear_nwo_cache();
    let now = dt(NOW);
    let root = Path::new("/repos/loom");
    let snap = snapshot("host-b", live_fleet(now, &[KEY]), now);

    let decision =
        decide_with_roster(sharded(1, 4), root, Some(KEY), &active_config(), Some(&snap), now);

    assert_eq!(decision.roster, RosterMode::Off(RosterOff::StaticShardWins));
    assert_eq!(
        decision.posture,
        sharded(1, 4),
        "a resolved static pair must keep #6374's ring verbatim — it is the documented \
                 escape hatch for a roster outage"
    );
    // ...and it is byte-identical to the pre-roster decision.
    let mut pre_roster = decide_with(sharded(1, 4), root, Some(KEY));
    pre_roster.roster = RosterMode::Off(RosterOff::StaticShardWins);
    assert_eq!(decision, pre_roster);
}

#[test]
#[serial]
fn a_disabled_roster_is_byte_identical_to_the_static_decision() {
    clear_nwo_cache();
    let now = dt(NOW);
    let root = Path::new("/repos/loom");
    // Even with a live snapshot sitting in the cache, a disabled
    // roster must not be consulted at all.
    let snap = snapshot("host-b", live_fleet(now, &[KEY]), now);
    assert_eq!(
        decide_with_roster(unsharded(), root, Some(KEY), &disabled_config(), Some(&snap), now),
        decide_with(unsharded(), root, Some(KEY)),
    );
}

#[test]
#[serial]
fn an_enabled_roster_this_host_has_never_read_falls_back_to_the_static_posture() {
    // Design record rung 4: "never got a roster at all" keeps #6374's
    // duplicate-biased fallback. Yielding here would let one typo in
    // `roster.issue` silently stop role rotation fleet-wide.
    clear_nwo_cache();
    let root = Path::new("/repos/loom");
    let decision =
        decide_with_roster(unsharded(), root, Some(KEY), &active_config(), None, dt(NOW));
    assert_eq!(decision.roster, RosterMode::Off(RosterOff::NeverJoined));
    assert!(decision.owned, "an unreachable roster must not stop role rotation");
    assert!(decision.admits_role_tick());
}

#[test]
#[serial]
fn a_snapshot_read_from_a_different_roster_issue_is_not_used() {
    clear_nwo_cache();
    let now = dt(NOW);
    let mut snap = snapshot("host-b", live_fleet(now, &[KEY]), now);
    snap.issue = roster::RosterIssueRef::parse("someone/else#7").unwrap();
    let decision = decide_with_roster(
        unsharded(),
        Path::new("/repos/loom"),
        Some(KEY),
        &active_config(),
        Some(&snap),
        now,
    );
    assert_eq!(decision.roster, RosterMode::Off(RosterOff::SnapshotForAnotherIssue));
    assert!(decision.admits_role_tick());
}

#[test]
#[serial]
fn a_misconfigured_roster_falls_back_to_the_static_posture() {
    clear_nwo_cache();
    let config = roster::RosterConfig {
        state: roster::RosterState::MisconfiguredNoIssue,
        ..active_config()
    };
    let decision = decide_with_roster(
        unsharded(),
        Path::new("/repos/loom"),
        Some(KEY),
        &config,
        None,
        dt(NOW),
    );
    assert_eq!(decision.roster, RosterMode::Off(RosterOff::Misconfigured));
    assert!(decision.admits_role_tick());
}

// ---- The inverted fail-safe, and the dispatcher's exemption from it ----

#[test]
#[serial]
fn a_host_that_joined_and_then_lost_the_roster_yields_role_ticks() {
    clear_nwo_cache();
    roster::clear_generation_fence_for_tests();
    let now = dt(NOW);
    // This host's own heartbeat is 20m stale: it cannot know whether
    // the fleet has evicted it, so it yields (the inverted fail-safe).
    let mut comments = live_fleet(now, &[KEY]);
    comments[1].updated_at = now - ChronoDuration::seconds(1200);
    let snap = snapshot("host-b", comments, now);

    let decision = decide_with_roster(
        unsharded(),
        Path::new("/repos/loom"),
        Some(KEY),
        &active_config(),
        Some(&snap),
        now,
    );

    assert!(
        matches!(decision.roster, RosterMode::Yield(roster::RosterYield::SelfStale { .. })),
        "got {:?}",
        decision.roster
    );
    assert!(!decision.admits_role_tick(), "a fenced-out host must run NO role ticks");
    // ...but the DISPATCHER's preferred-slice consumer (#6243) keeps
    // the pre-roster verdict, or a fence yield would starve dispatch
    // instead of merely pausing role rotation.
    assert!(
        decision.owned,
        "a roster yield must degrade to work_finder's work-conserving fallback, never to \
                 `owns nothing`"
    );
    assert_eq!(
        decision.posture,
        unsharded(),
        "the dispatcher must see exactly the posture it saw before the roster existed"
    );
    roster::clear_generation_fence_for_tests();
}

#[test]
#[serial]
fn the_describe_line_names_the_fence_state() {
    clear_nwo_cache();
    roster::clear_generation_fence_for_tests();
    let now = dt(NOW);
    let root = Path::new("/repos/loom");
    let mut comments = live_fleet(now, &[KEY]);
    comments[1].updated_at = now - ChronoDuration::seconds(1200);
    let yielded = decide_with_roster(
        unsharded(),
        root,
        Some(KEY),
        &active_config(),
        Some(&snapshot("host-b", comments, now)),
        now,
    );
    let line = yielded.describe(root);
    assert!(line.contains("YIELDING"), "{line}");
    assert!(line.contains("not owned here"), "{line}");

    // Fresh process state for the second half: the eviction boundary
    // the yielding view above carried has already ratcheted this
    // process's high-water mark, and a host whose record expired
    // rejoins with a NEW comment id in production (see
    // `resolve_publish_action`), never by silently re-freshening the
    // same one.
    roster::clear_generation_fence_for_tests();
    let admitted = decide_with_roster(
        unsharded(),
        root,
        Some(KEY),
        &active_config(),
        Some(&snapshot("host-b", live_fleet(now, &[KEY]), now)),
        now,
    );
    assert!(admitted.describe(root).contains("roster: ring settled"));
    roster::clear_generation_fence_for_tests();
}

// ====================================================================
// Adversarial scenarios
//
// These drive `roster::admission` + `ShardPosture::owns` directly
// rather than `decide_with_roster`, for one reason: the generation
// high-water mark is process-global (one daemon = one process), so two
// *simulated* hosts sharing this test process would contaminate each
// other's fence. The arithmetic below is exactly what
// `decide_with_roster` does with an admitted ring — pinned by
// `the_ring_rank_and_size_are_derived_from_the_live_roster` above —
// and passing `None` for the high-water mark is the *weaker*
// assumption, so a fence that holds here holds a fortiori in
// production.
// ====================================================================

fn roster_owns(
    view: &[roster::RosterComment],
    host: &str,
    key: &str,
    now: DateTime<Utc>,
    ttl: u64,
    settle: u64,
) -> bool {
    match roster::admission(view, host, hash_key(key), now, ttl, settle, None) {
        roster::RosterAdmission::Ring { index, count, .. } => ShardPosture::Sharded {
            index,
            count,
            index_source: ValueSource::Roster,
            count_source: ValueSource::Roster,
        }
        .owns(key),
        roster::RosterAdmission::Yield(_) => false,
    }
}

/// SPLIT VIEW (AC): two hosts reading the same roster at different
/// staleness must never both own the same key at the same instant.
///
/// A lagging host is modelled as reading the true comment set as of
/// `t - lag` while evaluating at `t` — which is what an ETag-cached or
/// replica-lagged read actually looks like, including the fact that
/// the host's own record looks `lag` seconds staler to itself.
#[test]
#[serial]
fn a_split_view_never_gives_one_key_two_owners() {
    let keys: Vec<String> = (0..12).map(|i| format!("2amlogic/repo-{i}")).collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let join = dt("2026-01-01T02:00:00Z");

    // The true, forge-side comment set at instant `t`.
    let truth = |t: DateTime<Utc>| {
        let mut c = live_fleet(t, &key_refs);
        if t >= join {
            c.push(record(4, "host-d", &key_refs, join, t - ChronoDuration::seconds(60)));
        }
        c
    };

    // Deliberately divergent read staleness, including one host
    // lagging far enough that only the SELF-LIVENESS condition can
    // save the invariant.
    let hosts = [
        ("host-a", 0i64),
        ("host-b", 120),
        ("host-c", 600),
        ("host-d", 1500),
    ];

    let start = join - ChronoDuration::seconds(1800);
    for step in 0..240 {
        let t = start + ChronoDuration::seconds(step * 30);
        for key in &keys {
            let owners: Vec<&str> = hosts
                .iter()
                .filter(|(host, lag)| {
                    let view = truth(t - ChronoDuration::seconds(*lag));
                    roster_owns(&view, host, key, t, TTL, SETTLE)
                })
                .map(|(host, _)| *host)
                .collect();
            assert!(
                owners.len() <= 1,
                "{key} owned by {owners:?} at {t} — a membership disagreement must YIELD, \
                         never duplicate (#6704)"
            );
        }
    }
}

/// SPLIT VIEW, second half (AC): outside the settle window every key
/// still has an owner — the fence trades a bounded gap for the
/// duplicate, it does not strand work indefinitely.
#[test]
#[serial]
fn every_key_has_exactly_one_owner_outside_the_settle_window() {
    let keys: Vec<String> = (0..12).map(|i| format!("2amlogic/repo-{i}")).collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let join = dt("2026-01-01T02:00:00Z");
    let truth = |t: DateTime<Utc>| {
        let mut c = live_fleet(t, &key_refs);
        if t >= join {
            c.push(record(4, "host-d", &key_refs, join, t - ChronoDuration::seconds(60)));
        }
        c
    };
    // Both hosts read within the TTL, as a healthy fleet does.
    let hosts = [
        ("host-a", 0i64),
        ("host-b", 120),
        ("host-c", 240),
        ("host-d", 60),
    ];
    let settle_window = join..(join + ChronoDuration::seconds(SETTLE as i64 + 240));

    let start = join - ChronoDuration::seconds(1800);
    for step in 0..240 {
        let t = start + ChronoDuration::seconds(step * 30);
        if settle_window.contains(&t) {
            continue;
        }
        for key in &keys {
            let owners: Vec<&str> = hosts
                .iter()
                .filter(|(host, lag)| {
                    if t < join && *host == "host-d" {
                        return false; // not a member yet
                    }
                    let view = truth(t - ChronoDuration::seconds(*lag));
                    roster_owns(&view, host, key, t, TTL, SETTLE)
                })
                .map(|(host, _)| *host)
                .collect();
            assert_eq!(
                owners.len(),
                1,
                "{key} owned by {owners:?} at {t} (outside the settle window every key \
                         must have exactly one owner)"
            );
        }
    }
}

/// KILL HOST (AC): when a member's record expires, a survivor picks up
/// its slice within `ttl + settleSecs` (+ one role interval for tick
/// alignment, which is the cadence, not the fence) — and **not
/// before**.
#[test]
#[serial]
fn a_dead_hosts_slice_is_reassigned_after_ttl_plus_settle_and_not_before() {
    let keys: Vec<String> = (0..12).map(|i| format!("2amlogic/repo-{i}")).collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let death = dt("2026-01-01T02:00:00Z");
    let survivors = ["host-a", "host-b"];

    // host-c's last beat is at `death`; a, b keep beating.
    let view = |t: DateTime<Utc>| {
        let mut c = live_fleet(t, &key_refs);
        c[2].updated_at = death;
        c
    };

    // Its slice: the keys host-c owned while it was alive.
    let just_before_death = death - ChronoDuration::seconds(1);
    let orphaned: Vec<&String> = keys
        .iter()
        .filter(|k| {
            roster_owns(&view(just_before_death), "host-c", k, just_before_death, TTL, SETTLE)
        })
        .collect();
    assert!(!orphaned.is_empty(), "precondition: host-c must own part of the ring");

    // NOT BEFORE: for the whole `ttl + settle` window, no survivor
    // touches the orphaned slice.
    let reassigned_at = death + ChronoDuration::seconds((TTL + SETTLE) as i64);
    let mut t = death;
    while t < reassigned_at {
        for key in &orphaned {
            for host in survivors {
                assert!(
                    !roster_owns(&view(t), host, key, t, TTL, SETTLE),
                    "{host} picked up {key} at {t}, before ttl+settle had elapsed — the \
                             reassignment window must be bounded BELOW as well as above (#6704)"
                );
            }
        }
        t += ChronoDuration::seconds(30);
    }

    // AND NOT NEVER: at `death + ttl + settle` every orphaned key has
    // exactly one live owner again.
    for key in &orphaned {
        let owners: Vec<&str> = survivors
            .into_iter()
            .filter(|h| roster_owns(&view(reassigned_at), h, key, reassigned_at, TTL, SETTLE))
            .collect();
        assert_eq!(
            owners.len(),
            1,
            "{key} had owners {owners:?} at ttl+settle after its host died; a dead host's \
                     slice must be reassigned within a bounded window (#6704 AC2)"
        );
    }
    // And the whole ring is covered again, not just the orphans.
    for key in &keys {
        let owners: Vec<&str> = survivors
            .into_iter()
            .filter(|h| roster_owns(&view(reassigned_at), h, key, reassigned_at, TTL, SETTLE))
            .collect();
        assert_eq!(owners.len(), 1, "{key} owned by {owners:?} after reassignment");
    }
}

/// SELF-FENCE (AC), at the surface that spends tokens: a host whose
/// own heartbeat is stale runs no roster-gated role tick for ANY key.
#[test]
#[serial]
fn a_host_with_a_stale_heartbeat_runs_no_roster_gated_role_ticks() {
    clear_nwo_cache();
    roster::clear_generation_fence_for_tests();
    let now = dt(NOW);
    let keys: Vec<String> = (0..12).map(|i| format!("2amlogic/repo-{i}")).collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let mut comments = live_fleet(now, &key_refs);
    comments[0].updated_at = now - ChronoDuration::seconds(1200);
    let snap = snapshot("host-a", comments, now);

    for key in &keys {
        let decision = decide_with_roster(
            unsharded(),
            Path::new("/repos/loom"),
            Some(key),
            &active_config(),
            Some(&snap),
            now,
        );
        assert!(
            !decision.admits_role_tick(),
            "{key}: a self-fenced host must run no role ticks at all"
        );
        assert!(decision.owned, "{key}: dispatch preference must be unaffected");
    }
    roster::clear_generation_fence_for_tests();
}

/// JOIN FENCE (AC) at the same surface: a host that has just joined
/// runs nothing until its own record is `ttl` old.
#[test]
#[serial]
fn a_newly_joined_host_runs_nothing_until_its_record_is_ttl_old() {
    clear_nwo_cache();
    roster::clear_generation_fence_for_tests();
    let keys: Vec<String> = (0..12).map(|i| format!("2amlogic/repo-{i}")).collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let join = dt(NOW);

    let view = |t: DateTime<Utc>| {
        let mut c = live_fleet(t, &key_refs);
        c.push(record(4, "host-d", &key_refs, join, t - ChronoDuration::seconds(60)));
        c
    };

    // Anywhere inside its first ttl, the joiner is fenced out for
    // every key.
    let mut t = join;
    while t < join + ChronoDuration::seconds(TTL as i64) {
        for key in &keys {
            let decision = decide_with_roster(
                unsharded(),
                Path::new("/repos/loom"),
                Some(key),
                &active_config(),
                Some(&snapshot("host-d", view(t), t)),
                t,
            );
            assert!(
                !decision.admits_role_tick(),
                "{key}: a joiner must not act until its own record is a full ttl old \
                         (t={t})"
            );
        }
        t += ChronoDuration::seconds(120);
    }

    // Once both the join fence and the settle window have passed, it
    // takes up its share.
    let after = join + ChronoDuration::seconds((TTL + SETTLE) as i64);
    let owned: Vec<&String> = keys
        .iter()
        .filter(|key| {
            decide_with_roster(
                unsharded(),
                Path::new("/repos/loom"),
                Some(key),
                &active_config(),
                Some(&snapshot("host-d", view(after), after)),
                after,
            )
            .admits_role_tick()
        })
        .collect();
    assert!(
        !owned.is_empty(),
        "a joined, settled host must eventually carry part of the ring"
    );
    roster::clear_generation_fence_for_tests();
}

// ---- End-to-end through `decide` (config + snapshot cache) ----

#[test]
#[serial]
fn decide_reads_the_roster_from_config_and_the_snapshot_cache() {
    let _env = EnvGuard::capture();
    let _roster_env = RosterEnvGuard::capture();
    clear_nwo_cache();
    roster::clear_generation_fence_for_tests();

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join(".loom")).expect("mkdir .loom");
    std::fs::write(
        root.join(crate::config_resolver::LEGACY_CONFIG_REL),
        r#"{"autonomous":{"roleRunner":{"enabled":true,"shardKey":"rjwalters/loom",
                   "roster":{"enabled":true,"issue":"rjwalters/loom#1234"}}}}"#,
    )
    .expect("write config");

    // No snapshot yet: the never-joined fallback, NOT a yield.
    roster::clear_roster_snapshot_for_tests();
    let before = decide(root);
    assert_eq!(before.roster, RosterMode::Off(RosterOff::NeverJoined));
    assert!(before.admits_role_tick());

    // The heartbeat task publishes a snapshot; now the ring is live.
    let now = chrono::Utc::now();
    roster::set_roster_snapshot(snapshot("host-c", live_fleet(now, &[KEY]), now));
    let after = decide(root);
    assert!(matches!(after.roster, RosterMode::Ring { .. }), "got {:?}", after.roster);
    assert_eq!(after.posture.count(), Some(3));
    assert_eq!(after.posture.index(), Some(2), "host-c is third in the ring");
    assert!(after.posture.describe().contains("from roster"));

    roster::clear_roster_snapshot_for_tests();
    roster::clear_generation_fence_for_tests();
}

/// Restore the roster env knobs, so a stray `LOOM_ROLE_RUNNER_ROSTER`
/// in the ambient environment cannot steer the config-driven test.
struct RosterEnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl RosterEnvGuard {
    fn capture() -> Self {
        let names = [
            roster::ROSTER_ENABLED_ENV,
            roster::ROSTER_ISSUE_ENV,
            roster::ROSTER_HEARTBEAT_SECS_ENV,
            roster::ROSTER_TTL_SECS_ENV,
            roster::ROSTER_SETTLE_SECS_ENV,
        ];
        let saved = names.iter().map(|n| (*n, std::env::var(*n).ok())).collect();
        for n in names {
            std::env::remove_var(n);
        }
        Self { saved }
    }
}

impl Drop for RosterEnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}
