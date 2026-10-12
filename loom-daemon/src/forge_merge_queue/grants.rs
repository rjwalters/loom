//! Forge-durable [`GrantStore`]: grants and revocations are trusted PR
//! comments carrying a machine marker (#10256, Phase B2).
//!
//! # Why comments, not a local file
//!
//! The grant is read by the required `loom/merge-authorization` check, which
//! must run wherever GitHub evaluates the merge group — a workflow runner or
//! any fleet host — not only on the host that enqueued. A host-local file
//! would make the check fail closed everywhere else (safe, but every merge
//! would time out of the queue). A trusted comment is readable from every
//! host, survives a daemon restart, and is an audit trail.
//!
//! # Semantics
//!
//! - **The newest marker for this PR wins.** A grant is live until a later
//!   revoke marker (or a later grant) supersedes it. Markers naming a
//!   different PR are ignored (copy-paste safety).
//! - **Only trusted authors count** — the listing comes from
//!   [`LifecycleForge::trusted_comments`], so an outsider cannot grant.
//! - **Idempotent.** Re-granting the live head and revoking a PR with no live
//!   grant post nothing, so duplicate events and restarts write nothing new.
//! - **Revocation is durable on `Ok`**: the comment POST was confirmed.
//! - Every grant has a `nonce` (a generation id) that the telemetry ledger
//!   keys on, so a re-enqueue at the same head is a new generation.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;

use super::authz::{Grant, GrantStore, StoreError};
use super::forge::LifecycleForge;

/// The newest grant-or-revoke marker for one PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantRecord {
    None,
    Live {
        sha: String,
        nonce: u64,
        at: String,
    },
    Revoked {
        nonce: u64,
        reason: String,
        /// The head the revoked grant covered.
        sha: Option<String>,
        /// When the revoked grant was written (for enqueue→removal latency).
        granted_at: Option<String>,
    },
}

impl GrantRecord {
    #[must_use]
    pub fn nonce(&self) -> Option<u64> {
        match self {
            GrantRecord::None => None,
            GrantRecord::Live { nonce, .. } | GrantRecord::Revoked { nonce, .. } => Some(*nonce),
        }
    }
}

fn marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        #[allow(clippy::unwrap_used)] // a literal pattern
        Regex::new(
            r"<!-- loom:merge-queue-(grant|revoke) pr=(\d+)(?: sha=([0-9a-f]{40}))? nonce=(\d+)(?: reason=([a-z0-9-]+))? at=(\S+) -->",
        )
        .unwrap()
    })
}

/// The newest record for `pr` in `bodies` (oldest first). Pure.
#[must_use]
pub fn latest_record(bodies: &[String], pr: u32) -> GrantRecord {
    let mut rec = GrantRecord::None;
    let mut last_grant: Option<(String, u64, String)> = None;
    for body in bodies {
        for c in marker_re().captures_iter(body) {
            if c[2].parse::<u32>().ok() != Some(pr) {
                continue;
            }
            let Ok(nonce) = c[4].parse::<u64>() else {
                continue;
            };
            let at = c[6].to_string();
            match &c[1] {
                "grant" => {
                    let Some(sha) = c.get(3).map(|m| m.as_str().to_string()) else {
                        continue;
                    };
                    last_grant = Some((sha.clone(), nonce, at.clone()));
                    rec = GrantRecord::Live { sha, nonce, at };
                }
                _ => {
                    let granted = last_grant.as_ref().filter(|(_, n, _)| *n == nonce);
                    rec = GrantRecord::Revoked {
                        nonce,
                        reason: c.get(5).map_or("unspecified", |m| m.as_str()).to_string(),
                        sha: granted.map(|(s, _, _)| s.clone()),
                        granted_at: granted.map(|(_, _, a)| a.clone()),
                    };
                }
            }
        }
    }
    rec
}

