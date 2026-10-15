//! `autonomous.roleRunner.championMode` (#11859): the parser, the prompt it
//! produces, and that the prompt reaches a real interval admission.

use super::*;
use crate::role_runner::champion_mode::{
    parse_champion_mode, prompt_arg, ChampionMode, NON_ITEM_ARG,
};

fn champion_spec() -> RoleSpec {
    *DEFAULT_ROLES
        .iter()
        .find(|s| s.name == "champion")
        .expect("champion is shipped")
}

#[test]
fn champion_mode_parses_only_the_two_spellings() {
    assert_eq!(ChampionMode::parse("all"), Some(ChampionMode::All));
    assert_eq!(ChampionMode::parse("non-item"), Some(ChampionMode::NonItem));
    assert_eq!(ChampionMode::parse("  Non-Item "), Some(ChampionMode::NonItem));
    for bad in [
        "",
        "nonitem",
        "non_item",
        "items",
        "champion:non-item",
        "true",
    ] {
        assert_eq!(ChampionMode::parse(bad), None, "{bad:?} must not parse");
    }
    for mode in [ChampionMode::All, ChampionMode::NonItem] {
        assert_eq!(ChampionMode::parse(mode.as_str()), Some(mode), "round trip");
    }
    assert_eq!(ChampionMode::default(), ChampionMode::All);
}

#[test]
fn champion_mode_block_parse_keeps_the_value_only_to_warn() {
    let block = |v: &str| serde_json::from_str::<serde_json::Value>(v).unwrap();
    assert_eq!(parse_champion_mode(&block(r"{}")), None, "absent");
    assert_eq!(
        parse_champion_mode(&block(r#"{"championMode":"non-item"}"#)),
        Some(ChampionMode::NonItem)
    );
    // Retired: "all" parses (and warns) but changes nothing below.
    assert_eq!(
        parse_champion_mode(&block(r#"{"championMode":"all"}"#)),
        Some(ChampionMode::All)
    );
    for bad in [
        r#"{"championMode":"nonitem"}"#,
        r#"{"championMode":true}"#,
        r#"{"championMode":null}"#,
    ] {
        assert_eq!(parse_champion_mode(&block(bad)), None, "{bad}");
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
fn champion_mode_reaches_the_resolved_prompt_and_nothing_else() {
    std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
    // Every configuration resolves Champion to its non-item run (#11909).
    for cfg in [
        RoleRunnerConfig::default(),
        RoleRunnerConfig {
            champion_mode: Some(ChampionMode::All),
            ..Default::default()
        },
        RoleRunnerConfig {
            champion_mode: Some(ChampionMode::NonItem),
            ..Default::default()
        },
    ] {
        assert_eq!(resolve_role_prompt(&champion_spec(), &cfg), "/loom:champion --non-item");
    }
}

/// The item roles never fire on the idle edge (#11909); their interval loops
/// are never spawned (`daemon_service`).
#[test]
fn item_roles_never_resolve_for_the_idle_edge() {
    for role in crate::role_runner::champion_mode::STEP_ROLES {
        assert!(crate::role_runner::champion_mode::is_step_role(role));
        let cfg = RoleRunnerConfig {
            on_idle: Some(vec![role.to_string(), "guide".to_string()]),
            ..Default::default()
        };
        let names: Vec<&str> = resolve_on_idle_roles(&cfg).iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["guide"], "{role}");
    }
    assert!(!crate::role_runner::champion_mode::is_step_role("champion"));
}

#[test]
#[serial(loom_config_env)]
fn champion_mode_is_read_from_the_roots_config() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous":{"roleRunner":{"enabled":true,"championMode":"non-item"}}}"#,
    );
    let set = read_role_runner_config(tmp.path());
    let absent = tempfile::tempdir().unwrap();
    write_config(absent.path(), r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);
    let unset = read_role_runner_config(absent.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(set.champion_mode, Some(ChampionMode::NonItem));
    assert_eq!(unset.champion_mode, None);
    // The knob does not touch role membership: champion stays in the list.
    assert!(resolve_roles(&set).iter().any(|r| r.name == "champion"));
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
