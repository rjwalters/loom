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
            },
            Desired {
                name: "product".to_string(),
                path: PathBuf::from("/home/op/src/product-src"),
                priority: DEFAULT_WORKSPACE_PRIORITY,
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
