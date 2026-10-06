//! Which identity a credential directory's token was minted as (#10571).
//!
//! Every publication writes an `identity.json` sidecar beside the token
//! ([`crate::forge_identity::sidecar`]). A directory is keyed by that sidecar
//! when it has one, and by the pre-#10571 derivation otherwise: the writer
//! App from the identity roster and the owner from the path (or the
//! workspace's `origin` remote). When a writer directory's sidecar disagrees
//! with a known derivation — another App minted into the directory — the
//! sidecar wins and `bucket_book.writer_identity_mismatch` counts it, once
//! per directory and minted identity, with a log line naming the directory.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use super::DirClass;
use crate::forge_identity::{read_sidecar, Sidecar, SIDECAR};

/// The event counter bumped when a writer directory's sidecar disagrees with
/// the roster/remote derivation.
pub const WRITER_IDENTITY_MISMATCH: &str = "bucket_book.writer_identity_mismatch";

/// Where a [`CredIdentity`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    /// The directory's `identity.json`: what was minted into it.
    Sidecar,
    /// The roster and the path / `origin` remote (no usable sidecar).
    Derived,
}

/// The bucket a credential directory's token spends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredIdentity {
    /// `app-<id>` / `app-unknown`.
    pub account: String,
    /// The installation's owner, lowercased, when known.
    pub owner: Option<String>,
    /// The installation id (digits), when a sidecar names one.
    pub installation: Option<String>,
    pub source: IdentitySource,
}

/// `id` when it is a plausible numeric GitHub id (never a token or a path).
fn numeric(id: &str) -> Option<String> {
    (!id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit()))
        .then(|| id.to_string())
}

/// `dir`'s sidecar, memoised on the file's modification time and length.
fn cached_sidecar(dir: &Path) -> Option<Sidecar> {
    type Stamp = Option<(SystemTime, u64)>;
    static MEMO: OnceLock<Mutex<HashMap<PathBuf, (Stamp, Option<Sidecar>)>>> = OnceLock::new();
    let stamp: Stamp = std::fs::metadata(dir.join(SIDECAR))
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())));
    stamp?;
    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((held, side)) = memo.lock().ok().and_then(|m| m.get(dir).cloned()) {
        if held == stamp {
            return side;
        }
    }
    let side = read_sidecar(dir);
    if let Ok(mut m) = memo.lock() {
        m.insert(dir.to_path_buf(), (stamp, side.clone()));
    }
    side
}

/// The identity `dir` (classified as `class`) spends: its sidecar's when it
/// names a valid App, else the pre-#10571 derivation. `None` for a
/// directory that is not an App credential ([`DirClass::Other`]).
#[must_use]
pub fn dir_identity(dir: &Path, class: &DirClass) -> Option<CredIdentity> {
    let label = |app_id: &str| {
        let l = crate::observability::ops::ratelimit::app_account_label(app_id);
        if l == "unknown" {
            "app-unknown".to_string()
        } else {
            l
        }
    };
    let (derived_account, derived_owner) = match class {
        DirClass::Reader { owner, app_id } => {
            // A reader directory is named by its App: only a sidecar for that
            // same App can name its installation.
            let installation = cached_sidecar(dir)
                .filter(|s| s.app_id == *app_id)
                .and_then(|s| numeric(&s.installation_id));
            return Some(CredIdentity {
                account: label(app_id),
                owner: Some(owner.clone()),
                source: if installation.is_some() {
                    IdentitySource::Sidecar
                } else {
                    IdentitySource::Derived
                },
                installation,
            });
        }
        DirClass::OwnerWriter { root, owner } => {
            (super::writer_account(root), Some(owner.clone()))
        }
        DirClass::PrimaryWriter { root } => {
            (super::writer_account(root), super::primary_owner(root))
        }
        DirClass::Other => return None,
    };
    let derived = CredIdentity {
        account: derived_account,
        owner: derived_owner,
        installation: None,
        source: IdentitySource::Derived,
    };
    let Some(side) = cached_sidecar(dir).filter(|s| numeric(&s.app_id).is_some()) else {
        return Some(derived);
    };
    let minted = CredIdentity {
        account: label(&side.app_id),
        owner: side
            .owner
            .as_deref()
            .filter(|o| super::valid_owner(o))
            .map(str::to_ascii_lowercase)
            .or_else(|| derived.owner.clone()),
        installation: numeric(&side.installation_id),
        source: IdentitySource::Sidecar,
    };
    // An unknown derivation (no roster writer, an unresolved remote) has
    // nothing to disagree with.
    let account_differs = derived.account != "app-unknown" && minted.account != derived.account;
    let owner_differs = derived.owner.is_some() && minted.owner != derived.owner;
    if account_differs || owner_differs {
        report_mismatch(dir, &derived, &minted);
    }
    Some(minted)
}

/// Count and log a writer directory whose sidecar disagrees with the
/// roster/remote derivation, once per `(dir, minted identity)`.
fn report_mismatch(dir: &Path, derived: &CredIdentity, minted: &CredIdentity) {
    type Seen = HashSet<(PathBuf, String, Option<String>, Option<String>)>;
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let key = (
        dir.to_path_buf(),
        minted.account.clone(),
        minted.owner.clone(),
        minted.installation.clone(),
    );
    let fresh = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .is_ok_and(|mut seen| seen.insert(key));
    if !fresh {
        return;
    }
    crate::forge_call_stats::counters::bump(WRITER_IDENTITY_MISMATCH);
    log::warn!(
        "forge_bucket_book: {} holds a token minted as {} for {} (installation {}), but the \
         roster/remote say {} for {}; booking it under the minted identity — another App is \
         publishing into this writer directory (#10571)",
        dir.display(),
        minted.account,
        minted.owner.as_deref().unwrap_or("unknown"),
        minted.installation.as_deref().unwrap_or("-"),
        derived.account,
        derived.owner.as_deref().unwrap_or("unknown"),
    );
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
