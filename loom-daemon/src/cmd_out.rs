//! Typed command and forge results over [`crate::proc_exec`] (epic #7810, PR 2).
//!
//! PR 1 established *what actually ran* — spawn failure, timeout, exit status,
//! raw bytes, all kept apart. This layer establishes *what the output meant*,
//! and keeps those apart too.
//!
//! # The distinction that was missing
//!
//! Before this, three separate runners each collapsed a different set of
//! outcomes into one value:
//!
//! - `script_helpers::GhResult` — `{ success: bool, stdout: String, stderr:
//!   String }`, lossily decoded, with a **spawn error reported as `success:
//!   false`**, so "gh is not installed" and "gh ran and exited 1" were the same
//!   value.
//! - `main_health_gate::run_git` — `Result<(String, String), String>`, where a
//!   spawn failure and a non-zero exit both became an `Err(String)` message.
//! - `forge_cmd.rs` — raw `Command::new` with per-site handling (`.ok()?`,
//!   `Err(_) => return base`, ad-hoc `match`).
//!
//! All three then leaned on the same idiom to decide whether a query had
//! *found* anything:
//!
//! ```ignore
//! if r.success && !r.trimmed_stdout().is_empty() { /* treat as found */ }
//! ```
//!
//! That conflates **a query that legitimately matched nothing** with **a query
//! that could not be answered**. For a forge, those call for opposite
//! responses: the first is a fact to act on, the second is a reason to retry or
//! escalate. [`Query`] refuses to merge them.
//!
//! # Why `--jq` goes away
//!
//! Every JSON query previously shaped its own result inside the subprocess:
//!
//! ```ignore
//! &["pr", "view", "7", "--json", "state", "--jq", ".state"]
//! ```
//!
//! That runs a second interpreter in the pipeline and hands back a bare string
//! in which *command failed*, *field absent*, *valid-but-empty* and *no match*
//! are all the empty string. Asking for `--json` alone and decoding in-process
//! keeps the structure — and lets a decode failure be reported as
//! [`Query::Malformed`] instead of silently reading as "nothing found".
//!
//! # No trimming at the boundary
//!
//! Output is exposed as raw bytes and trimmed only where a caller asks. This is
//! not fastidiousness: `main_health_gate` already carries a separate
//! `git_status_porcelain` helper *solely* because its `run_git`'s blanket
//! `.trim()` would eat the leading space of a porcelain v1 status line (`" M
//! file"` — an unstaged modification of a tracked file) and change what the
//! status means. A shared layer that trims by default would reintroduce that
//! bug everywhere at once.

use crate::proc_exec::{run_bounded, Completion, ExecError};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Default ceiling for a forge or `git` command.
///
/// Generous on purpose — it is a hang ceiling, not a performance budget. A
/// `gh` call that legitimately takes 30s is rare; one that takes forever is the
/// failure mode this bounds (the 2026-07-26 wedged-`gh` incident).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// What a command did, with the outcomes that call for different responses kept
/// apart.
///
/// `Ran` deliberately carries the whole [`std::process::Output`]: raw bytes and
/// the real `ExitStatus`, so a non-zero exit and a signal death stay
/// distinguishable and nothing is decoded before a caller asks.
#[derive(Debug)]
pub enum CmdOutcome {
    /// The command ran to completion. It may still have exited non-zero — that
    /// is an *answer*, not an absence of one.
    Ran(std::process::Output),

    /// The command could not be run, or did not finish inside its deadline.
    ///
    /// The one outcome that means *we do not know*. Merging it into "it failed"
    /// is how a wedged forge becomes an apparently-negative answer.
    Unavailable(Unavailable),
}

/// Why a command produced no answer at all.
#[derive(Debug)]
pub enum Unavailable {
    /// The binary could not be started (absent, not executable, bad cwd).
    /// Nothing ran, so nothing had side effects.
    Spawn(String),
    /// It started but its result could not be collected.
    Collect(String),
    /// It started and outlived its deadline; its process group was terminated.
    TimedOut {
        after: Duration,
        partial_stdout: Vec<u8>,
    },
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unavailable::Spawn(e) => write!(f, "could not start: {e}"),
            Unavailable::Collect(e) => write!(f, "could not collect output: {e}"),
            Unavailable::TimedOut { after, .. } => write!(f, "timed out after {after:?}"),
        }
    }
}

