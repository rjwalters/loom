//! Champion's non-item-only role-runner run (#11859, #11909, #11911): the
//! retired `championMode` key, the prompt, and that the prompt reaches a real
//! interval admission.

use super::*;
use crate::role_runner::champion_mode::{prompt_arg, warn_champion_mode, NON_ITEM_ARG};

fn champion_spec() -> RoleSpec {
    *DEFAULT_ROLES
        .iter()
        .find(|s| s.name == "champion")
        .expect("champion is shipped")
}

/// `championMode` is retired (#11911): there is no mode left to parse, and no
/// value of any type is an error. Only the accepted spelling stays quiet.
#[test]
fn champion_mode_is_parsed_only_to_warn() {
    let block = |v: &str| serde_json::from_str::<serde_json::Value>(v).unwrap();
    for cfg in [
        r"{}",
        r#"{"championMode":"non-item"}"#,
        r#"{"championMode":"  Non-Item "}"#,
        r#"{"championMode":"all"}"#,
        r#"{"championMode":"nonitem"}"#,
        r#"{"championMode":true}"#,
        r#"{"championMode":null}"#,
    ] {
        warn_champion_mode(&block(cfg));
    }
}

#[test]
fn champion_always_runs_non_item_and_no_other_role_gets_the_argument() {
    assert_eq!(prompt_arg("champion"), Some(NON_ITEM_ARG));
    for spec in DEFAULT_ROLES.iter().filter(|s| s.name != "champion") {
        assert_eq!(prompt_arg(spec.name), None, "{}", spec.name);
    }
}

#[test]
#[serial]
fn champion_resolves_to_its_non_item_prompt() {
    std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
    assert_eq!(
        resolve_role_prompt(&champion_spec(), &RoleRunnerConfig::default()),
        "/loom:champion --non-item"
    );
}

/// The item roles never tick on the role runner (#11909): not by default, not
/// when a config names them.
#[test]
fn item_roles_never_resolve_for_the_interval_or_the_idle_edge() {
    let names = |specs: Vec<RoleSpec>| specs.iter().map(|s| s.name).collect::<Vec<_>>();
    for role in crate::role_runner::champion_mode::STEP_ROLES {
        assert!(!names(interval_default_roles()).contains(&role), "{role}");
        let cfg = RoleRunnerConfig {
            roles: Some(vec![role.to_string(), "guide".to_string()]),
            on_idle: Some(vec![role.to_string()]),
            ..Default::default()
        };
        assert_eq!(names(resolve_roles(&cfg)), vec!["guide"]);
        assert!(resolve_on_idle_roles(&cfg).is_empty());
    }
    assert!(names(interval_default_roles()).contains(&"champion"));
}

/// A root that still sets `championMode`, to any value, keeps resolving its
/// config, and Champion stays in the role list.
#[test]
#[serial(loom_config_env)]
fn a_retired_champion_mode_changes_nothing_in_the_roots_config() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    for mode in ["\"non-item\"", "\"all\"", "3"] {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            &format!(
                r#"{{"autonomous":{{"roleRunner":{{"enabled":true,"championMode":{mode}}}}}}}"#
            ),
        );
        let set = read_role_runner_config(tmp.path());
        assert_eq!(set.enabled, Some(true), "{mode}");
        assert!(resolve_roles(&set).iter().any(|r| r.name == "champion"), "{mode}");
    }
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

/// End to end through the interval admission path: the prompt the tick is
/// admitted with is the one the session is launched with.
#[test]
#[serial]
fn champion_mode_non_item_is_the_admitted_interval_prompt() {
    let _env = ShardEnvGuard::capture();
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["champion"],"championMode":"non-item"}}}"#,
    );
    let in_progress = new_in_progress_guard();
    let decision = decide_root_tick(
        tmp.path(),
        &champion_spec(),
        &in_progress,
        &mut HashSet::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
    );
    let (prompt, _guard) = decision.expect("champion is enabled and listed");
    assert_eq!(prompt, "/loom:champion --non-item");
}
