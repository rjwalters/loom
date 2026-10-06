//! Issue #9979: the container is Codex's only boundary, so the gate that
//! decides to drop Codex's sandbox is pinned here, property by property.

use serde_json::{json, Value};

use super::*;

fn hardened_host() -> Value {
    json!({
        "State": {"Running": true},
        "Config": {"Labels": {"loom.session-posture": "container-boundary-v1"}},
        "HostConfig": {
            "Privileged": false,
            "NetworkMode": "bridge",
            "PidMode": "",
            "IpcMode": "private",
            "UTSMode": "",
            "UsernsMode": "",
            "Devices": [],
            "CapDrop": ["ALL"],
            "CapAdd": null,
            "SecurityOpt": ["no-new-privileges"]
        },
        "Mounts": [
            {"Source": "/p", "Destination": "/home/loom/.codex-profile"},
            {"Source": "/w/loom", "Destination": "/w/loom"},
            {"Source": "/d/.loom/gh-config", "Destination": "/d/.loom/gh-config"},
            {"Type": "bind", "Source": "/p/hooks.json", "Destination": "/home/loom/.codex-profile/hooks.json", "RW": false},
            {"Type": "bind", "Source": "/p/config.toml", "Destination": "/home/loom/.codex-profile/config.toml", "RW": false},
            {"Type": "bind", "Source": "/p/loom-codex-hooks.json", "Destination": "/home/loom/.codex-profile/loom-codex-hooks.json", "RW": false}
        ]
    })
}

/// Index of `name`'s read-only bind in [`hardened_host`]'s mounts.
fn control_index(name: &str) -> usize {
    3 + PROFILE_CONTROLS.iter().position(|n| *n == name).unwrap()
}

/// `decide` with a container whose view of the profile controls matches the
/// host's.
fn judge(state: Option<&Value>, env: &Env) -> Decision {
    decide(&args(), state, env, || Ok(Vec::new()))
}

fn private_clone() -> Value {
    let mut state = hardened_host();
    state["Config"]["Labels"] = json!({"loom.workspace-mode": "private-clone"});
    state["HostConfig"]["NetworkMode"] = json!("none");
    state
}

fn args() -> PostureArgs {
    PostureArgs {
        container: "loom-codex-session-acct".into(),
        profile: "acct".into(),
        requested: "workspace-write".into(),
        codex_home: None,
        docker: "docker".into(),
    }
}

#[test]
fn hardened_containers_drop_the_sandbox() {
    for (state, mode) in [
        (hardened_host(), "host"),
        (private_clone(), "private-clone"),
    ] {
        let d = judge(Some(&state), &Env::default());
        assert_eq!((d.code, d.mode.as_str(), d.sandbox.as_str()), (0, mode, "danger-full-access"));
        assert!(d.messages[0].contains(&format!("posture={mode}")), "{:?}", d.messages);
    }
}

#[test]
fn an_unlabelled_running_container_is_refused() {
    let mut state = hardened_host();
    state["Config"]["Labels"] = json!({});
    let d = judge(Some(&state), &Env::default());
    assert_eq!(d.code, 78);
    let all = d.messages.join("\n");
    assert!(all.contains("created before the container-boundary hardening"), "{all}");
    assert!(all.contains("loom-daemon accounts session stop acct"), "{all}");
    // A spoofed or wrong posture value is not the posture.
    state["Config"]["Labels"] = json!({"loom.session-posture": "container-boundary-v0"});
    assert_eq!(judge(Some(&state), &Env::default()).code, 78);
}

