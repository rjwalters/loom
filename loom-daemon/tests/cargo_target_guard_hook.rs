//! End-to-end: the `loom:cargo-target-dir` rule (#11582) — the real
//! `defaults/hooks/guard-loom-workflow.sh` driving this build's `loom-daemon
//! cargo-target-dir guard`. Pins what the unit tests in
//! `cargo_target_guard/tests.rs` cannot: the hook's prefilter and masking,
//! the deny/allow wiring (exit code + prefix), the session-env read, the
//! toggle, and the deny span landing in the execution's trace journal; and the
//! shared-target ask (#10646) wired through the same verb.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Write;
use std::path::{Path, PathBuf};
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
        std::fs::create_dir_all(repo.join(".loom/targets")).unwrap();
        std::fs::create_dir_all(root.path().join("home/.cache")).unwrap();
        std::fs::create_dir_all(root.path().join("tmpdir")).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success());
        Self { root }
    }

    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    /// Run the hook; returns (decision, reason).
    fn run(&self, command: &str, env: &[(&str, &Path)]) -> (String, String) {
        let mut cmd = Command::new("bash");
        cmd.arg(HOOK)
            .env("LOOM_DAEMON_SELF_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
            .env("HOME", self.home())
            .env("TMPDIR", self.root.path().join("tmpdir"))
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env("LOOM_GUARD_DECISION_LOG", "0")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("LOOM_GUARD_CARGO_TARGET_DIR")
            .env_remove("LOOM_GUARD_CARGO_SHARED_TARGET")
            .env_remove("CARGO_HOME")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .env_remove("LOOM_WORKTREE_PATH")
            .env_remove("LOOM_WORKTREE_CARGO_TARGET_DIR")
            .env_remove("LOOM_TRACEPARENT")
            .env_remove("LOOM_TRACE_CONTEXT_FILE")
            .env_remove("LOOM_ROLE")
            .env_remove("LOOM_FORGE_EGRESS_POLICY")
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

    #[track_caller]
    fn assert_denied(&self, command: &str, env: &[(&str, &Path)], root: &str) -> String {
        let (decision, reason) = self.run(command, env);
        assert_eq!(decision, "deny", "{command}: expected deny, got allow");
        assert!(reason.starts_with("BLOCKED [cargo-target-dir]"), "{command}: {reason}");
        assert!(reason.contains(&format!("which is under {root}.")), "{command}: {reason}");
        reason
    }

    #[track_caller]
    fn assert_allowed(&self, command: &str, env: &[(&str, &Path)]) {
        let (decision, reason) = self.run(command, env);
        assert_eq!(decision, "allow", "{command}: expected allow, got {decision}: {reason}");
    }
}

#[test]
fn acceptance_matrix() {
    let f = Fixture::new();
    let repo = f.repo();
    let home = f.home();

    // Allowed.
    f.assert_allowed("cargo test -p loom-daemon", &[]);
    f.assert_allowed(
        &format!("CARGO_TARGET_DIR={}/.loom/targets/x cargo test", repo.display()),
        &[],
    );
    f.assert_allowed(&format!("CARGO_TARGET_DIR={}/target cargo test", repo.display()), &[]);
    f.assert_allowed("cargo build", &[("CARGO_TARGET_DIR", &repo.join(".loom/targets/judge-1"))]);

    // Denied.
    f.assert_denied("CARGO_TARGET_DIR=/tmp/x cargo test", &[], "/tmp");
    f.assert_denied("CARGO_TARGET_DIR=~/.cache/x-target cargo build", &[], "~/.cache");
    f.assert_denied("CARGO_TARGET_DIR=$HOME/x cargo check", &[], "~");
    f.assert_denied("CARGO_TARGET_DIR=.loom/target-9 cargo test", &[], ".loom/target-*");
    f.assert_denied("cargo test --target-dir /tmp/x", &[], "/tmp");
    f.assert_denied("env CARGO_TARGET_DIR=/tmp/x cargo test", &[], "/tmp");
    f.assert_denied("CARGO_TARGET_DIR=$(mktemp -d) cargo nextest run", &[], "$TMPDIR");
    f.assert_denied(
        "cargo build",
        &[("CARGO_TARGET_DIR", &home.join(".cache/cargo-target"))],
        "~/.cache",
    );
}

#[test]
fn deny_names_the_loom_owned_alternative() {
    let f = Fixture::new();
    let reason = f.assert_denied("CARGO_TARGET_DIR=/tmp/x cargo test", &[], "/tmp");
    assert!(reason.contains("/target\" cargo"), "{reason}");
    let owned = f.repo().join(".loom/targets/doctor-1");
    let reason = f.assert_denied(
        "CARGO_TARGET_DIR=/tmp/x cargo test",
        &[("CARGO_TARGET_DIR", &owned)],
        "/tmp",
    );
    assert!(reason.contains(&owned.display().to_string()), "{reason}");
}

#[test]
fn prose_is_masked_and_unrelated_commands_allow() {
    let f = Fixture::new();
    for cmd in [
        "git commit -m 'never CARGO_TARGET_DIR=/tmp/x cargo test again'",
        "gh issue comment 1 --body 'CARGO_TARGET_DIR=/tmp/x cargo build leaked'",
        "echo CARGO_TARGET_DIR=/tmp/x cargo build",
        "ls /tmp",
        "cargo fmt --all",
    ] {
        f.assert_allowed(cmd, &[]);
    }
}

#[test]
fn toggle_off_allows() {
    let f = Fixture::new();
    let off = Path::new("0");
    f.assert_allowed("CARGO_TARGET_DIR=/tmp/x cargo test", &[("LOOM_GUARD_CARGO_TARGET_DIR", off)]);
    std::fs::write(f.repo().join(".loom/config.json"), r#"{"guards":{"cargoTargetDir":false}}"#)
        .unwrap();
    f.assert_allowed("CARGO_TARGET_DIR=/tmp/x cargo test", &[]);
}

#[test]
fn deny_is_journalled_as_a_span_with_role_and_root_only() {
    let f = Fixture::new();
    let journal = f.root.path().join("trace/context.jsonl");
    let context_file = f.root.path().join("trace/context.json");
    let traceparent = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01";
    let mut cmd_env: Vec<(&str, &Path)> = vec![
        ("LOOM_TRACE_CONTEXT_FILE", &context_file),
        ("LOOM_ROLE", Path::new("judge")),
    ];
    cmd_env.push(("LOOM_TRACEPARENT", Path::new(traceparent)));
    f.assert_denied("CARGO_TARGET_DIR=~/.cache/judge-9-target cargo test", &cmd_env, "~/.cache");
    let body = std::fs::read_to_string(&journal).expect("a deny must journal its span");
    assert!(body.contains("guard.cargo_target_dir.denied"), "{body}");
    assert!(body.contains("\"loom.guard.root\":\"~/.cache\""), "{body}");
    assert!(body.contains("\"loom.guard.source\":\"inline\""), "{body}");
    assert!(body.contains("\"loom.role\":\"judge\""), "{body}");
    assert!(!body.contains("judge-9-target"), "the path leaked: {body}");
    assert!(
        body.contains("0123456789abcdef0123456789abcdef"),
        "not in the execution's trace: {body}"
    );

    // An allow journals nothing.
    std::fs::remove_file(&journal).unwrap();
    f.assert_allowed("cargo test", &cmd_env);
    assert!(!journal.exists());
}

/// #10646: a Loom-managed worktree whose host `~/.cargo/config.toml` sets a
/// shared `build.target-dir`.
fn shared_fixture() -> (Fixture, PathBuf) {
    let f = Fixture::new();
    let shared = f.root.path().join("shared-target");
    std::fs::write(f.repo().join(".loom-managed"), "").unwrap();
    std::fs::create_dir_all(f.home().join(".cargo")).unwrap();
    std::fs::write(
        f.home().join(".cargo/config.toml"),
        format!("[build]\ntarget-dir = \"{}\"\n", shared.display()),
    )
    .unwrap();
    (f, shared)
}

#[test]
fn shared_build_target_dir_asks_with_the_per_worktree_prefix() {
    let (f, shared) = shared_fixture();
    let (decision, reason) = f.run("cargo test -p loom-daemon", &[]);
    assert_eq!(decision, "ask", "{reason}");
    assert!(reason.starts_with("ASK [cargo-shared-target]"), "{reason}");
    assert!(reason.contains(&shared.display().to_string()), "{reason}");
    assert!(
        reason.contains(&format!("CARGO_TARGET_DIR=\"{}/target\"", f.repo().display())),
        "{reason}"
    );

    // With the #8458 marker (valid only beside a Cargo manifest), the prefix
    // is the marker's dir.
    std::fs::write(f.repo().join("Cargo.toml"), "[workspace]\n").unwrap();
    let per_wt = shared.join("wt").join(f.repo().file_name().unwrap());
    std::fs::create_dir_all(&per_wt).unwrap();
    std::fs::write(f.repo().join(".loom-cargo-target-dir"), format!("{}\n", per_wt.display()))
        .unwrap();
    let (decision, reason) = f.run("cargo build", &[]);
    assert_eq!(decision, "ask", "{reason}");
    assert!(
        reason.contains(&format!("CARGO_TARGET_DIR=\"{}\"", per_wt.display())),
        "{reason}"
    );
}

#[test]
fn shared_ask_is_silent_with_an_explicit_dir_outside_a_worktree_or_unconfigured() {
    let (f, _) = shared_fixture();
    let repo = f.repo();
    f.assert_allowed(&format!("CARGO_TARGET_DIR={}/target cargo test", repo.display()), &[]);
    f.assert_allowed("cargo test", &[("CARGO_TARGET_DIR", &repo.join(".loom/targets/b"))]);
    f.assert_allowed("cargo test --target-dir target", &[]);
    f.assert_allowed("cargo fmt --all", &[]);
    f.assert_allowed("ls", &[]);

    // No shared build.target-dir configured.
    std::fs::remove_file(f.home().join(".cargo/config.toml")).unwrap();
    f.assert_allowed("cargo test", &[]);

    // Not a Loom-managed worktree.
    let (f, _) = shared_fixture();
    std::fs::remove_file(f.repo().join(".loom-managed")).unwrap();
    f.assert_allowed("cargo test", &[]);
}

#[test]
fn shared_ask_toggle_off_allows() {
    let (f, _) = shared_fixture();
    f.assert_allowed("cargo test", &[("LOOM_GUARD_CARGO_SHARED_TARGET", Path::new("0"))]);
    std::fs::write(f.repo().join(".loom/config.json"), r#"{"guards":{"cargoSharedTarget":false}}"#)
        .unwrap();
    f.assert_allowed("cargo test", &[]);
    // The deny keeps its own toggle.
    f.assert_denied("CARGO_TARGET_DIR=/tmp/x cargo test", &[], "/tmp");
}
