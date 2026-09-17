//! The `dep-recheck-fingerprint` subcommand (epic #7810, PR 4).
//!
//! Backs `dep-recheck-fingerprint.sh`, which is now a thin stub that `exec`s
//! into this. Its subcommands, flags, stdout keys and exit codes are contract:
//! `curator.md` invokes the stub by path and `eval`s its `KEY=VALUE` output.
//! What each answers is documented on [`loom_daemon::dep_recheck`].
//!
//! The args live here rather than in `main.rs`'s `Commands` enum for the same
//! reason `RestartArgs` and `DepClassifyCommand` do (#6969): `main.rs` is over
//! `.loom/docs/file-size-policy.md`'s threshold and frozen.

use anyhow::Result;
use loom_daemon::dep_recheck::cli;
use std::io::Read as _;
use std::path::PathBuf;

/// Flags shared by every subcommand. Grouped so each variant stays readable
/// and the parsing lives in one place.
#[derive(clap::Args, Debug, Clone, Default)]
pub(crate) struct CommonArgs {
    /// Live mode: fetch current state for this issue.
    #[arg(long, value_name = "N")]
    pub(crate) number: Option<i64>,

    /// Target repo for live mode. Defaults to the cwd's git remote.
    #[arg(long, value_name = "OWNER/NAME")]
    pub(crate) repo: Option<String>,

    /// `operator-premise` live mode: the already-extracted reference numbers,
    /// space-separated (typically `extract-refs`'s own REFS output).
    #[arg(long, value_name = "N1 N2 ...")]
    pub(crate) refs: Option<String>,

    /// Offline mode: read the JSON document on stdin instead of calling `gh`.
    #[arg(long)]
    pub(crate) stdin: bool,

    /// Emit a JSON object instead of KEY=VALUE lines.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Subcommand)]
pub(crate) enum DepRecheckCommand {
    /// The "Re-check Idempotency" fingerprint (#4986): VERDICT plus one
    /// `<pr#>:<state>:<block-label|no-block-label>:<conflicting|mergeable>`
    /// line per PR that closes the issue.
    ///
    /// The label component is deliberately narrow (#7362) and an UNKNOWN merge
    /// state fails safe to conflicting (#7281) — both so ordinary review-cycle
    /// churn never changes CONCLUSION_HASH.
    DepRecheck {
        #[command(flatten)]
        common: CommonArgs,

        /// Override the computed VERDICT. Required when `prs` is empty and the
        /// true verdict comes from curator.md's secondary heuristic rather than
        /// PR state — that case cannot be inferred here.
        #[arg(long, value_name = "blocked|clear")]
        verdict: Option<String>,

        /// Folded into CONCLUSION_HASH verbatim. A judgment call made by
        /// reading prose, so it is supplied, never computed.
        #[arg(long = "block-reason", value_name = "TEXT", default_value = "")]
        block_reason: String,

        /// The diagnosed-but-orthogonal condition's stable identity (#6516),
        /// folded into CONCLUSION_HASH verbatim. Empty leaves every existing
        /// fingerprint unaffected.
        #[arg(long, value_name = "ID", default_value = "")]
        orthogonal: String,
    },

    /// The "Checking Operator-Only Premises" fingerprint (#6849). VERDICT is
    /// `stale-premise` or `open`; CONCLUSION_HASH is left EMPTY when `open` —
    /// nothing to report this pass, so nothing to compare.
    OperatorPremise {
        #[command(flatten)]
        common: CommonArgs,
    },

    /// The `## Dependencies` checklist fingerprint (#7314) — the shape
    /// `dep-recheck` cannot see: a checklist item naming a different,
    /// non-closing issue or PR as a prerequisite.
    NamedDependency {
        #[command(flatten)]
        common: CommonArgs,
    },

    /// Reference extraction for "Checking Operator-Only Premises" (#4963).
    /// Scans the body plus any comment not authored by the automation identity
    /// and not carrying its own marker — which is what stops the
    /// self-perpetuating loop from #4507.
    ExtractRefs {
        #[command(flatten)]
        common: CommonArgs,

        /// The automation identity whose own comments are excluded. Matched
        /// case-insensitively after stripping a leading `app/` or trailing
        /// `[bot]`.
        #[arg(long = "bot-login", value_name = "LOGIN")]
        bot_login: Option<String>,
    },

    /// The four-way decision (#7617): ACTION and whether posting it requires
    /// claiming `loom:curating` first. Pure comparison — never calls `gh`, so
    /// it is always safe to run before any claim.
    Decide {
        #[command(flatten)]
        common: CommonArgs,

        /// This pass's CONCLUSION_HASH. Required; pass `--hash ''` when there
        /// is nothing to report.
        #[arg(long, value_name = "HASH")]
        hash: String,

        /// The most recent prior marker's CONCLUSION_HASH. Empty means no
        /// prior re-check comment was found.
        #[arg(long = "prior-hash", value_name = "HASH", default_value = "")]
        prior_hash: String,

        /// Age in hours of the prior marker comment. Required whenever
        /// `--prior-hash` is non-empty.
        #[arg(long = "prior-age-hours", value_name = "N")]
        prior_age_hours: Option<String>,

        /// The staleness window. Defaults to
        /// `$LOOM_DEP_RECHECK_HEARTBEAT_HOURS`, else 24.
        #[arg(long = "heartbeat-hours", value_name = "N")]
        heartbeat_hours: Option<String>,
    },
}

impl DepRecheckCommand {
    /// Never returns: exits with the subcommand's own code, which callers
    /// branch on.
    pub(crate) fn run(self) -> Result<()> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let (sub, common, mut opts) = match self {
            DepRecheckCommand::DepRecheck {
                common,
                verdict,
                block_reason,
                orthogonal,
            } => (
                cli::Sub::DepRecheck,
                common,
                cli::Opts {
                    verdict,
                    block_reason,
                    orthogonal,
                    ..Default::default()
                },
            ),
            DepRecheckCommand::OperatorPremise { common } => {
                (cli::Sub::OperatorPremise, common, cli::Opts::default())
            }
            DepRecheckCommand::NamedDependency { common } => {
                (cli::Sub::NamedDependency, common, cli::Opts::default())
            }
            DepRecheckCommand::ExtractRefs { common, bot_login } => (
                cli::Sub::ExtractRefs,
                common,
                cli::Opts {
                    bot_login,
                    ..Default::default()
                },
            ),
            DepRecheckCommand::Decide {
                common,
                hash,
                prior_hash,
                prior_age_hours,
                heartbeat_hours,
            } => (
                cli::Sub::Decide,
                common,
                cli::Opts {
                    // `--hash ''` is meaningful, so presence is recorded
                    // separately from emptiness. clap makes the flag required,
                    // which is what the shell's own HASH_SET tracked.
                    hash: Some(hash),
                    prior_hash,
                    prior_age_hours,
                    heartbeat_hours,
                    ..Default::default()
                },
            ),
        };

        opts.number = common.number;
        opts.repo = common.repo;
        opts.refs = common.refs;
        opts.stdin = common.stdin;
        opts.json = common.json;

        // Read stdin eagerly only when asked for: a subcommand in live mode
        // must not block on a terminal.
        let stdin_text = if opts.stdin {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).ok();
            Some(buf)
        } else {
            None
        };

        std::process::exit(cli::run(&cwd, sub, &opts, stdin_text.as_deref()));
    }
}
