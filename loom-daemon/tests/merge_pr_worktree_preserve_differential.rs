//! Differential test: the Rust port of `merge-pr.sh`'s tripled #4186/#6694/
//! #6264 remove-vs-preserve decision must agree with the retired shell, byte
//! for byte, on a shared corpus (`defaults/docs/verification-recipes.md` §6 —
//! the #8191 slice that moved it into
//! `loom_daemon::merge_pr::worktree_preserve`).
//!
//! # Why this is the successor to the retired source-text assertions
//!
//! `test-merge-pr-worktree-path.sh` used to `grep` `merge-pr.sh`'s own source
//! for the message text ("Preserving worktree at", "Found co-existing
//! Judge/Doctor review worktree", the `#6694` wording, …). That text no longer
//! lives in the shell at all — it is a Rust string literal the CLI prints —
//! so those greps cannot pass, structurally: there is no heredoc, no string
//! interpolation, nothing shell-shaped left to find. This test is the
//! successor `verification-recipes.md` §6 requires: instead of asserting the
//! OLD implementation's source text, it drives the retired implementation
//! itself (frozen verbatim in `tests/fixtures/merge-pr-worktree-preserve-
//! retired.sh`) and the real port side by side, and asserts their observable
//! behaviour — which lines get printed, in what order, and whether the
//! worktree gets removed — is unchanged.
//!
//! # What "observable behaviour" means here
//!
//! Both sides render the same trace shape: one `LEVEL<TAB>message` line per
//! `info`/`warning` call (the frozen shell's stubs write these directly; the
//! Rust side renders [`loom_daemon::merge_pr::worktree_preserve::decide`]'s
//! output the same way `_worktree_cleanup_decide` in the live shell does),
//! followed by `REMOVED<TAB><path>` if and only if `_remove_loom_worktree`
//! was (or would be) called. Comparing that trace, not just the final
//! action, catches an ordering divergence (e.g. printing the removal note
//! AFTER removing rather than before) that comparing only "removed: yes/no"
//! would miss.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/merge-pr-worktree-preserve-retired.sh")
}

#[derive(Clone, Copy)]
enum Kind {
    Default,
    Discovered,
    JudgePr,
}

impl Kind {
    fn retired_fn(self) -> &'static str {
        match self {
            Kind::Default => "_retired_default_path",
            Kind::Discovered => "_retired_discovered_path",
            Kind::JudgePr => "_retired_judge_pr_path",
        }
    }

    fn cli_value(self) -> &'static str {
        match self {
            Kind::Default => "default",
            Kind::Discovered => "discovered",
            Kind::JudgePr => "judge-pr",
        }
    }
}

struct Case {
    kind: Kind,
    path: &'static str,
    repo_root: &'static str,
    pr_number: &'static str,
    branch: &'static str,
    issue_num: &'static str,
    /// `_issue_is_closed_for_cleanup`'s stubbed exit code: `0` (authorized,
    /// so `preserve_check` is false) or `1` (preserve, so `preserve_check` is
    /// true) — only consulted by the retired block, and by the CLI wrapper,
    /// when `issue_num` is non-empty.
    gate_rc: i32,
    /// `branch_has_landed`'s stubbed exit code, consulted only when the gate
    /// says preserve.
    landed_rc: i32,
    landed_verdict: &'static str,
    landed_evidence: &'static str,
}

