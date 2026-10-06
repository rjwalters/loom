//! End-to-end: the `loom:forge-egress` rule (#9989) — the real
//! `defaults/hooks/guard-loom-workflow.sh` driving this build's `loom-daemon
//! forge egress guard`, against a policy named by `LOOM_FORGE_EGRESS_POLICY`.
//! Pins what the unit tests in `forge_egress/guard_tests.rs` cannot: the
//! hook's prefilter lets every class through, its masking keeps prose inert,
//! and the deny/allow wiring (exit code + prefix) holds.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Write;
use std::path::Path;
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
        let policy = json!({
            "schemaVersion": 1,
            "enforcement": {"api": "required", "runtimeEgress": "unverified", "negativeCanary": null},
            "toolchain": {"launcherPath": "/usr/local/bin/gh"},
        });
        std::fs::write(root.path().join("required.json"), policy.to_string()).unwrap();
        let mut observe = policy;
        observe["enforcement"]["api"] = json!("observe");
        std::fs::write(root.path().join("observe.json"), observe.to_string()).unwrap();
        Self { root }
    }

    fn repo(&self) -> std::path::PathBuf {
        self.root.path().join("repo")
    }

    /// Run the hook; returns (decision, reason).
    fn run(&self, command: &str, policy: Option<&str>, env: &[(&str, &str)]) -> (String, String) {
        let mut cmd = Command::new("bash");
        cmd.arg(HOOK)
            .env("LOOM_DAEMON_SELF_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
            .env("HOME", self.root.path().join("home"))
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env("LOOM_GUARD_DECISION_LOG", "0")
            .env_remove("LOOM_FORGE_EGRESS_POLICY")
            .env_remove("LOOM_GUARD_FORGE_EGRESS")
            .env_remove("LOOM_WORKTREE_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(p) = policy {
            cmd.env("LOOM_FORGE_EGRESS_POLICY", self.root.path().join(p));
        }
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
            return ("allow".into(), String::new());
        }
        let v: Value = serde_json::from_str(&text).unwrap();
        let h = &v["hookSpecificOutput"];
        (
            h["permissionDecision"].as_str().unwrap().to_string(),
            h["permissionDecisionReason"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        )
    }
}

const POSITIVE: &[&str] = &[
    "curl -sS https://api.github.com/zen",
    "wget -qO- https://uploads.github.com/x",
    "gh api https://api.github.com/repos/o/r",
    "GH_HOST=github-proxy.example gh issue list",
    "GH_CONFIG_DIR=/tmp/x gh issue list",
    "gh api repos/o/r --hostname other.example",
    "gh config set api_host other.example",
    "gh auth login --with-token",
    "gh auth setup-git",
    "gh auth refresh -s repo",
    "env -i PATH=/usr/bin gh issue list",
    "/opt/homebrew/bin/gh issue list",
    "pip install PyGithub",
    "npm install @octokit/rest",
    "cargo add octocrab",
    "go get github.com/google/go-github/v60",
    "python3 -c 'from github import Github'",
];

const NEGATIVE: &[&str] = &[
    "gh issue list",
    "gh api repos/o/r",
    "gh api graphql -f query='query { viewer { login } }'",
    "./.loom/scripts/merge-pr.sh 123",
    "./.loom/scripts/create-issue.sh --title t --body b",
    "git push",
    "/usr/local/bin/gh issue list",
    "git commit -m 'docs: never curl https://api.github.com, use plain gh'",
    "gh issue comment 1 --body 'GH_HOST=x and gh auth login are denied'",
];

