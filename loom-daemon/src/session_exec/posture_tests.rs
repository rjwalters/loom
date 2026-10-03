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
            {"Source": "/d/.loom/gh-config", "Destination": "/d/.loom/gh-config"}
        ]
    })
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
        docker: "docker".into(),
    }
}

#[test]
fn hardened_containers_drop_the_sandbox() {
    for (state, mode) in [
        (hardened_host(), "host"),
        (private_clone(), "private-clone"),
    ] {
        let d = decide(&args(), Some(&state), &Env::default());
        assert_eq!((d.code, d.mode.as_str(), d.sandbox.as_str()), (0, mode, "danger-full-access"));
        assert!(d.messages[0].contains(&format!("posture={mode}")), "{:?}", d.messages);
    }
}

#[test]
fn an_unlabelled_running_container_is_refused() {
    let mut state = hardened_host();
    state["Config"]["Labels"] = json!({});
    let d = decide(&args(), Some(&state), &Env::default());
    assert_eq!(d.code, 78);
    let all = d.messages.join("\n");
    assert!(all.contains("created before the container-boundary hardening"), "{all}");
    assert!(all.contains("loom-daemon accounts session stop acct"), "{all}");
    // A spoofed or wrong posture value is not the posture.
    state["Config"]["Labels"] = json!({"loom.session-posture": "container-boundary-v0"});
    assert_eq!(decide(&args(), Some(&state), &Env::default()).code, 78);
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
            let d = decide(&args(), Some(&state), &Env::default());
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
    assert_eq!(decide(&args(), Some(&state), &Env::default()).mode, "host");
}

#[test]
fn a_missing_stopped_or_unreadable_container_keeps_the_sandbox() {
    let mut stopped = hardened_host();
    stopped["State"]["Running"] = json!(false);
    for state in [None, Some(Value::Null), Some(json!(false)), Some(stopped)] {
        let d = decide(&args(), state.as_ref(), &Env::default());
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
    let d = decide(&args(), Some(&hardened_host()), &env);
    assert_eq!((d.code, d.sandbox.as_str()), (0, "workspace-write"));
    assert!(
        d.messages[0].contains("LOOM_CODEX_CONTAINER_SANDBOX=codex keeps sandbox=workspace-write")
    );
    let env = Env {
        container_sandbox: Some("bogus".into()),
        ..Env::default()
    };
    assert_eq!(decide(&args(), Some(&hardened_host()), &env).code, 78);
}

#[test]
fn gh_config_dir_is_forwarded_only_where_it_is_mounted() {
    let env = |dir: &str| Env {
        gh_config_dir: Some(dir.into()),
        ..Env::default()
    };
    let host = hardened_host();
    assert!(decide(&args(), Some(&host), &env("/d/.loom/gh-config")).forward_gh);
    assert!(!decide(&args(), Some(&host), &env("/d/.loom/gh-config-by-owner/o")).forward_gh);
    let d = decide(&args(), Some(&host), &env("/elsewhere/gh"));
    assert!(!d.forward_gh);
    assert!(d
        .messages
        .iter()
        .any(|m| m.contains("gh inside the session will be unauthenticated")));
    // Component-wise: `/w/loomX` is not inside the `/w/loom` mount.
    assert!(!decide(&args(), Some(&host), &env("/w/loomX/.loom/gh-config")).forward_gh);
    // A private clone carries its own; a leased launch never gets one.
    assert!(!decide(&args(), Some(&private_clone()), &env("/w/loom/.loom/gh-config")).forward_gh);
    let leased = Env {
        leased: true,
        ..env("/d/.loom/gh-config")
    };
    assert!(!decide(&args(), Some(&host), &leased).forward_gh);
    // No GH_CONFIG_DIR, nothing to forward.
    assert!(!decide(&args(), Some(&host), &Env::default()).forward_gh);
}
