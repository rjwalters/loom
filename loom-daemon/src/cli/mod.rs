//! `loom-daemon` CLI subcommand handlers (Issue #4712).
//!
//! `main.rs` keeps only the clap `Cli`/`Commands` derive tree and the thin
//! dispatch match (`handle_cli_command`); every subcommand's handler body
//! lives in one of these submodules, grouped by family. `daemon_service`
//! (a sibling of this module, not nested under it) owns the daemon's own
//! bootstrap/service-loop body, which is not a CLI subcommand handler.

pub(crate) mod accounts;
pub(crate) mod cancel;
pub(crate) mod cleanup_ops;
pub(crate) mod common;
pub(crate) mod dispatch;
pub(crate) mod health;
pub(crate) mod legacy_script_cmds;
pub(crate) mod misc_cmds;
pub(crate) mod quarantine;
pub(crate) mod restart;
pub(crate) mod serve_cmd;
pub(crate) mod stats;
pub(crate) mod status;
pub(crate) mod status_render;
pub(crate) mod tokens;
pub(crate) mod watch;
pub(crate) mod workspace_fleet;
