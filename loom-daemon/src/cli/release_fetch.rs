//! `loom-daemon release-fetch` (epic #7810, PR 6a).
//!
//! Backs `loom-daemon-update.sh`'s `fetch_and_verify_artifact`, which now
//! delegates here exactly as `--resolve-json` delegates to
//! `loom-daemon release-resolve` (#7977). Human-facing progress/verdict lines
//! go to **stderr**, worded exactly like the shell functions they replace --
//! `test-loom-daemon-update-fetch.sh`'s assertions grep for them unchanged
//! (this deliberately moves the shell original's `ok()` lines, which used to
//! be plain stdout, onto stderr too — no test distinguishes the two streams,
//! and reserving stdout for the contract below is the same design
//! `--resolve-json` already uses for its JSON object). On success, EXACTLY
//! the `KEY=value` lines below go to stdout, one per line, for the shell
//! wrapper to parse back into its own globals (`ARTIFACT_BIN` and friends):
//!
//!   BIN_PATH=<path to the verified binary>
//!   TMP_DIR=<the scratch dir BIN_PATH lives in -- fold into
//!            _LOOM_FETCH_TMPDIRS for the existing EXIT trap>
//!   VERSION_OUTPUT=<the verified binary's full `--version` output>
//!   COMMIT=<its embedded commit, or empty>
//!   HAD_AUTHORITY=<true|false|empty -- see release_fetch::signature>
//!
//! Exit codes (distinct from a plain 0/1 verdict, because the shell wrapper
//! reacts differently to each):
//!   0  verified. stdout carries the KEY=value lines above.
//!   1  verification FAILED (checksum mismatch or an invalid signature) --
//!      tamper evidence, never a soft fallback (AC2/AC3). The shell wrapper
//!      exits the whole script on this code immediately, matching the
//!      pre-port shell calling `exit 1` directly from inside
//!      `fetch_and_verify_artifact` rather than returning to its caller.
//!   2  could not even download the required assets (network blip, a
//!      vanished asset, no scratch dir). The shell wrapper treats this like
//!      the pre-port function's `return 1`: ITS OWN caller prints the
//!      generic "Artifact download failed" line and exits 1.

use anyhow::Result;
use loom_daemon::release_fetch::{fetch_and_verify, FetchInputs, FetchOutcome};
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct ReleaseFetchArgs {
    /// The checkout used to resolve a checked-in cosign key
    /// (`.loom/cosign.pub` / `defaults/cosign.pub`) and as `gh`'s working
    /// directory. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub(crate) repo_root: Option<PathBuf>,

    /// The release target triple (`loom-daemon-<target>` names the asset).
    #[arg(long, value_name = "TRIPLE")]
    pub(crate) target: String,

    /// `owner/repo` to download from.
    #[arg(long, value_name = "OWNER/NAME")]
    pub(crate) repo: String,

    /// The release tag to fetch.
    #[arg(long, value_name = "TAG")]
    pub(crate) tag: String,
}

impl ReleaseFetchArgs {
    /// Never returns: exits 0/1/2 per the module contract above.
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let bin_name = format!("loom-daemon-{}", self.target);
        let sha_name = format!("{bin_name}.sha256");
        eprintln!("Downloading {bin_name} + {sha_name} from {}@{}...", self.repo, self.tag);

        let inputs = FetchInputs {
            repo_root: &root,
            target: &self.target,
            repo_slug: &self.repo,
            tag: &self.tag,
            cosign_pubkey_env: std::env::var("LOOM_DAEMON_UPDATE_COSIGN_PUBKEY").ok(),
            cosign_identity_env: std::env::var("LOOM_DAEMON_UPDATE_COSIGN_IDENTITY").ok(),
            cosign_oidc_issuer_env: std::env::var("LOOM_DAEMON_UPDATE_COSIGN_OIDC_ISSUER").ok(),
        };

        match fetch_and_verify(&inputs) {
            FetchOutcome::Verified {
                artifact,
                checksum_line,
                signature_line,
            } => {
                eprintln!("{checksum_line}");
                if !signature_line.is_empty() {
                    eprintln!("{signature_line}");
                }
                println!("BIN_PATH={}", artifact.bin_path.display());
                println!("TMP_DIR={}", artifact.tmp_dir.display());
                println!("VERSION_OUTPUT={}", artifact.version_output);
                println!("COMMIT={}", artifact.commit.unwrap_or_default());
                println!(
                    "HAD_AUTHORITY={}",
                    artifact
                        .had_authority
                        .map(|b| b.to_string())
                        .unwrap_or_default()
                );
                std::process::exit(0);
            }
            FetchOutcome::VerificationFailed { lines } => {
                for line in lines {
                    eprintln!("{line}");
                }
                std::process::exit(1);
            }
            FetchOutcome::DownloadFailed(message) => {
                eprintln!("{message}");
                std::process::exit(2);
            }
        }
    }
}
