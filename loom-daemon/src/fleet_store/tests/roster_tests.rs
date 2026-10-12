use std::path::{Path, PathBuf};

use super::*;

const ROSTER: &str = r#"# fleet roster
root: ~/src

repos:
  - name: tooling
    remote: git@example.com:acme/tooling.git
    dir: tooling
    visibility: public          # ignored: not part of the contract
    fleet: true
    fleet_priority: 10          # drains first
    purpose: Build tooling — "the" tools.
    consumes: [lib-a, lib-b]    # ignored

  - name: product
    remote: git@example.com:acme/product.git
    dir: product-src
    fleet: true
    purpose: >-
      Folded purpose text
      over two lines.

  - name: secrets
    remote: git@example.com:acme/secrets.git
    dir: secrets
    firewall: true
    fleet: false

  - name: docs
    remote: null
    dir: docs
    fleet: false
"#;

fn home() -> PathBuf {
    PathBuf::from("/home/op")
}

fn ident(p: &Path) -> PathBuf {
    p.to_path_buf()
}

fn reg(path: &str, priority: u32) -> Registered {
    Registered {
        root: PathBuf::from(path),
        priority,
        maintain_only: false,
    }
}

#[test]
fn parses_the_contract_and_ignores_other_keys() {
    let r = parse(ROSTER, &home()).unwrap();
    assert_eq!(r.root, PathBuf::from("/home/op/src"));
    assert_eq!(r.records.len(), 4);
    assert_eq!(
        r.records[0],
        Record {
            name: "tooling".to_string(),
            dir: "tooling".to_string(),
            remote: Some("git@example.com:acme/tooling.git".to_string()),
            fleet: true,
            maintain_only: false,
            firewall: false,
            fleet_priority: Some(10),
        }
    );
    assert_eq!(r.records[3].remote, None);
}

#[test]
fn desired_is_fleet_true_and_not_firewall_with_default_priority() {
    let r = parse(ROSTER, &home()).unwrap();
    let d = r.desired();
    assert_eq!(
        d,
        vec![
            Desired {
                name: "tooling".to_string(),
                path: PathBuf::from("/home/op/src/tooling"),
                priority: 10,
                maintain_only: false,
            },
            Desired {
                name: "product".to_string(),
                path: PathBuf::from("/home/op/src/product-src"),
                priority: DEFAULT_WORKSPACE_PRIORITY,
                maintain_only: false,
            },
        ]
    );
}

#[test]
fn fleet_and_firewall_together_is_a_hard_error_not_a_silent_exclusion() {
    let text =
        ROSTER.replace("firewall: true\n    fleet: false", "firewall: true\n    fleet: true");
    let err = parse(&text, &home()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("`secrets`") && msg.contains("fleet: true AND firewall: true"),
        "{msg}"
    );
}

#[test]
fn malformed_flags_and_priorities_are_hard_errors() {
    for (from, to) in [
        ("fleet_priority: 10", "fleet_priority: high"),
        ("fleet_priority: 10", "fleet_priority: -1"),
        ("firewall: true", "firewall: \"yes\""),
        ("firewall: true", "firewall: yes"),
        ("dir: docs", "dir: ../docs"),
        ("dir: product-src", "dir: tooling"),
        ("name: docs", "name: tooling"),
        ("root: ~/src", "root: relative/src"),
    ] {
        let text = ROSTER.replacen(from, to, 1);
        assert!(parse(&text, &home()).is_err(), "{from} -> {to} must be refused");
    }
}

#[test]
fn unparseable_yaml_fails_closed() {
    assert!(parse("root: /x\nrepos:\n  - name: a\n\tfleet: true\n", &home()).is_err());
    assert!(parse("root: /x\nrepos:\n  - name: &a app\n", &home()).is_err());
    assert!(parse("root: /x\n", &home()).is_err(), "missing repos");
    assert!(parse("- a\n- b\n", &home()).is_err(), "not a mapping");
}

