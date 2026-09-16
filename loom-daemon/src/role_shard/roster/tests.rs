use super::*;
use serial_test::serial;

fn dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn comment(host: &str, serves: &[u64], created: &str, updated: &str) -> RosterComment {
    RosterComment {
        id: 1,
        host: host.to_string(),
        serves: serves.iter().copied().collect(),
        created_at: dt(created),
        updated_at: dt(updated),
    }
}

fn comment_id(id: u64, host: &str, serves: &[u64], created: &str, updated: &str) -> RosterComment {
    RosterComment {
        id,
        ..comment(host, serves, created, updated)
    }
}

// ---- RosterIssueRef ----

#[test]
fn parses_a_well_formed_issue_ref() {
    let r = RosterIssueRef::parse("rjwalters/loom#1234").expect("parses");
    assert_eq!(r.owner, "rjwalters");
    assert_eq!(r.repo, "loom");
    assert_eq!(r.number, 1234);
    assert_eq!(r.display(), "rjwalters/loom#1234");
}

#[test]
fn rejects_malformed_issue_refs() {
    assert!(RosterIssueRef::parse("").is_none());
    assert!(RosterIssueRef::parse("loom#123").is_none());
    assert!(RosterIssueRef::parse("rjwalters/loom").is_none());
    assert!(RosterIssueRef::parse("rjwalters/loom#0").is_none());
    assert!(RosterIssueRef::parse("rjwalters/loom#abc").is_none());
    assert!(RosterIssueRef::parse("/loom#123").is_none());
    assert!(RosterIssueRef::parse("rjwalters/#123").is_none());
}

// ---- Config resolution ----

#[test]
#[serial]
fn disabled_by_default() {
    let _e = EnvGuard::capture();
    let config = resolve_roster_config_from(None);
    assert_eq!(config.state, RosterState::Disabled);
    assert!(!config.is_active());
    assert_eq!(config.heartbeat_secs, ROSTER_DEFAULT_HEARTBEAT_SECS);
    assert_eq!(config.ttl_secs, ROSTER_DEFAULT_TTL_SECS);
    assert_eq!(config.settle_secs, ROSTER_DEFAULT_SETTLE_SECS);
}

#[test]
#[serial]
fn enabled_with_no_issue_is_misconfigured_not_silently_unsharded() {
    let _e = EnvGuard::capture();
    let block = serde_json::json!({ "enabled": true });
    let config = resolve_roster_config_from(Some(&block));
    assert_eq!(config.state, RosterState::MisconfiguredNoIssue);
    assert!(!config.is_active());
    assert!(config.issue().is_none());
}

#[test]
#[serial]
fn enabled_with_a_valid_issue_is_active() {
    let _e = EnvGuard::capture();
    let block = serde_json::json!({ "enabled": true, "issue": "rjwalters/loom#42" });
    let config = resolve_roster_config_from(Some(&block));
    assert!(config.is_active());
    assert_eq!(config.issue().unwrap().display(), "rjwalters/loom#42");
}

#[test]
#[serial]
fn ttl_is_floored_at_3x_heartbeat() {
    let _e = EnvGuard::capture();
    let block = serde_json::json!({
        "enabled": true,
        "issue": "rjwalters/loom#42",
        "heartbeatSecs": 100,
        "ttlSecs": 120,
    });
    let config = resolve_roster_config_from(Some(&block));
    assert_eq!(config.heartbeat_secs, 100);
    // ttlSecs=120 is below the 3x100=300 floor, so the floor wins.
    assert_eq!(config.ttl_secs, 300);
}

#[test]
#[serial]
fn an_explicit_ttl_above_the_floor_is_kept() {
    let _e = EnvGuard::capture();
    let block = serde_json::json!({
        "enabled": true,
        "issue": "rjwalters/loom#42",
        "heartbeatSecs": 100,
        "ttlSecs": 1000,
    });
    let config = resolve_roster_config_from(Some(&block));
    assert_eq!(config.ttl_secs, 1000);
}

