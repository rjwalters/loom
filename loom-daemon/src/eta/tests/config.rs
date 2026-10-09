//! `autonomous.eta` resolution.

use crate::eta::config::{resolve, HistoryScopeMode, DEFAULT_REFRESH_SECS, MIN_REFRESH_SECS};
use crate::eta::Kind;
use serde_json::json;

fn no_env(_: &str) -> Option<String> {
    None
}

#[test]
fn enabled_by_default_at_five_minutes() {
    let config = resolve(&json!({}), no_env);
    assert!(config.enabled, "operator decision: on by default");
    assert!(!config.dry_run);
    assert_eq!(config.refresh_secs, DEFAULT_REFRESH_SECS);
    assert_eq!(DEFAULT_REFRESH_SECS, 300);
    assert_eq!(config.current(Kind::Land), None);
    // #9343: `augment` by default, which is a no-op until a snapshot is
    // cached — an unconfigured host behaves exactly as it did before.
    assert_eq!(config.history_scope, HistoryScopeMode::Augment);
    // #10245: the daily refit is on by default (a no-op with no snapshot).
    assert!(config.fit_enabled);
}

#[test]
fn the_daily_refit_follows_env_then_config_then_default() {
    let off = json!({"autonomous": {"eta": {"fit": {"enabled": false}}}});
    assert!(!resolve(&off, no_env).fit_enabled);
    let env_on = |key: &str| (key == "LOOM_ETA_FIT_ENABLED").then(|| "1".to_string());
    assert!(resolve(&off, env_on).fit_enabled, "env beats config");
    let env_off = |key: &str| (key == "LOOM_ETA_FIT_ENABLED").then(|| "off".to_string());
    assert!(!resolve(&json!({}), env_off).fit_enabled);
    // A non-boolean leaves the default in force.
    let typo = json!({"autonomous": {"eta": {"fit": {"enabled": "nope"}}}});
    assert!(resolve(&typo, no_env).fit_enabled);
    // Independent of the tracker switch; the task needs both.
    let eta_off = json!({"autonomous": {"eta": {"enabled": false}}});
    let config = resolve(&eta_off, no_env);
    assert!(!config.enabled);
    assert!(config.fit_enabled);
}

#[test]
fn the_history_scope_follows_env_then_config_then_default() {
    let file = json!({"autonomous": {"eta": {"historyScope": "fleet"}}});
    assert_eq!(resolve(&file, no_env).history_scope, HistoryScopeMode::Fleet);

    let env = |key: &str| (key == "LOOM_ETA_HISTORY_SCOPE").then(|| "local".to_string());
    assert_eq!(resolve(&file, env).history_scope, HistoryScopeMode::Local);

    // An unrecognised value leaves the default in force rather than picking
    // a scope at random.
    let typo = json!({"autonomous": {"eta": {"historyScope": "global"}}});
    assert_eq!(resolve(&typo, no_env).history_scope, HistoryScopeMode::Augment);
}

#[test]
fn env_beats_config_beats_default() {
    let file = json!({"autonomous": {"eta": {
        "enabled": false, "dryRun": false, "refreshSecs": 600,
        "current": {"land": "land-v1", "finish": "finish-v1"}
    }}});
    let config = resolve(&file, no_env);
    assert!(!config.enabled);
    assert_eq!(config.refresh_secs, 600);
    assert_eq!(config.current(Kind::Land), Some("land-v1"));
    assert_eq!(config.current(Kind::Finish), Some("finish-v1"));

    let env = |key: &str| match key {
        "LOOM_ETA_ENABLED" => Some("1".to_string()),
        "LOOM_ETA_DRY_RUN" => Some("true".to_string()),
        "LOOM_ETA_REFRESH_SECS" => Some("5".to_string()),
        _ => None,
    };
    let config = resolve(&file, env);
    assert!(config.enabled);
    assert!(config.dry_run);
    assert_eq!(config.refresh_secs, MIN_REFRESH_SECS, "floored");
}

// -- autonomous.eta.fleetRefresh (#10263) -----------------------------------

#[test]
fn fleet_refresh_is_on_by_default_with_the_pinned_budgets() {
    use crate::eta::config::FleetRefreshConfig;
    let c = resolve(&json!({}), no_env).fleet_refresh;
    assert_eq!(
        c,
        FleetRefreshConfig {
            enabled: true,
            env_enabled: None,
            interval_secs: 3600,
            max_calls_per_cycle: 300,
            backfill_max_calls_per_cycle: 600,
            reserve_calls: 1500,
            backfill_days: 21,
            gap_fill_max_calls_per_pass: 100,
            signoz: crate::eta::config::FleetSignozConfig::default(),
        }
    );
}