#[test]
fn a_label_without_the_hardening_is_refused_for_every_property() {
    let cases: Vec<(&str, Value, &str)> = vec![
        ("/HostConfig/Privileged", json!(true), "privileged=true"),
        ("/HostConfig/Privileged", Value::Null, "privileged=null"),
        ("/HostConfig/NetworkMode", json!("host"), "host-namespace(network)"),
        ("/HostConfig/PidMode", json!("host"), "host-namespace(pid)"),
        ("/HostConfig/IpcMode", json!("host"), "host-namespace(ipc)"),
        ("/HostConfig/UTSMode", json!("host"), "host-namespace(uts)"),
        ("/HostConfig/UsernsMode", json!("host"), "host-namespace(userns)"),
        ("/HostConfig/CapDrop", json!([]), "cap-drop-ALL-missing"),
        ("/HostConfig/CapDrop", Value::Null, "cap-drop-ALL-missing"),
        ("/HostConfig/CapAdd", json!(["SYS_ADMIN"]), "cap-add=SYS_ADMIN"),
        ("/HostConfig/SecurityOpt", json!([]), "no-new-privileges-missing"),
        (
            "/HostConfig/SecurityOpt",
            json!(["no-new-privileges", "seccomp=unconfined"]),
            "security-opt=seccomp=unconfined",
        ),
        (
            "/HostConfig/SecurityOpt",
            json!(["no-new-privileges", "apparmor=unconfined"]),
            "security-opt=apparmor=unconfined",
        ),
        ("/HostConfig/Devices", json!([{"PathOnHost": "/dev/kvm"}]), "devices"),
        (
            "/Mounts",
            json!([{"Source": "/var/run/docker.sock", "Destination": "/var/run/docker.sock"}]),
            "docker-socket-mounted",
        ),
    ];
    for base in [hardened_host(), private_clone()] {
        for (pointer, value, why) in &cases {
            let mut state = base.clone();
            *state
                .pointer_mut(pointer)
                .unwrap_or_else(|| panic!("{pointer}")) = value.clone();
            let d = judge(Some(&state), &Env::default());
            assert_eq!((d.code, d.mode.as_str()), (78, "posture-mismatch"), "{pointer}={value}");
            assert!(d.messages[0].contains(why), "{why}: {:?}", d.messages);
            assert_eq!(d.sandbox, "workspace-write", "never dropped for {why}");
        }
    }
}

#[test]
fn docker_spellings_of_the_hardening_are_accepted() {
    let mut state = hardened_host();
    state["HostConfig"]["SecurityOpt"] = json!(["no-new-privileges:true", "label=disable"]);
    state["HostConfig"]["CapDrop"] = json!(["all"]);
    assert_eq!(judge(Some(&state), &Env::default()).mode, "host");
}

#[test]
fn a_missing_stopped_or_unreadable_container_keeps_the_sandbox() {
    let mut stopped = hardened_host();
    stopped["State"]["Running"] = json!(false);
    let mut restarting = hardened_host();
    restarting["State"]["Restarting"] = json!(true);
    for state in [
        None,
        Some(Value::Null),
        Some(json!(false)),
        Some(stopped),
        Some(restarting),
    ] {
        let d = judge(state.as_ref(), &Env::default());
        assert_eq!(
            (d.code, d.mode.as_str(), d.sandbox.as_str()),
            (0, "not-running", "workspace-write")
        );
    }
}

#[test]
fn the_codex_escape_hatch_keeps_the_requested_sandbox_and_bad_values_refuse() {
    let env = Env {
        container_sandbox: Some("codex".into()),
        ..Env::default()
    };
    let d = judge(Some(&hardened_host()), &env);
    assert_eq!((d.code, d.sandbox.as_str()), (0, "workspace-write"));
    assert!(
        d.messages[0].contains("LOOM_CODEX_CONTAINER_SANDBOX=codex keeps sandbox=workspace-write")
    );
    let env = Env {
        container_sandbox: Some("bogus".into()),
        ..Env::default()
    };
    assert_eq!(judge(Some(&hardened_host()), &env).code, 78);
}

#[test]
fn gh_config_dir_is_forwarded_only_where_it_is_mounted() {
    let env = |dir: &str| Env {
        gh_config_dir: Some(dir.into()),
        ..Env::default()
    };
    let host = hardened_host();
    assert!(judge(Some(&host), &env("/d/.loom/gh-config")).forward_gh);
    assert!(!judge(Some(&host), &env("/d/.loom/gh-config-by-owner/o")).forward_gh);
    let d = judge(Some(&host), &env("/elsewhere/gh"));
    assert!(!d.forward_gh);
    assert!(d
        .messages
        .iter()
        .any(|m| m.contains("gh inside the session will be unauthenticated")));
    // Component-wise: `/w/loomX` is not inside the `/w/loom` mount.
    assert!(!judge(Some(&host), &env("/w/loomX/.loom/gh-config")).forward_gh);
    // A private clone carries its own; a leased launch never gets one.
    assert!(!judge(Some(&private_clone()), &env("/w/loom/.loom/gh-config")).forward_gh);
    let leased = Env {
        leased: true,
        ..env("/d/.loom/gh-config")
    };
    assert!(!judge(Some(&host), &leased).forward_gh);
    // No GH_CONFIG_DIR, nothing to forward.
    assert!(!judge(Some(&host), &Env::default()).forward_gh);
}

// ---------------------------------------------------------------------------
// The profile's hook-control files (issue #9979 follow-up): read-only binds,
// and in host mode the container's copy must be the host's.
// ---------------------------------------------------------------------------