#[test]
#[serial]
fn env_overrides_config_for_every_knob() {
    let _e = EnvGuard::capture();
    let block = serde_json::json!({
        "enabled": false,
        "issue": "someone/else#1",
        "heartbeatSecs": 999,
    });
    std::env::set_var(ROSTER_ENABLED_ENV, "1");
    std::env::set_var(ROSTER_ISSUE_ENV, "rjwalters/loom#42");
    std::env::set_var(ROSTER_HEARTBEAT_SECS_ENV, "60");
    let config = resolve_roster_config_from(Some(&block));
    assert!(config.is_active());
    assert_eq!(config.issue().unwrap().display(), "rjwalters/loom#42");
    assert_eq!(config.heartbeat_secs, 60);
}

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn capture() -> Self {
        let names = [
            ROSTER_ENABLED_ENV,
            ROSTER_ISSUE_ENV,
            ROSTER_HEARTBEAT_SECS_ENV,
            ROSTER_TTL_SECS_ENV,
            ROSTER_SETTLE_SECS_ENV,
        ];
        let saved = names.iter().map(|n| (*n, std::env::var(*n).ok())).collect();
        for n in names {
            std::env::remove_var(n);
        }
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}

// ---- Marker parsing ----

#[test]
fn parses_a_well_formed_marker_line() {
    let body = "<!-- loom:roster host=host-abc123 serves=1a,2b,03 -->\nsome prose";
    let (host, serves) = parse_roster_marker_line(body).expect("parses");
    assert_eq!(host, "host-abc123");
    assert_eq!(serves, [0x1a, 0x2b, 0x03].into_iter().collect());
}

#[test]
fn a_host_with_no_served_workspaces_parses_to_an_empty_set() {
    let body = "<!-- loom:roster host=host-abc123 serves= -->";
    let (host, serves) = parse_roster_marker_line(body).expect("parses");
    assert_eq!(host, "host-abc123");
    assert!(serves.is_empty());
}

#[test]
fn rejects_bodies_that_do_not_match_the_marker_shape() {
    assert!(parse_roster_marker_line("not a marker at all").is_none());
    assert!(parse_roster_marker_line("<!-- loom:lease host=x sweep=y -->").is_none());
    assert!(parse_roster_marker_line("<!-- loom:roster host= serves=1a -->").is_none());
}

#[test]
fn render_serves_round_trips_through_parse() {
    let serves: BTreeSet<u64> = [1, 2, 0xdead_beef].into_iter().collect();
    let rendered = render_serves(&serves);
    let body = format!("{}h serves={} -->", ROSTER_MARKER_PREFIX, rendered);
    let (host, parsed) = parse_roster_marker_line(&body).expect("parses");
    assert_eq!(host, "h");
    assert_eq!(parsed, serves);
}

#[test]
fn build_roster_comment_body_round_trips_its_own_marker_line() {
    let serves: BTreeSet<u64> = [1, 2, 3].into_iter().collect();
    let body = build_roster_comment_body("host-abc", &serves);
    let (host, parsed) = parse_roster_marker_line(&body).expect("parses");
    assert_eq!(host, "host-abc");
    assert_eq!(parsed, serves);
}

#[test]
fn build_roster_comment_body_changes_on_every_call() {
    // Even with identical `serves`, the trailing `at=` timestamp must
    // differ so a PATCH of this body always advances `updated_at`
    // (defaults/docs/lease-renewal.md's "must change something" rule).
    let serves: BTreeSet<u64> = [1].into_iter().collect();
    let a = build_roster_comment_body("host-abc", &serves);
    std::thread::sleep(std::time::Duration::from_millis(2));
    let b = build_roster_comment_body("host-abc", &serves);
    assert_ne!(a, b);
}

// ---- NDJSON parsing ----

#[test]
fn parses_multiple_ndjson_lines_and_drops_malformed_ones() {
    let stdout = format!(
            "{{\"id\":1,\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:05:00Z\",\"body\":\"{p}host-a serves=1a -->\"}}\n\
             not json\n\
             {{\"id\":2,\"body\":\"no timestamps\"}}\n\
             {{\"id\":3,\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:05:00Z\",\"body\":\"unrelated comment\"}}\n",
            p = ROSTER_MARKER_PREFIX
        );
    let parsed = parse_roster_comments_json(stdout.as_bytes());
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].id, 1);
    assert_eq!(parsed[0].host, "host-a");
}

// ---- members / ring / generation: AC "just-expired and just-joined" fixtures ----

