use super::*;
use crate::types::ForgeBudgetReading;

fn t(secs: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-04T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + Duration::seconds(secs)
}

const REPO: &str = "org/repo";

fn snapshot() -> StallSnapshot {
    StallSnapshot {
        observed_at: t(-60),
        budgets: BTreeMap::from([(
            REPO.to_string(),
            RepoBudget {
                core: Some(BudgetReading {
                    remaining: 0,
                    reset_at: Some(t(1800)),
                }),
                graphql: Some(BudgetReading {
                    remaining: 4000,
                    reset_at: None,
                }),
            },
        )]),
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
    write_to(snap, REPO, as_of, &mut features, &mut omitted);
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
    snap.budgets.get_mut(REPO).unwrap().core = None;
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
    partial.budgets.get_mut(REPO).unwrap().graphql = None;
    for snap in [None, Some(snapshot()), Some(partial)] {
        let (f, omitted) = write(snap.as_ref(), t(0));
        let value = serde_json::to_value(&f).unwrap();
        for name in NAMES {
            let n = omitted.iter().filter(|o| o.name == name).count();
            assert_eq!(usize::from(value[name].is_null()), n, "{name}");
        }
    }
}

fn reading_of(remaining: u64, pool: &str) -> ForgeBudgetReading {
    ForgeBudgetReading {
        pool: pool.to_string(),
        remaining,
        used: None,
        reset_at: Some(t(100)),
        observed_at: t(-10),
        source: "headers".to_string(),
    }
}

#[test]
fn two_readers_of_one_role_and_a_writer_fallback_are_never_mixed() {
    // Reader A (serving org-a/x) is exhausted; reader B (serving org-b/y) is
    // fresh and newer. org-c/z has no reader and reads on the writer.
    let buckets = BTreeMap::from([
        ("reader:1@org-a".to_string(), vec![reading_of(0, "core")]),
        ("reader:2@org-b".to_string(), vec![reading_of(4000, "core")]),
    ]);
    let bucket_of = |repo: &str| match repo {
        "org-a/x" => Some("reader:1@org-a".to_string()),
        "org-b/y" => Some("reader:2@org-b".to_string()),
        _ => None,
    };
    let repos: Vec<String> = ["org-a/x", "org-b/y", "org-c/z", "org-a/w"]
        .map(String::from)
        .to_vec();
    let mut snap = snapshot();
    snap.budgets = repo_budgets(&repos, bucket_of, &buckets, t(0));
    let remaining = |snap: &StallSnapshot, repo: &str| {
        let mut f = Features::default();
        let mut omitted = Vec::new();
        write_to(Some(snap), repo, t(0), &mut f, &mut omitted);
        (
            f.ratelimit_core_remaining,
            reason_of(&omitted, "ratelimit_core_remaining").map(str::to_string),
        )
    };
    assert_eq!(remaining(&snap, "org-a/x"), (Some(0), None));
    assert_eq!(remaining(&snap, "org-b/y"), (Some(4000), None));
    // No reader: the writer-fallback budget is not borrowed.
    assert_eq!(
        remaining(&snap, "org-c/z"),
        (None, Some(reason::NO_READER_FOR_REPO.to_string()))
    );
    // A reader with no reading of its own is omitted, not filled from another.
    snap.budgets = repo_budgets(
        &["org-a/w".to_string()],
        |_| Some("reader:9@org-a".to_string()),
        &buckets,
        t(0),
    );
    assert_eq!(
        remaining(&snap, "org-a/w"),
        (None, Some(reason::NO_IDENTITY_READING.to_string()))
    );
}
