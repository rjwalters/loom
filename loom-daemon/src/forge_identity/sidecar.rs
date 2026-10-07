//! The `identity.json` sidecar beside every published App credential
//! directory: which App, which installation and which owner the token in
//! that directory's `hosts.yml` was minted as.
//!
//! Readers have carried one since #9537 (their `expires_at` gates every
//! read). Since #10571 every writer publication writes one too
//! ([`crate::credential_preflight::publish_minted`]), so the bucket book keys
//! a directory's readings by the identity that was actually minted into it —
//! not by the roster (`.loom/config.json`) and the `origin` remote, which say
//! what *should* be there. The sidecar is always written after the token, so
//! a reader of the directory never sees a fresh sidecar in front of a stale
//! token. It never holds a token.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::Identity;

/// The sidecar's file name.
pub const SIDECAR: &str = "identity.json";

/// Which kind of credential a sidecar describes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidecarRole {
    /// The writer App's token (`.loom/gh-config`, `gh-config-by-owner/<owner>`).
    Writer,
    /// A reader App's token (`gh-config-by-owner/<owner>/<app id>`). The
    /// default: every pre-#10571 sidecar was a reader's.
    #[default]
    Reader,
}

impl SidecarRole {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Writer => "writer",
            Self::Reader => "reader",
        }
    }
}

/// What a publication records beside a token.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Sidecar {
    /// The App id the token was minted for.
    pub app_id: String,
    /// Its slug, when known.
    pub slug: Option<String>,
    /// The installation the token belongs to.
    pub installation_id: String,
    /// The owner the installation covers, lowercased (#10571; absent in a
    /// pre-#10571 reader sidecar, whose directory name carries it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Writer or reader (#10571; a pre-#10571 sidecar reads as `reader`).
    #[serde(default)]
    pub role: SidecarRole,
    /// The token's expiry (RFC 3339, from the minter).
    pub expires_at: String,
}

/// The sidecar in `dir`, if readable.
#[must_use]
pub fn read_sidecar(dir: &Path) -> Option<Sidecar> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(SIDECAR)).ok()?).ok()
}

/// Write `side` into `dir` atomically (write-then-rename). The temporary
/// name is unique per process and call, so two publishers racing on one
/// directory never interleave one file.
///
/// # Errors
///
/// When the write or the rename fails.
pub fn write_sidecar(dir: &Path, side: &Sidecar) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{SIDECAR}.{}.{seq}.tmp", std::process::id()));
    let written = std::fs::write(&tmp, serde_json::to_vec(side).unwrap_or_default())
        .and_then(|()| std::fs::rename(&tmp, dir.join(SIDECAR)));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Remove `dir`'s sidecar (a token-less profile describes no identity).
pub fn remove_sidecar(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(SIDECAR));
}

/// Publish a reader's token for `owner` and its sidecar (via
/// [`crate::credential_preflight::publish_minted`], token first).
pub(super) fn publish(
    dir: &Path,
    token: &str,
    reader: &Identity,
    owner: &str,
    installation_id: &str,
    expires_at: &str,
) -> std::io::Result<()> {
    crate::credential_preflight::publish_minted(
        dir,
        &crate::credential_preflight::Minted {
            token: token.to_string(),
            app_id: reader.app_id.clone(),
            slug: reader.slug.clone(),
            installation_id: installation_id.to_string(),
            owner: owner.to_ascii_lowercase(),
            role: SidecarRole::Reader,
            expires_at: expires_at.to_string(),
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_pre_10571_reader_sidecar_still_parses_as_a_reader() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(SIDECAR),
            r#"{"appId":"7","slug":null,"installationId":"9","expiresAt":"2030-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let side = read_sidecar(tmp.path()).unwrap();
        assert_eq!((side.app_id.as_str(), side.installation_id.as_str()), ("7", "9"));
        assert_eq!((side.owner, side.role), (None, SidecarRole::Reader));
    }

    #[test]
    fn a_writer_sidecar_round_trips_and_leaves_no_temporary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let side = Sidecar {
            app_id: "4486636".into(),
            installation_id: "151241341".into(),
            owner: Some("2amlogic".into()),
            role: SidecarRole::Writer,
            expires_at: "2030-01-01T00:00:00Z".into(),
            ..Sidecar::default()
        };
        write_sidecar(tmp.path(), &side).unwrap();
        write_sidecar(tmp.path(), &side).unwrap();
        assert_eq!(read_sidecar(tmp.path()), Some(side));
        let names: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [SIDECAR]);
        let raw = std::fs::read_to_string(tmp.path().join(SIDECAR)).unwrap();
        assert!(raw.contains(r#""role":"writer""#), "{raw}");
        remove_sidecar(tmp.path());
        assert!(read_sidecar(tmp.path()).is_none());
    }
}