fn fixture() -> Vec<RosterComment> {
    vec![
        // A: alive at t=00:25:00 -- its last beat (00:20:00) is recent
        // enough that its boundary (00:20:00 + 15m = 00:35:00) is still
        // in the future.
        comment("host-a", &[1], "2026-01-01T00:00:00Z", "2026-01-01T00:20:00Z"),
        // B: JUST EXPIRED at t=00:25:00 with ttl=900s (15m): its last beat
        // was at 00:10:00, so it expires at 00:25:00 exactly.
        comment("host-b", &[1], "2026-01-01T00:00:00Z", "2026-01-01T00:10:00Z"),
        // C: JUST JOINED at t=00:25:00 (created_at == now).
        comment("host-c", &[1], "2026-01-01T00:25:00Z", "2026-01-01T00:25:00Z"),
    ]
}

#[test]
fn members_excludes_a_just_expired_host_and_includes_a_just_joined_one() {
    let comments = fixture();
    let ttl_secs = 900; // 15 minutes
    let now = dt("2026-01-01T00:25:00Z");
    let live = members(&comments, now, ttl_secs);
    // host-a is still within ttl of its last beat (00:20:00), well
    // before its boundary at 00:35:00.
    assert!(live.contains("host-a"));
    // host-b's boundary (updated_at + ttl = 00:10:00 + 15m = 00:25:00) is
    // NOT after `now` (`t < boundary` is false at equality) -- expired.
    assert!(!live.contains("host-b"), "host-b must be expired at its own boundary instant");
    // host-c's created_at == now, so `created_at <= t` holds -- a
    // just-joined host is a member from its very first instant.
    assert!(live.contains("host-c"));
}

#[test]
fn a_host_created_in_the_future_is_not_yet_a_member() {
    let comments = vec![comment(
        "host-future",
        &[1],
        "2026-01-01T01:00:00Z",
        "2026-01-01T01:00:00Z",
    )];
    let now = dt("2026-01-01T00:00:00Z");
    assert!(members(&comments, now, 900).is_empty());
}

#[test]
fn ring_only_includes_live_members_serving_the_key() {
    let mut comments = fixture();
    // host-d is live but does not serve key digest 1.
    comments.push(comment("host-d", &[2], "2026-01-01T00:00:00Z", "2026-01-01T00:24:00Z"));
    let now = dt("2026-01-01T00:25:00Z");
    let r = ring(&comments, now, 900, 1);
    assert_eq!(r, vec!["host-a".to_string(), "host-c".to_string()]);
}

#[test]
fn ring_is_sorted_and_deterministic() {
    let comments = fixture();
    let now = dt("2026-01-01T00:25:00Z");
    let r1 = ring(&comments, now, 900, 1);
    let r2 = ring(&comments, now, 900, 1);
    assert_eq!(r1, r2);
    let mut sorted = r1.clone();
    sorted.sort();
    assert_eq!(r1, sorted);
}

#[test]
fn generation_is_the_latest_boundary_at_or_before_now() {
    let comments = fixture();
    // Boundaries: host-a: created 00:00, expires 00:20+15m=00:35.
    // host-b: created 00:00, expires 00:10+15m=00:25. host-c: created
    // 00:25, expires 00:40. At t=00:26:00, the boundaries at or before
    // `now` are {00:00 (a/b create), 00:25 (b's expiry), 00:25 (c's
    // create)} -- the latest is 00:25.
    let now = dt("2026-01-01T00:26:00Z");
    let gen = generation(&comments, now, 900).expect("some boundary exists");
    assert_eq!(gen, dt("2026-01-01T00:25:00Z"));
}

#[test]
fn generation_is_none_for_an_empty_comment_set() {
    assert_eq!(generation(&[], Utc::now(), 900), None);
}

// ---- Determinism across "hosts" (AC: identical gen + ring from identical input) ----

#[test]
fn two_hosts_with_the_identical_comment_set_compute_the_identical_generation_and_ring() {
    let comments = fixture();
    let now = dt("2026-01-01T00:30:00Z");
    // Simulate two independent hosts by calling the pure functions twice
    // against clones of the identical input.
    let host_a_view =
        (generation(&comments.clone(), now, 900), ring(&comments.clone(), now, 900, 1));
    let host_b_view = (generation(&comments, now, 900), ring(&comments, now, 900, 1));
    assert_eq!(host_a_view, host_b_view);
}

// ---- Status view ----

