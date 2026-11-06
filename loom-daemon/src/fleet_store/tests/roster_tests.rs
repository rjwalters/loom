use crate::workspace_registry::DEFAULT_WORKSPACE_WEIGHT;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::*;

/// The roster fixture, as the top level of a compiled `fleet.json`.
fn roster_doc() -> serde_json::Value {
    json!({
        "root": "~/src",
        "repos": [
            {"name": "tooling", "remote": "git@example.com:acme/tooling.git", "dir": "tooling",
             "visibility": "public", "fleet": true, "weight": 10, "fleet_priority": 3,
             "purpose": "Build tooling", "consumes": ["lib-a", "lib-b"]},
            {"name": "product", "remote": "git@example.com:acme/product.git",
             "dir": "product-src", "fleet": true, "weight": 1},
            {"name": "secrets", "remote": "git@example.com:acme/secrets.git", "dir": "secrets",
             "firewall": true, "fleet": false},
            {"name": "docs", "remote": null, "dir": "docs", "fleet": false}
        ]
    })
}

/// `roster_doc()` with `edit` applied to its `repos[idx]` record.
fn with_repo(idx: usize, edit: impl FnOnce(&mut serde_json::Value)) -> serde_json::Value {
    let mut doc = roster_doc();
    edit(&mut doc["repos"][idx]);
    doc
}

/// Validate a roster document (the compiled `fleet.json` top level).
fn parse(doc: &serde_json::Value, home: &Path) -> anyhow::Result<Roster> {
    from_compiled(
        doc.as_object()
            .ok_or_else(|| anyhow::anyhow!("not a mapping"))?,
        home,
    )
}

fn home() -> PathBuf {
    PathBuf::from("/home/op")
}

fn ident(p: &Path) -> PathBuf {
    p.to_path_buf()
}

fn reg(path: &str, weight: u32) -> Registered {
    Registered {
        root: PathBuf::from(path),
        weight,
        maintain_only: false,
    }
}

#[test]
fn parses_the_contract_and_ignores_other_keys() {
    let r = parse(&roster_doc(), &home()).unwrap();
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
            weight: Some(10),
        }
    );
    assert_eq!(r.records[3].remote, None);
}

#[test]
fn desired_is_fleet_true_and_not_firewall_with_its_weight() {
    let r = parse(&roster_doc(), &home()).unwrap();
    let d = r.desired();
    assert_eq!(
        d,
        vec![
            Desired {
                name: "tooling".to_string(),
                path: PathBuf::from("/home/op/src/tooling"),
                weight: 10,
                maintain_only: false,
            },
            Desired {
                name: "product".to_string(),
                path: PathBuf::from("/home/op/src/product-src"),
                weight: DEFAULT_WORKSPACE_WEIGHT,
                maintain_only: false,
            },
        ]
    );
}

#[test]
fn fleet_and_firewall_together_is_a_hard_error_not_a_silent_exclusion() {
    let doc = with_repo(2, |r| r["fleet"] = json!(true));
    let err = parse(&doc, &home()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("`secrets`") && msg.contains("fleet: true AND firewall: true"),
        "{msg}"
    );
}

#[test]
fn malformed_flags_and_weights_are_hard_errors() {
    type Edit = fn(&mut serde_json::Value);
    let cases: [(&str, Edit); 10] = [
        ("weight: high", |d| d["repos"][0]["weight"] = json!("high")),
        ("weight: -1", |d| d["repos"][0]["weight"] = json!(-1)),
        ("weight: 1.5", |d| d["repos"][0]["weight"] = json!(1.5)),
        // #11742: a fleet record with no weight is refused, with no default.
        ("no weight", |d| {
            d["repos"][1].as_object_mut().unwrap().remove("weight");
        }),
        ("firewall: \"yes\"", |d| d["repos"][2]["firewall"] = json!("yes")),
        ("firewall: 1", |d| d["repos"][2]["firewall"] = json!(1)),
        ("dir: ../docs", |d| d["repos"][3]["dir"] = json!("../docs")),
        ("dir: tooling twice", |d| d["repos"][1]["dir"] = json!("tooling")),
        ("name: tooling twice", |d| d["repos"][3]["name"] = json!("tooling")),
        ("root: relative/src", |d| d["root"] = json!("relative/src")),
    ];
    for (what, edit) in cases {
        let mut doc = roster_doc();
        edit(&mut doc);
        assert!(parse(&doc, &home()).is_err(), "{what} must be refused");
    }
}

