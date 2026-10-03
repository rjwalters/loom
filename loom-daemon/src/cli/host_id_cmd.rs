//! `loom-daemon host-id` (Issue #10023): print this host's identity.
//!
//! The single resolution every shell exporter on the host calls instead of
//! re-deriving `LOOM_HOST_ID` / `$HOSTNAME` / `hostname` in bash, so a
//! script's `host_id` is byte-identical to the daemon's `host.id`. The rule
//! lives in [`loom_daemon::host_identity`].
//!
//! Contract: exit `0` with exactly one line on stdout — the id (or, with
//! `--source`, `<id>\t<source>`; with `--json`, one JSON object). A host with
//! no resolvable identity prints `unknown-host`, still exit `0`: that sentinel
//! is a value, not an error. Callers fall back to `${LOOM_HOST_ID:-unknown-host}`
//! only when this subcommand itself is unavailable (an older binary exits `2`
//! with clap's "unrecognized subcommand").

use anyhow::Result;

#[derive(clap::Args)]
pub(crate) struct HostIdArgs {
    /// Also print where the id came from (`env`, `config`, `persisted`,
    /// `generated`, `unknown`), tab-separated.
    #[arg(long, conflicts_with = "json")]
    pub(crate) source: bool,
    /// Print `{"host_id", "source", "path"}` as one JSON object.
    #[arg(long)]
    pub(crate) json: bool,
}

impl HostIdArgs {
    pub(crate) fn run(self) -> Result<()> {
        let resolved = loom_daemon::host_identity::resolve();
        if self.json {
            println!(
                "{}",
                serde_json::json!({
                    "host_id": resolved.id,
                    "source": resolved.source.as_str(),
                    "path": resolved.path.map(|p| p.to_string_lossy().into_owned()),
                })
            );
        } else if self.source {
            println!("{}\t{}", resolved.id, resolved.source.as_str());
        } else {
            println!("{}", resolved.id);
        }
        Ok(())
    }
}
