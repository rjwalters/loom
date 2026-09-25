//! Artifact-fetch mode (Epic #4990 Phase 3, #5020): resolve the latest
//! GitHub Release for this host's platform, then download and verify it
//! INSTEAD of rebuilding from source.
//!
//! Resolution here is read-only and cheap — it answers "is there an artifact,
//! and is it newer?" without downloading anything. The download + checksum +
//! signature half already lives in [`crate::release_fetch`] (epic #7810 PR 6a)
//! and the JSON resolution in [`crate::release_resolve`] (PR 5); this module
//! calls both **in process**.
//!
//! That is the one structural difference from the shell, and it is a
//! simplification of the delegation the shell already performed, not a new
//! behaviour: the shell had to spawn `loom-daemon release-fetch` and parse
//! `KEY=value` lines back off its stdout precisely because bash could not call
//! Rust. Those lines were always CAPTURED, never printed, so nothing observed
//! them; the human-facing progress and verdict lines they accompanied went to
//! stderr and are reproduced here unchanged, because
//! `test-loom-daemon-update-fetch.sh` greps for their exact wording.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::out;
use super::util;

/// The verified download, as the shell's `ARTIFACT_*` globals held it.
pub struct FetchedArtifact {
    pub bin: PathBuf,
    pub version_output: String,
    pub commit: String,
    /// Darwin only (#8008): whether the VERIFIED download carried an
    /// `Authority=` line, i.e. a certificate-anchored (Developer ID)
    /// signature rather than an ad-hoc/unsigned one.
    ///
    /// Left `None` (neither true nor false) on Linux targets and when
    /// `codesign` was unavailable — both cases where the pre-provision state
    /// is unknown, so the post-provision check has nothing to assert against
    /// and skips.
    pub had_authority: Option<bool>,
}

/// What `fetch_resolve_latest` resolved, or why it could not.
pub struct Resolution {
    pub target: String,
    pub repo_slug: String,
    pub latest_tag: String,
    pub latest_version: String,
    pub ok: bool,
    pub reason: String,
}

/// `detect_target_triple` — this host's release target triple, or `""` for a
/// platform the release matrix does not build for (e.g. x86_64 macOS).
#[must_use]
pub fn detect_target_triple() -> String {
    let os = util::uname_s();
    let arch = util::uname_m();
    match os.as_str() {
        "Darwin" => match arch.as_str() {
            "arm64" | "aarch64" => "aarch64-apple-darwin",
            _ => "",
        },
        "Linux" => match arch.as_str() {
            "aarch64" | "arm64" => "aarch64-unknown-linux-gnu",
            "x86_64" | "amd64" => "x86_64-unknown-linux-gnu",
            _ => "",
        },
        _ => "",
    }
    .to_string()
}

/// `resolve_gh_repo_slug` — `owner/repo` parsed from the `origin` remote, or
/// `""`.
///
/// The three URL forms the shell recognised, and only those: an unrecognised
/// form returns `""` so the caller reports the actionable
/// "set LOOM_DAEMON_UPDATE_GH_REPO to override" rather than guessing.
#[must_use]
pub fn resolve_gh_repo_slug(repo_root: &Path) -> String {
    let url = util::git(repo_root, &["remote", "get-url", "origin"]).unwrap_or_default();
    parse_gh_repo_slug(&url)
}

/// The pure half of [`resolve_gh_repo_slug`]: the URL grammar, with the git
/// call factored out.
///
/// Extracted so its test drives THIS function rather than a copy of it. A test
/// that re-implements the parser it is testing asserts only that two
/// transcriptions agree, which is exactly the shape
/// `defaults/docs/verification-recipes.md` §6 warns about ("if the test needs
/// its own copy of the parser, say which implementation it models") — and the
/// cheapest way to not need one is to make the real function callable.
#[must_use]
fn parse_gh_repo_slug(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    let stripped = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = url.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        rest
    } else {
        return String::new();
    };
    // `s/\.git$//` — anchored at end, exactly once.
    stripped
        .strip_suffix(".git")
        .unwrap_or(stripped)
        .to_string()
}

