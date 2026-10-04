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