/// The grant marker comment body.
#[must_use]
pub fn grant_body(pr: u32, sha: &str, nonce: u64, at: &str) -> String {
    format!(
        "**Merge queue: authorization granted** for head `{sha}`.\n\n\
         The required `loom/merge-authorization` check passes only while this exact head keeps \
         its Judge approval and no hold, review claim or contradicting label appears; any change \
         fails it and blocks the merge (#10256).\n\n\
         <!-- loom:merge-queue-grant pr={pr} sha={sha} nonce={nonce} at={at} -->"
    )
}

/// The revoke marker comment body; `prose` explains why.
#[must_use]
pub fn revoke_body(pr: u32, nonce: u64, reason: &str, at: &str, prose: &str) -> String {
    format!(
        "**Merge queue: authorization revoked** (`{reason}`).\n\n{prose}\n\n\
         <!-- loom:merge-queue-revoke pr={pr} nonce={nonce} reason={reason} at={at} -->"
    )
}

/// Lower-case, `[a-z0-9-]` only, so it always round-trips through the marker.
#[must_use]
pub fn reason_token(raw: &str) -> String {
    let t: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let t = t.trim_matches('-').to_string();
    if t.is_empty() {
        "unspecified".to_string()
    } else {
        t
    }
}

/// [`GrantStore`] over trusted PR comments.
pub struct CommentGrantStore<'a> {
    forge: &'a dyn LifecycleForge,
    reason: String,
    now: DateTime<Utc>,
}

impl<'a> CommentGrantStore<'a> {
    /// `reason` is what a plain [`GrantStore::revoke`] records.
    #[must_use]
    pub fn new(forge: &'a dyn LifecycleForge, reason: &str, now: DateTime<Utc>) -> Self {
        Self {
            forge,
            reason: reason_token(reason),
            now,
        }
    }

    fn at(&self) -> String {
        self.now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// The newest record for `pr`.
    ///
    /// # Errors
    ///
    /// The comment listing could not be read.
    pub fn record(&self, pr: u32) -> Result<GrantRecord, StoreError> {
        let bodies = self.forge.trusted_comments(pr).map_err(StoreError)?;
        Ok(latest_record(&bodies, pr))
    }

    /// Revoke with an explicit reason and explanation. Returns `Ok(false)`
    /// when there was no live grant (nothing posted).
    ///
    /// # Errors
    ///
    /// The listing or the POST failed; the grant may still be live.
    pub fn revoke_with(&self, pr: u32, reason: &str, prose: &str) -> Result<bool, StoreError> {
        match self.record(pr)? {
            GrantRecord::Live { nonce, .. } => {
                let body = revoke_body(pr, nonce, &reason_token(reason), &self.at(), prose);
                self.forge.post_comment(pr, &body).map_err(StoreError)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

impl GrantStore for CommentGrantStore<'_> {
    fn get(&self, pr: u32) -> Result<Option<Grant>, StoreError> {
        Ok(match self.record(pr)? {
            GrantRecord::Live { sha, .. } => Some(Grant {
                pr,
                approved_sha: sha,
            }),
            _ => None,
        })
    }

    fn put(&self, grant: Grant) -> Result<(), StoreError> {
        let sha = grant.approved_sha.to_ascii_lowercase();
        let rec = self.record(grant.pr)?;
        if let GrantRecord::Live { sha: live, .. } = &rec {
            if *live == sha {
                return Ok(());
            }
        }
        let millis = u64::try_from(self.now.timestamp_millis()).unwrap_or(0);
        let nonce = millis.max(rec.nonce().map_or(0, |n| n + 1));
        self.forge
            .post_comment(grant.pr, &grant_body(grant.pr, &sha, nonce, &self.at()))
            .map_err(StoreError)
    }

    fn revoke(&self, pr: u32) -> Result<(), StoreError> {
        let prose = "The PR no longer meets the queue authorization conditions; the required \
                     `loom/merge-authorization` check now fails for it.";
        self.revoke_with(pr, &self.reason.clone(), prose)
            .map(|_| ())
    }
}