/// Run the frozen block for `case.kind`, returning its trace: one line per
/// `info`/`warning` call plus `REMOVED\t<path>` if `_remove_loom_worktree` was
/// called.
fn run_frozen_shell(case: &Case) -> String {
    let driver = format!(
        r#"set -euo pipefail
info() {{ printf 'INFO\t%s\n' "$1"; }}
warning() {{ printf 'WARNING\t%s\n' "$1"; }}
_remove_loom_worktree() {{ printf 'REMOVED\t%s\n' "$1"; }}
_issue_is_closed_for_cleanup() {{ return {gate_rc}; }}
branch_has_landed() {{ return {landed_rc}; }}
ISSUE_NUM="$1"
PR_NUMBER="$2"
PR_BRANCH="$3"
REPO_ROOT="$4"
DEFAULT_BRANCH_NAME="main"
PR_HEAD_SHA="deadbeef"
BRANCH_LANDED_VERDICT="$5"
BRANCH_LANDED_EVIDENCE="$6"
source "$7"
{func} "$8"
"#,
        gate_rc = case.gate_rc,
        landed_rc = case.landed_rc,
        func = case.kind.retired_fn(),
    );
    let out = Command::new("bash")
        .args(["-c", &driver, "driver"])
        .args([
            case.issue_num,
            case.pr_number,
            case.branch,
            case.repo_root,
            case.landed_verdict,
            case.landed_evidence,
        ])
        .arg(fixture_path())
        .arg(case.path)
        .output()
        .expect("bash ran the frozen shell side");
    assert!(
        out.status.success(),
        "frozen shell side exited {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run the real CLI, then render its answer the same way the live shell's
/// `_worktree_cleanup_decide` does: the `LEVEL<TAB>message` lines it printed,
/// followed by `REMOVED<TAB><path>` iff the action was `REMOVE`.
fn run_rust_cli(case: &Case) -> String {
    let mut args = vec![
        "merge-pr".to_string(),
        "worktree-preserve".to_string(),
        "--kind".to_string(),
        case.kind.cli_value().to_string(),
        "--path".to_string(),
        case.path.to_string(),
        "--repo-root".to_string(),
        case.repo_root.to_string(),
        "--pr".to_string(),
        case.pr_number.to_string(),
        "--branch".to_string(),
        case.branch.to_string(),
        "--issue-num".to_string(),
        case.issue_num.to_string(),
        "--landed-verdict".to_string(),
        case.landed_verdict.to_string(),
        "--landed-evidence".to_string(),
        case.landed_evidence.to_string(),
    ];
    // Mirrors `_worktree_cleanup_decide`'s own gating: `--preserve-check` is
    // passed only when the issue gate says preserve (never when issue_num is
    // empty), and `--landed` only alongside it.
    let preserve_check = !case.issue_num.is_empty() && case.gate_rc != 0;
    if preserve_check {
        args.push("--preserve-check".to_string());
        if case.landed_rc == 0 {
            args.push("--landed".to_string());
        }
    }

    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(&args)
        .output()
        .expect("loom-daemon spawned");
    assert!(
        out.status.success(),
        "loom-daemon merge-pr worktree-preserve exited {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = stdout.lines();
    let action = lines.next().expect("an action line");
    let mut trace = String::new();
    for line in lines {
        trace.push_str(line);
        trace.push('\n');
    }
    match action {
        "REMOVE" => trace.push_str(&format!("REMOVED\t{}\n", case.path)),
        "PRESERVE" => {}
        other => panic!("unrecognized action line {other:?}"),
    }
    trace
}

#[test]
fn frozen_shell_and_rust_cli_agree_byte_for_byte() {
    let cases: &[Case] = &[
        // No issue_num at all (the external-fork/pr-<N> shape, #4186) — the
        // gate is skipped entirely regardless of gate_rc/landed_rc.
        Case {
            kind: Kind::Default,
            path: "/repo/.loom/worktrees/pr-99",
            repo_root: "/repo",
            pr_number: "99",
            branch: "pr-99-external",
            issue_num: "",
            gate_rc: 1,
            landed_rc: 1,
            landed_verdict: "",
            landed_evidence: "",
        },
        // issue_num set, gate says "authorized" (Closes-target, or a live
        // CLOSED state) — removes unconditionally, no message (default kind).
        Case {
            kind: Kind::Default,
            path: "/repo/.loom/worktrees/issue-42",
            repo_root: "/repo",
            pr_number: "99",
            branch: "feature/issue-42",
            issue_num: "42",
            gate_rc: 0,
            landed_rc: 1,
            landed_verdict: "unknown",
            landed_evidence: "no ancestry proof",
        },
        // issue_num set, gate says preserve, branch NOT landed — preserves
        // with the two-line #6694 warning+retry message (default kind).
        Case {
            kind: Kind::Default,
            path: "/repo/.loom/worktrees/issue-42",
            repo_root: "/repo",
            pr_number: "99",
            branch: "feature/issue-42",
            issue_num: "42",
            gate_rc: 1,
            landed_rc: 1,
            landed_verdict: "unknown",
            landed_evidence: "no ancestry proof",
        },
        // issue_num set, gate says preserve, branch IS landed — the #6694
        // override removes anyway (default kind).
        Case {
            kind: Kind::Default,
            path: "/repo/.loom/worktrees/issue-42",
            repo_root: "/repo",
            pr_number: "99",
            branch: "feature/issue-42",
            issue_num: "42",
            gate_rc: 1,
            landed_rc: 0,
            landed_verdict: "landed",
            landed_evidence: "tip matches merged head",
        },
        // Same four shapes for the discovered-path call site, which — unlike
        // Default — DOES log a note on the plain-authorized removal path.
        Case {
            kind: Kind::Discovered,
            path: "/repo/.loom/worktrees/weird path",
            repo_root: "/repo",
            pr_number: "7",
            branch: "feature/issue-7",
            issue_num: "",
            gate_rc: 1,
            landed_rc: 1,
            landed_verdict: "",
            landed_evidence: "",
        },
        Case {
            kind: Kind::Discovered,
            path: "/repo/.loom/worktrees/weird path",
            repo_root: "/repo",
            pr_number: "7",
            branch: "feature/issue-7",
            issue_num: "7",
            gate_rc: 0,
            landed_rc: 1,
            landed_verdict: "unknown",
            landed_evidence: "unknown",
        },
        Case {
            kind: Kind::Discovered,
            path: "/repo/.loom/worktrees/weird path",
            repo_root: "/repo",
            pr_number: "7",
            branch: "feature/issue-7",
            issue_num: "7",
            gate_rc: 1,
            landed_rc: 1,
            landed_verdict: "unknown",
            landed_evidence: "unknown",
        },
        Case {
            kind: Kind::Discovered,
            path: "/repo/.loom/worktrees/weird path",
            repo_root: "/repo",
            pr_number: "7",
            branch: "feature/issue-7",
            issue_num: "7",
            gate_rc: 1,
            landed_rc: 0,
            landed_verdict: "landed",
            landed_evidence: "PR reports merged with a matching tip",
        },
        // The Judge/Doctor review worktree call site's own four shapes —
        // issue_num is always non-empty there in the live shell, but the
        // decision module makes no such assumption, so this corpus does not
        // special-case it away.
        Case {
            kind: Kind::JudgePr,
            path: "/repo/.loom/worktrees/pr-123",
            repo_root: "/repo",
            pr_number: "123",
            branch: "feature/issue-55",
            issue_num: "55",
            gate_rc: 0,
            landed_rc: 1,
            landed_verdict: "unknown",
            landed_evidence: "unknown",
        },
        Case {
            kind: Kind::JudgePr,
            path: "/repo/.loom/worktrees/pr-123",
            repo_root: "/repo",
            pr_number: "123",
            branch: "feature/issue-55",
            issue_num: "55",
            gate_rc: 1,
            landed_rc: 1,
            landed_verdict: "unknown",
            landed_evidence: "no ancestry proof",
        },
        Case {
            kind: Kind::JudgePr,
            path: "/repo/.loom/worktrees/pr-123",
            repo_root: "/repo",
            pr_number: "123",
            branch: "feature/issue-55",
            issue_num: "55",
            gate_rc: 1,
            landed_rc: 0,
            landed_verdict: "landed",
            landed_evidence: "ancestor of main",
        },
    ];

    let mut saw_remove_without_message = false;
    let mut saw_preserve = false;
    let mut saw_landed_override_remove = false;
    for case in cases {
        let shell_trace = run_frozen_shell(case);
        let rust_trace = run_rust_cli(case);
        assert_eq!(
            shell_trace,
            rust_trace,
            "divergence for kind={:?} path={:?} issue_num={:?} gate_rc={} landed_rc={}",
            case.kind.cli_value(),
            case.path,
            case.issue_num,
            case.gate_rc,
            case.landed_rc
        );
        if shell_trace == format!("REMOVED\t{}\n", case.path) {
            saw_remove_without_message = true;
        }
        if shell_trace.starts_with("WARNING\t") {
            saw_preserve = true;
        }
        if shell_trace.contains("(#6694)")
            && shell_trace.ends_with(&format!("REMOVED\t{}\n", case.path))
        {
            saw_landed_override_remove = true;
        }
    }
    assert!(
        saw_remove_without_message,
        "corpus never exercised the silent default-kind removal"
    );
    assert!(saw_preserve, "corpus never exercised a preserve outcome");
    assert!(
        saw_landed_override_remove,
        "corpus never exercised the #6694 landed-branch override"
    );
}
