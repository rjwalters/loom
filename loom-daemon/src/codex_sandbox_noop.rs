//! Detect a Codex session that exited 0 having run nothing, because Codex's
//! own sandbox refused every shell command (#10003; cause #9979).
//!
//! Inside a container whose seccomp / AppArmor profile denies unprivileged
//! user namespaces, Codex's `workspace-write` sandbox (bubblewrap) cannot
//! start. Every shell command the agent tries fails before it runs, with
//! `bwrap: …` as its only output. The agent then writes a polite "I couldn't
//! start" message, and `codex exec` exits 0. Classified on exit code alone,
//! that is a success, so the runtime fall-through never fires and every count
//! reads healthy.
//!
//! `codex exec` echoes each shell call to stderr:
//!
//! ```text
//! exec
//! /bin/bash -lc 'cat SKILL.md' in /path/to/workspace
//!  exited 1 in 0ms:
//! bwrap: No permissions to create a new namespace, …
//! ```
//!
//! A command that ran prints ` succeeded in <dur>:` instead. That framing is
//! the signal. Prose never is: models quote the bwrap error back in their
//! final message, and a curator may legitimately read an issue *about* bwrap.
//!
//! Two shapes count. Both were measured, not guessed. Per-tick, across every
//! Codex role tick in the role logs on robb-studio and loom-worker-1/2, the
//! rule matched all 2,579 exit-0 no-ops and none of the 60 exit-0 ticks that
//! ran a command:
//!
//! - [`Shape::ExecDenied`]: ≥1 exec result, **zero** ` succeeded in` results,
//!   and **every** ` exited N in …:` result's first output line starts with
//!   `bwrap:`. A tick that ran even one command is not a no-op, and neither
//!   is one where some command failed for an ordinary reason (it reached a
//!   shell).
//! - [`Shape::StartupWarning`]: **zero** exec results (Codex ran its calls
//!   through a tool that does not echo results to stderr). Codex printed its
//!   own startup verdict that its Linux sandbox cannot create user namespaces,
//!   and the session banner's `sandbox:` is not `danger-full-access`, which
//!   does not use bubblewrap at all.
//!
//! The scan is a pure function of the text. Callers apply it only to an exit-0
//! session: a non-zero exit is already a failure with its own classification.
//! `spawn-codex.sh` reaches it through `loom-daemon codex-sandbox-noop`, and
//! `role_runner` calls it in-process as the fallback for a tick whose adapter
//! predates the verdict.

use std::fmt;

/// The startup line Codex prints when bubblewrap cannot create a user
/// namespace, matched as a substring of a line that starts `warning:`.
pub const USERNS_WARNING: &str =
    "Linux sandbox uses bubblewrap and needs access to create user namespaces";

/// Which measured no-op shape matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Every echoed exec result was refused by bubblewrap.
    ExecDenied,
    /// No exec echoed, plus Codex's own user-namespace startup verdict.
    StartupWarning,
}

impl Shape {
    /// Stable wire name, used in the `# LOOM_RUNTIME_NOOP` line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExecDenied => "exec-denied",
            Self::StartupWarning => "startup-warning",
        }
    }
}

/// A session that ran nothing, with the counts that prove it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoOp {
    pub shape: Shape,
    /// Exec results echoed (` succeeded in` + ` exited N in`).
    pub execs: usize,
    /// Failed exec results whose first output line was `bwrap:`.
    pub denied: usize,
    /// Exec results that succeeded — always `0` for a no-op; carried so the
    /// rendered line states it rather than implying it.
    pub succeeded: usize,
}

impl fmt::Display for NoOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "shape={} execs={} denied={} succeeded={}",
            self.shape.as_str(),
            self.execs,
            self.denied,
            self.succeeded
        )
    }
}

