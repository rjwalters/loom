//! End-to-end: the `loom:remote-fleet-exec` rule (#11568) — the real
//! `defaults/hooks/guard-loom-workflow.sh` driving this build's `loom-daemon
//! guards remote-exec`, against a fleet roster named by `LOOM_FLEET_PATH` and
//! a tailscale status file. Pins the issue's acceptance cases through the hook
//! (prefilter, raw-command hand-off, cwd-relative script paths, the `ask`
//! decision and its reason) and the `guards.remoteFleetExec` opt-out.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

const HOOK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks/guard-loom-workflow.sh");

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join(".loom")).unwrap();
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success());
        std::fs::write(
            repo.join("s.sh"),
            "cd ~/GitHub/2am\ngit rebase --abort || true\ngit reset --hard origin/main\n",
        )
        .unwrap();
        std::fs::write(repo.join("ro.sh"), "git status\ngrep -n foo README.md\n").unwrap();
        let fleet = json!({"version": 1, "workers": [
            {"ssh_host": "loom-worker-1", "bootstrapped_at": "2026-01-01T00:00:00Z"}
        ]});
        std::fs::write(root.path().join("fleet.json"), fleet.to_string()).unwrap();
        let ts = json!({"Peer": {"k": {"HostName": "loom-worker-2",
            "DNSName": "loom-worker-2.example.ts.net.", "TailscaleIPs": ["100.70.0.2"]}}});
        std::fs::write(root.path().join("tailscale.json"), ts.to_string()).unwrap();
        Self { root }
    }

    fn repo(&self) -> std::path::PathBuf {
        self.root.path().join("repo")
    }

    /// Run the hook; returns (decision, reason, additionalContext).
    fn run(&self, command: &str, env: &[(&str, &str)]) -> (String, String, String) {
        let mut cmd = Command::new("bash");
        cmd.arg(HOOK)
            .env("LOOM_DAEMON_SELF_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
            .env("HOME", self.root.path().join("home"))
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env("LOOM_GUARD_DECISION_LOG", "0")
            .env("LOOM_FLEET_PATH", self.root.path().join("fleet.json"))
            .env("LOOM_GUARD_TAILSCALE_STATUS_FILE", self.root.path().join("tailscale.json"))
            .env_remove("LOOM_GUARD_REMOTE_FLEET_EXEC")
            .env_remove("LOOM_FORGE_EGRESS_POLICY")
            .env_remove("LOOM_ROLE")
            .env_remove("LOOM_WORKTREE_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let input =
            json!({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": self.repo()});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hook must always exit 0");
        let text = String::from_utf8_lossy(&out.stdout);
        if text.trim().is_empty() {
            return ("allow".into(), String::new(), String::new());
        }
        let v: Value = serde_json::from_str(&text).unwrap();
        let h = &v["hookSpecificOutput"];
        let s = |k: &str| h[k].as_str().unwrap_or("").to_string();
        (s("permissionDecision"), s("permissionDecisionReason"), s("additionalContext"))
    }
}

#[test]
fn acceptance_cases_through_the_hook() {
    let f = Fixture::new();

    let (d, reason, ctx) = f.run("ssh ubuntu@100.101.102.103 'bash -s' < s.sh", &[]);
    assert_eq!(d, "ask", "{reason}");
    assert!(reason.starts_with("ASK [remote-fleet-exec]"), "{reason}");
    assert!(reason.contains("break-glass/"), "{reason}");
    assert_eq!(ctx, reason, "the reminder must reach the session too");

    let (d, reason, _) = f.run("ssh ubuntu@100.101.102.103 'bash -s' < ro.sh", &[]);
    assert_eq!(d, "allow", "{reason}");

    let (d, reason, _) =
        f.run("ssh loom-worker-1 'git -C ~/GitHub/2am reset --hard origin/main'", &[]);
    assert_eq!(d, "ask", "{reason}");

    let (d, reason, _) = f.run("ssh loom-worker-2 'bash -s' < nowhere.sh", &[]);
    assert_eq!(d, "ask", "{reason}");
    assert!(reason.contains("cannot be checked"), "{reason}");

    let (d, reason, _) = f.run("ssh dev-vm.example.test 'bash -s' < s.sh", &[]);
    assert_eq!(d, "allow", "{reason}");
}

#[test]
fn prose_and_unrelated_commands_are_untouched() {
    let f = Fixture::new();
    for cmd in [
        "gh issue comment 1 --body \"ran ssh loom-worker-1 'git reset --hard' by hand\"",
        "git commit -m 'docs: ssh loom-worker-1 bash -s < s.sh is now guarded'",
        "ssh loom-worker-1 'git status'",
        "ssh-keygen -l -f ~/.ssh/id_ed25519.pub",
        "ls -la",
    ] {
        let (d, reason, _) = f.run(cmd, &[]);
        assert_eq!(d, "allow", "{cmd}: {reason}");
    }
}

#[test]
fn toggle_opts_the_repo_out() {
    let f = Fixture::new();
    let cmd = "ssh loom-worker-1 'git reset --hard'";
    let (d, _, _) = f.run(cmd, &[("LOOM_GUARD_REMOTE_FLEET_EXEC", "0")]);
    assert_eq!(d, "allow");
    std::fs::write(f.repo().join(".loom/config.json"), r#"{"guards": {"remoteFleetExec": false}}"#)
        .unwrap();
    let (d, _, _) = f.run(cmd, &[]);
    assert_eq!(d, "allow");
    let (d, _, _) = f.run(cmd, &[("LOOM_GUARD_REMOTE_FLEET_EXEC", "1")]);
    assert_eq!(d, "ask");
}

#[test]
fn a_daemon_without_the_verb_leaves_the_rule_inert() {
    let f = Fixture::new();
    let (d, _, _) = f.run(
        "ssh loom-worker-1 'git reset --hard'",
        &[("LOOM_DAEMON_SELF_BIN", "/usr/bin/false")],
    );
    assert_eq!(d, "allow");
}

/// Judge cycle 1 on #11568: payloads built at run time fail closed, and
/// `tailscale ssh` / `xargs` / `parallel` fan-out are transports too.
#[test]
fn runtime_built_payloads_and_fan_out_ask_through_the_hook() {
    let f = Fixture::new();
    for cmd in [
        "ssh ubuntu@100.101.102.103 \"$(cat s.sh)\"",
        "CMD='git reset --hard'; ssh ubuntu@100.101.102.103 \"$CMD\"",
        "ssh ubuntu@100.101.102.103 bash -s <<< \"$(cat s.sh)\"",
        "ssh ubuntu@100.101.102.103 bash -s <<EOS\n$(cat s.sh)\nEOS",
        "printf '%s\\n' \"$CMD\" | ssh ubuntu@100.101.102.103 bash",
        "tailscale ssh ubuntu@loom-worker-2 'git reset --hard'",
        "tailscale ssh ubuntu@loom-worker-1 'bash -s' < s.sh",
        "echo ubuntu@100.101.102.103 | xargs -I{} ssh {} 'git reset --hard'",
        "parallel ssh {} 'git reset --hard' ::: ubuntu@100.101.102.103",
    ] {
        let (d, reason, _) = f.run(cmd, &[]);
        assert_eq!(d, "ask", "{cmd}: {reason}");
        assert!(reason.starts_with("ASK [remote-fleet-exec]"), "{cmd}: {reason}");
    }
    for cmd in [
        "ssh ubuntu@100.101.102.103 \"ls $DIR\"",
        "tailscale ssh ubuntu@loom-worker-2 uptime",
        "echo ubuntu@100.101.102.103 | xargs -I{} ssh {} uptime",
        "tailscale status",
    ] {
        let (d, reason, _) = f.run(cmd, &[]);
        assert_eq!(d, "allow", "{cmd}: {reason}");
    }
}
