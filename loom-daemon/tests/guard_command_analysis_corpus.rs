//! Corpus: the `loom:gh-pr-merge-redirect` rule of `guard-loom-workflow.sh`
//! answered by the shared command lexer (#12236, slice 1 of #11113). Each case
//! is the exact `PreToolUse` stdin the real shipped hook receives, run against
//! this build's `loom-daemon command-analysis gh-pr-merge`. The #8861 class —
//! the phrase quoted inside a heredoc or string — must be allowed; a command
//! that would really execute it must still be denied, and so must any input the
//! daemon cannot answer for.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

const HOOK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks/guard-loom-workflow.sh");
const BIN: &str = env!("CARGO_BIN_EXE_loom-daemon");
const TAG: &str = "loom:gh-pr-merge-redirect";

/// Commands that merely MENTION the phrase.
const ALLOWED: &[(&str, &str)] = &[
    ("heredoc to cat", "cat <<'EOF'\nnever run gh pr merge 12 by hand\nEOF"),
    ("unquoted-delimiter heredoc to cat", "cat <<EOF\ngh pr merge 12 is blocked\nEOF"),
    (
        "dash heredoc with tab-indented terminator",
        "cat <<-EOF\n\tgh pr merge 12\n\tEOF",
    ),
    (
        "commit message heredoc",
        "git commit -m \"$(cat <<'EOF'\nfix: document gh pr merge redirect\nEOF\n)\"",
    ),
    ("two heredocs on one line", "cat <<A <<B\ngh pr merge 1\nA\ngh pr merge 2\nB"),
    ("double-quoted argument", "echo \"use gh pr merge 12\""),
    ("single-quoted argument", "echo 'gh pr merge 12'"),
    ("comment body flag", "gh pr comment 12 --body 'do not gh pr merge this'"),
    ("after a comment", "ls # gh pr merge 12"),
    ("other gh pr verb", "gh pr view 12 --comments"),
    ("merge-pr.sh", "./.loom/scripts/merge-pr.sh 12"),
    ("gh pr list searching for merged", "gh pr list --search 'merge'"),
];

/// Commands that would execute it, however they are dressed.
const DENIED: &[(&str, &str)] = &[
    ("plain", "gh pr merge 12"),
    ("with flags", "gh pr merge 12 --squash --delete-branch"),
    ("repo flag first", "gh -R o/r pr merge 12"),
    ("and-chained", "cd /tmp && gh pr merge 12"),
    ("semicolon-chained", "echo hi; gh pr merge 12"),
    ("piped", "echo y | gh pr merge 12"),
    ("bash -c payload", "bash -c 'gh pr merge 12'"),
    ("sh -ec payload", "sh -ec \"gh pr merge 12\""),
    ("eval payload", "eval \"gh pr merge 12\""),
    ("command substitution", "echo $(gh pr merge 12)"),
    ("backticks", "echo `gh pr merge 12`"),
    ("nested bash -c in $( )", "echo $(bash -c 'gh pr merge 12')"),
    ("env wrapper", "env GH_TOKEN=x gh pr merge 12"),
    ("sudo wrapper", "sudo -u bob gh pr merge 12"),
    ("xargs wrapper", "echo 12 | xargs gh pr merge"),
    ("heredoc fed to a shell", "bash <<'EOF'\ngh pr merge 12\nEOF"),
    (
        "heredoc written to a file (#7773)",
        "cat > /tmp/x.sh <<'EOF'\ngh pr merge 12\nEOF\nbash /tmp/x.sh",
    ),
    ("live merge after a heredoc", "cat <<'EOF'\nhello\nEOF\ngh pr merge 12"),
    ("unterminated heredoc fed to a shell", "bash <<EOF\ngh pr merge 12"),
    ("quote-escaped word", "gh pr 'merge' 12"),
    ("live merge after an arithmetic shift", "(( x = 1 << 3 ))\ngh pr merge 12"),
    ("live merge after a shift-assign", "(( n <<= 1 ))\ngh pr merge 12"),
];

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
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

    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    /// Run the real hook on `command` with `bin` as the daemon; returns the
    /// decision and the whole hook stdout.
    fn run(&self, command: &str, bin: &str) -> (String, String) {
        let input =
            json!({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": self.repo()});
        let mut child = Command::new("bash")
            .arg(Path::new(HOOK))
            .current_dir(self.repo())
            .env("LOOM_DAEMON_SELF_BIN", bin)
            .env("HOME", self.root.path())
            .env("LOOM_GUARD_DECISION_LOG", "0")
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env_remove("LOOM_ROLE")
            .env_remove("LOOM_WORKTREE_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hook must always exit 0");
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if text.trim().is_empty() {
            return ("allow".into(), text);
        }
        let v: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
        let decision = v["hookSpecificOutput"]["permissionDecision"]
            .as_str()
            .unwrap_or("malformed")
            .to_string();
        (decision, text)
    }

    /// True when the merge-redirect rule (not some other rule) denied.
    fn merge_denied(&self, command: &str, bin: &str) -> bool {
        let (decision, text) = self.run(command, bin);
        decision == "deny" && text.contains("merge-pr.sh")
    }
}

#[test]
fn mention_of_the_phrase_is_allowed() {
    let fx = Fixture::new();
    for (name, cmd) in ALLOWED {
        assert!(!fx.merge_denied(cmd, BIN), "{name}: a mention must not trip {TAG}: {cmd:?}");
    }
}

#[test]
fn live_merge_is_denied() {
    let fx = Fixture::new();
    for (name, cmd) in DENIED {
        assert!(fx.merge_denied(cmd, BIN), "{name}: a live merge must still trip {TAG}: {cmd:?}");
    }
}

#[test]
fn unanswered_daemon_fails_closed() {
    let fx = Fixture::new();
    // A binary that does not exist, and one that answers with an unexpected code.
    for bin in [
        "/nonexistent/loom-daemon",
        "/bin/false-not-here",
        "/usr/bin/env",
    ] {
        assert!(
            fx.merge_denied("gh pr merge 12", bin),
            "no usable answer from {bin}: a plain merge must be denied"
        );
        assert!(
            !fx.merge_denied("ls -la", bin),
            "no usable answer from {bin}: an unrelated command must pass"
        );
    }
}