#[test]
fn every_profile_control_must_be_a_read_only_bind() {
    for base in [hardened_host(), private_clone()] {
        for name in PROFILE_CONTROLS {
            let why = format!("profile-control-writable({name})");
            let index = control_index(name);
            let mut writable = base.clone();
            writable["Mounts"][index]["RW"] = json!(true);
            let mut volume = base.clone();
            volume["Mounts"][index]["Type"] = json!("volume");
            let mut elsewhere = base.clone();
            elsewhere["Mounts"][index]["Destination"] = json!(format!("/tmp/{name}"));
            let mut missing = base.clone();
            missing["Mounts"].as_array_mut().unwrap().remove(index);
            for (how, state) in [
                ("RW", writable),
                ("volume", volume),
                ("elsewhere", elsewhere),
                ("missing", missing),
            ] {
                let d = judge(Some(&state), &Env::default());
                assert_eq!((d.code, d.mode.as_str()), (78, "posture-mismatch"), "{name} {how}");
                assert!(d.messages[0].contains(&why), "{name} {how}: {:?}", d.messages);
                assert_eq!(d.sandbox, "workspace-write", "{name} {how}");
            }
        }
    }
}

#[test]
fn a_host_container_whose_controls_drifted_from_the_host_is_refused() {
    for drift in [
        Ok(vec!["hooks.json".to_string()]),
        Ok(vec![
            "config.toml".to_string(),
            "loom-codex-hooks.json".to_string(),
        ]),
        Err("no --codex-home was given".to_string()),
    ] {
        let expected = match &drift {
            Ok(names) => names.join(" "),
            Err(error) => format!("unverifiable ({error})"),
        };
        let d = decide(&args(), Some(&hardened_host()), &Env::default(), || drift);
        assert_eq!((d.code, d.sandbox.as_str()), (78, "workspace-write"), "{expected}");
        let all = d.messages.join("\n");
        assert!(all.contains(&expected), "{all}");
        assert!(all.contains("does not follow a host-side replace"), "{all}");
        assert!(all.contains("loom-daemon accounts session stop acct"), "{all}");
    }
}

#[test]
fn the_drift_probe_runs_only_for_a_hardened_host_container() {
    let never = || -> ControlDrift { panic!("drift probed") };
    assert_eq!(decide(&args(), Some(&private_clone()), &Env::default(), never).code, 0);
    let mut unlabelled = hardened_host();
    unlabelled["Config"]["Labels"] = json!({});
    assert_eq!(decide(&args(), Some(&unlabelled), &Env::default(), never).code, 78);
    assert_eq!(decide(&args(), None, &Env::default(), never).code, 0);
}

#[test]
fn drifted_compares_each_host_file_with_the_containers_hash() {
    let profile = tempfile::tempdir().unwrap();
    let mut seen = BTreeMap::new();
    for name in PROFILE_CONTROLS {
        std::fs::write(profile.path().join(name), format!("{name}-content")).unwrap();
        seen.insert(
            profile_control_destination(name),
            sha256_hex(format!("{name}-content").as_bytes()),
        );
    }
    assert!(drifted(profile.path(), &seen).is_empty());
    // A host-side replace the container did not follow.
    std::fs::write(profile.path().join("hooks.json"), "new registration").unwrap();
    assert_eq!(drifted(profile.path(), &seen), vec!["hooks.json"]);
    // Missing inside the container (Docker Desktop after a host rename).
    std::fs::write(profile.path().join("hooks.json"), "hooks.json-content").unwrap();
    seen.remove(&profile_control_destination("config.toml"));
    assert_eq!(drifted(profile.path(), &seen), vec!["config.toml"]);
    // Missing on the host.
    std::fs::remove_file(profile.path().join("config.toml")).unwrap();
    assert_eq!(drifted(profile.path(), &BTreeMap::new()).len(), 3);
}

#[test]
fn sha256sum_output_parses_by_path() {
    let parsed = parse_sha256sum(
        "ABC  /home/loom/.codex-profile/hooks.json\ndef */home/loom/.codex-profile/config.toml\nsha256sum: x: No such file\n",
    );
    assert_eq!(
        parsed
            .get("/home/loom/.codex-profile/hooks.json")
            .map(String::as_str),
        Some("abc")
    );
    assert_eq!(
        parsed
            .get("/home/loom/.codex-profile/config.toml")
            .map(String::as_str),
        Some("def")
    );
    assert_eq!(parsed.len(), 2);
}