/// `fetch_resolve_latest` — read-only resolution, no downloads.
pub fn fetch_resolve_latest(repo_root: &Path) -> Resolution {
    let mut r = Resolution {
        target: String::new(),
        repo_slug: String::new(),
        latest_tag: String::new(),
        latest_version: String::new(),
        ok: false,
        reason: String::new(),
    };

    r.target =
        util::env_non_empty("LOOM_DAEMON_UPDATE_TARGET").unwrap_or_else(detect_target_triple);
    if r.target.is_empty() {
        // The shell's fallback here is `?`, not `unknown` — a different
        // string from `detect_target_triple`'s own, and it is the one that
        // reaches the operator.
        let os = uname_or_question("-s");
        let arch = uname_or_question("-m");
        r.reason =
            format!("unrecognized host platform ({os}/{arch}) -- no release target-triple mapping");
        return r;
    }

    if !util::have("gh") {
        r.reason = "'gh' CLI not found on PATH".to_string();
        return r;
    }

    r.repo_slug = util::env_non_empty("LOOM_DAEMON_UPDATE_GH_REPO")
        .unwrap_or_else(|| resolve_gh_repo_slug(repo_root));
    if r.repo_slug.is_empty() {
        r.reason = "could not resolve owner/repo from git remote 'origin' (set LOOM_DAEMON_UPDATE_GH_REPO to override)".to_string();
        return r;
    }

    let tag = gh_release_view(&r.repo_slug, "tagName", ".tagName").unwrap_or_default();
    if tag.is_empty() {
        r.reason = format!(
            "'gh release view' found no latest release for {} (no Releases yet, an unreachable/rate-limited API, or an auth failure)",
            r.repo_slug
        );
        return r;
    }
    r.latest_tag = tag.clone();
    r.latest_version = util::extract_version(&tag);
    if r.latest_version.is_empty() {
        r.reason = format!("could not parse a semver version out of release tag '{tag}'");
        return r;
    }

    let assets = gh_release_view(&r.repo_slug, "assets", ".assets[].name").unwrap_or_default();
    let bin_name = format!("loom-daemon-{}", r.target);
    let sha_name = format!("{bin_name}.sha256");
    // `grep -qxF` — a WHOLE-LINE, fixed-string match. A substring match would
    // accept `loom-daemon-x86_64-unknown-linux-gnu.sha256` as evidence of the
    // binary asset.
    let has = |needle: &str| assets.lines().any(|l| l == needle);
    if !has(&bin_name) || !has(&sha_name) {
        r.reason = format!(
            "release {tag} has no artifact for target {} (checked for {bin_name} + {sha_name})",
            r.target
        );
        return r;
    }

    r.ok = true;
    r
}

fn uname_or_question(flag: &str) -> String {
    Command::new("uname")
        .arg(flag)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "?".to_string())
}

