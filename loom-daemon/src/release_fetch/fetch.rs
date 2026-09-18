//! Download + verify orchestration (epic #7810, PR 6a).
//!
//! Port of `loom-daemon-update.sh`'s `fetch_and_verify_artifact`: download
//! `loom-daemon-<target>` + its `.sha256` (required) + its `.sig`/`.pem`
//! (both best-effort) for `<tag>` from `<repo>`, verify checksum
//! (unconditional) and signature (when present), and report the verified
//! binary.
//!
//! A checksum or signature-verification FAILURE is reported as
//! [`FetchOutcome::VerificationFailed`] -- a hard abort, never a soft
//! fallback. A DOWNLOAD failure (network blip, an asset that vanished
//! between resolution and download) is [`FetchOutcome::DownloadFailed`] --
//! the caller has already committed to fetch mode by the time this runs, so
//! in practice both are fatal, but the shell wrapper (`cli/release_fetch.rs`)
//! keeps them on distinct exit codes so its own caller can tell "tamper
//! evidence" apart from "could not even ask".

use super::{checksum, signature};
use crate::cmd_out::{self, CmdOutcome};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Ceiling for a `gh release download`. Generous relative to
/// [`cmd_out::DEFAULT_TIMEOUT`] -- this downloads a real binary, not a JSON
/// query, and a slow-but-progressing transfer must not be mistaken for a
/// hang the way a wedged JSON call would be.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Inputs to one fetch-and-verify.
pub struct FetchInputs<'a> {
    pub repo_root: &'a Path,
    pub target: &'a str,
    pub repo_slug: &'a str,
    pub tag: &'a str,
    /// `LOOM_DAEMON_UPDATE_COSIGN_PUBKEY`.
    pub cosign_pubkey_env: Option<String>,
    /// `LOOM_DAEMON_UPDATE_COSIGN_IDENTITY`.
    pub cosign_identity_env: Option<String>,
    /// `LOOM_DAEMON_UPDATE_COSIGN_OIDC_ISSUER`.
    pub cosign_oidc_issuer_env: Option<String>,
}

/// The verified artifact, plus every fact the shell wrapper's own globals
/// (`ARTIFACT_BIN`, `ARTIFACT_VERSION_OUTPUT`, `ARTIFACT_COMMIT`,
/// `ARTIFACT_SIGNATURE_HAD_AUTHORITY`) need to keep working unmodified.
pub struct VerifiedArtifact {
    /// The verified binary's path, inside `tmp_dir`.
    pub bin_path: PathBuf,
    /// The scratch directory `bin_path` lives in. NOT removed by this
    /// process -- ownership transfers to the caller, which is the shell
    /// wrapper's own `_LOOM_FETCH_TMPDIRS` EXIT trap (the same one that
    /// already owns the `cargo build` JSON-log scratch file, #6160). See the
    /// module docs on [`ScratchDir`].
    pub tmp_dir: PathBuf,
    /// The verified binary's full `--version` output (the post-provision
    /// identity `verify_destination_artifact` compares against). Empty when
    /// the binary would not answer.
    pub version_output: String,
    /// The commit embedded in `version_output`, when extractable.
    pub commit: Option<String>,
    /// macOS only -- see [`signature::VerifyResult::had_authority`].
    pub had_authority: Option<bool>,
}

/// What one fetch-and-verify concluded.
pub enum FetchOutcome {
    /// Downloaded, checksum-verified, and signature-verified-or-loud-skipped.
    Verified {
        artifact: VerifiedArtifact,
        /// The `ok()`-worded checksum line, always present on this path.
        checksum_line: String,
        /// The signature module's own `ok`/`warn`-worded line -- empty on the
        /// two silent paths (no `.sig` published, an unrecognized target).
        signature_line: String,
    },
    /// A checksum mismatch or an invalid signature -- tamper evidence. `lines`
    /// are the `err()`-worded messages to print, in order, ending with the
    /// shared "the running daemon (if any) was left untouched" line.
    VerificationFailed { lines: Vec<String> },
    /// Could not even download the required assets. A single `err()`-worded
    /// line.
    DownloadFailed(String),
}

/// An RAII scratch directory: removed on ANY early return (a failed
/// download, a failed checksum, a failed signature) so an abort never leaves
/// an unverified artifact lying around -- the Rust equivalent of the shell's
/// `trap _cleanup_fetch_tmpdirs EXIT` over `_LOOM_FETCH_TMPDIRS`, scoped to
/// this one directory. [`ScratchDir::persist`] disarms cleanup for the one
/// path that needs the directory to outlive this process: a verified
/// artifact the shell still has to provision from after this process exits.
struct ScratchDir(Option<PathBuf>);

