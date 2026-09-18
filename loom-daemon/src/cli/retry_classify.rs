//! The `retry-classify` subcommand (epic #7810, #8037).
//!
//! Backs the six classifiers in `claude-wrapper.sh` that #8037 ported, plus the
//! `model-class` mark-scoping decision #8138 added, all of which now delegate
//! to it.
//! What each answers is documented on [`loom_daemon::retry_classify`]; the
//! frozen argv / stdout / exit-code contract is on
//! [`loom_daemon::retry_classify::cli`].
//!
//! The args live here rather than in `main.rs`'s `Commands` enum for the same
//! reason `DepRecheckCommand` does (#6969): `main.rs` is over
//! `.loom/docs/file-size-policy.md`'s threshold and frozen.

use anyhow::Result;
use loom_daemon::retry_classify::cli;
use std::io::Read as _;

/// Flags shared by the five predicates. The wrapper passes the same shape for
/// each, so parsing lives in one place.
#[derive(clap::Args, Debug, Clone, Default)]
pub(crate) struct PredicateArgs {
    /// The child's exit code. Defaults to 1, matching the shell's `${2:-1}`.
    #[arg(long = "exit-code", value_name = "N", default_value_t = 1)]
    pub(crate) exit_code: i32,

    /// `classify_error`'s category for this failure. OMIT it to select the
    /// degraded path — that is how a caller reports that
    /// `lib/classify-error.sh` was not available, and it is deliberately not
    /// spelled as an empty string (which is an unknown category, not a missing
    /// classifier).
    #[arg(long, value_name = "CATEGORY")]
    pub(crate) classification: Option<String>,

    /// Present when `classification_is_transient` said that category is
    /// retryable. A flag rather than a value because its absence and `false`
    /// mean the same thing here, and because the verdict is supplied rather
    /// than recomputed: that deny-list is `lib/classify-error.sh`'s and is the
    /// fleet's single source of truth for retryability (#4501). Meaningful
    /// only alongside `--classification`; ignored without it.
    #[arg(long = "classification-transient")]
    pub(crate) classification_transient: bool,

    /// Read the child's captured output from stdin. Without it the output is
    /// empty — a predicate must never block on a terminal.
    #[arg(long)]
    pub(crate) stdin: bool,
}

#[derive(clap::Subcommand)]
pub(crate) enum RetryClassifyCommand {
    /// Retry, or give up (`is_transient_error`). Prints the category the
    /// verdict was derived from; exits 0 to retry, 1 to stop.
    Transient {
        #[command(flatten)]
        predicate: PredicateArgs,
    },

    /// Rotate to another account (`is_account_exhaustion`).
    AccountExhaustion {
        #[command(flatten)]
        predicate: PredicateArgs,
    },

    /// Rotate AND mark the credential dead (`is_account_auth_dead`) — a
    /// revoked/expired token, which no amount of waiting fixes.
    AuthDead {
        #[command(flatten)]
        predicate: PredicateArgs,
    },

    /// Re-select a sibling account without marking anything bad
    /// (`is_account_session_limit`) — a concurrency cap, not a quota.
    SessionLimit {
        #[command(flatten)]
        predicate: PredicateArgs,
    },

    /// An MCP/plugin failure worth a rebuild (`is_mcp_error`). Matches on
    /// output alone; `--exit-code` is accepted and ignored, as in the shell.
    McpError {
        #[command(flatten)]
        predicate: PredicateArgs,
    },

    /// How narrow the `.bad_tokens` mark for this death may be
    /// (`loom_model_class_marker`, #8058/#8138). Prints the
    /// ` [model-class:<model>]` reason suffix and exits 0 when the death is
    /// provably scoped to ONE model class; prints nothing and exits 1 when the
    /// mark must stay account-wide.
    ModelClass {
        #[command(flatten)]
        predicate: PredicateArgs,

        /// The resolved model in flight (`$LOOM_MODEL`) — the class a scoped
        /// mark would name. Empty (the default) is an ANSWER, not a missing
        /// value: it means the spawn took the session default, so there is no
        /// class to scope to and the mark stays account-wide. That is why this
        /// is not spelled like `--classification`, whose absence selects a
        /// different code path rather than a different verdict.
        #[arg(long, value_name = "MODEL", default_value = "")]
        model: String,
    },

    /// The backoff curve (`calculate_wait_time`): prints
    /// `INITIAL_WAIT * MULTIPLIER^(attempt-1)`, capped at `MAX_WAIT`.
    WaitTime {
        /// Which attempt is about to be waited out (1-based).
        #[arg(long, value_name = "N")]
        attempt: i64,

        /// The wrapper's `INITIAL_WAIT` (`LOOM_INITIAL_WAIT`).
        #[arg(long = "initial-wait", value_name = "SECONDS", default_value_t = 60)]
        initial_wait: i64,

        /// The wrapper's `MULTIPLIER` (`LOOM_BACKOFF_MULTIPLIER`).
        #[arg(long, value_name = "N", default_value_t = 2)]
        multiplier: i64,

        /// The wrapper's `MAX_WAIT` ceiling (`LOOM_MAX_WAIT`).
        #[arg(long = "max-wait", value_name = "SECONDS", default_value_t = 1800)]
        max_wait: i64,
    },
}

impl RetryClassifyCommand {
    /// Never returns: exits 0 (predicate TRUE), 1 (FALSE), or 2 (usage), which
    /// `claude-wrapper.sh` branches on.
    pub(crate) fn run(self) -> Result<()> {
        let (sub, predicate, mut opts) = match self {
            RetryClassifyCommand::Transient { predicate } => {
                (cli::Sub::Transient, Some(predicate), cli::Opts::default())
            }
            RetryClassifyCommand::AccountExhaustion { predicate } => {
                (cli::Sub::AccountExhaustion, Some(predicate), cli::Opts::default())
            }
            RetryClassifyCommand::AuthDead { predicate } => {
                (cli::Sub::AuthDead, Some(predicate), cli::Opts::default())
            }
            RetryClassifyCommand::SessionLimit { predicate } => {
                (cli::Sub::SessionLimit, Some(predicate), cli::Opts::default())
            }
            RetryClassifyCommand::McpError { predicate } => {
                (cli::Sub::McpError, Some(predicate), cli::Opts::default())
            }
            RetryClassifyCommand::ModelClass { predicate, model } => (
                cli::Sub::ModelClass,
                Some(predicate),
                cli::Opts {
                    model,
                    ..Default::default()
                },
            ),
            RetryClassifyCommand::WaitTime {
                attempt,
                initial_wait,
                multiplier,
                max_wait,
            } => (
                cli::Sub::WaitTime,
                None,
                cli::Opts {
                    attempt,
                    initial_wait,
                    multiplier,
                    max_wait,
                    ..Default::default()
                },
            ),
        };

        let mut output = String::new();
        if let Some(predicate) = predicate {
            opts.exit_code = predicate.exit_code;
            opts.classification = predicate.classification;
            opts.classification_is_transient = predicate.classification_transient;
            // Read stdin only when asked for: the child transcript is routinely
            // far too large for argv, but a predicate must not block on a
            // terminal either.
            if predicate.stdin {
                std::io::stdin().read_to_string(&mut output).ok();
            }
        }

        std::process::exit(cli::run(sub, &opts, &output));
    }
}