#[test]
fn plan_adds_removes_and_reprioritizes() {
    let r = parse(ROSTER, &home()).unwrap();
    let registered = vec![
        reg("/home/op/src/tooling", 50),  // priority change 50 -> 10
        reg("/home/op/src/secrets", 100), // firewall: must go
        reg("/home/op/src/docs", 100),    // fleet: false: must go
        reg("/home/op/loom-daemon", 100), // not in the store: left alone
    ];
    let plan = plan(&r, &registered, &ident, &|_| true);
    assert_eq!(
        plan.changes,
        vec![
            Change::Remove {
                name: "secrets".to_string(),
                path: PathBuf::from("/home/op/src/secrets"),
                reason: "firewall".to_string(),
            },
            Change::Remove {
                name: "docs".to_string(),
                path: PathBuf::from("/home/op/src/docs"),
                reason: "fleet: false".to_string(),
            },
            Change::Add {
                name: "product".to_string(),
                path: PathBuf::from("/home/op/src/product-src"),
                priority: DEFAULT_WORKSPACE_PRIORITY,
                maintain_only: false,
            },
            Change::SetPriority {
                name: "tooling".to_string(),
                path: PathBuf::from("/home/op/src/tooling"),
                from: 50,
                to: 10,
            },
        ]
    );
    assert_eq!(plan.unmanaged, vec![PathBuf::from("/home/op/loom-daemon")]);
    assert_eq!(plan.check_exit_code(), 1);
}

#[test]
fn a_missing_clone_is_reported_never_added() {
    let r = parse(ROSTER, &home()).unwrap();
    let registered = vec![reg("/home/op/src/tooling", 10)];
    let plan = plan(&r, &registered, &ident, &|p| !p.ends_with("product-src"));
    assert_eq!(
        plan.changes,
        vec![Change::MissingClone {
            name: "product".to_string(),
            path: PathBuf::from("/home/op/src/product-src"),
            remote: Some("git@example.com:acme/product.git".to_string()),
            priority: DEFAULT_WORKSPACE_PRIORITY,
            maintain_only: false,
        }]
    );
    assert_eq!(plan.check_exit_code(), 1);
}

#[test]
fn a_matching_registry_is_in_sync() {
    let r = parse(ROSTER, &home()).unwrap();
    let registered = vec![
        reg("/home/op/src/tooling", 10),
        reg("/home/op/src/product-src", DEFAULT_WORKSPACE_PRIORITY),
    ];
    let plan = plan(&r, &registered, &ident, &|_| true);
    assert!(plan.is_in_sync());
    assert_eq!(plan.in_sync, 2);
    assert_eq!(plan.check_exit_code(), 0);
}

#[test]
fn paths_are_compared_after_normalization() {
    let r = parse(ROSTER, &home()).unwrap();
    // The registry stores canonical paths (e.g. a symlinked home resolved).
    let canon = |p: &Path| PathBuf::from(p.to_string_lossy().replace("/home/op", "/data/op"));
    let registered = vec![
        reg("/data/op/src/tooling", 10),
        reg("/data/op/src/product-src", DEFAULT_WORKSPACE_PRIORITY),
        reg("/data/op/src/secrets", 100),
    ];
    let plan = plan(&r, &registered, &canon, &|_| true);
    assert_eq!(plan.changes.len(), 1, "{:?}", plan.changes);
    assert!(matches!(&plan.changes[0], Change::Remove { name, .. } if name == "secrets"));
}

// ----------------------------------------------------------------------------
// #11186: `fleet: maintain`
// ----------------------------------------------------------------------------

const PRODUCT: &str = "dir: product-src\n    fleet: true\n";

fn maintain(path: &str, priority: u32) -> Registered {
    Registered {
        maintain_only: true,
        ..reg(path, priority)
    }
}

#[test]
fn fleet_maintain_is_desired_and_maintain_only() {
    let text = ROSTER.replacen(PRODUCT, "dir: product-src\n    fleet: maintain\n", 1);
    let r = parse(&text, &home()).unwrap();
    assert!(r.records[1].fleet && r.records[1].maintain_only);
    assert!(!r.records[0].maintain_only);
    let d = r.desired();
    assert_eq!(d.len(), 2, "a maintain-only repo is registered and stays registered");
    assert!(d[1].maintain_only && !d[0].maintain_only);

    // The compiled `fleet.json` form reads the same.
    let doc = serde_json::json!({"root": "/r", "repos": [{"name": "x", "fleet": "maintain"}]});
    let r = from_compiled(doc.as_object().unwrap(), &home()).unwrap();
    assert!(r.desired()[0].maintain_only);
}

