//! The per-session environment both spawn paths export: the isolated
//! `CLAUDE_CONFIG_DIR` / `TMPDIR`, and what keeps an agent's own `gh` calls
//! in the host's forge-call ledger (W5).
//!
//! # Why the ledger needs this
//!
//! Every session gets `TMPDIR=<CLAUDE_CONFIG_DIR>/tmp`. The forge-call sink
//! defaults to `${TMPDIR:-/tmp}/loom-forge-call-stats`, so the agent `gh`
//! front ([`crate::agent_gh::ledger`]) — which runs inside the session —
//! would write its rows under the session's private tmp, a directory no
//! host rollup (`loom-daemon forge calls`, the status row) ever reads. The
//! spawner therefore exports the sink directory **it** resolved through
//! `LOOM_FORGE_CALL_STATS_DIR`, the override every reader and writer already
//! honours: wherever `TMPDIR` is delivered, the sink directory is too. A
//! sink the spawner has disabled is exported as `off`, so the session does
//! not quietly write somewhere else instead.
//!
//! `LOOM_GH_BOOKED` is the facade's "this `gh` child is already a ledger
//! row" marker ([`crate::gh_invocation::BOOKED_ENV`]). It must only ever be
//! set by the facade on its own child. A session that inherited `1` from
//! whatever started it would have every one of its real `gh` calls skipped
//! by the front, so the spawner blanks it. Blank rather than unset, for the
//! reason `CLAUDECODE` is blanked: tmux's `-u` would let the server
//! environment's value leak back in.

use super::{set_session_env, sh_escape, AgentEnv};
use std::path::Path;

/// The sink-directory override the forge-call ledger honours.
pub const SINK_DIR_ENV: &str = "LOOM_FORGE_CALL_STATS_DIR";

/// The value that disables the sink.
const SINK_OFF: &str = "off";

/// `(name, value)` of the two ledger variables: the host sink directory as
/// this process resolves it (absolute; `off` when disabled), and a blank
/// `LOOM_GH_BOOKED`.
#[must_use]
pub fn ledger_vars() -> [(&'static str, String); 2] {
    let sink = crate::forge_call_stats::host_sink_dir().map_or_else(
        || SINK_OFF.to_string(),
        |dir| {
            std::path::absolute(&dir)
                .unwrap_or(dir)
                .to_string_lossy()
                .into_owned()
        },
    );
    [
        (SINK_DIR_ENV, sink),
        (crate::gh_invocation::BOOKED_ENV, String::new()),
    ]
}

/// Every variable a session is given for `config_dir`, in export order.
#[must_use]
pub fn vars(config_dir: &Path) -> Vec<(&'static str, String)> {
    let mut vars = vec![
        ("CLAUDE_CONFIG_DIR", config_dir.to_string_lossy().into_owned()),
        ("TMPDIR", config_dir.join("tmp").to_string_lossy().into_owned()),
    ];
    vars.extend(ledger_vars());
    vars
}

/// Set [`vars`] on the tmux session.
pub fn export(env: &dyn AgentEnv, session: &str, config_dir: &Path) {
    for (key, value) in vars(config_dir) {
        set_session_env(env, session, key, &value);
    }
}

/// [`ledger_vars`] as a shell assignment prefix (`K='v' K2='' `), for the
/// command line that also carries `TMPDIR`.
#[must_use]
pub fn ledger_prefix() -> String {
    ledger_vars()
        .iter()
        .map(|(key, value)| format!("{key}='{}' ", sh_escape(value)))
        .collect()
}

#[cfg(test)]
#[path = "isolation_tests.rs"]
mod tests;
