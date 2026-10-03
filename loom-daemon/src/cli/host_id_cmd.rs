//! `loom-daemon host-id` (Issue #10023): print this host's identity.
//!
//! The single resolution every shell exporter on the host calls instead of
//! re-deriving `LOOM_HOST_ID` / `$HOSTNAME` / `hostname` in bash, so a
//! script's `host_id` is byte-identical to the daemon's `host.id`. The rule
//! lives in [`loom_daemon::host_identity`].
//!
//! Contract: exit `0` with exactly one line on stdout — the id (or, with
//! `--source`, `<id>\t<source>`; with `--json`, one JSON object).
//!
//! **No identity is an error, not a value.** When nothing resolves (no
//! `LOOM_HOST_ID`, no `fleet.hostId`, and the persisted id can be neither read
//! nor created — no home directory, a read-only `~/.loom`, any I/O error) the
//! plain form prints nothing on stdout, explains on stderr and exits `1`;
//! `--source` / `--json` still print their diagnostic line (source `unknown`)
//! and also exit `1`. `unknown-host` is the same string on every such host, so
//! a caller that published or compared under it would collide fleet-wide
//! (#5063). Callers fall back to `$LOOM_HOST_ID` only when this subcommand is
//! unavailable (an older binary exits `2` with clap's "unrecognized
//! subcommand"), and otherwise fail loudly.

use anyhow::{bail, Result};
use loom_daemon::host_identity::{HostIdSource, HOST_ID_ENV};

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
        let unknown = resolved.source == HostIdSource::Unknown;
        if self.json {
            println!(
                "{}",
                serde_json::json!({
                    "host_id": resolved.id,
                    "source": resolved.source.as_str(),
                    "path": resolved.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                })
            );
        } else if self.source {
            println!("{}\t{}", resolved.id, resolved.source.as_str());
        } else if !unknown {
            println!("{}", resolved.id);
        }
        if unknown {
            let at = resolved
                .path
                .as_ref()
                .map_or_else(|| "no home directory".to_string(), |p| p.display().to_string());
            bail!(
                "no host identity: ${HOST_ID_ENV} and fleet.hostId are unset and the persisted id \
                 ({at}) could not be read or created. Set ${HOST_ID_ENV} or fix that path (#10023)."
            );
        }
        Ok(())
    }
}