#[test]
fn build_roster_status_reports_live_seen_generation_and_members() {
    let issue = RosterIssueRef::parse("rjwalters/loom#1234").unwrap();
    let comments = fixture();
    let now = dt("2026-01-01T00:25:00Z");
    let status = build_roster_status(&issue, &comments, "host-a", now, 900);
    assert_eq!(status.issue, "rjwalters/loom#1234");
    assert_eq!(status.seen_count, 3);
    assert_eq!(status.live_count, 2); // host-a, host-c (host-b just expired)
    assert!(status.generation.is_some());
    assert!(status.settled_secs.unwrap() >= 0);
    assert_eq!(status.members.len(), 3);
    let a = status.members.iter().find(|m| m.host == "host-a").unwrap();
    assert!(a.fresh);
    assert!(a.is_this_host);
    let b = status.members.iter().find(|m| m.host == "host-b").unwrap();
    assert!(!b.fresh, "an expired member must stay visible but marked non-fresh");
    assert!(!b.is_this_host);
}

#[test]
fn build_roster_status_on_an_empty_roster_reports_zero_and_no_generation() {
    let issue = RosterIssueRef::parse("rjwalters/loom#1234").unwrap();
    let status = build_roster_status(&issue, &[], "host-a", Utc::now(), 900);
    assert_eq!(status.live_count, 0);
    assert_eq!(status.seen_count, 0);
    assert!(status.generation.is_none());
    assert!(status.settled_secs.is_none());
    assert!(status.members.is_empty());
}

// ---- The admission fence (Issue #7691, Phase B of #6704) ----
//
// One test per condition, each driving the condition it names to failure
// while every OTHER condition passes -- so a test that goes green for the
// wrong reason (e.g. a yield that was really a settle failure) fails
// instead.

/// A settled, long-lived three-host roster serving key digest `1`, all
/// created at 00:00 (an hours-old, long-settled membership) and all still
/// beating — their last heartbeat is 60s before `now`, as a live fleet's
/// would be at any instant. Conditions 1/3/4 therefore all pass, so each
/// test below can break exactly one of them and nothing else.
const NOW: &str = "2026-01-01T10:00:00Z";
const FLEET_CREATED: &str = "2026-01-01T00:00:00Z";

fn settled_fleet_at(now: DateTime<Utc>) -> Vec<RosterComment> {
    ["host-a", "host-b", "host-c"]
        .iter()
        .enumerate()
        .map(|(i, host)| RosterComment {
            id: u64::try_from(i).unwrap() + 1,
            host: (*host).to_string(),
            serves: [1].into_iter().collect(),
            created_at: dt(FLEET_CREATED),
            updated_at: now - ChronoDuration::seconds(60),
        })
        .collect()
}

#[test]
fn a_settled_fleet_admits_every_member_with_its_ring_rank() {
    let now = dt(NOW);
    let comments = settled_fleet_at(now);
    for (expected_index, host) in ["host-a", "host-b", "host-c"].iter().enumerate() {
        match admission(&comments, host, 1, now, 900, 900, None) {
            RosterAdmission::Ring {
                index,
                count,
                generation,
            } => {
                assert_eq!(index, expected_index, "{host} ranked wrong in the ring");
                assert_eq!(count, 3);
                assert_eq!(generation, dt(FLEET_CREATED));
            }
            other => panic!("{host} must be admitted by a settled roster, got {other:?}"),
        }
    }
}

#[test]
fn condition_1_a_host_whose_own_heartbeat_is_stale_yields() {
    // host-a stopped beating 20m ago (> ttl 15m) — e.g. its forge reads
    // are failing, so it cannot know whether the fleet has evicted it (it
    // has). Every other condition still passes for it, so the yield can
    // only come from self-liveness.
    let now = dt(NOW);
    let mut comments = settled_fleet_at(now);
    comments[0].updated_at = now - ChronoDuration::seconds(1200);
    let verdict = admission(&comments, "host-a", 1, now, 900, 900, None);
    assert!(
        matches!(verdict, RosterAdmission::Yield(RosterYield::SelfStale { .. })),
        "a host the fleet has evicted must run NO roster-gated role ticks, got {verdict:?}"
    );
    // Control: with a fresh beat, the identical call is admitted — so the
    // yield above is self-liveness and nothing else.
    assert!(matches!(
        admission(&settled_fleet_at(now), "host-a", 1, now, 900, 900, None),
        RosterAdmission::Ring { .. }
    ));
}

#[test]
fn condition_1_a_host_with_no_record_at_all_yields() {
    let verdict = admission(&settled_fleet_at(dt(NOW)), "host-z", 1, dt(NOW), 900, 900, None);
    assert_eq!(verdict, RosterAdmission::Yield(RosterYield::SelfMissing));
}

