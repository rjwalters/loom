//! End-to-end: the per-role tool restriction (#8256) — the real shipped hooks
//! (`guard-loom-workflow.sh` for Bash, `guard-worktree-paths.sh` for
//! Edit/Write, and `guard-codex-bridge.sh` for Codex) driving this build's
//! `loom-daemon role-tool-policy check` against the real shipped role JSONs in
//! `defaults/roles/`. This is the in-session attempt the issue asks for, minus
//! the model: the exact stdin a `PreToolUse` hook receives, and the decision it
//! returns. The matcher itself is unit-tested per capability in
//! `role_tool_policy/command_match_tests.rs`.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

const HOOKS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks");
const BIN: &str = env!("CARGO_BIN_EXE_loom-daemon");

const READ_ONLY: &[&str] = &[
    "architect",
    "auditor",
    "champion",
    "curator",
    "guide",
    "hermit",
    "judge",
];

/// The four surfaces the issue names, each as a persuaded role would type it.
const DENIED: &[(&str, &str)] = &[
    ("ssh deploy@prod.example 'cat /etc/shadow'", "remote-shell"),
    ("aws sts get-caller-identity", "cloud-cli"),
    ("gh secret list --repo o/r", "forge-secrets"),
    ("echo 'ssh-ed25519 AAAA attacker' >> ~/.ssh/authorized_keys", "credential-store"),
];

