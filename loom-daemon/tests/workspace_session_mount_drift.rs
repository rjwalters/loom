//! #10364 — `workspace add` / `workspace remove` name the host-mode Codex
//! session containers the registry change just left with drifted mounts.
//!
//! Covers the call site (`cli::workspace_fleet::report_session_mount_drift`)
//! through the real compiled binary: which environment it reads, that the
//! report lands on stdout after the registry is saved, that it never fails
//! the command, and that a host with no session-managed profile makes no
//! `docker` call at all. The report's own logic is unit-tested in
//! `tokens_pool::session_drift_report`.
//!
//! Hermetic: `LOOM_CODEX_PROFILE_ROOT` and `LOOM_CODEX_SESSION_DOCKER` point
//! at fixtures, so neither the host's profiles nor its docker are consulted.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    registry: PathBuf,
    profiles: PathBuf,
    docker: PathBuf,
    calls: PathBuf,
}

impl Fixture {
    /// A workspace parent `ws/` holding git repos `old` and `new`, and a fake
    /// docker whose one session container (`agent-1`, labelled for `ws/`)
    /// mounts only `ws/old`.
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        for repo in ["old", "new"] {
            let status = Command::new("git")
                .args(["init", "--quiet"])
                .arg(root.join("ws").join(repo))
                .status()
                .unwrap();
            assert!(status.success(), "git init failed");
        }
        let inspect = serde_json::json!([{
            "Name": "/loom-codex-session-agent-1",
            "State": {"Running": true},
            "Config": {"Labels": {"loom.workspace": root.join("ws").display().to_string()}},
            "Mounts": [{
                "Type": "bind",
                "Destination": root.join("ws/old").display().to_string(),
                "RW": true,
            }],
        }]);
        let payload = root.join("inspect.json");
        std::fs::write(&payload, inspect.to_string()).unwrap();
        let calls = root.join("docker-calls.log");
        let docker = root.join("fake-docker");
        std::fs::write(
            &docker,
            format!(
                "#!/bin/sh\necho \"$1\" >> '{}'\ncase \"$1\" in\n  ps) echo \
                 loom-codex-session-agent-1 ;;\n  *) cat '{}' ;;\nesac\n",
                calls.display(),
                payload.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let profiles = root.join("profiles");
        std::fs::create_dir_all(profiles.join("agent-1")).unwrap();
        Self {
            registry: root.join("workspaces.json"),
            profiles,
            docker,
            calls,
            root,
            _tmp: tmp,
        }
    }

    /// Mark `agent-1`'s profile as adopted by a session container.
    fn adopt(&self) {
        std::fs::write(
            self.profiles.join("agent-1/.session-managed.json"),
            r#"{"schema_version":1,"container_name":"loom-codex-session-agent-1","adopted_at_unix":0}"#,
        )
        .unwrap();
    }

    fn repo(&self, name: &str) -> String {
        self.root.join("ws").join(name).display().to_string()
    }

    fn run(&self, args: &[&str]) -> Output {
        let defaults_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("defaults");
        Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
            .args(args)
            .current_dir(&self.root)
            .env("LOOM_WORKSPACES_PATH", &self.registry)
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env("LOOM_SHARED_TOKENS_DIR", "")
            .env("LOOM_DAEMON_DEFAULTS_DIR", defaults_dir)
            .env("LOOM_CODEX_PROFILE_ROOT", &self.profiles)
            .env("LOOM_CODEX_SESSION_DOCKER", &self.docker)
            .output()
            .unwrap()
    }

    fn docker_calls(&self) -> String {
        std::fs::read_to_string(&self.calls).unwrap_or_default()
    }
}

fn stdout(output: &Output) -> String {
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn add_and_remove_report_the_containers_they_left_drifted() {
    let fixture = Fixture::new();
    fixture.adopt();

    // `new` is registered after the container was created: it is missing
    // from the container, and the unregistered `old` is still mounted.
    let added = stdout(&fixture.run(&["workspace", "add", &fixture.repo("new")]));
    let registered = added.find("Registered workspace:").expect(&added);
    let report = added
        .find("loom-codex-session-agent-1 (account agent-1)")
        .expect(&added);
    assert!(registered < report, "the report follows the registration line: {added}");
    assert!(added.contains(&format!("does not mount {}", fixture.repo("new"))), "{added}");
    assert!(added.contains(&format!("still mounts {}", fixture.repo("old"))), "{added}");
    assert!(
        added.contains(&format!(
            "loom-daemon accounts session stop agent-1 && loom-daemon accounts session start \
             agent-1 --mount-workspace {}",
            fixture.root.join("ws").display()
        )),
        "{added}"
    );
    // No private-clone container on this host, so no advice about them.
    assert!(!added.contains("--private-clone"), "{added}");
    // One bounded snapshot per registry change.
    assert_eq!(fixture.docker_calls(), "ps\ninspect\n");

    let removed = stdout(&fixture.run(&["workspace", "remove", &fixture.repo("new")]));
    assert!(removed.contains("Deregistered workspace:"), "{removed}");
    assert!(removed.contains(&format!("still mounts {}", fixture.repo("old"))), "{removed}");
    assert!(!removed.contains("does not mount"), "{removed}");
    assert_eq!(fixture.docker_calls(), "ps\ninspect\nps\ninspect\n");

    // Removing what is not registered changes nothing, so reports nothing.
    let noop = stdout(&fixture.run(&["workspace", "remove", &fixture.repo("new")]));
    assert!(noop.contains("Not registered (no-op)"), "{noop}");
    assert_eq!(fixture.docker_calls(), "ps\ninspect\nps\ninspect\n");
}

#[test]
fn a_host_with_no_session_managed_profile_never_calls_docker() {
    let fixture = Fixture::new();
    let added = stdout(&fixture.run(&["workspace", "add", &fixture.repo("new")]));
    assert!(added.contains("Registered workspace:"), "{added}");
    assert!(!added.contains("loom-codex-session-"), "{added}");
    assert_eq!(fixture.docker_calls(), "", "docker must not be called");
}

#[test]
fn an_unanswerable_docker_never_fails_the_registry_command() {
    let fixture = Fixture::new();
    fixture.adopt();
    std::fs::write(&fixture.docker, "#!/bin/sh\nexit 1\n").unwrap();
    let added = stdout(&fixture.run(&["workspace", "add", &fixture.repo("new")]));
    assert!(added.contains("Registered workspace:"), "{added}");
    assert!(added.contains("not checked for mount drift"), "{added}");
}
