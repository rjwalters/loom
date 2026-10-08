//! `loom-daemon install-binary` (#10983): the binary write behind
//! `provision_machine_daemon` in `scripts/install/provision-daemon.sh`.
//!
//! The script used to write the destination in place (`install -m 755`, else
//! `cp`) and kept its only backup in `/tmp` until the copy answered
//! `--version`. It now stages, checks and signs the candidate BESIDE the
//! destination and calls here for the two steps that touch the filesystem,
//! so the machine-level path gets the same write-then-rename, retained
//! previous binary and transaction record as the `LOOM_DAEMON_BIN` override
//! (`daemon_update::provision`):
//!
//! ```text
//! staged="$(loom-daemon install-binary stage <candidate> <dest>)"
//! # ... loadability check and signing, on "$staged" ...
//! loom-daemon install-binary publish "$staged" <dest>
//! ```
//!
//! Contract:
//!
//! * `stage` prints exactly one line on stdout, the staged file's path, and
//!   exits `0`. Anything else (including an older binary's clap "unrecognized
//!   subcommand", exit `2`) means nothing was staged; the script then tries
//!   its next helper candidate.
//! * `publish` exits `0` once `<dest>` is the staged file. On any failure it
//!   exits `1`, removes the staged file and leaves `<dest>` as it was.
//! * Everything human-facing goes to stderr.
//!
//! There is no restore here and no decision about whether an install was
//! healthy: see `daemon_update::provision::txn`.

use std::path::PathBuf;

use anyhow::Result;
use loom_daemon::daemon_update::{out, provision};

#[derive(clap::Subcommand)]
pub(crate) enum InstallBinaryCommand {
    /// Copy CANDIDATE to a temp file beside DEST and print that file's path.
    /// DEST is not touched.
    Stage {
        /// The binary to install.
        candidate: PathBuf,
        /// The path it will be installed at.
        dest: PathBuf,
    },
    /// Keep the binary at DEST as DEST.previous, record the install, and
    /// rename STAGED over DEST. Refuses when the previous binary cannot be
    /// kept.
    Publish {
        /// A file `stage` printed for this DEST.
        staged: PathBuf,
        /// The path to install at.
        dest: PathBuf,
    },
}

impl InstallBinaryCommand {
    /// Never returns: exits `0` on success, `1` on failure.
    pub(crate) fn run(self) -> Result<()> {
        // stdout is the `stage` contract; keep every other line off it.
        out::divert_stdout_to_stderr();
        match self {
            Self::Stage { candidate, dest } => match provision::stage(&candidate, &dest) {
                Ok(staged) => {
                    println!("{}", staged.display());
                    std::process::exit(0);
                }
                Err(e) => {
                    out::err(&provision::InstallError::Stage(e).to_string());
                    std::process::exit(1);
                }
            },
            Self::Publish { staged, dest } => match provision::publish(&staged, &dest) {
                Ok(done) => {
                    if let Some(line) = done.retained_line() {
                        out::say(&line);
                    }
                    std::process::exit(0);
                }
                Err(e) => {
                    out::err(&e.to_string());
                    std::process::exit(1);
                }
            },
        }
    }
}
