//! The artifact verdict: what a resolved release means for the installed
//! binary (extracted from `auto_update.rs` in #8513).
//!
//! Pure — no I/O, no state — so the whole decision matrix is unit-testable
//! with plain values. It lives here rather than in `auto_update.rs` because
//! that file is over `.loom/docs/file-size-policy.md`'s threshold and frozen,
//! and because #8513's new [`ArtifactVerdict::StaleRepo`] verdict belongs
//! immediately next to the version comparison that derives it.

use super::ArtifactInfo;

/// What the resolved artifact means for the installed binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactVerdict {
    /// The release is a newer version than what is installed.
    Newer {
        /// The installed version, or `None` when no binary was resolvable.
        installed: Option<String>,
        /// The release's version.
        artifact: String,
    },
    /// Same version, different bytes — the host built this version from source
    /// before the release existed (or the binary was re-signed locally after
    /// install). Fetching converges it onto the released, verified bytes.
    ShaDiffers {
        /// The (shared) version.
        version: String,
        /// The release's published sha256 — also the convergence key recorded
        /// in [`super::ArtifactRollRecord`], so one unsuccessful convergence cannot
        /// turn into a fetch/restart loop.
        asset_sha256: String,
        /// The installed binary's own sha256.
        installed_sha256: String,
    },
    /// Nothing to do.
    UpToDate {
        /// The release's version.
        version: String,
        /// Why there is nothing to do (sha matched, or the comparison could
        /// not be made).
        why: String,
    },
    /// The resolved release is OLDER than what is installed — deliberately
    /// NOT [`Self::UpToDate`] (#8513): see [`stale_repo`](super::stale_repo) for why that is a
    /// probable wrong-repo resolution rather than a healthy no-op.
    StaleRepo {
        /// The release's (older) version.
        artifact: String,
        /// The installed version being compared against.
        installed: String,
        /// The repo this release was resolved from.
        repo: String,
    },
}

/// Compare two dotted-numeric versions the same way the update script's
/// `semver_compare` does: up to three components, non-numeric characters
/// stripped defensively, missing components treated as `0`. Deliberately NOT a
/// full semver implementation — the daemon's own versions are always
/// `MAJOR.MINOR.PATCH`, and disagreeing with the shell comparison that drives
/// the actual fetch would be worse than being simplistic.
#[must_use]
pub(super) fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    fn component(s: Option<&str>) -> u64 {
        s.map(|part| {
            part.chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|digits| digits.parse::<u64>().ok())
        .unwrap_or(0)
    }
    let mut left = a.split('.');
    let mut right = b.split('.');
    for _ in 0..3 {
        let ord = component(left.next()).cmp(&component(right.next()));
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// Classify a resolved artifact against the installed binary. Pure — no I/O,
/// no state — so the whole decision matrix is unit-testable with plain values.
#[must_use]
pub fn classify_artifact(info: &ArtifactInfo) -> ArtifactVerdict {
    let installed = info
        .installed_version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let Some(installed) = installed else {
        // No resolvable installed binary at all — any published artifact is
        // strictly better than nothing (this is the update script's own
        // "no loom-daemon binary currently resolvable ⇒ update needed" rule).
        return ArtifactVerdict::Newer {
            installed: None,
            artifact: info.version.clone(),
        };
    };
    match compare_versions(&info.version, installed) {
        std::cmp::Ordering::Greater => ArtifactVerdict::Newer {
            installed: Some(installed.to_string()),
            artifact: info.version.clone(),
        },
        std::cmp::Ordering::Less => ArtifactVerdict::StaleRepo {
            artifact: info.version.clone(),
            installed: installed.to_string(),
            repo: info.repo.clone(),
        },
        std::cmp::Ordering::Equal => {
            let asset = info
                .asset_sha256
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let local = info
                .installed_sha256
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match (asset, local) {
                (Some(asset), Some(local)) if asset.eq_ignore_ascii_case(local) => {
                    ArtifactVerdict::UpToDate {
                        version: info.version.clone(),
                        why: "artifact == installed, sha matches".to_string(),
                    }
                }
                (Some(asset), Some(local)) => ArtifactVerdict::ShaDiffers {
                    version: info.version.clone(),
                    asset_sha256: asset.to_string(),
                    installed_sha256: local.to_string(),
                },
                // One side's checksum is unknown, so "same bytes?" cannot be
                // answered. Treat as converged rather than guessing: a wrong
                // "differs" here would re-fetch (and restart) on every single
                // tick forever, which is far worse than a missed convergence.
                _ => ArtifactVerdict::UpToDate {
                    version: info.version.clone(),
                    why: "artifact == installed, but no published/installed checksum is available \
                          to compare — assuming converged"
                        .to_string(),
                },
            }
        }
    }
}