#[test]
fn signoz_history_primary_is_opt_in_and_the_gap_fill_budget_is_configurable() {
    let c = resolve(&json!({}), no_env).fleet_refresh;
    assert!(!c.signoz.history_primary, "off by default (FLAGS-OFF)");
    let c = resolve(
        &json!({"autonomous": {"eta": {"fleetRefresh": {"signoz": {"enabled": true}}}}}),
        no_env,
    )
    .fleet_refresh;
    assert!(
        !c.signoz.history_primary,
        "enabling SigNoz alone does not switch the history source"
    );
    let c = resolve(
        &json!({"autonomous": {"eta": {"fleetRefresh": {
            "gapFillMaxCallsPerPass": 7,
            "signoz": {"historyPrimary": true}
        }}}}),
        no_env,
    )
    .fleet_refresh;
    assert_eq!(c.gap_fill_max_calls_per_pass, 7);
    assert!(c.signoz.history_primary, "opt-in via config");
    let env = |k: &str| match k {
        "LOOM_ETA_FLEET_REFRESH_GAP_FILL_MAX_CALLS" => Some("3".to_string()),
        "LOOM_ETA_FLEET_SIGNOZ_HISTORY_PRIMARY" => Some("1".to_string()),
        _ => None,
    };
    let c = resolve(
        &json!({"autonomous": {"eta": {"fleetRefresh": {"signoz": {"historyPrimary": false}}}}}),
        env,
    )
    .fleet_refresh;
    assert_eq!(c.gap_fill_max_calls_per_pass, 3, "env > config");
    assert!(c.signoz.history_primary, "env > config");
}

/// A positive gap-fill budget must always make progress (#10520): one listing
/// page plus one timeline page. Lower values, `0` included, are raised.
#[test]
fn the_gap_fill_budget_has_a_floor_of_one_listing_and_one_timeline_page() {
    use crate::eta::config::MIN_FLEET_REFRESH_GAP_FILL_MAX_CALLS;
    assert_eq!(MIN_FLEET_REFRESH_GAP_FILL_MAX_CALLS, 2);
    for low in [0, 1] {
        let c = resolve(
            &json!({"autonomous": {"eta": {"fleetRefresh": {"gapFillMaxCallsPerPass": low}}}}),
            no_env,
        )
        .fleet_refresh;
        assert_eq!(c.gap_fill_max_calls_per_pass, 2, "config {low} is raised");
    }
    let env = |k: &str| (k == "LOOM_ETA_FLEET_REFRESH_GAP_FILL_MAX_CALLS").then(|| "0".to_string());
    let c = resolve(&json!({}), env).fleet_refresh;
    assert_eq!(c.gap_fill_max_calls_per_pass, 2, "env 0 is raised");
    let at_floor = json!({"autonomous": {"eta": {"fleetRefresh": {"gapFillMaxCallsPerPass": 2}}}});
    assert_eq!(
        resolve(&at_floor, no_env)
            .fleet_refresh
            .gap_fill_max_calls_per_pass,
        2
    );
}

// -- autonomous.eta.fleetRefresh.signoz (#9758) ----------------------------

#[test]
fn the_signoz_half_is_off_by_default_with_no_endpoint_or_credential() {
    let c = resolve(&json!({}), no_env).fleet_refresh.signoz;
    assert!(!c.enabled);
    assert_eq!(c.endpoint, None);
    assert_eq!(c.user, None);
    assert_eq!(c.credential_file, None);
    assert_eq!(c.page_size, 500);
    assert_eq!(c.max_pages, 200);
}

