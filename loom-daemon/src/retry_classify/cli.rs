//! The CLI boundary for the wrapper's retry classifiers (#8037).
//!
//! # What is contract here
//!
//! `claude-wrapper.sh` invokes this per failed attempt and branches on the exit
//! status, so three things are frozen:
//!
//! - **exit codes**: `0` the predicate is TRUE, `1` it is FALSE, `2` a usage
//!   error. `0`/`1` are an *answer*; anything else means the question could not
//!   be asked, and the wrapper falls back to its documented fail-safe rather
//!   than reading a failure as `false`. This is why a usage error may not reuse
//!   `1` — an old binary that does not know this subcommand exits `2`, and must
//!   never look like "not exhausted".
//! - **stdout**: `transient` prints the category the verdict was derived from
//!   (`_LAST_ERROR_CLASSIFICATION`, #4501); `wait-time` prints the number of
//!   seconds; every other subcommand prints nothing.
//! - **stdin**: the child's captured output arrives on stdin, never in argv. It
//!   is routinely megabytes of CLI transcript, which argv cannot hold.
//!
//! `defaults/scripts/tests/test-claude-wrapper-retry.sh` drives all of it
//! through the shell functions and was kept rather than translated: assertions
//! written against the shell implementation still passing against this one is
//! the equivalence evidence.

#[cfg(test)]
mod tests;

use super::{calculate_wait_time, is_account_auth_dead, is_account_exhaustion};
use super::{is_account_session_limit, is_mcp_error, is_transient, Backoff, Input};

/// Exit code for a usage error — deliberately distinct from the FALSE verdict.
pub const EX_USAGE: i32 = 2;

/// Which predicate is being asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sub {
    Transient,
    AccountExhaustion,
    AuthDead,
    SessionLimit,
    McpError,
    WaitTime,
}

/// The parsed argv.
#[derive(Debug, Clone, Default)]
pub struct Opts {
    /// The child's exit code. Defaults to 1, matching the shell's `${2:-1}`.
    pub exit_code: i32,
    /// `classify_error`'s category. `None` selects the degraded path, and is a
    /// state rather than a missing value: it means `lib/classify-error.sh` was
    /// not sourced by the caller.
    pub classification: Option<String>,
    /// `classification_is_transient`'s verdict for that category.
    pub classification_is_transient: bool,
    /// `wait-time`: which attempt is about to be waited out.
    pub attempt: i64,
    /// `wait-time`: the wrapper's `INITIAL_WAIT` / `MULTIPLIER` / `MAX_WAIT`.
    pub initial_wait: i64,
    pub multiplier: i64,
    pub max_wait: i64,
}

/// Runs one predicate and returns the process exit code.
///
/// `output` is the child transcript read from stdin (empty when none was
/// piped).
#[must_use]
pub fn run(sub: Sub, opts: &Opts, output: &str) -> i32 {
    if sub == Sub::WaitTime {
        let backoff = Backoff {
            initial_wait: opts.initial_wait,
            multiplier: opts.multiplier,
            max_wait: opts.max_wait,
        };
        println!("{}", calculate_wait_time(opts.attempt, &backoff));
        return 0;
    }

    let input = Input {
        output,
        exit_code: opts.exit_code,
        classification: opts.classification.as_deref(),
        classification_is_transient: opts.classification_is_transient,
    };

    let verdict = match sub {
        Sub::Transient => {
            let t = is_transient(&input);
            // Printed before the exit code is chosen so the caller can cache
            // the category it acted on even when the answer is "do not retry".
            println!("{}", t.classification);
            t.retry
        }
        Sub::AccountExhaustion => is_account_exhaustion(&input),
        Sub::AuthDead => is_account_auth_dead(&input),
        Sub::SessionLimit => is_account_session_limit(&input),
        Sub::McpError => is_mcp_error(output),
        Sub::WaitTime => unreachable!("handled above"),
    };

    i32::from(!verdict)
}