impl ScratchDir {
    fn create() -> std::io::Result<Self> {
        let base = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate = base.join(format!("loom-daemon-fetch.{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&candidate)?;
        Ok(Self(Some(candidate)))
    }

    fn path(&self) -> &Path {
        self.0.as_deref().expect("scratch dir already persisted")
    }

    /// Disarm cleanup and hand the path to the caller.
    fn persist(mut self) -> PathBuf {
        self.0.take().expect("scratch dir already persisted")
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

fn download(repo_root: &Path, repo_slug: &str, tag: &str, patterns: &[&str], dest: &Path) -> bool {
    let mut cmd = Command::new("gh");
    cmd.arg("release")
        .arg("download")
        .arg(tag)
        .arg("-R")
        .arg(repo_slug);
    for p in patterns {
        cmd.arg("-p").arg(p);
    }
    cmd.arg("-D").arg(dest).arg("--clobber");
    cmd.current_dir(repo_root).stdin(Stdio::null());
    cmd_out::run_command(cmd, DOWNLOAD_TIMEOUT).succeeded()
}

/// Best-effort download of one optional asset (a `.sig` or `.pem`): `None`
/// on any failure, including a `gh` that reports success but the file is
/// somehow not there -- a key-signed release publishes no `.pem`, and that
/// must never turn a "pattern matched nothing" download into an error.
fn download_optional(
    repo_root: &Path,
    repo_slug: &str,
    tag: &str,
    name: &str,
    dest: &Path,
) -> Option<PathBuf> {
    download(repo_root, repo_slug, tag, &[name], dest)
        .then(|| dest.join(name))
        .filter(|p| p.is_file())
}

#[cfg(unix)]
fn make_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(p) {
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(p, perms);
    }
}
#[cfg(not(unix))]
fn make_executable(_p: &Path) {}

fn read_version_output(bin_path: &Path) -> String {
    let mut cmd = Command::new(bin_path);
    cmd.arg("--version").stdin(Stdio::null());
    match cmd_out::run_command(cmd, cmd_out::DEFAULT_TIMEOUT) {
        CmdOutcome::Ran(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

const ABORT_LINE: &str = "Aborting the update; the running daemon (if any) is left untouched.";

/// Download + verify one release artifact.
#[must_use]
pub fn fetch_and_verify(inputs: &FetchInputs<'_>) -> FetchOutcome {
    let scratch = match ScratchDir::create() {
        Ok(s) => s,
        Err(e) => {
            return FetchOutcome::DownloadFailed(format!(
                "Could not create a temp dir for the artifact download: {e}"
            ))
        }
    };

    let bin_name = format!("loom-daemon-{}", inputs.target);
    let sha_name = format!("{bin_name}.sha256");
    let sig_name = format!("{bin_name}.sig");
    let cert_name = format!("{bin_name}.pem");

    if !download(
        inputs.repo_root,
        inputs.repo_slug,
        inputs.tag,
        &[&bin_name, &sha_name],
        scratch.path(),
    ) {
        return FetchOutcome::DownloadFailed(format!(
            "Failed to download release assets ({bin_name}, {sha_name}) for {} from {}.",
            inputs.tag, inputs.repo_slug
        ));
    }
    let bin_path = scratch.path().join(&bin_name);
    let sha_path = scratch.path().join(&sha_name);
    if !bin_path.is_file() || !sha_path.is_file() {
        return FetchOutcome::DownloadFailed(format!(
            "Download reported success but expected files are missing under {}.",
            scratch.path().display()
        ));
    }
    make_executable(&bin_path);

    // ---- checksum: unconditional ----
    if !checksum::verify(&bin_path, &sha_path) {
        return FetchOutcome::VerificationFailed {
            lines: vec![
                format!(
                    "Checksum verification FAILED for {bin_name} -- the downloaded artifact does \
                     not match its published {sha_name}."
                ),
                ABORT_LINE.to_string(),
            ],
        };
    }
    let checksum_line = format!("Checksum verified: {bin_name} matches {sha_name}.");

    // ---- signature: best-effort download (may not exist), verify when present ----
    let sig_path = download_optional(
        inputs.repo_root,
        inputs.repo_slug,
        inputs.tag,
        &sig_name,
        scratch.path(),
    );
    let cert_path = if sig_path.is_some() {
        download_optional(
            inputs.repo_root,
            inputs.repo_slug,
            inputs.tag,
            &cert_name,
            scratch.path(),
        )
    } else {
        None
    };

    let sig_result = signature::verify(&signature::VerifyInputs {
        target: inputs.target,
        bin_path: &bin_path,
        sig_path: sig_path.as_deref(),
        cert_path: cert_path.as_deref(),
        repo_root: inputs.repo_root,
        repo_slug: inputs.repo_slug,
        tag: inputs.tag,
        cosign_pubkey_env: inputs.cosign_pubkey_env.as_deref(),
        cosign_identity_env: inputs.cosign_identity_env.as_deref(),
        cosign_oidc_issuer_env: inputs.cosign_oidc_issuer_env.as_deref(),
    });

    if sig_result.outcome == signature::Outcome::Failed {
        return FetchOutcome::VerificationFailed {
            lines: vec![
                sig_result.message,
                format!("Signature verification FAILED for {bin_name} (see above)."),
                ABORT_LINE.to_string(),
            ],
        };
    }

    let version_output = read_version_output(&bin_path);
    let commit = crate::release_resolve::semver::extract_commit(&version_output);
    let had_authority = sig_result.had_authority;
    let signature_line = sig_result.message;
    let tmp_dir = scratch.persist();

    FetchOutcome::Verified {
        artifact: VerifiedArtifact {
            bin_path,
            tmp_dir,
            version_output,
            commit,
            had_authority,
        },
        checksum_line,
        signature_line,
    }
}

#[cfg(test)]
mod tests;