#[test]
fn the_signoz_half_follows_env_then_config_then_default() {
    let file = json!({"autonomous": {"eta": {"fleetRefresh": {"signoz": {
        "enabled": true, "endpoint": "https://ch.example:8443", "user": "reader",
        "credentialFile": "/home/op/.loom/observability/signoz-read.key",
        "pageSize": 100, "maxPages": 0
    }}}}});
    let c = resolve(&file, no_env).fleet_refresh.signoz;
    assert!(c.enabled);
    assert_eq!(c.endpoint.as_deref(), Some("https://ch.example:8443"));
    assert_eq!(c.user.as_deref(), Some("reader"));
    assert_eq!(
        c.credential_file.as_deref(),
        Some(std::path::Path::new("/home/op/.loom/observability/signoz-read.key"))
    );
    assert_eq!(c.page_size, 100);
    // Zero is not a usable ceiling: it falls back to the default.
    assert_eq!(c.max_pages, 200);
    let env = |key: &str| match key {
        "LOOM_ETA_FLEET_SIGNOZ_ENABLED" => Some("off".to_string()),
        "LOOM_ETA_FLEET_SIGNOZ_ENDPOINT" => Some("http://127.0.0.1:8123".to_string()),
        _ => None,
    };
    let c = resolve(&file, env).fleet_refresh.signoz;
    assert!(!c.enabled);
    assert_eq!(c.endpoint.as_deref(), Some("http://127.0.0.1:8123"));
}

#[test]
fn fleet_refresh_follows_env_then_config_then_default() {
    let file = json!({"autonomous": {"eta": {"fleetRefresh": {
        "enabled": false, "intervalSecs": 1800, "maxCallsPerCycle": 50,
        "backfillMaxCallsPerCycle": 700, "reserveCalls": 900, "backfillDays": 30
    }}}});
    let c = resolve(&file, no_env).fleet_refresh;
    assert!(!c.enabled);
    assert_eq!(
        (c.interval_secs, c.max_calls_per_cycle, c.backfill_max_calls_per_cycle),
        (1800, 50, 700)
    );
    assert_eq!((c.reserve_calls, c.backfill_days), (900, 30));

    let env = |key: &str| {
        match key {
            "LOOM_ETA_FLEET_REFRESH_ENABLED" => Some("1"),
            "LOOM_ETA_FLEET_REFRESH_INTERVAL_SECS" => Some("7200"),
            "LOOM_ETA_FLEET_REFRESH_MAX_CALLS" => Some("10"),
            "LOOM_ETA_FLEET_REFRESH_BACKFILL_MAX_CALLS" => Some("20"),
            "LOOM_ETA_FLEET_REFRESH_RESERVE" => Some("30"),
            "LOOM_ETA_FLEET_REFRESH_BACKFILL_DAYS" => Some("40"),
            _ => None,
        }
        .map(str::to_string)
    };
    let c = resolve(&file, env).fleet_refresh;
    assert!(c.enabled);
    assert_eq!(
        (c.interval_secs, c.max_calls_per_cycle, c.backfill_max_calls_per_cycle),
        (7200, 10, 20)
    );
    assert_eq!((c.reserve_calls, c.backfill_days), (30, 40));

    let off = |key: &str| (key == "LOOM_ETA_FLEET_REFRESH_ENABLED").then(|| "0".to_string());
    assert!(!resolve(&json!({}), off).fleet_refresh.enabled);
    // #10918: the env override is kept apart, so an env `false` stays the hard
    // stop even on the explicit ETA authority; a config `false` is not one.
    assert_eq!(resolve(&json!({}), off).fleet_refresh.env_enabled, Some(false));
    let config_off = json!({"autonomous": {"eta": {"fleetRefresh": {"enabled": false}}}});
    assert_eq!(resolve(&config_off, no_env).fleet_refresh.env_enabled, None);
}

#[test]
fn fleet_refresh_clamps_the_interval_and_the_backfill_depth() {
    use crate::eta::config::{MIN_FLEET_REFRESH_BACKFILL_DAYS, MIN_FLEET_REFRESH_INTERVAL_SECS};
    let file = json!({"autonomous": {"eta": {"fleetRefresh": {
        "intervalSecs": 5, "backfillDays": 1
    }}}});
    let c = resolve(&file, no_env).fleet_refresh;
    assert_eq!(c.interval_secs, MIN_FLEET_REFRESH_INTERVAL_SECS);
    assert_eq!(MIN_FLEET_REFRESH_INTERVAL_SECS, 900);
    assert_eq!(c.backfill_days, MIN_FLEET_REFRESH_BACKFILL_DAYS);
    assert_eq!(MIN_FLEET_REFRESH_BACKFILL_DAYS, crate::eta::fit::WINDOW_DAYS + 1);
    assert_eq!(MIN_FLEET_REFRESH_BACKFILL_DAYS, 15);
}
