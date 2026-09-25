//! The concierge role's **second** opt-in — the config-level gate (#7947,
//! Phase 3b of #4196).
//!
//! The operator-agent persona is an inbound control channel wired to a chat
//! room. Two independent gates keep it from arriving by accident:
//!
//!   1. It is excluded from the "unset `roles` ⇒ all defaults" fallback (the
//!      same mechanism as architect's #5656 carve-out, a different reason).
//!   2. It is gated AGAIN on `safehouse.concierge` resolving — naming it in
//!      `autonomous.roleRunner.roles` is deliberately not enough.
//!
//! These tests pin both, and pin that no OTHER shipped role acquired a config
//! gate by accident. Split out of `role_runner/tests.rs` so the
//! over-threshold parent shrinks rather than grows
//! (`.loom/docs/file-size-policy.md`).

use super::*;

/// The shipped concierge spec, looked up by name rather than reconstructed,
/// so a drift in cadence/prompt is caught here instead of hidden.
fn concierge_spec() -> RoleSpec {
    *DEFAULT_ROLES
        .iter()
        .find(|s| s.name == crate::concierge::CONCIERGE_ROLE)
        .expect("#7947: DEFAULT_ROLES must include concierge")
}

/// Clear every `LOOM_SAFEHOUSE_CONCIERGE_*` override so a test measures the
/// config layer, not an ambient env var. Paired with `#[serial]`.
fn clear_concierge_env() {
    for key in [
        "LOOM_SAFEHOUSE_CONCIERGE_ENABLED",
        "LOOM_SAFEHOUSE_CONCIERGE_SENDERS",
        "LOOM_SAFEHOUSE_CONCIERGE_ROOM",
        "LOOM_SAFEHOUSE_CONCIERGE_PERSONA",
        "LOOM_SAFEHOUSE_CONCIERGE_MAX_MESSAGES",
        "LOOM_SAFEHOUSE_CONCIERGE_MAX_TURNS",
    ] {
        std::env::remove_var(key);
    }
}

#[test]
fn concierge_is_shipped_but_never_an_interval_default() {
    let spec = concierge_spec();
    assert_eq!(spec.prompt, "/loom:concierge");
    assert!(
        !spec.is_interval_default(),
        "#7947: an unpinned repo must never acquire a chat-room-triggered agent \
         by omitting `autonomous.roleRunner.roles`"
    );
    // The parent module's `test_default_roles_includes_architect_as_idle_only`
    // pins the *count* of non-interval-default roles; this pins WHICH ones, so
    // a future carve-out cannot be smuggled in by swapping one for another.
    let carve_outs: Vec<&str> = DEFAULT_ROLES
        .iter()
        .filter(|s| !s.is_interval_default())
        .map(|s| s.name)
        .collect();
    assert_eq!(carve_outs, vec!["architect", crate::concierge::CONCIERGE_ROLE]);
}

#[test]
#[serial]
fn concierge_is_config_gated_off_without_a_safehouse_concierge_block() {
    clear_concierge_env();
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);

    let reason = role_is_config_gated(&concierge_spec(), tmp.path())
        .expect("absent safehouse.concierge must gate the role off");
    assert!(
        reason.contains("safehouse.concierge"),
        "the gate must name the key an operator has to set: {reason}"
    );
}

#[test]
#[serial]
fn concierge_ungates_only_once_the_block_names_a_usable_sender() {
    clear_concierge_env();
    let tmp = tempfile::tempdir().unwrap();

    // An explicitly disabled block is still off.
    write_config(
        tmp.path(),
        r#"{"safehouse":{"concierge":{"enabled":false,"allowedSenders":["@you:example.org"]}}}"#,
    );
    assert!(role_is_config_gated(&concierge_spec(), tmp.path()).is_some());

    // An enabled block naming NOBODY is off too — an empty allowlist is
    // deny-all, never "allow everyone".
    write_config(
        tmp.path(),
        r#"{"safehouse":{"concierge":{"enabled":true,"allowedSenders":[]}}}"#,
    );
    assert!(
        role_is_config_gated(&concierge_spec(), tmp.path()).is_some(),
        "an empty allowlist must keep the persona off, not admit everyone"
    );

    // Only a block naming a usable Matrix ID opens the gate.
    write_config(
        tmp.path(),
        r#"{"safehouse":{"concierge":{"enabled":true,"allowedSenders":["@you:example.org"]}}}"#,
    );
    assert!(
        role_is_config_gated(&concierge_spec(), tmp.path()).is_none(),
        "a configured block must let the role tick"
    );
}

#[test]
#[serial]
fn concierge_is_the_only_config_gated_role() {
    clear_concierge_env();
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);

    let gated: Vec<&str> = DEFAULT_ROLES
        .iter()
        .filter(|spec| role_is_config_gated(spec, tmp.path()).is_some())
        .map(|spec| spec.name)
        .collect();
    assert_eq!(
        gated,
        vec![crate::concierge::CONCIERGE_ROLE],
        "a new config-gated role needs its own docs update; an EXISTING role \
         acquiring a gate would silently stop ticking on every unconfigured repo"
    );
}

#[test]
#[serial]
fn naming_concierge_in_roles_is_not_enough_to_make_it_tick() {
    // The whole point of the second gate: an explicit `roles` allowlist is
    // one opt-in, and one opt-in is not enough for a chat-room-driven agent.
    let _shard = ShardEnvGuard::capture();
    ShardEnvGuard::become_host(0, 1);
    clear_concierge_env();

    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["concierge"]}}}"#,
    );

    // `resolve_roles` DOES select it — the allowlist is honored…
    let config = RoleRunnerConfig {
        roles: Some(vec!["concierge".to_string()]),
        ..Default::default()
    };
    assert_eq!(resolve_roles(&config).len(), 1);

    // …and the tick decision still refuses, because the config block is absent.
    let in_progress = new_in_progress_guard();
    let decision = decide_root_tick(
        tmp.path(),
        &concierge_spec(),
        &in_progress,
        &mut HashSet::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
    );
    assert!(
        decision.is_none(),
        "#7947: `roles: [\"concierge\"]` alone must not start listening to a room"
    );

    // Adding the config block is what makes the same workspace tick.
    write_config(
        tmp.path(),
        r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["concierge"]}},
            "safehouse":{"concierge":{"allowedSenders":["@you:example.org"]}}}"#,
    );
    let in_progress = new_in_progress_guard();
    assert!(
        decide_root_tick(
            tmp.path(),
            &concierge_spec(),
            &in_progress,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .is_some(),
        "both opt-ins present must admit the tick"
    );
}
