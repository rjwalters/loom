#![allow(clippy::unwrap_used)]

use super::*;
use serde_json::json;
use std::collections::BTreeSet;

fn example() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/forge-egress/policy.example.json"))
        .unwrap()
}

fn codes(findings: &[Finding]) -> BTreeSet<&'static str> {
    findings.iter().map(|f| f.code).collect()
}

fn write(dir: &Path, name: &str, body: &Value) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body.to_string()).unwrap();
    p
}

#[test]
fn schema_stays_inside_the_supported_keyword_subset() {
    fn walk(node: &Value, under_properties: bool, at: &str, bad: &mut Vec<String>) {
        let Value::Object(map) = node else { return };
        for (k, v) in map {
            if under_properties {
                walk(v, false, &format!("{at}.{k}"), bad);
                continue;
            }
            if !SUPPORTED_KEYWORDS.contains(&k.as_str()) && !k.starts_with('$') {
                bad.push(format!("{at}.{k}"));
                continue;
            }
            if k == "properties" {
                walk(v, true, &format!("{at}.{k}"), bad);
            } else if k == "items" {
                walk(v, false, &format!("{at}.{k}"), bad);
            }
        }
    }
    let mut bad = Vec::new();
    walk(&schema(), false, "$", &mut bad);
    assert!(bad.is_empty(), "unsupported schema keywords: {bad:?}");
    assert_eq!(schema()["properties"]["schemaVersion"]["const"], 1);
}

#[test]
fn the_example_policy_validates_with_no_shape_findings() {
    assert!(schema_errors(&example(), &schema(), "$").is_empty());
    assert!(assert_policy_shape(&example()).is_empty());
}

#[test]
fn the_subset_validator_actually_rejects() {
    type Mutation = Box<dyn Fn(&mut Value)>;
    let cases: Vec<(&str, Mutation)> = vec![
        ("unexpected property", Box::new(|p| p["surprise"] = json!(1))),
        (
            "missing required",
            Box::new(|p| {
                p.as_object_mut().unwrap().remove("principal");
            }),
        ),
        ("bad enum", Box::new(|p| p["enforcement"]["api"] = json!("optional"))),
        (
            "bad pattern",
            Box::new(|p| p["github"]["apiOrigin"] = json!("http://insecure.example")),
        ),
        ("wrong type", Box::new(|p| p["policyEpoch"] = json!("one"))),
        ("bad nested enum", Box::new(|p| p["principal"]["profile"] = json!("root"))),
        ("non-absolute path", Box::new(|p| p["toolchain"]["launcherPath"] = json!("gh"))),
        ("below minimum", Box::new(|p| p["policyEpoch"] = json!(-1))),
        (
            "too many items",
            Box::new(|p| p["github"]["allowedDirectEgress"] = json!(["a", "b", "c", "d", "e"])),
        ),
    ];
    for (label, mutate) in cases {
        let mut p = example();
        mutate(&mut p);
        assert!(!schema_errors(&p, &schema(), "$").is_empty(), "did not reject: {label}");
        assert!(codes(&assert_policy_shape(&p)).contains("policy.schema"), "{label}");
    }
}

#[test]
fn an_unknown_schema_version_is_incomplete_never_aligned() {
    for bad in [json!(99), json!(0), json!("1"), Value::Null] {
        let mut p = example();
        p["schemaVersion"] = bad.clone();
        let f = assert_policy_shape(&p);
        assert_eq!(codes(&f), BTreeSet::from(["policy.schema-version"]), "{bad}");
        assert_eq!(super::super::report::exit_code(&f), 2, "{bad}");
    }
}

#[test]
fn inline_secret_and_origin_rules() {
    let token = format!("ghp_{}", "A".repeat(36));
    let mut p = example();
    p["principal"]["credentialRef"] = json!(token);
    let f = assert_policy_shape(&p);
    assert!(codes(&f).contains("policy.inline-secret"));
    let blob: String = f.iter().map(|x| x.to_json().to_string()).collect();
    assert!(!blob.contains(&token), "{blob}");

    let mut p = example();
    p["github"]["apiOrigin"] = json!("https://github-proxy.2amlogic.com:8443");
    assert!(codes(&assert_policy_shape(&p)).contains("policy.api-origin-port"));
    let mut p = example();
    p["github"]["apiOrigin"] = json!("https://github.com");
    assert!(codes(&assert_policy_shape(&p)).contains("policy.origin-equals-logical-host"));
    assert_eq!(expected_api_host(&example()), "github-proxy.2amlogic.com");
}