#[test]
fn condition_2_a_view_older_than_the_newest_observed_generation_is_discarded() {
    let now = dt(NOW);
    let comments = settled_fleet_at(now);
    // The process has already observed a NEWER generation than this view
    // can produce (gen here is 00:00:00) -- a stale read, e.g. an
    // ETag-cached response or a lagging replica.
    let newest = Some(dt("2026-01-01T05:00:00Z"));
    let verdict = admission(&comments, "host-a", 1, now, 900, 900, newest);
    assert!(
        matches!(verdict, RosterAdmission::Yield(RosterYield::StaleGeneration { .. })),
        "an older view must be discarded, not acted on: {verdict:?}"
    );
    // The identical call with no prior observation is admitted, proving
    // the yield above came from monotonicity and nothing else.
    assert!(matches!(
        admission(&comments, "host-a", 1, now, 900, 900, None),
        RosterAdmission::Ring { .. }
    ));
}

/// A live four-host view at `now`, where host-d joined at `join` — the
/// incumbents keep beating, so only the join boundary distinguishes the
/// instants under test.
fn fleet_with_joiner_at(now: DateTime<Utc>, join: DateTime<Utc>) -> Vec<RosterComment> {
    let mut comments = settled_fleet_at(now);
    comments.push(RosterComment {
        id: 4,
        host: "host-d".to_string(),
        serves: [1].into_iter().collect(),
        created_at: join,
        updated_at: now - ChronoDuration::seconds(60),
    });
    comments
}

#[test]
fn condition_3_a_ring_that_just_changed_is_not_actionable_until_it_settles() {
    // host-d joins 60s before `now`: a fresh membership boundary, so
    // NOBODY acts under either ring for settle (900s). The gap is the
    // deliberate cost of never overlapping.
    let now = dt(NOW);
    let join = now - ChronoDuration::seconds(60);
    for host in ["host-a", "host-b", "host-c", "host-d"] {
        let verdict = admission(&fleet_with_joiner_at(now, join), host, 1, now, 900, 900, None);
        assert!(
            matches!(verdict, RosterAdmission::Yield(_)),
            "{host} acted under a ring that changed 60s ago: {verdict:?}"
        );
    }
    // At the settle deadline — an ABSOLUTE instant (`gen + settle`) that
    // every host computes identically from the same forge timestamps,
    // regardless of when each of them read — the whole fleet resumes
    // together under the new 4-ring. That simultaneity is the property
    // the fence exists to provide.
    let settled = join + ChronoDuration::seconds(900);
    for host in ["host-a", "host-b", "host-c", "host-d"] {
        assert!(
            matches!(
                admission(&fleet_with_joiner_at(settled, join), host, 1, settled, 900, 900, None),
                RosterAdmission::Ring { count: 4, .. }
            ),
            "{host} must resume once the new ring has settled"
        );
    }
    // One second earlier, nobody has resumed.
    let just_before = settled - ChronoDuration::seconds(1);
    for host in ["host-a", "host-b", "host-c", "host-d"] {
        assert!(matches!(
            admission(
                &fleet_with_joiner_at(just_before, join),
                host,
                1,
                just_before,
                900,
                900,
                None
            ),
            RosterAdmission::Yield(_)
        ));
    }
}

#[test]
fn condition_4_a_newly_joined_host_waits_a_full_ttl_before_acting() {
    // Isolating the join fence needs settle < ttl: at the production
    // floor (settle >= ttl, see `settle_secs_is_floored_at_ttl_secs`) the
    // settle window already covers the whole join fence, so condition 3
    // would mask condition 4 and this test would prove nothing.
    let now = dt(NOW);
    let join = now - ChronoDuration::seconds(300);
    let comments = fleet_with_joiner_at(now, join);
    // gen == join, settled for 300s >= settle(60): condition 3 passes...
    for host in ["host-a", "host-b", "host-c"] {
        assert!(
            matches!(
                admission(&comments, host, 1, now, 900, 60, None),
                RosterAdmission::Ring { count: 4, .. }
            ),
            "{host} (an incumbent) must be admitted once the join has settled"
        );
    }
    // ...but the joiner itself is still held out: its own record is only
    // 300s old, not a full ttl (900s).
    assert!(
        matches!(
            admission(&comments, "host-d", 1, now, 900, 60, None),
            RosterAdmission::Yield(RosterYield::Joining {
                age_secs: 300,
                ttl_secs: 900
            })
        ),
        "a joiner must not act until its own record is a full ttl old"
    );
    // At exactly `created_at + ttl` the join fence lifts.
    let at_ttl = join + ChronoDuration::seconds(900);
    assert!(matches!(
        admission(&fleet_with_joiner_at(at_ttl, join), "host-d", 1, at_ttl, 900, 60, None),
        RosterAdmission::Ring { count: 4, .. }
    ));
    // And under the production floor (settle == ttl) the joiner is fenced
    // for at least as long — the floor can only ever hold it out longer.
    assert!(matches!(
        admission(&comments, "host-d", 1, now, 900, 900, None),
        RosterAdmission::Yield(_)
    ));
}

