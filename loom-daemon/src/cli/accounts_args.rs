//! clap argument definitions for `loom-daemon accounts` (issue #8672).
//!
//! Extracted from `main.rs` so the `accounts` command family can keep growing
//! without pushing `main.rs` past its `scripts/file-size-baseline.txt` entry —
//! the "put the new code in a NEW sibling module" rule from
//! `.loom/docs/file-size-policy.md`. Pure relocation: the derive tree below is
//! byte-for-byte what `main.rs` carried, plus the new `Provision` sub-action.
//!
//! `main.rs` re-exports the enum (`pub(crate) use`), so `crate::AccountsAction`
//! continues to resolve for every existing caller. `SessionAction` lives in its
//! own sibling, `cli::accounts_session`, alongside its handler.

use clap::Subcommand;
use std::path::PathBuf;

use super::accounts_session::SessionAction;

/// Sub-actions for `loom-daemon accounts`.
#[derive(Subcommand)]
pub(crate) enum AccountsAction {
    /// Create a named profile and run the provider's interactive login.
    Add {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        device_auth: bool,
        /// Register this account's email as an alternate lookup key (issue
        /// #7389) -- `loom-daemon accounts session <action>`/`codex-agent`
        /// then accept either `NAME` or this email as `<account>`. Profile
        /// names should stay short identifiers; put the email here instead
        /// of in `NAME`, which is never sanitized against `docker run --name`.
        #[arg(long, value_name = "EMAIL")]
        email: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Import an explicit opaque Codex auth file into a new named profile.
    Import {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long, value_name = "PATH")]
        auth_file: PathBuf,
        /// Register this account's email as an alternate lookup key (issue
        /// #7389) -- see `accounts add --email`.
        #[arg(long, value_name = "EMAIL")]
        email: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List registered accounts and secret-free structural diagnostics.
    List {
        #[arg(long, value_name = "PROVIDER", default_value = "codex")]
        provider: String,
        #[arg(long)]
        json: bool,
    },
    /// Report each account's **availability** — quota headroom and reset
    /// horizon — the `tokens check` analogue for the Codex pool (issue
    /// #8407). Reads each profile's own recorded rate-limit snapshot; makes
    /// no API call and starts no `codex` process.
    Check {
        #[arg(long, value_name = "PROVIDER", default_value = "codex")]
        provider: String,
        /// Persist what the probe learned: write the provider-namespaced
        /// ranking file and feed each conclusive reading into account health,
        /// where selection already consults it. Without this flag the command
        /// is a pure read.
        #[arg(long)]
        ranking: bool,
        #[arg(long)]
        json: bool,
    },
    /// Probe one account's structural and login status.
    Status {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Make an account ineligible without changing its credential state.
    Disable {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Restore eligibility after structural and permission validation.
    Enable {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Reauthenticate the existing canonical profile in place.
    Reauth {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        device_auth: bool,
        #[arg(long)]
        json: bool,
    },
    /// Retire to private quarantine, or irreversibly delete with `--purge`.
    Remove {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        /// Irreversibly delete credential state instead of quarantining it.
        #[arg(long)]
        purge: bool,
        #[arg(long)]
        json: bool,
    },
    /// Per-account session-container lifecycle (issue #6925, Epic #6896
    /// Phase 2): a long-lived `loom-worker-session` container that owns the
    /// account's `CODEX_HOME` volume, persisting the Codex auth-refresh
    /// chain across daemon restarts and serializing every refresh through
    /// one owning process.
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Move a profile directory and its registry entry to a new name (issue
    /// #7401). Refuses when the account is session-managed; stop its
    /// session container first.
    Rename {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "OLD_NAME")]
        old_name: String,
        #[arg(value_name = "NEW_NAME")]
        new_name: String,
        #[arg(long)]
        json: bool,
    },
    /// Register an on-disk, credentialed profile directory that predates (or
    /// was created outside of) the registry (issue #7401) — the supported
    /// recovery path once `.loom/accounts.json` exists, since directory
    /// discovery stops at that point.
    Adopt {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Populate a pooled profile from the operator's **default** profile
    /// (issue #8672), so a rotated account is not a blank install: symlink
    /// the capability and session trees, copy the plain files, key-merge the
    /// settings documents under a credential/identity denylist, and install
    /// the managed `pre_tool_use` hook bridge.
    ///
    /// Idempotent, and never destructive: `<profile>/.loom-profile.json`
    /// records what Loom wrote, so a surface an operator has since edited by
    /// hand is theirs permanently. Credentials (`auth.json`), identity keys,
    /// and per-project trust state are never read or written.
    ///
    /// `accounts add`/`import` run this automatically; so does daemon
    /// startup, for every pooled profile.
    Provision {
        #[arg(long, value_name = "PROVIDER", default_value = "codex")]
        provider: String,
        /// The account to provision. Omit it and pass `--all` to do every
        /// pooled profile in the registry.
        #[arg(value_name = "NAME")]
        name: Option<String>,
        /// Provision every registered account for this provider.
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// The default profile to provision FROM. Defaults to
        /// `LOOM_CODEX_DEFAULT_HOME`, else `~/.codex`. Deliberately never
        /// inferred from the ambient `CODEX_HOME`, which a dispatched agent
        /// already has pointed at a *pooled* profile.
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Report what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Provision the surfaces but skip the managed hook-bridge install.
        #[arg(long)]
        skip_hooks: bool,
        #[arg(long)]
        json: bool,
    },
}