#[test]
fn required_policy_denies_every_class_including_wrapped_spellings() {
    let f = Fixture::new();
    for cmd in POSITIVE {
        let escaped = cmd.replace('"', "\\\"");
        for spelling in [
            (*cmd).to_string(),
            format!("bash -c \"{escaped}\""),
            format!("eval \"{escaped}\""),
            format!("echo \"{escaped}\" | sh"),
        ] {
            let (decision, reason) = f.run(&spelling, Some("required.json"), &[]);
            assert_eq!(decision, "deny", "{spelling}");
            assert!(reason.contains("routing.denied-by-guard"), "{reason}");
            assert!(reason.contains("plain `gh …`"), "{reason}");
            assert!(reason.contains("origin: env"), "{reason}");
        }
    }
}

/// Spellings the shell joins back into `gh` (PR #10543 review): the hook's
/// prefilter must hand them to the daemon, and the daemon must classify them.
const ESCAPED: &[&str] = &[
    r"g\h auth login",
    r"g\h api https://api.example.test/user",
    r#"g""h auth login"#,
    r"g''h auth login",
    r#""g"h auth login"#,
    r"$'gh' auth login",
];

#[test]
fn escaped_and_quote_joined_spellings_deny_only_under_an_enforcing_policy() {
    let f = Fixture::new();
    for cmd in ESCAPED {
        let (decision, reason) = f.run(cmd, Some("required.json"), &[]);
        assert_eq!(decision, "deny", "{cmd}");
        assert!(reason.contains("routing.denied-by-guard"), "{reason}");
        assert_eq!(f.run(cmd, Some("observe.json"), &[]).0, "allow", "observe: {cmd}");
        assert_eq!(
            f.run(cmd, Some("required.json"), &[("LOOM_GUARD_FORGE_EGRESS", "0")])
                .0,
            "allow",
            "toggle off: {cmd}"
        );
    }
    for prose in [
        r#"git commit -m 'never run g\h auth login'"#,
        r#"gh issue comment 1 --body 'g""h auth login is denied'"#,
    ] {
        assert_eq!(f.run(prose, Some("required.json"), &[]).0, "allow", "{prose}");
    }
}

#[test]
fn managed_commands_and_inert_prose_are_allowed() {
    let f = Fixture::new();
    for cmd in NEGATIVE {
        assert_eq!(f.run(cmd, Some("required.json"), &[]).0, "allow", "{cmd}");
    }
}

#[test]
fn inert_without_an_enforcing_policy() {
    let f = Fixture::new();
    let curl = "curl -sS https://api.github.com/zen";
    assert_eq!(f.run(curl, Some("observe.json"), &[]).0, "allow");
    // "No policy" is only provable on a host without a machine policy.
    if !Path::new("/etc/loom/forge-egress/policy.json").exists() {
        assert_eq!(f.run(curl, None, &[]).0, "allow");
    }
}

#[test]
fn toggle_off_by_env_or_config() {
    let f = Fixture::new();
    let curl = "curl -sS https://api.github.com/zen";
    assert_eq!(
        f.run(curl, Some("required.json"), &[("LOOM_GUARD_FORGE_EGRESS", "0")])
            .0,
        "allow"
    );
    std::fs::write(f.repo().join(".loom/config.json"), r#"{"guards": {"forgeEgress": false}}"#)
        .unwrap();
    assert_eq!(f.run(curl, Some("required.json"), &[]).0, "allow");
    // Env beats config.
    assert_eq!(
        f.run(curl, Some("required.json"), &[("LOOM_GUARD_FORGE_EGRESS", "1")])
            .0,
        "deny"
    );
}

#[test]
fn denial_never_echoes_the_command_or_a_credential() {
    let f = Fixture::new();
    // Built at runtime so the source never carries a token-shaped literal (#9133).
    let secret = format!("{}_{}", "ghp", "Ab9".repeat(12));
    let cmd = format!("curl -H 'Authorization: token {secret}' https://api.github.com/user");
    let (decision, reason) = f.run(&cmd, Some("required.json"), &[]);
    assert_eq!(decision, "deny");
    assert!(!reason.contains(&secret));
    assert!(!reason.contains("Authorization"));
    assert!(!reason.contains(&*f.root.path().to_string_lossy()), "policy path leaked");
}
