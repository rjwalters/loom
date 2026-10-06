#![allow(clippy::unwrap_used)]
use super::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

/// The vendored example policy: schema-valid, so only the field a test
/// changes can produce a finding.
fn example() -> serde_json::Value {
    serde_json::from_str(include_str!("../../tests/fixtures/forge-egress/policy.example.json"))
        .unwrap()
}

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
        let mut doc = example();
        doc["toolchain"]["launcherPath"] = json!(launcher.display().to_string());
        doc["toolchain"]["upstreamGhPath"] = json!(upstream.display().to_string());
        doc["principal"]["credentialRef"] = json!(format!("file:{}", cred.display()));
        doc["enforcement"]["api"] = json!(api);
        let policy = dir.path().join("policy.json");
        std::fs::write(&policy, doc.to_string()).unwrap();
        let sources = PolicySources {
            env_path: Some(policy),
            machine_path: None,
            repo_path: None,
            ..PolicySources::default()
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
        ..PolicySources::default()
    };
    assert!(WorkerEgress::from_sources(&none).is_none());
    // Production resolution is off in a unit-test build.
    assert!(WorkerEgress::from_process().is_none());
    let (fx, _) = Fx::new("required");
    let repo = PolicySources {
        env_path: None,
        machine_path: None,
        repo_path: Some(fx.dir.path().join("policy.json")),
        ..PolicySources::default()
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

fn env_sources(policy: std::path::PathBuf) -> PolicySources {
    PolicySources {
        env_path: Some(policy),
        machine_path: None,
        repo_path: None,
        ..PolicySources::default()
    }
}

#[test]
fn unreadable_or_invalid_policy_is_a_named_refusal_not_none() {
    let dir = tempfile::tempdir().unwrap();
    let missing = env_sources(dir.path().join("absent.json"));
    let f = WorkerEgress::try_from_sources(&missing).unwrap_err();
    assert_eq!(f.code, "policy.unreadable");
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{not json").unwrap();
    let f = WorkerEgress::try_from_sources(&env_sources(bad)).unwrap_err();
    assert_eq!(f.code, "policy.unreadable");
}

#[test]
fn absent_policy_is_ok_none() {
    let none = PolicySources {
        env_path: None,
        machine_path: None,
        repo_path: None,
        ..PolicySources::default()
    };
    assert_eq!(WorkerEgress::try_from_sources(&none), Ok(None));
}

/// `example()` with `edit` applied, written as the env-tier policy.
fn admit_with(edit: impl FnOnce(&mut serde_json::Value)) -> Result<Admission, Box<Finding>> {
    let dir = tempfile::tempdir().unwrap();
    let policy = dir.path().join("policy.json");
    let mut doc = example();
    doc["toolchain"]["launcherPath"] = json!(dir.path().join("nope/gh").display().to_string());
    edit(&mut doc);
    std::fs::write(&policy, doc.to_string()).unwrap();
    WorkerEgress::admit(&env_sources(policy))
}

#[test]
fn missing_launcher_refuses_under_required_and_is_logged_under_observe() {
    let f = admit_with(|d| d["enforcement"]["api"] = json!("required")).unwrap_err();
    assert_eq!(f.code, "toolchain.launcher-missing");
    let a = admit_with(|d| d["enforcement"]["api"] = json!("observe")).unwrap();
    assert!(a.configured && a.egress.is_none());
    assert_eq!(a.status(), "observe-unmanaged");
    assert_eq!(a.warnings[0].code, "toolchain.launcher-missing");
    assert!(observe_message(&a.warnings[0]).contains("spawn proceeds"));
}

#[test]
fn required_policy_without_launcher_path_refuses_never_none() {
    // Absent: schema `required`; the configured policy must not read as none.
    let f = admit_with(|d| {
        d["enforcement"]["api"] = json!("required");
        d["toolchain"]
            .as_object_mut()
            .unwrap()
            .remove("launcherPath");
    })
    .unwrap_err();
    assert_eq!(f.code, "policy.schema");
    assert!(f.observed.contains("launcherPath"), "{f:?}");
    // Empty: fails the schema's `^/` pattern.
    let f = admit_with(|d| {
        d["enforcement"]["api"] = json!("required");
        d["toolchain"]["launcherPath"] = json!("");
    })
    .unwrap_err();
    assert_eq!(f.code, "policy.schema");
    assert!(f.observed.contains("launcherPath"), "{f:?}");
}

#[test]
fn unsupported_schema_version_refuses_even_when_it_says_observe() {
    for api in ["required", "observe"] {
        let f = admit_with(|d| {
            d["schemaVersion"] = json!(2);
            d["enforcement"]["api"] = json!(api);
        })
        .unwrap_err();
        assert_eq!(f.code, "policy.schema-version", "{api}");
        assert!(refusal_message(&f).contains("policy.schema-version"));
    }
}

#[test]
fn missing_or_unknown_enforcement_fails_closed_as_required() {
    let f = admit_with(|d| {
        d["enforcement"].as_object_mut().unwrap().remove("api");
    })
    .unwrap_err();
    assert!(f.code.starts_with("policy.schema"), "{f:?}");
}

#[test]
fn observe_logs_an_invalid_policy_and_proceeds_without_a_launcher() {
    let a = admit_with(|d| {
        d["enforcement"]["api"] = json!("observe");
        d["toolchain"]["launcherPath"] = json!("");
    })
    .unwrap();
    assert_eq!(a.status(), "observe-unmanaged");
    let codes: Vec<_> = a.warnings.iter().map(|f| f.code).collect();
    assert_eq!(codes, ["policy.schema", "toolchain.launcher-missing"]);
}

#[test]
fn admission_status_is_explicit_for_every_outcome() {
    let none = PolicySources::default();
    assert_eq!(WorkerEgress::admit(&none).unwrap().status(), "unconfigured");
    let (fx, _) = Fx::new("required");
    let a = WorkerEgress::admit(&env_sources(fx.dir.path().join("policy.json"))).unwrap();
    assert_eq!(a.status(), "managed");
    assert!(a.warnings.is_empty(), "{:?}", a.warnings);
    assert!(a.egress.unwrap().required);
}

#[test]
fn legacy_args_only_without_a_managed_launcher() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/gh")).unwrap();
    let h = home.path().to_path_buf();
    let lookup = |vars: &'static [(&'static str, &'static str)]| {
        let h = h.clone();
        move |k: &str| -> Option<std::ffi::OsString> {
            if k == "HOME" {
                return Some(h.clone().into_os_string());
            }
            vars.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).into())
        }
    };
    let mount = format!("{0}:{0}:ro", h.join(".config/gh").display());
    // No token: the read-only gh config mount, no -e.
    assert_eq!(legacy_docker_args(lookup(&[])), ["-v", mount.as_str()]);
    // A token is forwarded by NAME only, and replaces the mount.
    let args = legacy_docker_args(lookup(&[("GH_TOKEN", "ghp_x"), ("GITHUB_TOKEN", "")]));
    assert_eq!(args, ["-e", "GH_TOKEN", "-e", "GITHUB_TOKEN"]);
    // An admission with a launcher never carries any of it.
    let (_fx, egress) = Fx::new("required");
    let managed = Admission {
        configured: true,
        egress: Some(egress),
        warnings: vec![],
    };
    let args = managed.docker_args(lookup(&[("GH_TOKEN", "ghp_x")]));
    assert!(
        !args
            .iter()
            .any(|a| a.contains("GH_TOKEN") || a.contains(".config/gh")),
        "{args:?}"
    );
    assert_eq!(Admission::default().docker_args(lookup(&[])), ["-v", mount.as_str()]);
}

#[test]
fn container_launcher_must_resolve_to_the_launcher() {
    let (_fx, egress) = Fx::new("required");
    let launcher = egress.launcher.display().to_string();
    assert!(container_launcher_finding(&egress, "img", Some(&launcher)).is_none());
    assert!(container_launcher_finding(&egress, "img", None).is_none());
    let f = container_launcher_finding(&egress, "img", Some("/usr/bin/gh")).unwrap();
    assert_eq!(f.code, "toolchain.launcher-not-first");
    let f = container_launcher_finding(&egress, "img", Some("")).unwrap();
    assert_eq!(f.observed, "(none on PATH)");
    let (_fx, observe) = Fx::new("observe");
    assert!(container_launcher_finding(&observe, "img", Some("/usr/bin/gh")).is_none());
}