#[test]
fn condition_5_a_host_that_does_not_serve_the_key_is_not_in_its_ring() {
    let comments = settled_fleet_at(dt(NOW));
    // Nobody serves key digest 99.
    assert_eq!(
        admission(&comments, "host-a", 99, dt(NOW), 900, 900, None),
        RosterAdmission::Yield(RosterYield::NotInRing)
    );
}

#[test]
fn every_yield_reason_labels_and_describes_itself() {
    let reasons = [
        RosterYield::SelfMissing,
        RosterYield::SelfStale {
            last_beat_secs: 1200,
            ttl_secs: 900,
        },
        RosterYield::StaleGeneration {
            observed: dt(NOW),
            newest: dt(NOW),
        },
        RosterYield::NotSettled {
            settled_secs: 60,
            settle_secs: 900,
        },
        RosterYield::NoGeneration,
        RosterYield::Joining {
            age_secs: 60,
            ttl_secs: 900,
        },
        RosterYield::NotInRing,
    ];
    for reason in reasons {
        assert!(!reason.label().is_empty());
        assert!(!reason.describe().is_empty(), "{:?}", reason.label());
    }
}

// ---- Generation high-water mark (condition 2's only state) ----

#[test]
#[serial]
fn the_generation_high_water_mark_ratchets_upward() {
    clear_generation_fence_for_tests();
    let ids: BTreeSet<u64> = [1, 2, 3].into_iter().collect();
    let newer = dt("2026-01-01T05:00:00Z");
    assert_eq!(observe_generation(&ids, newer), newer);
    // An older reading over the SAME comment set does not lower it.
    assert_eq!(observe_generation(&ids, dt("2026-01-01T00:00:00Z")), newer);
    clear_generation_fence_for_tests();
}

#[test]
#[serial]
fn the_generation_high_water_mark_resets_when_a_comment_disappears() {
    // Without this reset a host that saw a record which later vanished
    // (an operator tidying the roster issue, or a host republishing after
    // eviction) would yield FOREVER against a generation no live comment
    // set can reach again.
    clear_generation_fence_for_tests();
    let ids: BTreeSet<u64> = [1, 2, 3].into_iter().collect();
    let high = dt("2026-01-01T05:00:00Z");
    assert_eq!(observe_generation(&ids, high), high);
    let shrunk: BTreeSet<u64> = [1, 2].into_iter().collect();
    let lower = dt("2026-01-01T00:00:00Z");
    assert_eq!(
        observe_generation(&shrunk, lower),
        lower,
        "a vanished record must reset the ratchet, not deadlock the host"
    );
    clear_generation_fence_for_tests();
}

#[test]
#[serial]
fn admit_folds_the_high_water_mark_into_the_pure_fence() {
    clear_generation_fence_for_tests();
    let now = dt(NOW);
    let snapshot = |comments: Vec<RosterComment>| RosterSnapshot {
        issue: RosterIssueRef::parse("rjwalters/loom#1234").unwrap(),
        host: "host-a".to_string(),
        comments,
        ttl_secs: 900,
        settle_secs: 900,
        fetched_at: now,
    };
    let baseline = snapshot(settled_fleet_at(now));
    assert!(matches!(
        admit(&baseline, 1, now),
        RosterAdmission::Ring {
            index: 0,
            count: 3,
            ..
        }
    ));
    // host-c dies; well past its expiry + settle, the survivors act under
    // the newer generation (its eviction boundary).
    let later = now + ChronoDuration::seconds(3600);
    let mut dead_c = settled_fleet_at(later);
    dead_c[2].updated_at = later - ChronoDuration::seconds(1900);
    assert!(matches!(
        admit(&snapshot(dead_c), 1, later),
        RosterAdmission::Ring { count: 2, .. }
    ));
    // A stale read now replays the older view: discarded, never acted on.
    assert!(
        matches!(
            admit(&baseline, 1, now),
            RosterAdmission::Yield(RosterYield::StaleGeneration { .. })
        ),
        "a replayed older view must be discarded by the high-water mark"
    );
    clear_generation_fence_for_tests();
}