fn is_exec_result(line: &str, verb: &str) -> bool {
    // ` succeeded in <dur>:` / ` exited <n> in <dur>:` — a leading space,
    // a duration with no `:` in it, and a trailing `:`.
    let Some(rest) = line.strip_prefix(' ') else {
        return false;
    };
    let rest = rest.trim_end();
    let Some(rest) = rest.strip_suffix(':') else {
        return false;
    };
    let body = match verb {
        "succeeded" => rest.strip_prefix("succeeded in "),
        _ => rest.strip_prefix("exited ").and_then(|tail| {
            let (code, after) = tail.split_once(' ')?;
            let digits = code.strip_prefix('-').unwrap_or(code);
            (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
                .then_some(after)?
                .strip_prefix("in ")
        }),
    };
    body.is_some_and(|duration| !duration.is_empty() && !duration.contains(':'))
}

/// Scan a Codex session's captured output. `Some` only for a no-op.
#[must_use]
pub fn scan(text: &str) -> Option<NoOp> {
    let (mut ok, mut failed, mut denied) = (0usize, 0usize, 0usize);
    let (mut userns_warning, mut full_access) = (false, false);
    let mut awaiting_first_output = false;
    for line in text.lines() {
        if awaiting_first_output {
            awaiting_first_output = false;
            if line.starts_with("bwrap:") {
                denied += 1;
            }
        }
        if is_exec_result(line, "succeeded") {
            ok += 1;
        } else if is_exec_result(line, "exited") {
            failed += 1;
            awaiting_first_output = true;
        } else if line.starts_with("sandbox:") && line.contains("danger-full-access") {
            full_access = true;
        } else if line.starts_with("warning:") && line.contains(USERNS_WARNING) {
            userns_warning = true;
        }
    }
    let execs = ok + failed;
    let shape = if execs > 0 && ok == 0 && denied == failed {
        Shape::ExecDenied
    } else if execs == 0 && userns_warning && !full_access {
        Shape::StartupWarning
    } else {
        return None;
    };
    Some(NoOp {
        shape,
        execs,
        denied,
        succeeded: ok,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BANNER: &str = "OpenAI Codex v0.160.0\n--------\nworkdir: /home/ubuntu/GitHub/2am\n\
        model: gpt-6.1-sol\napproval: never\nsandbox: workspace-write [workdir, /tmp, $TMPDIR]\n\
        session id: 01a0fe6f-a81c-78f2-9f87-4bed06270797\n--------\nuser\n/loom:curator\n";
    const DENIED: &str = "exec\n/bin/bash -lc 'cat .agents/skills/loom-curator/SKILL.md' in /w\n \
        exited 1 in 0ms:\nbwrap: No permissions to create a new namespace, likely because the \
        kernel does not allow non-privileged user namespaces.\n\n";
    const SUCCEEDED: &str =
        "exec\n/bin/bash -lc 'gh issue list --label loom:curated' in /w\n succeeded in 812ms:\n[]\n\n";
    const ORDINARY_FAIL: &str = "exec\n/bin/bash -lc 'cat nope' in /w\n exited 1 in 3ms:\n\
        cat: nope: No such file or directory\n\n";
    const PROSE: &str = "codex\nThe curator run is blocked: the shell failed \
        (`bwrap: No permissions to create a new namespace`).\ntokens used\n17,813\n";

    fn shape_of(text: &str) -> Option<String> {
        scan(text).map(|noop| noop.to_string())
    }

    #[test]
    fn one_exec_refused_by_bwrap_is_a_no_op() {
        let text = format!(
            "{BANNER}warning: Codex could not find bubblewrap on PATH. Codex will use the bundled \
             bubblewrap in the meantime.\ncodex\nUsing the loom-curator skill.\n{DENIED}{PROSE}"
        );
        assert_eq!(
            shape_of(&text).as_deref(),
            Some("shape=exec-denied execs=1 denied=1 succeeded=0")
        );
    }

    #[test]
    fn every_one_of_several_refused_execs_is_a_no_op() {
        let text = format!("{BANNER}{DENIED}{DENIED}{PROSE}");
        assert_eq!(
            shape_of(&text).as_deref(),
            Some("shape=exec-denied execs=2 denied=2 succeeded=0")
        );
    }

    #[test]
    fn a_different_bwrap_refusal_still_counts() {
        // The Ubuntu AppArmor shape (#9979's loom-worker-2 probe).
        let text = format!(
            "{BANNER}exec\n/bin/bash -lc 'gh pr list' in /w\n exited 1 in 0ms:\n\
             bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted\n{PROSE}"
        );
        assert!(matches!(
            scan(&text),
            Some(NoOp {
                shape: Shape::ExecDenied,
                ..
            })
        ));
    }

    #[test]
    fn a_seconds_valued_duration_is_still_an_exec_result() {
        let text = "exec\n/bin/bash -lc 'make' in /w\n exited 2 in 1.2s:\nbwrap: denied\n";
        assert_eq!(
            shape_of(text).as_deref(),
            Some("shape=exec-denied execs=1 denied=1 succeeded=0")
        );
    }

    #[test]
    fn the_startup_verdict_with_no_echoed_exec_is_a_no_op() {
        // loom-worker-1's auditor tick: calls ran through the JS exec tool.
        let text = format!(
            "{BANNER}warning: Codex's Linux sandbox uses bubblewrap and needs access to create \
             user namespaces.\ncodex\nThe audit is blocked.\n```text\nbwrap: loopback: Failed \
             RTM_NEWADDR: Operation not permitted\n```\ntokens used\n17,344\n"
        );
        assert_eq!(
            shape_of(&text).as_deref(),
            Some("shape=startup-warning execs=0 denied=0 succeeded=0")
        );
    }

    #[test]
    fn a_genuine_nothing_to_do_tick_is_not_a_no_op() {
        let text = format!("{BANNER}{SUCCEEDED}codex\nNo issues need curation.\n");
        assert_eq!(scan(&text), None);
    }

    #[test]
    fn one_refused_exec_beside_one_that_ran_is_not_a_no_op() {
        assert_eq!(scan(&format!("{BANNER}{DENIED}{SUCCEEDED}{PROSE}")), None);
    }

    #[test]
    fn an_ordinary_command_failure_reached_the_shell() {
        assert_eq!(scan(&format!("{BANNER}{ORDINARY_FAIL}{PROSE}")), None);
        assert_eq!(scan(&format!("{BANNER}{DENIED}{ORDINARY_FAIL}")), None);
    }

    #[test]
    fn a_command_whose_output_quotes_the_error_ran() {
        let text = "exec\n/bin/bash -lc 'gh issue view 9979' in /w\n succeeded in 400ms:\n\
                    bwrap: No permissions to create a new namespace\n";
        assert_eq!(scan(text), None);
    }

    #[test]
    fn bwrap_text_that_is_not_the_first_output_line_does_not_count() {
        let text = "exec\n/bin/bash -lc 'false' in /w\n exited 1 in 2ms:\nsome output first\n\
                    bwrap: No permissions to create a new namespace\n";
        assert_eq!(scan(text), None);
    }

    #[test]
    fn prose_alone_never_matches() {
        assert_eq!(scan(&format!("{BANNER}{PROSE}")), None);
        assert_eq!(scan(""), None);
    }

    #[test]
    fn the_startup_verdict_under_danger_full_access_does_not_match() {
        let text = "sandbox: danger-full-access\nwarning: Codex's Linux sandbox uses bubblewrap \
                    and needs access to create user namespaces.\ncodex\nDone.\n";
        assert_eq!(scan(text), None);
    }

    #[test]
    fn near_miss_result_lines_are_not_exec_results() {
        for line in [
            "exited 1 in 0ms:",        // no leading space
            " exited x in 0ms:",       // non-numeric code
            " exited 1 in :",          // empty duration
            " succeeded in 3ms",       // no trailing colon
            " exited 1 in 0ms: extra", // text after the colon
        ] {
            assert!(
                !is_exec_result(line, "succeeded") && !is_exec_result(line, "exited"),
                "{line:?}"
            );
        }
        assert!(is_exec_result(" exited -1 in 10ms:", "exited"));
        assert!(is_exec_result(" succeeded in 2m 3s:", "succeeded"));
    }
}
