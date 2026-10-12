//! `loom-daemon host-id` (Issue #11530): print this host's identity.
//!
//! The resolution lives in [`loom_daemon::host_identity`]: `fleet.hostId` in
//! the machine-tier config, then `$LOOM_HOST_ID`, then the persisted
//! `~/.loom/host-id` (created on first use). Never the hostname.
//!
//! Contract: exit `0` with exactly one line on stdout, the id. `--source`
//! additionally names where it came from (`config`, `env`, `persisted`,
//! `generated`) on **stderr**, so stdout stays the bare id.
//!
//! **No identity is an error, not a value.** When nothing resolves (the
//! persisted id can be neither read nor created) this prints nothing on
//! stdout, explains on stderr and exits non-zero. `unknown-host` is the same
//! string on every such host, so a shell caller that adopted it would collide
//! fleet-wide (#5063).

use anyhow::{bail, Result};
use loom_daemon::host_identity::{HostIdSource, CONFIG_KEY, HOST_ID_ENV};

#[derive(clap::Args)]
pub(crate) struct HostIdArgs {
    /// Also print where the id came from (`config`, `env`, `persisted`,
    /// `generated`) on stderr.
    #[arg(long)]
    pub(crate) source: bool,
}

impl HostIdArgs {
    pub(crate) fn run(self) -> Result<()> {
        let resolved = loom_daemon::host_identity::resolve();
        if resolved.source == HostIdSource::Unknown {
            let at = resolved
                .path
                .as_ref()
                .map_or_else(|| "no home directory".to_string(), |p| p.display().to_string());
            bail!(
                "no host identity: {CONFIG_KEY} and ${HOST_ID_ENV} are unset and the persisted id \
                 ({at}) could not be read or created. Set {CONFIG_KEY} or ${HOST_ID_ENV}, or fix \
                 that path (#11530)."
            );
        }
        println!("{}", resolved.id);
        if self.source {
            eprintln!("source: {}", resolved.source.as_str());
        }
        Ok(())
    }
}