#[test]
fn resolution_precedence_env_then_machine_then_repo_then_none() {
    let dir = tempfile::tempdir().unwrap();
    let mut env_p = example();
    env_p["deployment"] = json!("env");
    let mut machine_p = example();
    machine_p["deployment"] = json!("machine");
    let mut repo_p = example();
    repo_p["deployment"] = json!("repo");
    let env_path = write(dir.path(), "env.json", &env_p);
    let machine_path = write(dir.path(), "machine.json", &machine_p);
    let repo_path = write(dir.path(), "repo.json", &repo_p);
    let all = PolicySources {
        env_path: Some(env_path.clone()),
        machine_path: Some(machine_path.clone()),
        repo_path: Some(repo_path.clone()),
    };
    let deployment = |s: &PolicySources| match resolve(s) {
        Resolution::Loaded(d) => {
            (d.origin, d.data["deployment"].as_str().unwrap().to_string(), d.ignored.len())
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(deployment(&all), (Origin::Env, "env".into(), 2));
    let no_env = PolicySources {
        env_path: None,
        ..all.clone()
    };
    assert_eq!(deployment(&no_env), (Origin::Machine, "machine".into(), 1));
    let repo_only = PolicySources {
        env_path: None,
        machine_path: Some(dir.path().join("absent.json")),
        ..all.clone()
    };
    assert_eq!(deployment(&repo_only), (Origin::Repo, "repo".into(), 0));
    let none = PolicySources {
        env_path: None,
        machine_path: Some(dir.path().join("absent.json")),
        repo_path: None,
    };
    assert!(matches!(resolve(&none), Resolution::Unconfigured));
}

#[test]
fn machine_wins_and_the_repo_policy_is_reported_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let mut weaker = example();
    weaker["enforcement"]["api"] = json!("observe");
    let mut machine = example();
    machine["enforcement"]["api"] = json!("required");
    let s = PolicySources {
        env_path: None,
        machine_path: Some(write(dir.path(), "machine.json", &machine)),
        repo_path: Some(write(dir.path(), "repo.json", &weaker)),
    };
    let Resolution::Loaded(doc) = resolve(&s) else {
        panic!()
    };
    assert_eq!(doc.origin, Origin::Machine);
    assert!(!is_observe_only(&doc.data), "a repo policy can never weaken the machine one");
    assert_eq!(
        doc.ignored,
        vec![Candidate {
            origin: Origin::Repo,
            path: dir.path().join("repo.json")
        }]
    );
}

#[cfg(unix)]
#[test]
fn a_machine_policy_that_cannot_be_stated_is_unreadable_not_absent() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let machine_path = write(&locked, "policy.json", &example());
    let mut weaker = example();
    weaker["enforcement"]["api"] = json!("observe");
    let repo_path = write(dir.path(), "repo.json", &weaker);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root (or CAP_DAC_OVERRIDE) bypasses the denial: the precondition does
    // not reproduce, so skip loudly rather than pass vacuously.
    let denied = std::fs::symlink_metadata(&machine_path)
        .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
    let resolution = denied.then(|| {
        resolve(&PolicySources {
            env_path: None,
            machine_path: Some(machine_path.clone()),
            repo_path: Some(repo_path.clone()),
        })
    });
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Some(resolution) = resolution else {
        eprintln!("skipping: permission denial does not reproduce (running as root?)");
        return;
    };
    let Resolution::Unreadable {
        candidate, ignored, ..
    } = resolution
    else {
        panic!("a denied machine policy must resolve Unreadable, got {resolution:?}")
    };
    assert_eq!(candidate.origin, Origin::Machine);
    assert_eq!(
        ignored,
        vec![Candidate {
            origin: Origin::Repo,
            path: repo_path
        }],
        "the repo policy must never win over a present-but-unreadable machine policy"
    );
}

#[test]
fn an_unreadable_winner_never_falls_through() {
    let dir = tempfile::tempdir().unwrap();
    let broken = dir.path().join("broken.json");
    std::fs::write(&broken, "{not json").unwrap();
    let s = PolicySources {
        env_path: Some(broken),
        machine_path: None,
        repo_path: Some(write(dir.path(), "repo.json", &example())),
    };
    let Resolution::Unreadable {
        candidate,
        error,
        ignored,
    } = resolve(&s)
    else {
        panic!()
    };
    assert_eq!(candidate.origin, Origin::Env);
    assert!(error.starts_with("JSONDecodeError"), "{error}");
    assert_eq!(ignored.len(), 1);
    let missing = PolicySources {
        env_path: Some(dir.path().join("nope.json")),
        ..PolicySources::default()
    };
    assert!(matches!(resolve(&missing), Resolution::Unreadable { .. }));
}

#[test]
fn repo_policy_path_reads_forge_egress_policy_path_relative_to_the_root() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(repo_policy_path(root.path()), None);
    std::fs::create_dir_all(root.path().join(".loom")).unwrap();
    std::fs::write(
        root.path().join(".loom/config.json"),
        json!({"forge": {"egress": {"policyPath": "ops/egress.json"}}}).to_string(),
    )
    .unwrap();
    assert_eq!(repo_policy_path(root.path()), Some(root.path().join("ops/egress.json")));
}
