//! Tests for pool state (Issue #8857). Burn is tested beside each store
//! under `quota/`.

use chrono::{DateTime, TimeZone, Utc};

use super::{api_key_accounts_in, PoolAccount, QuotaState};
use crate::telemetry::ops::{MetricName, MetricPoint, MetricValue};

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
}

fn value(point: &MetricPoint) -> i64 {
    match point.value {
        MetricValue::Int(v) => v,
        MetricValue::Double(_) => panic!("expected int"),
    }
}

fn find<'a>(
    points: &'a [MetricPoint],
    name: MetricName,
    labels: &[(&str, &str)],
) -> Option<&'a MetricPoint> {
    points.iter().find(|p| {
        p.name == name
            && labels
                .iter()
                .all(|(k, v)| p.labels.get(*k).map(String::as_str) == Some(*v))
    })
}

// ------------------------------------------------------------------ pool

fn account(provider: &str, name: &str, usable: bool, exhausted: bool) -> PoolAccount {
    PoolAccount {
        provider: provider.into(),
        account: name.into(),
        usable,
        exhausted,
    }
}

#[test]
fn pool_gauges_count_usable_and_exhausted_per_provider() {
    let mut state = QuotaState::default();
    let accounts = [
        account("claude", "a1", true, false),
        account("claude", "a2", false, true),
        account("codex", "c1", false, true),
        // Malformed/unverifiable API key: neither usable nor exhausted.
        account("zai", "z1", false, false),
    ];
    let points = state.pool_points(&accounts, at(0));
    let get = |name, labels: &[(&str, &str)]| value(find(&points, name, labels).unwrap());
    assert_eq!(get(MetricName::PoolAccounts, &[("provider", "claude"), ("state", "usable")]), 1);
    assert_eq!(
        get(MetricName::PoolAccounts, &[("provider", "claude"), ("state", "exhausted")]),
        1
    );
    assert_eq!(get(MetricName::PoolExhausted, &[("provider", "claude")]), 0);
    assert_eq!(get(MetricName::PoolExhausted, &[("provider", "codex")]), 1);
    assert_eq!(
        get(MetricName::PoolExhausted, &[("provider", "zai")]),
        0,
        "no usable account but none exhausted is not exhaustion"
    );
    assert!(
        find(&points, MetricName::PoolExhaustions, &[]).is_none()
            && find(&points, MetricName::PoolExhaustedSeconds, &[]).is_none(),
        "the first sample emits no deltas"
    );
    assert!(points.iter().all(|p| !p.labels.contains_key("account")));
}

#[test]
fn exhaustions_count_only_new_transitions_and_downtime_accrues_while_exhausted() {
    let mut state = QuotaState::default();
    state.pool_points(
        &[
            account("codex", "c1", true, false),
            account("codex", "c2", false, true),
        ],
        at(0),
    );

    // c1 runs dry too: one new exhaustion, and the pool is now exhausted.
    let second = state.pool_points(
        &[
            account("codex", "c1", false, true),
            account("codex", "c2", false, true),
        ],
        at(300),
    );
    assert_eq!(
        value(find(&second, MetricName::PoolExhaustions, &[("provider", "codex")]).unwrap()),
        1
    );
    assert_eq!(
        value(find(&second, MetricName::PoolExhausted, &[("provider", "codex")]).unwrap()),
        1
    );
    assert!(
        find(&second, MetricName::PoolExhaustedSeconds, &[]).is_none(),
        "the pool was not exhausted at the start of this interval"
    );

    // Still exhausted: the whole interval is downtime, no new exhaustion.
    let third = state.pool_points(
        &[
            account("codex", "c1", false, true),
            account("codex", "c2", false, true),
        ],
        at(600),
    );
    assert!(find(&third, MetricName::PoolExhaustions, &[]).is_none());
    assert_eq!(
        value(find(&third, MetricName::PoolExhaustedSeconds, &[("provider", "codex")]).unwrap()),
        300
    );

    // c2 recovers: the interval it recovered in still counts (sample-and-hold).
    let fourth = state.pool_points(
        &[
            account("codex", "c1", false, true),
            account("codex", "c2", true, false),
        ],
        at(900),
    );
    assert_eq!(
        value(find(&fourth, MetricName::PoolExhausted, &[("provider", "codex")]).unwrap()),
        0
    );
    assert_eq!(
        value(find(&fourth, MetricName::PoolExhaustedSeconds, &[("provider", "codex")]).unwrap()),
        300
    );
    let fifth = state.pool_points(
        &[
            account("codex", "c1", false, true),
            account("codex", "c2", true, false),
        ],
        at(1200),
    );
    assert!(find(&fifth, MetricName::PoolExhaustedSeconds, &[]).is_none());
}

#[test]
fn a_provider_first_seen_later_counts_its_exhausted_accounts_as_new() {
    let mut state = QuotaState::default();
    state.pool_points(&[account("claude", "a1", true, false)], at(0));
    let points = state.pool_points(
        &[
            account("claude", "a1", true, false),
            account("kimi", "k1", false, true),
        ],
        at(300),
    );
    assert_eq!(
        value(find(&points, MetricName::PoolExhaustions, &[("provider", "kimi")]).unwrap()),
        1
    );
}

#[test]
fn token_snapshot_accounts_map_to_pool_accounts() {
    let state = crate::telemetry::TokenAccountState {
        account: "agent-1".into(),
        provider: "claude".into(),
        rank: Some(1),
        usage_fraction: Some(0.4),
        limit_window_reset_at: None,
        exhausted: true,
    };
    assert_eq!(PoolAccount::from(&state), account("claude", "agent-1", false, true));
}

/// #8941 item 5: the API-key pool's eligibility maps to usable/exhausted.
#[test]
fn api_key_accounts_map_eligibility_to_usable_and_exhausted() {
    use crate::api_keys_pool::{bad_marks, paths};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("api-keys");
    let dir = paths::provider_dir(&root, "zai");
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["ok", "dry", "broken", "off"] {
        let body = if name == "broken" {
            ""
        } else {
            "ZAI_API_KEY=fake\n"
        };
        let path = dir.join(format!("{name}.env"));
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    bad_marks::mark_bad(&root, "zai", "dry", "simulated exhaustion", Some(3600)).unwrap();
    std::fs::write(dir.join(".disabled"), "off\n").unwrap();
    let mut accounts = api_key_accounts_in(&[root]).unwrap();
    accounts.sort_by(|a, b| a.account.cmp(&b.account));
    assert_eq!(
        accounts,
        vec![
            // Malformed: neither usable nor exhausted.
            account("zai", "broken", false, false),
            account("zai", "dry", false, true),
            account("zai", "ok", true, false),
        ],
        "a disabled account is not in the pool"
    );
}

/// #8941 item 1: a failed pool read carries the last good read forward
/// instead of making every account vanish (and then count as newly
/// exhausted on the next good read).
#[test]
fn a_failed_api_key_pool_read_carries_the_previous_accounts_forward() {
    let mut state = QuotaState::default();
    let good = vec![account("zai", "z1", false, true)];
    assert_eq!(state.api_key_pool(Some(good.clone())), good);
    let carried = state.api_key_pool(None);
    let first = state.pool_points(&carried, at(0));
    assert_eq!(
        value(
            find(&first, MetricName::PoolAccounts, &[("provider", "zai"), ("state", "exhausted")])
                .unwrap()
        ),
        1
    );
    let carried = state.api_key_pool(None);
    let second = state.pool_points(&carried, at(300));
    assert!(find(&second, MetricName::PoolExhaustions, &[]).is_none());
}
