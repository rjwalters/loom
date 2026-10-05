#![allow(clippy::unwrap_used)]
use super::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

struct Fx {
    dir: tempfile::TempDir,
}

impl Fx {
    fn new(api: &str) -> (Self, WorkerEgress) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("managed");
        std::fs::create_dir(&bin).unwrap();
        let launcher = bin.join("gh");
        std::fs::write(&launcher, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cred = dir.path().join("proxy.key");
        std::fs::write(&cred, "k").unwrap();
        let upstream = dir.path().join("upstream-gh");
        std::fs::write(&upstream, "x").unwrap();
        let doc = json!({
            "schemaVersion": 1,
            "toolchain": {
                "launcherPath": launcher.display().to_string(),
                "upstreamGhPath": upstream.display().to_string(),
            },
            "principal": {"credentialRef": format!("file:{}", cred.display())},
            "enforcement": {"api": api},
        });
        let policy = dir.path().join("policy.json");
        std::fs::write(&policy, doc.to_string()).unwrap();
        let sources = PolicySources {
            env_path: Some(policy),
            machine_path: None,
            repo_path: None,
        };
        let egress = WorkerEgress::from_sources(&sources).unwrap();
        (Self { dir }, egress)
    }
}

#[test]
fn no_policy_or_repo_policy_is_none() {
    let none = PolicySources {
        env_path: None,
        machine_path: None,
        repo_path: None,
    };
    assert!(WorkerEgress::from_sources(&none).is_none());
    // Production resolution is off in a unit-test build.
    assert!(WorkerEgress::from_process().is_none());
    let (fx, _) = Fx::new("required");
    let repo = PolicySources {
        env_path: None,
        machine_path: None,
        repo_path: Some(fx.dir.path().join("policy.json")),
    };
    assert!(WorkerEgress::from_sources(&repo).is_none());
}

#[test]
fn worker_path_puts_launcher_dir_first_once() {
    let (fx, e) = Fx::new("required");
    let bin = fx.dir.path().join("managed");
    let current = std::env::join_paths(["/usr/bin", bin.to_str().unwrap(), "/bin"]).unwrap();
    let path = e.worker_path(Some(&current)).unwrap();
    let dirs: Vec<_> = std::env::split_paths(&path).collect();
    assert_eq!(dirs[0], bin);
    assert_eq!(dirs.iter().filter(|d| **d == bin).count(), 1);
    assert!(e.launcher_first_finding(Some(&path)).is_none());
}

#[test]
fn required_refuses_a_worker_whose_first_gh_is_not_the_launcher() {
    let (fx, e) = Fx::new("required");
    let other = fx.dir.path().join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("gh"), "x").unwrap();
    let path = std::env::join_paths([other.as_path()]).unwrap();
    let f = e.launcher_first_finding(Some(&path)).unwrap();
    assert_eq!(f.code, "toolchain.launcher-not-first");
    assert!(refusal_message(&f).contains("toolchain.launcher-not-first"));
    assert!(e.launcher_first_finding(None).is_some());
}

#[test]
fn observe_never_refuses() {
    let (_fx, e) = Fx::new("observe");
    assert!(e
        .launcher_first_finding(Some(OsStr::new("/nonexistent")))
        .is_none());
}

#[test]
fn container_carries_policy_read_only_and_names_it_in_env() {
    let (fx, e) = Fx::new("required");
    let mounts = e.container_mounts();
    // launcher dir, upstream gh, policy file, credential file — all ro, same path.
    assert_eq!(mounts.len(), 4, "{mounts:?}");
    assert!(mounts
        .iter()
        .all(|(h, c, ro)| *ro && h.display().to_string() == *c));
    assert!(mounts
        .iter()
        .any(|(h, _, _)| *h == fx.dir.path().join("managed")));
    assert!(mounts
        .iter()
        .any(|(h, _, _)| *h == fx.dir.path().join("proxy.key")));
    let env = e.container_env();
    let p = fx.dir.path().join("policy.json").display().to_string();
    assert!(env.contains(&("LOOM_FORGE_EGRESS_POLICY", p.clone())));
    assert!(env.contains(&("GITHUB_EGRESS_POLICY", p)));
}

#[test]
fn python3_finding_is_named() {
    let f = python3_missing_finding("img:1");
    assert_eq!(f.code, "toolchain.launcher-python3-missing");
    assert!(f.source.contains("img:1"));
}
