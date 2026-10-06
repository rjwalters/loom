//! Publishing a minted App token together with the identity it was minted
//! as (#10571).
//!
//! Every token publication goes through [`publish_minted`]: the token first
//! ([`super::publish_github_app_token`], the #4458 atomic delivery), then the
//! directory's `identity.json` sidecar ([`crate::forge_identity::sidecar`])
//! naming the App, installation and owner of *that* token. The bucket book
//! keys the directory's rate-limit readings by the sidecar, so two Apps
//! taking turns minting into one directory show up as two buckets instead
//! of one bucket whose reset chain interleaves.

use std::fmt;
use std::path::Path;

use super::GithubAppOutcome;
use crate::forge_egress::publication::{stance_for, workspace_of_profile_dir, Stance};
use crate::forge_identity::sidecar::{remove_sidecar, write_sidecar, Sidecar, SidecarRole};

/// One minted installation token and the identity it belongs to.
/// `token` must never be logged; [`fmt::Debug`] redacts it.
#[derive(Clone, PartialEq, Eq)]
pub struct Minted {
    pub token: String,
    pub app_id: String,
    pub slug: Option<String>,
    pub installation_id: String,
    /// The owner the installation covers, lowercased.
    pub owner: String,
    pub role: SidecarRole,
    /// The token's expiry (RFC 3339, from the minter).
    pub expires_at: String,
}

impl fmt::Debug for Minted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Minted")
            .field("token", &"<redacted>")
            .field("app_id", &self.app_id)
            .field("installation_id", &self.installation_id)
            .field("owner", &self.owner)
            .field("role", &self.role)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Minted {
    /// The writer token in `outcome`, minted for `owner_repo` (its owner is
    /// the installation's). `None` unless `outcome` is
    /// [`GithubAppOutcome::Minted`].
    #[must_use]
    pub fn of(outcome: &GithubAppOutcome, owner_repo: &str) -> Option<Self> {
        let GithubAppOutcome::Minted {
            token,
            installation_id,
            app_id,
            expires_at,
        } = outcome
        else {
            return None;
        };
        Some(Self {
            token: token.clone(),
            app_id: app_id.clone(),
            slug: None,
            installation_id: installation_id.clone(),
            owner: super::owner_of_nwo(owner_repo).to_ascii_lowercase(),
            role: SidecarRole::Writer,
            expires_at: expires_at.clone(),
        })
    }

    /// The sidecar this token is published with.
    #[must_use]
    pub fn sidecar(&self) -> Sidecar {
        Sidecar {
            app_id: self.app_id.clone(),
            slug: self.slug.clone(),
            installation_id: self.installation_id.clone(),
            owner: Some(self.owner.clone()),
            role: self.role,
            expires_at: self.expires_at.clone(),
        }
    }
}

/// Publish `minted`'s token into `dir`, then its sidecar. A token-less
/// profile (`enforcement.api=required`, #9986) holds no identity: its
/// sidecar is removed instead.
///
/// # Errors
///
/// When the token or the sidecar cannot be written.
pub fn publish_minted(dir: &Path, minted: &Minted) -> std::io::Result<()> {
    super::publish_github_app_token(dir, &minted.token)?;
    let workspace = workspace_of_profile_dir(dir);
    if matches!(stance_for(workspace.as_deref()), Stance::Required { .. }) {
        remove_sidecar(dir);
        return Ok(());
    }
    write_sidecar(dir, &minted.sidecar())
}

/// [`publish_minted`] for a writer `outcome` minted for `owner_repo`.
///
/// # Errors
///
/// When `outcome` is not [`GithubAppOutcome::Minted`], or the publication
/// fails.
pub fn publish_outcome(
    dir: &Path,
    outcome: &GithubAppOutcome,
    owner_repo: &str,
) -> std::io::Result<()> {
    let minted = Minted::of(outcome, owner_repo)
        .ok_or_else(|| std::io::Error::other("no minted token to publish"))?;
    publish_minted(dir, &minted)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::forge_identity::read_sidecar;

    fn outcome() -> GithubAppOutcome {
        GithubAppOutcome::Minted {
            token: "ghs_minted_secret".into(),
            installation_id: "151241341".into(),
            app_id: "4486636".into(),
            expires_at: "2030-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn publish_minted_writes_the_sidecar_after_the_token() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join(".loom")
            .join("gh-config-by-owner")
            .join("2AMLogic");
        publish_outcome(&dir, &outcome(), "2AMLogic/2am").unwrap();
        assert!(dir.join("hosts.yml").is_file(), "token published");
        let side = read_sidecar(&dir).unwrap();
        assert_eq!(side.app_id, "4486636");
        assert_eq!(side.installation_id, "151241341");
        assert_eq!(side.owner.as_deref(), Some("2amlogic"));
        assert_eq!(side.role, SidecarRole::Writer);
        let raw = std::fs::read_to_string(dir.join("identity.json")).unwrap();
        assert!(!raw.contains("ghs_"), "the sidecar never holds the token");
        // The sidecar is younger than (or as old as) the token it describes.
        let mtime = |f: &str| std::fs::metadata(dir.join(f)).unwrap().modified().unwrap();
        assert!(mtime("identity.json") >= mtime("hosts.yml"));
    }

    #[test]
    fn a_second_publisher_into_one_dir_replaces_the_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".loom").join("gh-config");
        publish_outcome(&dir, &outcome(), "acme/repo").unwrap();
        let other = GithubAppOutcome::Minted {
            token: "ghs_other".into(),
            installation_id: "9".into(),
            app_id: "2".into(),
            expires_at: "2030-01-01T00:00:00Z".into(),
        };
        publish_outcome(&dir, &other, "acme/repo").unwrap();
        let side = read_sidecar(&dir).unwrap();
        assert_eq!((side.app_id.as_str(), side.installation_id.as_str()), ("2", "9"));
    }

    #[test]
    fn a_non_minted_outcome_publishes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gh-config");
        assert!(publish_outcome(&dir, &GithubAppOutcome::NotConfigured, "acme/repo").is_err());
        assert!(!dir.exists());
        assert!(Minted::of(&GithubAppOutcome::Error("x".into()), "acme/repo").is_none());
    }

    #[test]
    fn debug_never_shows_the_token() {
        let m = Minted::of(&outcome(), "acme/repo").unwrap();
        let shown = format!("{m:?}");
        assert!(!shown.contains("ghs_minted_secret"), "{shown}");
        assert!(shown.contains("151241341"), "{shown}");
    }
}