#[test]
fn a_malformed_roster_document_fails_closed() {
    assert!(parse(&json!({"root": "/x"}), &home()).is_err(), "missing repos");
    assert!(parse(&json!({"repos": []}), &home()).is_err(), "missing root");
    assert!(parse(&json!(["a", "b"]), &home()).is_err(), "not a mapping");
}

#[test]
fn plan_adds_removes_and_reprioritizes() {
    let r = parse(&roster_doc(), &home()).unwrap();
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
                weight: DEFAULT_WORKSPACE_WEIGHT,
                maintain_only: false,
            },
            Change::SetWeight {
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
    let r = parse(&roster_doc(), &home()).unwrap();
    let registered = vec![reg("/home/op/src/tooling", 10)];
    let plan = plan(&r, &registered, &ident, &|p| !p.ends_with("product-src"));
    assert_eq!(
        plan.changes,
        vec![Change::MissingClone {
            name: "product".to_string(),
            path: PathBuf::from("/home/op/src/product-src"),
            remote: Some("git@example.com:acme/product.git".to_string()),
            weight: DEFAULT_WORKSPACE_WEIGHT,
            maintain_only: false,
        }]
    );
    assert_eq!(plan.check_exit_code(), 1);
}

#[test]
fn a_matching_registry_is_in_sync() {
    let r = parse(&roster_doc(), &home()).unwrap();
    let registered = vec![
        reg("/home/op/src/tooling", 10),
        reg("/home/op/src/product-src", DEFAULT_WORKSPACE_WEIGHT),
    ];
    let plan = plan(&r, &registered, &ident, &|_| true);
    assert!(plan.is_in_sync());
    assert_eq!(plan.in_sync, 2);
    assert_eq!(plan.check_exit_code(), 0);
}

#[test]
fn paths_are_compared_after_normalization() {
    let r = parse(&roster_doc(), &home()).unwrap();
    // The registry stores canonical paths (e.g. a symlinked home resolved).
    let canon = |p: &Path| PathBuf::from(p.to_string_lossy().replace("/home/op", "/data/op"));
    let registered = vec![
        reg("/data/op/src/tooling", 10),
        reg("/data/op/src/product-src", DEFAULT_WORKSPACE_WEIGHT),
        reg("/data/op/src/secrets", 100),
    ];
    let plan = plan(&r, &registered, &canon, &|_| true);
    assert_eq!(plan.changes.len(), 1, "{:?}", plan.changes);
    assert!(matches!(&plan.changes[0], Change::Remove { name, .. } if name == "secrets"));
}

// ----------------------------------------------------------------------------
// #11186: `fleet: maintain`
// ----------------------------------------------------------------------------

/// `roster_doc()` with the `product` record's `fleet` set to `fleet`.
fn product_fleet(fleet: serde_json::Value) -> serde_json::Value {
    with_repo(1, |r| r["fleet"] = fleet)
}

fn maintain(path: &str, priority: u32) -> Registered {
    Registered {
        maintain_only: true,
        ..reg(path, priority)
    }
}

#[test]
fn fleet_maintain_is_desired_and_maintain_only() {
    let r = parse(&product_fleet(json!("maintain")), &home()).unwrap();
    assert!(r.records[1].fleet && r.records[1].maintain_only);
    assert!(!r.records[0].maintain_only);
    let d = r.desired();
    assert_eq!(d.len(), 2, "a maintain-only repo is registered and stays registered");
    assert!(d[1].maintain_only && !d[0].maintain_only);

    // The compiled `fleet.json` form reads the same.
    let doc = serde_json::json!({"root": "/r", "repos": [{"name": "x", "fleet": "maintain", "weight": 1}]});
    let r = from_compiled(doc.as_object().unwrap(), &home()).unwrap();
    assert!(r.desired()[0].maintain_only);
}