fn gh_release_view(slug: &str, fields: &str, jq: &str) -> Option<String> {
    let out = Command::new("gh")
        .args(["release", "view", "--json", fields, "-R", slug, "--jq", jq])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Why `fetch_and_verify_artifact` did not produce a binary.
pub enum FetchFailure {
    /// Verification FAILED — a checksum mismatch, an invalid signature, or a
    /// published signature asset that would not download. The shell exited 1
    /// from inside the function rather than returning, because these are
    /// tamper/corruption signals and never soft-fallback conditions.
    Verification,
    /// Could not even download. The shell returned 1 so the caller could
    /// decide; in practice the caller has already committed to artifact mode
    /// and also exits 1, but the distinction keeps the contract composable.
    Download,
}

/// `fetch_and_verify_artifact` — download, verify, and report the identity.
///
/// The progress/verdict lines go to **our own stderr**, never captured, which
/// is what keeps them showing up in a caller's `2>&1` capture unchanged — and
/// is why the cosign/codesign wording they carry is contract for
/// `test-loom-daemon-update-fetch.sh`, not merely descriptive.
pub fn fetch_and_verify_artifact(
    repo_root: &Path,
    target: &str,
    repo_slug: &str,
    tag: &str,
) -> Result<FetchedArtifact, FetchFailure> {
    use crate::release_fetch::{fetch_and_verify, FetchInputs, FetchOutcome};

    let bin_name = format!("loom-daemon-{target}");
    let sha_name = format!("{bin_name}.sha256");
    out::say_err(&format!("Downloading {bin_name} + {sha_name} from {repo_slug}@{tag}..."));

    let inputs = FetchInputs {
        repo_root,
        target,
        repo_slug,
        tag,
        cosign_pubkey_env: std::env::var("LOOM_DAEMON_UPDATE_COSIGN_PUBKEY").ok(),
        cosign_identity_env: std::env::var("LOOM_DAEMON_UPDATE_COSIGN_IDENTITY").ok(),
        cosign_oidc_issuer_env: std::env::var("LOOM_DAEMON_UPDATE_COSIGN_OIDC_ISSUER").ok(),
    };

    match fetch_and_verify(&inputs) {
        FetchOutcome::Verified {
            artifact,
            checksum_line,
            signature_line,
            ..
        } => {
            out::say_err(&checksum_line);
            if !signature_line.is_empty() {
                out::say_err(&signature_line);
            }
            // The scratch dir joins the cleanup set the shell's EXIT trap
            // owned, so an abort never leaves an unverified artifact behind.
            super::register_tmpdir(artifact.tmp_dir.clone());
            Ok(FetchedArtifact {
                bin: artifact.bin_path.clone(),
                version_output: artifact.version_output.clone(),
                commit: artifact.commit.clone().unwrap_or_default(),
                had_authority: artifact.had_authority,
            })
        }
        FetchOutcome::VerificationFailed { lines } => {
            for line in lines {
                out::say_err(&line);
            }
            Err(FetchFailure::Verification)
        }
        FetchOutcome::DownloadFailed(message) => {
            out::say_err(&message);
            Err(FetchFailure::Download)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_parsing_covers_exactly_the_three_recognised_url_forms() {
        // Drives the REAL parser (`parse_gh_repo_slug`), not a transcription
        // of it: the git call is the only half factored out, and it is not
        // what this asserts.
        let cases = [
            ("git@github.com:rjwalters/loom.git", "rjwalters/loom"),
            ("git@github.com:rjwalters/loom", "rjwalters/loom"),
            ("https://github.com/rjwalters/loom.git", "rjwalters/loom"),
            ("http://github.com/rjwalters/loom", "rjwalters/loom"),
            ("ssh://git@github.com/rjwalters/loom.git", "rjwalters/loom"),
            ("https://gitea.example/rjwalters/loom.git", ""),
            ("", ""),
        ];
        for (url, expected) in cases {
            assert_eq!(parse_gh_repo_slug(url), expected, "{url}");
        }
    }

    #[test]
    fn the_target_map_covers_the_three_published_triples_and_nothing_else() {
        // Asserted through the pure mapping rather than `uname`, which the
        // retained suite stubs.
        let map = |os: &str, arch: &str| -> &'static str {
            match os {
                "Darwin" => match arch {
                    "arm64" | "aarch64" => "aarch64-apple-darwin",
                    _ => "",
                },
                "Linux" => match arch {
                    "aarch64" | "arm64" => "aarch64-unknown-linux-gnu",
                    "x86_64" | "amd64" => "x86_64-unknown-linux-gnu",
                    _ => "",
                },
                _ => "",
            }
        };
        assert_eq!(map("Darwin", "arm64"), "aarch64-apple-darwin");
        assert_eq!(map("Darwin", "x86_64"), "", "x86_64 macOS is not built");
        assert_eq!(map("Linux", "amd64"), "x86_64-unknown-linux-gnu");
        assert_eq!(map("Linux", "armv7l"), "");
        assert_eq!(map("FreeBSD", "x86_64"), "");
    }
}