#[test]
fn fleet_must_be_true_false_or_maintain() {
    for bad in [
        "fleet: Maintain",
        "fleet: \"yes\"",
        "fleet: 1",
        "fleet: dispatch",
    ] {
        let text = ROSTER.replacen(PRODUCT, &format!("dir: product-src\n    {bad}\n"), 1);
        let msg = format!("{:#}", parse(&text, &home()).unwrap_err());
        assert!(msg.contains("true, false or maintain"), "{bad}: {msg}");
    }
    // Maintain plus firewall is refused like fleet: true plus firewall.
    let text =
        ROSTER.replace("firewall: true\n    fleet: false", "firewall: true\n    fleet: maintain");
    let msg = format!("{:#}", parse(&text, &home()).unwrap_err());
    assert!(msg.contains("fleet: maintain AND firewall: true"), "{msg}");
}

/// A separate `dispatch: false` key would be ignored like every other unknown
/// key, so the repo would be registered and dispatched: the reason the mode
/// is a value of `fleet` (see the module docs). Pinned so a later change to
/// "other keys are ignored" is a deliberate one.
#[test]
fn an_unknown_dispatch_key_is_ignored_which_is_why_the_mode_lives_in_fleet() {
    let text =
        ROSTER.replacen(PRODUCT, "dir: product-src\n    fleet: true\n    dispatch: false\n", 1);
    let r = parse(&text, &home()).unwrap();
    assert!(!r.desired()[1].maintain_only, "ignored: dispatched as fleet: true");
}

#[test]
fn flipping_the_mode_is_a_change_in_place_never_a_remove_and_add() {
    let into = ROSTER.replacen(PRODUCT, "dir: product-src\n    fleet: maintain\n", 1);
    let r = parse(&into, &home()).unwrap();
    let registered = vec![
        reg("/home/op/src/tooling", 10),
        reg("/home/op/src/product-src", DEFAULT_WORKSPACE_PRIORITY),
    ];
    let p = plan(&r, &registered, &ident, &|_| true);
    assert_eq!(
        p.changes,
        vec![Change::SetMaintainOnly {
            name: "product".to_string(),
            path: PathBuf::from("/home/op/src/product-src"),
            to: true,
        }]
    );
    assert_eq!(p.in_sync, 1);
    assert_eq!(describe(&p.changes[0]).split_whitespace().next(), Some("~"));
    assert!(describe(&p.changes[0]).contains("(dispatch -> maintain-only)"));

    // In sync once the registry has it, whoever set it.
    let synced = vec![
        reg("/home/op/src/tooling", 10),
        maintain("/home/op/src/product-src", DEFAULT_WORKSPACE_PRIORITY),
    ];
    assert!(plan(&r, &synced, &ident, &|_| true).is_in_sync());

    // And back out: the store says `fleet: true` again.
    let back = parse(ROSTER, &home()).unwrap();
    let p = plan(&back, &synced, &ident, &|_| true);
    assert_eq!(
        p.changes,
        vec![Change::SetMaintainOnly {
            name: "product".to_string(),
            path: PathBuf::from("/home/op/src/product-src"),
            to: false,
        }]
    );
    assert!(describe(&p.changes[0]).contains("(maintain-only -> dispatch)"));

    // A mode and a priority change together: both, the mode first.
    let p = plan(&r, &[reg("/home/op/src/product-src", 5)], &ident, &|_| true);
    assert!(matches!(p.changes[0], Change::SetMaintainOnly { to: true, .. }));
    assert!(matches!(p.changes[1], Change::Add { .. }), "tooling");
    assert!(matches!(p.changes[2], Change::SetPriority { from: 5, .. }));
}

#[test]
fn a_maintain_only_add_registers_it_maintain_only_in_one_change() {
    let text = ROSTER.replacen(PRODUCT, "dir: product-src\n    fleet: maintain\n", 1);
    let r = parse(&text, &home()).unwrap();
    let p = plan(&r, &[reg("/home/op/src/tooling", 10)], &ident, &|_| true);
    assert_eq!(
        p.changes,
        vec![Change::Add {
            name: "product".to_string(),
            path: PathBuf::from("/home/op/src/product-src"),
            priority: DEFAULT_WORKSPACE_PRIORITY,
            maintain_only: true,
        }]
    );
    assert!(describe(&p.changes[0]).contains("maintain-only"));
    let json = serde_json::to_value(&p.changes[0]).unwrap();
    assert_eq!(json["maintain_only"], true);
    // A normal add serializes exactly as before.
    let normal = parse(ROSTER, &home()).unwrap();
    let p = plan(&normal, &[reg("/home/op/src/tooling", 10)], &ident, &|_| true);
    assert!(serde_json::to_value(&p.changes[0])
        .unwrap()
        .get("maintain_only")
        .is_none());
}
