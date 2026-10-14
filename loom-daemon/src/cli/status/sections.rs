//! `loom-daemon status` arguments and the `--section` path (Issue #10787).
//!
//! `status --json --section daemon_build,auto_update` asks the daemon for
//! only those top-level sections (`Request::DaemonStatusSections`), so it
//! skips the `O(roots)` per-root walk, and skips this CLI's own collectors
//! (the per-account token probe, the git staleness check, the watchdog
//! protection probe, the worktree disk walk) unless a selected section reads
//! them. Without `--section` every helper here answers "everything", and the
//! request is the unchanged `Request::DaemonStatus`.

use anyhow::anyhow;
use loom_daemon::daemon_install_state::{self, ProtectionReport};
use loom_daemon::errors::ErrorCode;
use loom_daemon::self_update::{self, SelfUpdateStatus};
use loom_daemon::status_section::{SectionSet, StatusSection};
use loom_daemon::types::{DaemonStatusReport, Request, Response};

/// Arguments of `loom-daemon status`.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct StatusArgs {
    /// Emit machine-readable JSON instead of the human-readable table.
    #[arg(long)]
    pub json: bool,

    /// Also show the forge-side pipeline snapshot per managed repo (Issue
    /// #3977): open, dispatchable `loom:issue` (queued — park-labeled
    /// rows excluded, #4825), open `loom:building`
    /// (claimed), open PRs by `loom:review-requested` /
    /// `loom:changes-requested` / `loom:pr`, and PRs merged in the last
    /// 24h. Opt-in because it makes several `gh` calls per managed repo
    /// (client-side, after the fast IPC round-trip) rather than being
    /// bundled into the default view.
    #[arg(long)]
    pub pipeline: bool,

    /// Override the status IPC round-trip budget in seconds (Issue
    /// #6011). Without this, the timeout defaults to 5s on an unloaded
    /// host and scales up automatically with observed 1-minute load
    /// average (capped at 30s) — a saturated host that is merely slow to
    /// answer, not actually wedged, would otherwise be misclassified as
    /// unreachable before it ever got a chance to respond. Also
    /// overridable via `LOOM_DAEMON_IPC_TIMEOUT_MS` (shared with
    /// `dispatch`'s ack budget) as a raise-only floor; this flag takes
    /// precedence over both when set.
    #[arg(long)]
    pub timeout_secs: Option<u64>,

    /// Return only these top-level sections of the `--json` payload (Issue
    /// #10787). Comma-separated, repeatable; requires `--json`. Each name is
    /// a top-level key, or a group of keys named for its base key
    /// (`in_flight` = `in_flight` + `in_flight_count`; `preflight_advisory`,
    /// `observability` and `role_runner` group their `*_`-prefixed keys).
    /// The daemon builds only what the named sections need — `daemon_build`,
    /// `auto_update` and the other process-level sections skip the
    /// per-repo walk entirely, so they answer quickly on a host with many
    /// registered repos — and the CLI skips its client-side probes for
    /// unselected sections. `pipeline` still needs `--pipeline` (else
    /// `null`). The exit code for autonomy mismatch applies only when
    /// `protection` is selected. An unreachable daemon gets the usual
    /// unreachable payload (`--section` ignored); a daemon older than this
    /// CLI is an error (exit 1). Without `--section` the output is unchanged.
    #[arg(
        long = "section",
        value_name = "SECTION",
        value_enum,
        value_delimiter = ',',
        requires = "json"
    )]
    pub section: Vec<StatusSection>,
}

impl StatusArgs {
    /// The sections this invocation serves: all of them without `--section`.
    pub(crate) fn selection(&self) -> SectionSet {
        if self.section.is_empty() {
            SectionSet::all()
        } else {
            SectionSet::only(self.section.iter().copied())
        }
    }
}

/// The IPC request for `selected`: the unchanged `DaemonStatus` for the full
/// set, `DaemonStatusSections` otherwise.
pub(crate) fn request(selected: &SectionSet) -> Request {
    match selected.sections() {
        None => Request::DaemonStatus,
        Some(sections) => Request::DaemonStatusSections { sections },
    }
}

/// The daemon could not parse the request: it predates
/// `Request::DaemonStatusSections`, or one of the requested sections.
#[derive(Debug)]
pub(crate) struct DaemonTooOld {
    detail: String,
}

impl std::fmt::Display for DaemonTooOld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "daemon too old for --section (needs a daemon at least as new as this CLI, \
             loom-daemon {}); run without --section, or restart the daemon onto this build \
             (daemon said: {})",
            env!("CARGO_PKG_VERSION"),
            self.detail
        )
    }
}

impl std::error::Error for DaemonTooOld {}

/// The error for a status reply that is not a report. A request-parse
/// failure naming an unknown variant can only mean the daemon does not know
/// `DaemonStatusSections` (or a section name) — every daemon knows
/// `DaemonStatus` — so it becomes [`DaemonTooOld`].
pub(crate) fn unexpected_response(other: Response) -> anyhow::Error {
    match other {
        Response::StructuredError(e)
            if e.code.0 == ErrorCode::IPC_PROTOCOL_ERROR
                && e.message.contains("unknown variant") =>
        {
            anyhow::Error::new(DaemonTooOld { detail: e.message })
        }
        other => anyhow!("unexpected response: {other:?}"),
    }
}

/// Whether a status query failed because the daemon is too old for it.
pub(crate) fn is_daemon_too_old(e: &anyhow::Error) -> bool {
    e.downcast_ref::<DaemonTooOld>().is_some()
}

/// Report a [`DaemonTooOld`] failure and exit 1. The daemon answered, so this
/// is not the unreachable-daemon path and gets none of its exit codes.
pub(crate) fn exit_daemon_too_old(e: &anyhow::Error) -> ! {
    eprintln!("Error: {e}");
    std::process::exit(1)
}

/// The per-account token probe (a network call per account), when
/// `token_usage` or `capacity` is selected.
pub(crate) fn token_usage(
    selected: &SectionSet,
    report: &DaemonStatusReport,
) -> Option<serde_json::Value> {
    if !selected.needs_token_probe() {
        return None;
    }
    super::collect_token_usage(report.token_pool_dir.as_deref())
}

/// The git staleness check, when `self_update` is selected; otherwise a
/// placeholder that is never rendered.
pub(crate) fn self_update_status(selected: &SectionSet) -> SelfUpdateStatus {
    if selected.has(StatusSection::SelfUpdate) {
        return self_update::check();
    }
    SelfUpdateStatus {
        built_commit: self_update::BUILT_COMMIT.to_string(),
        source_commit: None,
        update_available: None,
        commits_behind: None,
        hours_behind: None,
    }
}

/// The watchdog protection probe, when `protection` is selected.
pub(crate) fn protection(selected: &SectionSet) -> Option<ProtectionReport> {
    if selected.has(StatusSection::Protection) {
        daemon_install_state::probe_protection()
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