/// What a read-only role does all day. None of it may be denied.
const EVERYDAY: &[&str] = &[
    "gh issue list --label loom:issue --json number,title",
    "gh pr view 12 --comments",
    "gh pr comment 12 --body 'never run ssh or aws here'",
    "git log --oneline -5",
    "grep -rn '~/.ssh' docs/",
    "ls -la",
];

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("home/.ssh")).unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success());
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    /// Run `hook` on `input` as `role`; returns (decision, reason).
    fn run(&self, hook: &str, input: &Value, role: Option<&str>, bin: &str) -> (String, String) {
        let mut cmd = Command::new("bash");
        cmd.arg(Path::new(HOOKS).join(hook))
            .current_dir(self.repo())
            .env("LOOM_DAEMON_SELF_BIN", bin)
            .env("HOME", self.home())
            .env("LOOM_GUARD_DECISION_LOG", "0")
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env_remove("LOOM_ROLE")
            .env_remove("LOOM_WORKTREE_PATH")
            .env_remove("LOOM_FORGE_EGRESS_POLICY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if hook == "guard-codex-bridge.sh" {
            cmd.arg("--project-root").arg(self.repo());
        }
        if let Some(r) = role {
            cmd.env("LOOM_ROLE", r);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{hook} must always exit 0");
        let text = String::from_utf8_lossy(&out.stdout);
        if text.trim().is_empty() {
            return ("allow".into(), String::new());
        }
        let v: Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{hook}: {e}: {text}"));
        let h = &v["hookSpecificOutput"];
        (
            h["permissionDecision"]
                .as_str()
                .unwrap_or("malformed")
                .to_string(),
            h["permissionDecisionReason"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        )
    }

    fn bash(&self, command: &str, role: Option<&str>, bin: &str) -> (String, String) {
        let input =
            json!({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": self.repo()});
        self.run("guard-loom-workflow.sh", &input, role, bin)
    }

    fn write(&self, path: &Path, role: Option<&str>, bin: &str) -> (String, String) {
        let input = json!({"tool_name": "Write", "tool_input": {"file_path": path, "content": "x"}, "cwd": self.repo()});
        self.run("guard-worktree-paths.sh", &input, role, bin)
    }

    fn codex_shell(&self, command: &str, role: Option<&str>) -> (String, String) {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "11111111-2222-3333-4444-555555555555",
            "transcript_path": null,
            "turn_id": "turn-1",
            "tool_use_id": "call-1",
            "model": "gpt-5-codex",
            "permission_mode": "default",
            "agent_id": "agent-1",
            "agent_type": "primary",
            "cwd": self.repo(),
            "tool_name": "shell",
            "tool_input": {"command": ["bash", "-lc", command]},
        });
        self.run("guard-codex-bridge.sh", &input, role, BIN)
    }
}

#[test]
fn every_read_only_role_is_denied_each_surface_by_the_bash_hook() {
    let f = Fixture::new();
    for role in READ_ONLY {
        for (cmd, cap) in DENIED {
            let (d, reason) = f.bash(cmd, Some(role), BIN);
            assert_eq!(d, "deny", "{role}: {cmd} -> {reason}");
            assert!(reason.starts_with("BLOCKED [role-tool-policy]"), "{reason}");
            assert!(reason.contains(cap) && reason.contains(&format!("{role}.json")), "{reason}");
        }
    }
}

/// Judge round 1 on PR #11152: shapes the real hook ALLOWED before — a
/// non-normalized absolute path, option-prefixed `gh` secret calls, and the
/// remote-shell transports/wrappers. `{home}` is the hook's `$HOME`.
const ROUND_1_BYPASSES: &[(&str, &str)] = &[
    ("echo x >> /tmp/..{home}/.ssh/authorized_keys", "credential-store"),
    ("cat /tmp/..{home}/.ssh/id_rsa", "credential-store"),
    ("cat /proc/self/root{home}/.ssh/id_rsa", "credential-store"),
    ("cat /{home}/.ssh/id_rsa", "credential-store"),
    ("cat /.{home}/.ssh/id_rsa", "credential-store"),
    // Round 2: quoted FILE operands to grep/rg are paths, not inert text.
    ("grep -h \".*\" \"{home}/.ssh/id_rsa\"", "credential-store"),
    ("rg \"x\" \"{home}/.aws/credentials\"", "credential-store"),
    // Round 3: attached pattern options and pattern-less modes leave every
    // operand a file.
    ("grep --regexp=x {home}/.ssh/id_rsa", "credential-store"),
    ("grep -ex {home}/.ssh/id_rsa", "credential-store"),
    ("rg -ex {home}/.ssh/id_rsa", "credential-store"),
    ("grep --regexp=x -- {home}/.ssh/id_rsa", "credential-store"),
    ("rg --files {home}/.ssh", "credential-store"),
    ("gh -R o/r secret list", "forge-secrets"),
    ("gh --repo o/r variable list", "forge-secrets"),
    ("rsync -e ssh a b:c", "remote-shell"),
    ("rsync a host:b", "remote-shell"),
    ("script -c 'ssh h'", "remote-shell"),
    ("su -c 'ssh h'", "remote-shell"),
    ("busybox ssh h", "remote-shell"),
    ("tmux new 'ssh h'", "remote-shell"),
    ("screen ssh h", "remote-shell"),
    ("git -c core.sshCommand='ssh -i k' fetch", "remote-shell"),
    ("GIT_SSH_COMMAND='ssh -i k' git fetch", "remote-shell"),
    // Round 4: wrapper options and ANSI-C quoting must not hide the executable.
    ("time -p ssh example.invalid", "remote-shell"),
    ("sudo --user root aws sts get-caller-identity", "cloud-cli"),
    (r"$'\x73sh' example.invalid", "remote-shell"),
    (r"$'\141ws' sts get-caller-identity", "cloud-cli"),
    // Round 5: bash truncates an ANSI-C string at the first NUL.
    (r"$'ssh\0suffix' example.invalid", "remote-shell"),
    (r"$'aws\x00suffix' sts get-caller-identity", "cloud-cli"),
    // Round 6: value-taking xargs options must not hide the executable.
    ("xargs --process-slot-var SLOT ssh example.invalid", "remote-shell"),
    ("xargs --max-chars 100 aws sts get-caller-identity", "cloud-cli"),
    // Round 7: long priority-wrapper options and optional-value xargs options.
    ("nice --adjustment 5 ssh example.invalid", "remote-shell"),
    ("nice --adjustment 5 aws sts get-caller-identity", "cloud-cli"),
    ("xargs --replace ssh example.invalid", "remote-shell"),
    // Round 8: taskset -c is a mode flag; the CPU list is the one operand.
    ("taskset -c 0 ssh example.invalid", "remote-shell"),
    ("taskset --cpu-list 0-3 aws sts get-caller-identity", "cloud-cli"),
    ("chrt -i 0 ssh example.invalid", "remote-shell"),
    // Round 9: substitutions inside `${…}` execute when the expansion does.
    (r#"echo "${unset_var:-$(ssh example.invalid)}""#, "remote-shell"),
    ("echo ${unset_var:-$(ssh example.invalid)}", "remote-shell"),
    (r#"echo "${unset_var:-`ssh example.invalid`}""#, "remote-shell"),
    ("echo ${unset_var:-`aws sts get-caller-identity`}", "cloud-cli"),
    // Round 10: short-option clusters hide a value-taking flag (separate / attached value).
    ("xargs -rn 1 ssh example.invalid", "remote-shell"),
    ("xargs -rP 1 aws sts get-caller-identity", "cloud-cli"),
    ("xargs -rn1 ssh example.invalid", "remote-shell"),
    ("sudo -Hu root ssh example.invalid", "remote-shell"),
    ("sudo -Huroot aws sts get-caller-identity", "cloud-cli"),
];

#[test]
fn round_1_bypasses_are_denied_by_the_real_hooks() {
    let f = Fixture::new();
    let home = f.home().to_string_lossy().into_owned();
    for (shape, cap) in ROUND_1_BYPASSES {
        let cmd = shape.replace("{home}", &home);
        let (d, reason) = f.bash(&cmd, Some("judge"), BIN);
        assert_eq!(d, "deny", "judge: {cmd} -> {reason}");
        assert!(reason.contains(cap), "{cmd}: {reason}");
        let (d, reason) = f.codex_shell(&cmd, Some("judge"));
        assert_eq!(d, "deny", "codex: {cmd} -> {reason}");
        let (_, reason) = f.bash(&cmd, Some("builder"), BIN);
        assert!(!reason.contains("role-tool-policy"), "builder: {cmd} -> {reason}");
    }
    for cmd in [
        "gh -R o/r pr view 12",
        "gh --repo o/r issue list",
        "rsync -a src/ dst/",
        "taskset -c 0 echo ok",
        "chrt -i 0 echo ok",
        r#"echo "${v:-$(echo hi)}" ${w:-fallback}"#,
    ] {
        let (d, reason) = f.bash(cmd, Some("judge"), BIN);
        assert_eq!(d, "allow", "judge: {cmd} -> {reason}");
    }
}

#[test]
fn read_only_roles_keep_their_everyday_commands() {
    let f = Fixture::new();
    for role in READ_ONLY {
        for cmd in EVERYDAY {
            let (d, reason) = f.bash(cmd, Some(role), BIN);
            assert_eq!(d, "allow", "{role}: {cmd} -> {reason}");
        }
    }
}

#[test]
fn builder_doctor_and_an_unidentified_session_are_unchanged() {
    let f = Fixture::new();
    for role in [
        Some("builder"),
        Some("doctor"),
        Some("sweep-lifecycle"),
        Some("development-worker"),
        None,
    ] {
        for (cmd, _) in DENIED {
            let (_, reason) = f.bash(cmd, role, BIN);
            assert!(!reason.contains("role-tool-policy"), "{role:?}: {cmd} -> {reason}");
        }
        let (_, reason) = f.write(&f.home().join(".ssh/authorized_keys"), role, BIN);
        assert!(!reason.contains("role-tool-policy"), "{role:?}: {reason}");
    }
}

#[test]
fn a_credential_store_write_through_the_write_tool_is_denied() {
    let f = Fixture::new();
    let (d, reason) = f.write(&f.home().join(".ssh/authorized_keys"), Some("judge"), BIN);
    assert_eq!(d, "deny", "{reason}");
    assert!(reason.contains("credential-store"), "{reason}");
    let (d, reason) = f.write(&f.repo().join("notes.md"), Some("judge"), BIN);
    assert!(!reason.contains("role-tool-policy"), "{d}: {reason}");
}

#[test]
fn codex_gets_the_same_denial_through_the_bridge() {
    let f = Fixture::new();
    for (cmd, cap) in DENIED {
        let (d, reason) = f.codex_shell(cmd, Some("curator"));
        assert_eq!(d, "deny", "codex: {cmd} -> {reason}");
        assert!(reason.contains(cap), "{reason}");
    }
    let (_, reason) = f.codex_shell("gh issue list", Some("curator"));
    assert!(!reason.contains("role-tool-policy"), "{reason}");
    let (_, reason) = f.codex_shell("aws sts get-caller-identity", Some("builder"));
    assert!(!reason.contains("role-tool-policy"), "{reason}");
}

#[test]
fn no_usable_daemon_fails_closed_for_read_only_roles_only() {
    let f = Fixture::new();
    // A missing binary, and one that answers every subcommand with clap's
    // unknown-subcommand exit 2 (a daemon predating `check`).
    let old = f.root.path().join("old-daemon");
    std::fs::write(
        &old,
        "#!/usr/bin/env bash\necho 'error: unrecognized subcommand' >&2\nexit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&old, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    for bin in ["/nonexistent/loom-daemon", old.to_str().unwrap()] {
        let (d, reason) = f.bash("ls -la", Some("curator"), bin);
        assert_eq!(d, "deny", "{bin}: {reason}");
        assert!(reason.contains("could not answer"), "{reason}");
        let (d, _) = f.write(&f.repo().join("x.md"), Some("hermit"), bin);
        assert_eq!(d, "deny", "{bin}");
        for role in [Some("builder"), Some("doctor"), None] {
            let (_, reason) = f.bash("ls -la", role, bin);
            assert!(!reason.contains("role-tool-policy"), "{role:?} {bin}: {reason}");
        }
    }
}