// ---- Write side: when a record is replaced rather than patched ----

#[test]
fn a_live_record_with_an_unchanged_serves_set_is_patched_in_place() {
    let serves: BTreeSet<u64> = [1].into_iter().collect();
    let existing = comment_id(7, "host-a", &[1], "2026-01-01T00:00:00Z", "2026-01-01T09:59:00Z");
    assert_eq!(
        resolve_publish_action(Some(&existing), &serves, dt(NOW), 900),
        RosterPublish::Patch { id: 7 }
    );
}

#[test]
fn a_first_heartbeat_creates_a_record() {
    let serves: BTreeSet<u64> = [1].into_iter().collect();
    assert_eq!(resolve_publish_action(None, &serves, dt(NOW), 900), RosterPublish::Create);
}

#[test]
fn an_expired_record_is_replaced_so_the_rejoin_gets_a_fresh_boundary() {
    // Patching an expired record in place would resurrect this host into
    // every peer's ring with NO membership boundary and no settle window
    // -- an unfenced ring change, which is the one thing the generation
    // fence cannot absorb.
    let serves: BTreeSet<u64> = [1].into_iter().collect();
    let stale = comment_id(7, "host-a", &[1], "2026-01-01T00:00:00Z", "2026-01-01T09:00:00Z");
    assert_eq!(
        resolve_publish_action(Some(&stale), &serves, dt(NOW), 900),
        RosterPublish::Republish {
            id: 7,
            reason: RosterRepublishReason::Expired,
        }
    );
}

#[test]
fn a_changed_serves_set_is_replaced_for_the_same_reason() {
    let serves: BTreeSet<u64> = [1, 2].into_iter().collect();
    let existing = comment_id(7, "host-a", &[1], "2026-01-01T00:00:00Z", "2026-01-01T09:59:00Z");
    assert_eq!(
        resolve_publish_action(Some(&existing), &serves, dt(NOW), 900),
        RosterPublish::Republish {
            id: 7,
            reason: RosterRepublishReason::ServesChanged,
        }
    );
}

#[test]
fn own_comment_picks_the_freshest_record_for_this_host() {
    let comments = vec![
        comment_id(1, "host-a", &[1], "2026-01-01T00:00:00Z", "2026-01-01T09:00:00Z"),
        comment_id(2, "host-a", &[1], "2026-01-01T08:00:00Z", "2026-01-01T09:59:00Z"),
    ];
    assert_eq!(own_comment(&comments, "host-a").map(|c| c.id), Some(2));
    assert_eq!(own_comment(&comments, "host-z"), None);
}

#[test]
#[serial]
fn settle_secs_is_floored_at_ttl_secs() {
    // The no-overlap argument needs settle >= ttl: a host holding a stale
    // view keeps acting until its OWN record expires (up to ttl after its
    // last read), so nobody may act under a new ring before then.
    let _e = EnvGuard::capture();
    let block = serde_json::json!({
        "enabled": true,
        "issue": "rjwalters/loom#42",
        "ttlSecs": 1800,
        "settleSecs": 60,
    });
    let config = resolve_roster_config_from(Some(&block));
    assert_eq!(config.ttl_secs, 1800);
    assert_eq!(config.settle_secs, 1800);
}

// ---- Snapshot cache ----

#[test]
#[serial]
fn snapshot_cache_round_trips() {
    clear_roster_snapshot_for_tests();
    assert!(roster_snapshot().is_none());
    let issue = RosterIssueRef::parse("rjwalters/loom#1234").unwrap();
    set_roster_snapshot(RosterSnapshot {
        issue: issue.clone(),
        host: "host-a".to_string(),
        comments: fixture(),
        ttl_secs: 900,
        settle_secs: 900,
        fetched_at: Utc::now(),
    });
    let snap = roster_snapshot().expect("was just set");
    assert_eq!(snap.issue, issue);
    assert_eq!(snap.comments.len(), 3);
    clear_roster_snapshot_for_tests();
    assert!(roster_snapshot().is_none());
}
