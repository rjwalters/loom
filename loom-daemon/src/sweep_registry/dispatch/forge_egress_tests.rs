//! Coverage for #9984: a failing `forge egress assert` under
//! `enforcement.api = required` refuses the dispatch before any side effect
//! and publishes `sweep.blocked reason=forge-egress`; `observe` admits.
//!
//! Sibling file (declared from `dispatch.rs`) because `dispatch/tests.rs` is
//! over the file-size ratchet threshold.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

/// A dispatch-ready workspace whose repo-tier policy (`forge.egress.
/// policyPath`) carries an unknown `schemaVersion` — exit 2, never aligned.
fn workspace_with_policy(workspace: &Path, api: &str) -> (SweepRegistry, PathBuf, PathBuf) {
    touch_sweep_command(workspace);
    let config_path = workspace.join(".loom/config.json");
    let mut config: serde_json::Value = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    config["forge"] = serde_json::json!({"egress": {"policyPath": "egress-policy.json"}});
    std::fs::write(&config_path, config.to_string()).unwrap();
    std::fs::write(
        workspace.join("egress-policy.json"),
        serde_json::json!({"schemaVersion": 99, "enforcement": {"api": api}}).to_string(),
    )
    .unwrap();
    let gh_marker = workspace.join("gh-called");
    let spawn_marker = workspace.join("spawn-called");
    let fake_gh = workspace.join("fake-gh.sh");
    let fake_spawn = workspace.join("fake-spawn.sh");
    executable(&fake_gh, &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", gh_marker.display()));
    executable(&fake_spawn, &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", spawn_marker.display()));
    let mut cfg = SweepRegistryConfig::new(workspace.to_path_buf());
    cfg.skip_label_flip = false;
    cfg.gh_bin = Some(fake_gh);
    cfg.spawn_bin = Some(fake_spawn);
    cfg.journal_path = Some(workspace.join("journal.json"));
    (SweepRegistry::new(cfg), gh_marker, spawn_marker)
}

fn machine_policy_present() -> bool {
    Path::new(crate::forge_egress::policy::MACHINE_POLICY_PATH).exists()
        || std::env::var_os(crate::forge_egress::policy::POLICY_ENV).is_some()
}

#[test]
#[serial]
fn failing_assert_refuses_dispatch_and_publishes_sweep_blocked() {
    if machine_policy_present() {
        return; // a host-level policy would outrank the repo fixture
    }
    let dir = tempdir().unwrap();
    let workspace = dir.path();
    let (mut registry, gh_marker, spawn_marker) = workspace_with_policy(workspace, "required");
    let bus = Arc::new(EventBus::with_capacity(8));
    let mut events = bus.subscribe(["sweep.blocked"]);
    registry.set_event_bus(bus);

    let error = registry
        .dispatch(&SweepKind::Issue(9984), None, None, None, None)
        .unwrap_err();
    assert!(error.to_string().contains("policy.schema-version"), "{error}");
    assert!(!gh_marker.exists(), "a forge call ran before admission");
    assert!(!spawn_marker.exists(), "a child was spawned");
    assert!(!workspace.join(".loom/locks/issues/9984").exists(), "claim lock was created");
    assert!(registry.entries.is_empty());

    let event = events.try_recv().expect("sweep.blocked was published");
    assert_eq!(event.topic(), "sweep.blocked");
    let Event::Generic { payload, .. } = event else {
        panic!("expected a Generic event")
    };
    assert_eq!(payload["reason"], "forge-egress");
    assert_eq!(payload["source"], "forge-egress");
    assert_eq!(payload["kind"], serde_json::json!({"type": "Issue", "value": 9984}));
    assert_eq!(payload["codes"][0], "policy.schema-version");
}

#[test]
#[serial]
fn unknown_schema_fails_closed_even_when_it_says_observe_and_observe_admits() {
    if machine_policy_present() {
        return;
    }
    let dir = tempdir().unwrap();
    let (_registry, _, _) = workspace_with_policy(dir.path(), "observe");
    // An unknown schemaVersion means `enforcement.api` cannot be trusted.
    assert!(crate::forge_egress::gate::dispatch_refusal(dir.path()).is_some());
    // A valid v1 policy in `observe` admits whatever this host's gh looks like.
    let example = include_str!("../../../tests/fixtures/forge-egress/policy.example.json");
    std::fs::write(dir.path().join("egress-policy.json"), example).unwrap();
    assert!(crate::forge_egress::gate::dispatch_refusal(dir.path()).is_none());
    // An unconfigured workspace is always admitted.
    let clean = tempdir().unwrap();
    assert!(crate::forge_egress::gate::dispatch_refusal(clean.path()).is_none());
}
