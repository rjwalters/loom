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
fn champion_mode_block_parse_soft_fails_to_none() {
    let block = |v: &str| serde_json::from_str::<serde_json::Value>(v).unwrap();
    assert_eq!(parse_champion_mode(&block(r"{}")), None, "absent");
    assert_eq!(
        parse_champion_mode(&block(r#"{"championMode":"non-item"}"#)),
        Some(ChampionMode::NonItem)
    );
    assert_eq!(
        parse_champion_mode(&block(r#"{"championMode":"all"}"#)),
        Some(ChampionMode::All)
    );
    // Typo, wrong type, null: ignored (warned), so Champion runs every queue.
    for bad in [
        r#"{"championMode":"nonitem"}"#,
        r#"{"championMode":true}"#,
        r#"{"championMode":null}"#,
    ] {
        assert_eq!(parse_champion_mode(&block(bad)), None, "{bad}");
    }
}

#[test]
fn champion_mode_argument_is_champion_and_non_item_only() {
    assert_eq!(prompt_arg("champion", Some(ChampionMode::NonItem)), Some(NON_ITEM_ARG));
    assert_eq!(prompt_arg("champion", Some(ChampionMode::All)), None);
    assert_eq!(prompt_arg("champion", None), None);
    // The knob names Champion; it never leaks onto another role's prompt.
    for spec in DEFAULT_ROLES.iter().filter(|s| s.name != "champion") {
        assert_eq!(prompt_arg(spec.name, Some(ChampionMode::NonItem)), None, "{}", spec.name);
    }
}

#[test]
#[serial]
fn champion_mode_reaches_the_resolved_prompt_and_nothing_else() {
    std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
    let non_item = RoleRunnerConfig {
        champion_mode: Some(ChampionMode::NonItem),
        ..Default::default()
    };
    assert_eq!(resolve_role_prompt(&champion_spec(), &non_item), "/loom:champion --non-item");
    // Batch mode is byte-for-byte unchanged: absent and explicit "all".
    for cfg in [
        RoleRunnerConfig::default(),
        RoleRunnerConfig {
            champion_mode: Some(ChampionMode::All),
            ..Default::default()
        },
    ] {
        assert_eq!(resolve_role_prompt(&champion_spec(), &cfg), "/loom:champion");
    }
    // Every other role resolves exactly as it does with no championMode.
    for spec in DEFAULT_ROLES.iter().filter(|s| s.name != "champion") {
        assert_eq!(
            resolve_role_prompt(spec, &non_item),
            resolve_role_prompt(spec, &RoleRunnerConfig::default()),
            "{}",
            spec.name
        );
    }
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