impl CmdOutcome {
    /// The output of a command that ran **and exited zero**.
    ///
    /// `None` for a non-zero exit and for [`CmdOutcome::Unavailable`] alike —
    /// use this only where those two genuinely warrant the same handling, and
    /// match on the variants where they do not.
    #[must_use]
    pub fn ok_output(&self) -> Option<&std::process::Output> {
        match self {
            CmdOutcome::Ran(o) if o.status.success() => Some(o),
            _ => None,
        }
    }

    /// Stdout of a successful run, decoded lossily and trimmed.
    ///
    /// The convenience the old `trimmed_stdout()` provided, now reachable only
    /// after the success/unavailable distinction has been made. **Not** for
    /// column-sensitive output — see the module docs on porcelain.
    #[must_use]
    pub fn ok_stdout_trimmed(&self) -> Option<String> {
        self.ok_output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    /// Stdout decoded lossily, **untrimmed**, regardless of exit status.
    ///
    /// The faithful replacement for `GhResult::stdout`. Untrimmed because some
    /// formats are column- or line-sensitive (porcelain v1's leading space,
    /// anything consumed with `.lines()`), and trimming them at the boundary
    /// changes what they mean — see the module docs.
    #[must_use]
    pub fn stdout_lossy(&self) -> String {
        match self {
            CmdOutcome::Ran(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            CmdOutcome::Unavailable(Unavailable::TimedOut { partial_stdout, .. }) => {
                String::from_utf8_lossy(partial_stdout).to_string()
            }
            CmdOutcome::Unavailable(_) => String::new(),
        }
    }

    /// Stdout decoded lossily and trimmed, **regardless of exit status**.
    ///
    /// The escape hatch, not the default. [`Self::ok_stdout_trimmed`] is the
    /// accessor to reach for: it forces the success question to be answered
    /// first, which is exactly what `GhResult::trimmed_stdout()` let callers
    /// skip. Use this only where the output is wanted even from a failed run —
    /// a diagnostic, or a command whose partial output is still informative.
    #[must_use]
    pub fn stdout_trimmed(&self) -> String {
        match self {
            CmdOutcome::Ran(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            CmdOutcome::Unavailable(Unavailable::TimedOut { partial_stdout, .. }) => {
                String::from_utf8_lossy(partial_stdout).trim().to_string()
            }
            CmdOutcome::Unavailable(_) => String::new(),
        }
    }

    /// Stderr, decoded lossily and trimmed, for diagnostics.
    #[must_use]
    pub fn stderr_trimmed(&self) -> String {
        match self {
            CmdOutcome::Ran(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
            CmdOutcome::Unavailable(u) => u.to_string(),
        }
    }

    /// Whether the command ran and exited zero.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        matches!(self, CmdOutcome::Ran(o) if o.status.success())
    }

    /// A human-readable account of a non-success, for logs and error messages.
    ///
    /// Names *which* kind of failure it was, because "could not ask" and "asked
    /// and got no" need different operator responses.
    #[must_use]
    pub fn failure_reason(&self, what: &str) -> String {
        match self {
            CmdOutcome::Ran(o) if o.status.success() => format!("`{what}` succeeded"),
            CmdOutcome::Ran(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let stderr = stderr.trim();
                if stderr.is_empty() {
                    format!("`{what}` exited with {}", o.status)
                } else {
                    format!("`{what}` exited with {}: {stderr}", o.status)
                }
            }
            CmdOutcome::Unavailable(u) => format!("`{what}` {u}"),
        }
    }
}

/// The meaning of a JSON query's result, with the four cases the old
/// `success && !is_empty()` idiom merged kept separate.
#[derive(Debug)]
pub enum Query<T> {
    /// Ran, exited zero, decoded, and there is something there.
    Populated(T),

    /// Ran, exited zero, decoded — and the result is legitimately empty.
    ///
    /// **A fact, not a failure.** "No open PRs for this branch" is an answer.
    Empty,

    /// Ran and exited zero, but the output was not what was asked for.
    ///
    /// Previously indistinguishable from `Empty`, because a `--jq` filter that
    /// matched nothing and a `gh` that printed something unexpected both left
    /// an empty-ish string.
    Malformed { raw: Vec<u8>, error: String },

    /// Ran and exited non-zero. The forge answered, and the answer was no.
    Failed { status: String, stderr: String },

    /// Could not be run, or did not finish. **Unknown**, never "no".
    Unavailable(Unavailable),
}

impl<T> Query<T> {
    /// The decoded value, if the query both ran and found something.
    ///
    /// Collapses the four non-populated cases — acceptable only where a caller
    /// genuinely treats "empty", "malformed", "failed" and "unknown" alike.
    #[must_use]
    pub fn value(self) -> Option<T> {
        match self {
            Query::Populated(v) => Some(v),
            _ => None,
        }
    }

    /// True only for [`Query::Empty`] — a definite, successful "nothing".
    #[must_use]
    pub fn is_definitely_empty(&self) -> bool {
        matches!(self, Query::Empty)
    }

    /// True when no answer was obtained: malformed, failed, or unavailable.
    ///
    /// The predicate the old idiom could not express, because it had already
    /// merged this with [`Query::Empty`].
    #[must_use]
    pub fn is_unanswered(&self) -> bool {
        matches!(self, Query::Malformed { .. } | Query::Failed { .. } | Query::Unavailable(_))
    }
}

/// Run `program` with `args` in `dir`, bounded by `timeout`.
///
/// `stdin` is nulled: nothing here reads input, and leaving it inherited lets a
/// `git` or `gh` that decides to prompt (credentials, a pager) block on the
/// operator's terminal forever — a hang the deadline would then have to mop up.
pub fn run(program: &str, args: &[&str], dir: &Path, timeout: Duration) -> CmdOutcome {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(dir).stdin(Stdio::null());
    finish(cmd, timeout)
}

/// Run an already-configured [`Command`] under `timeout`.
///
/// For call sites that need env vars, a different program resolution, or args
/// built in several steps. `stdin` is the caller's responsibility here.
pub fn run_command(cmd: Command, timeout: Duration) -> CmdOutcome {
    finish(cmd, timeout)
}

fn finish(cmd: Command, timeout: Duration) -> CmdOutcome {
    match run_bounded(cmd, timeout) {
        Ok(Completion::Exited(out)) => CmdOutcome::Ran(out),
        Ok(Completion::TimedOut { stdout, .. }) => CmdOutcome::Unavailable(Unavailable::TimedOut {
            after: timeout,
            partial_stdout: stdout,
        }),
        Err(ExecError::Spawn(e)) => CmdOutcome::Unavailable(Unavailable::Spawn(e.to_string())),
        Err(ExecError::Collect(e)) => CmdOutcome::Unavailable(Unavailable::Collect(e.to_string())),
    }
}

/// Decode a command's stdout as JSON into `T`, classifying the result.
///
/// `is_empty` decides whether a successfully-decoded value counts as
/// [`Query::Empty`]. It is a caller-supplied predicate rather than a
/// `T: IsEmpty` bound because emptiness is query-specific: an empty array means
/// "no matches", while a struct whose one field is `""` may mean the record
/// exists but the field is unset — a different fact.
pub fn decode_json<T, F>(outcome: CmdOutcome, is_empty: F) -> Query<T>
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(&T) -> bool,
{
    let out = match outcome {
        CmdOutcome::Unavailable(u) => return Query::Unavailable(u),
        CmdOutcome::Ran(o) => o,
    };

    if !out.status.success() {
        return Query::Failed {
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        };
    }

    // A zero exit with no output at all is a successful empty result, not
    // malformed JSON — `gh` prints nothing for some empty queries rather than
    // `[]`, and reporting that as a decode error would be a false alarm.
    if out.stdout.iter().all(u8::is_ascii_whitespace) {
        return Query::Empty;
    }

    match serde_json::from_slice::<T>(&out.stdout) {
        Ok(v) if is_empty(&v) => Query::Empty,
        Ok(v) => Query::Populated(v),
        Err(e) => Query::Malformed {
            raw: out.stdout,
            error: e.to_string(),
        },
    }
}

/// Run a `gh` JSON query and decode it, in one step.
///
/// Callers pass `--json <fields>` and **no `--jq`** — the whole point is that
/// the structure survives into Rust instead of being flattened to a scalar by a
/// second interpreter inside the subprocess.
pub fn gh_json<T, F>(
    gh: &Path,
    args: &[&str],
    dir: &Path,
    timeout: Duration,
    is_empty: F,
) -> Query<T>
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(&T) -> bool,
{
    debug_assert!(
        !args.contains(&"--jq"),
        "gh_json decodes in-process; passing --jq flattens the JSON in the subprocess \
         and reintroduces the ambiguity this exists to remove"
    );
    let mut cmd = Command::new(gh);
    cmd.args(args).current_dir(dir).stdin(Stdio::null());
    decode_json(finish(cmd, timeout), is_empty)
}

#[cfg(test)]
mod tests;
