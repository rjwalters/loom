use super::*;
use crate::types::ForgeBudgetReading;

fn t(secs: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-04T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + Duration::seconds(secs)
}

fn snapshot() -> StallSnapshot {
    StallSnapshot {
        observed_at: t(-60),
        core: Some(BudgetReading {
            remaining: 0,
            reset_at: Some(t(1800)),
        }),
        graphql: Some(BudgetReading {
            remaining: 4000,
            reset_at: None,
        }),
        breaker: Some(BreakerReading {
            state: "cooldown".to_string(),
            cooldown_until: Some(t(1800)),
        }),
        pool: PoolReading {
            usable: 0,
            total: 3,
        },
    }
}

fn write(snap: Option<&StallSnapshot>, as_of: DateTime<Utc>) -> (Features, Vec<FeatureOmitted>) {
    let mut features = Features::default();
    let mut omitted = Vec::new();
    write_to(snap, as_of, &mut features, &mut omitted);
    (features, omitted)
}

fn reason_of<'a>(omitted: &'a [FeatureOmitted], name: &str) -> Option<&'a str> {
    omitted
        .iter()
        .find(|o| o.name == name)
        .map(|o| o.reason.as_str())
}

#[test]
fn each_source_is_recorded() {
    let (f, omitted) = write(Some(&snapshot()), t(0));
    // Token pool.
    assert_eq!((f.pool_usable_accounts, f.pool_exhausted), (Some(0), Some(true)));
    // Rate limit, with its reset: the resume time #10210 needs.
    assert_eq!(f.ratelimit_core_remaining, Some(0));
    assert_eq!(f.ratelimit_core_reset_at, Some(t(1800)));
    assert_eq!(f.ratelimit_graphql_remaining, Some(4000));
    assert_eq!(
        reason_of(&omitted, "ratelimit_graphql_reset_at"),
        Some(reason::NO_RESET_IN_READING)
    );
    // Breaker.
    assert_eq!(f.breaker_state.as_deref(), Some("cooldown"));
    assert_eq!(f.breaker_cooldown_until, Some(t(1800)));
    assert_eq!(omitted.len(), 1, "{omitted:?}");
}

#[test]
fn missing_sources_carry_specific_reasons() {
    let mut snap = snapshot();
    snap.core = None;
    snap.breaker = None;
    snap.pool = PoolReading {
        usable: 0,
        total: 0,
    };
    let (f, omitted) = write(Some(&snap), t(0));
    assert_eq!(f.pool_exhausted, None);
    assert_eq!(reason_of(&omitted, "pool_exhausted"), Some(reason::NO_TOKEN_POOL));
    assert_eq!(
        reason_of(&omitted, "ratelimit_core_remaining"),
        Some(reason::NO_IDENTITY_READING)
    );
    assert_eq!(reason_of(&omitted, "breaker_state"), Some(reason::BREAKER_NOT_REGISTERED));

    snap.breaker = Some(BreakerReading {
        state: "closed".to_string(),
        cooldown_until: None,
    });
    let (f, omitted) = write(Some(&snap), t(0));
    assert_eq!(f.breaker_state.as_deref(), Some("closed"));
    assert_eq!(reason_of(&omitted, "breaker_cooldown_until"), Some(reason::BREAKER_CLOSED));
}

#[test]
fn a_snapshot_not_taken_before_as_of_or_too_old_is_not_used() {
    for (snap, as_of, why) in [
        (None, t(0), reason::NO_STALL_SNAPSHOT),
        (Some(snapshot()), t(-60), reason::NO_STALL_SNAPSHOT),
        (Some(snapshot()), t(MAX_AGE_SEC), reason::STALE_INPUTS),
    ] {
        let (f, omitted) = write(snap.as_ref(), as_of);
        assert_eq!(f, Features::default());
        assert_eq!(omitted.len(), NAMES.len());
        assert!(omitted.iter().all(|o| o.reason == why));
    }
}

#[test]
fn readings_are_taken_per_pool_and_only_when_fresh() {
    let reading = |pool: &str, age: i64| ForgeBudgetReading {
        pool: pool.to_string(),
        remaining: 7,
        used: None,
        reset_at: Some(t(100)),
        observed_at: t(-age),
        source: "headers".to_string(),
    };
    let budget = [
        reading("core", 10),
        reading("graphql", READING_MAX_AGE_SEC + 1),
    ];
    assert_eq!(
        super::reading(&budget, "core", t(0)),
        Some(BudgetReading {
            remaining: 7,
            reset_at: Some(t(100))
        })
    );
    assert_eq!(super::reading(&budget, "graphql", t(0)), None);
    assert_eq!(super::reading(&budget, "search", t(0)), None);
}

#[test]
fn the_names_are_declared_and_every_null_has_one_reason() {
    for name in NAMES {
        assert!(Features::NAMES.contains(&name), "{name}");
    }
    let mut partial = snapshot();
    partial.graphql = None;
    for snap in [None, Some(snapshot()), Some(partial)] {
        let (f, omitted) = write(snap.as_ref(), t(0));
        let value = serde_json::to_value(&f).unwrap();
        for name in NAMES {
            let n = omitted.iter().filter(|o| o.name == name).count();
            assert_eq!(usize::from(value[name].is_null()), n, "{name}");
        }
    }
}
