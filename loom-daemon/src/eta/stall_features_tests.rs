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
        writers: BTreeMap::new(),
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
    // The writer has no reading in this snapshot (#10334).
    assert_eq!(
        reason_of(&omitted, "ratelimit_writer_core_remaining"),
        Some(reason::NO_WRITER_READING)
    );
    assert_eq!(omitted.len(), 3, "{omitted:?}");
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

/// The writer bucket of `owner`, for building fixtures.
fn wb(owner: &str) -> String {
    crate::forge_identity::writer_bucket(owner).unwrap()
}

/// `writers` for `repos`, every repo served by `owner`'s writer.
fn writers_of(
    repos: &[String],
    owner: &str,
    buckets: &BTreeMap<String, Vec<ForgeBudgetReading>>,
) -> BTreeMap<String, RepoBudget> {
    repo_budgets(repos, |_| Some(wb(owner)), buckets, t(0))
}

/// #10334: a healthy reader cannot mask an exhausted writer.
#[test]
fn an_exhausted_writer_shows_through_a_healthy_reader() {
    let buckets = BTreeMap::from([
        (
            "reader:1@org".to_string(),
            vec![reading_of(4000, "core"), reading_of(5000, "graphql")],
        ),
        (wb("org"), vec![reading_of(0, "core"), reading_of(900, "graphql")]),
    ]);
    let repos = vec![REPO.to_string()];
    let mut snap = snapshot();
    snap.budgets = repo_budgets(&repos, |_| Some("reader:1@org".to_string()), &buckets, t(0));
    snap.writers = writers_of(&repos, "org", &buckets);
    let (f, omitted) = write(Some(&snap), t(0));
    assert_eq!(f.ratelimit_core_remaining, Some(4000));
    assert_eq!(f.ratelimit_writer_core_remaining, Some(0));
    assert_eq!(f.ratelimit_writer_graphql_remaining, Some(900));
    assert_eq!(f.ratelimit_min_remaining, Some(0));
    assert_eq!(f.ratelimit_exhausted, Some(true));
    assert!(reason_of(&omitted, "ratelimit_exhausted").is_none());

    // Both healthy: not exhausted, the minimum is the writer's GraphQL.
    let healthy = BTreeMap::from([
        ("reader:1@org".to_string(), vec![reading_of(4000, "core")]),
        (wb("org"), vec![reading_of(3000, "core"), reading_of(900, "graphql")]),
    ]);
    snap.budgets = repo_budgets(&repos, |_| Some("reader:1@org".to_string()), &healthy, t(0));
    snap.writers = writers_of(&repos, "org", &healthy);
    let (f, _) = write(Some(&snap), t(0));
    assert_eq!((f.ratelimit_min_remaining, f.ratelimit_exhausted), (Some(900), Some(false)));

    // A zero reading whose reset has passed constrains nothing.
    let mut reset = reading_of(0, "core");
    reset.reset_at = Some(t(-1));
    snap.writers = writers_of(&repos, "org", &BTreeMap::from([(wb("org"), vec![reset])]));
    snap.budgets.clear();
    let (f, _) = write(Some(&snap), t(0));
    assert_eq!(f.ratelimit_exhausted, None);
    assert_eq!(
        reason_of(&write(Some(&snap), t(0)).1, "ratelimit_exhausted"),
        Some(reason::NO_BUDGET_READING)
    );
}

/// #10334 (Judge P2): a multi-owner fleet holds one writer per owner. Owner
/// A's writer is exhausted; owner B's writer answers later with budget left.
/// Through the sink and the per-repo writer selection, A's repo keeps its own
/// exhausted signal and B's repo its own healthy one — neither writer stands
/// in for the other, and no label is credential-shaped.
#[test]
fn two_owners_writers_each_keep_their_own_budget() {
    use crate::forge_identity::serving_writer_bucket;
    let at_time = |secs: i64, rem: u64| ForgeBudgetReading {
        observed_at: t(secs),
        ..reading_of(rem, "core")
    };
    // Readings as the sink returns them: A exhausted (older), B healthy (newer).
    let (a_bucket, b_bucket) = (wb("Org-A"), wb("org-b"));
    let (a, b) = (at_time(-30, 0), at_time(-5, 4000));
    assert_ne!(a_bucket, b_bucket);
    let buckets = BTreeMap::from([(a_bucket.clone(), vec![a]), (b_bucket.clone(), vec![b])]);
    let repos = vec!["org-a/x".to_string(), "org-b/y".to_string()];
    // Both owners have their own registered writer; the primary is org-a.
    let writers = repo_budgets(
        &repos,
        |repo| serving_writer_bucket(repo, true, Some("org-a")),
        &buckets,
        t(0),
    );
    let mut snap = snapshot();
    snap.budgets.clear();
    snap.writers = writers;
    let at = |repo: &str| {
        let mut features = Features::default();
        let mut omitted = Vec::new();
        write_to(Some(&snap), repo, t(0), &mut features, &mut omitted);
        (features, omitted)
    };
    let (fa, oa) = at("org-a/x");
    assert_eq!(fa.ratelimit_writer_core_remaining, Some(0));
    assert_eq!((fa.ratelimit_min_remaining, fa.ratelimit_exhausted), (Some(0), Some(true)));
    let (fb, _) = at("org-b/y");
    assert_eq!(fb.ratelimit_writer_core_remaining, Some(4000));
    assert_eq!(fb.ratelimit_exhausted, Some(false));

    // A repo of an owner with no writer of its own is served by the primary
    // writer (org-a here), so it inherits A's exhaustion, not B's health.
    assert_eq!(serving_writer_bucket("org-c/z", false, Some("org-a")), Some(a_bucket.clone()));

    // Nothing carried or keyed is credential-shaped.
    let text = format!(
        "{a_bucket}{b_bucket}{}{}{}",
        serde_json::to_string(&fa).unwrap(),
        serde_json::to_string(&fb).unwrap(),
        serde_json::to_string(&oa.iter().map(|o| o.reason.clone()).collect::<Vec<_>>()).unwrap(),
    );
    assert_no_credential_shape(&text);
}

fn assert_no_credential_shape(text: &str) {
    for shape in [
        "ghp_",
        "gho_",
        "ghs_",
        "ghu_",
        "github_pat_",
        "Bearer ",
        "-----BEGIN",
    ] {
        assert!(!text.contains(shape), "{shape} in {text}");
    }
}

/// #10334: nothing recorded or carried is credential-shaped, and a
/// credential-shaped or invalid "owner" never becomes a writer label.
#[test]
fn no_credential_shaped_string_is_carried() {
    let buckets = BTreeMap::from([(wb("org"), vec![reading_of(5, "core")])]);
    let mut snap = snapshot();
    snap.writers = writers_of(&[REPO.to_string()], "org", &buckets);
    let (f, omitted) = write(Some(&snap), t(0));
    let text = format!(
        "{}{}{}",
        serde_json::to_string(&f).unwrap(),
        serde_json::to_string(&omitted.iter().map(|o| o.reason.clone()).collect::<Vec<_>>())
            .unwrap(),
        wb("org"),
    );
    assert_no_credential_shape(&text);
    assert_eq!(wb("Org"), "writer@org");
    for bad in ["ghp_abc123", "github_pat_x", "Bearer x", "", "-org", "a/b"] {
        assert_eq!(crate::forge_identity::writer_bucket(bad), None, "{bad}");
    }
}