#[test]
fn fleet_must_be_true_false_or_maintain() {
    for bad in [json!("Maintain"), json!("yes"), json!(1), json!("dispatch")] {
        let msg = format!("{:#}", parse(&product_fleet(bad.clone()), &home()).unwrap_err());
        assert!(msg.contains("true, false or maintain"), "{bad}: {msg}");
    }
    // Maintain plus firewall is refused like fleet: true plus firewall.
    let doc = with_repo(2, |r| r["fleet"] = json!("maintain"));
    let msg = format!("{:#}", parse(&doc, &home()).unwrap_err());
    assert!(msg.contains("fleet: maintain AND firewall: true"), "{msg}");
}

/// A separate `dispatch: false` key would be ignored like every other unknown
/// key, so the repo would be registered and dispatched: the reason the mode
/// is a value of `fleet` (see the module docs). Pinned so a later change to
/// "other keys are ignored" is a deliberate one.
#[test]
fn an_unknown_dispatch_key_is_ignored_which_is_why_the_mode_lives_in_fleet() {
    let doc = with_repo(1, |r| r["dispatch"] = json!(false));
    let r = parse(&doc, &home()).unwrap();
    assert!(!r.desired()[1].maintain_only, "ignored: dispatched as fleet: true");
}

#[test]
fn flipping_the_mode_is_a_change_in_place_never_a_remove_and_add() {
    let r = parse(&product_fleet(json!("maintain")), &home()).unwrap();
    let registered = vec![
        reg("/home/op/src/tooling", 10),
        reg("/home/op/src/product-src", DEFAULT_WORKSPACE_WEIGHT),
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
        maintain("/home/op/src/product-src", DEFAULT_WORKSPACE_WEIGHT),
    ];
    assert!(plan(&r, &synced, &ident, &|_| true).is_in_sync());

    // And back out: the store says `fleet: true` again.
    let back = parse(&roster_doc(), &home()).unwrap();
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
    assert!(matches!(p.changes[2], Change::SetWeight { from: 5, .. }));
}

#[test]
fn a_maintain_only_add_registers_it_maintain_only_in_one_change() {
    let r = parse(&product_fleet(json!("maintain")), &home()).unwrap();
    let p = plan(&r, &[reg("/home/op/src/tooling", 10)], &ident, &|_| true);
    assert_eq!(
        p.changes,
        vec![Change::Add {
            name: "product".to_string(),
            path: PathBuf::from("/home/op/src/product-src"),
            weight: DEFAULT_WORKSPACE_WEIGHT,
            maintain_only: true,
        }]
    );
    assert!(describe(&p.changes[0]).contains("maintain-only"));
    let json = serde_json::to_value(&p.changes[0]).unwrap();
    assert_eq!(json["maintain_only"], true);
    // A normal add serializes exactly as before.
    let normal = parse(&roster_doc(), &home()).unwrap();
    let p = plan(&normal, &[reg("/home/op/src/tooling", 10)], &ident, &|_| true);
    assert!(serde_json::to_value(&p.changes[0])
        .unwrap()
        .get("maintain_only")
        .is_none());
}

/// #11742: a `fleet: maintain` record needs no weight (it is never
/// dispatched); it registers at weight 0.
#[test]
fn a_maintain_record_needs_no_weight() {
    let doc = with_repo(1, |r| {
        r["fleet"] = json!("maintain");
        r.as_object_mut().unwrap().remove("weight");
    });
    let r = parse(&doc, &home()).unwrap();
    let d = r.desired();
    assert!(d[1].maintain_only);
    assert_eq!(d[1].weight, 0);
}
