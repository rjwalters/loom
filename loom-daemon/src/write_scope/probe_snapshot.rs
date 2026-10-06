//! The write-scope probe's installation leg, answered from the writer's own
//! installation snapshot (W8, [`crate::forge_repo_facts::installation`]).
//!
//! An App installation token can write only inside its installation, and the
//! snapshot is exactly that set, read under the credential the writes carry
//! (the probe stays writer-only). So a fresh installation snapshot decides
//! alone, with no per-repo read: listed is WRITE, absent is `Insufficient`.
//! A user token has no installation listing, and leg 1 (`permissions`)
//! decides.
//!
//! When the snapshot cannot be had, leg 1 decides what it can:
//!
//! - a WRITE stands;
//! - a named lesser role (`pull`, `triage`) stands as `Insufficient`. Only a
//!   user token is told a role, and for a user token leg 1 is the whole
//!   answer — a definitive refusal, not an outage to retry every minute;
//! - an all-`false` `permissions` is what an App installation token gets
//!   everywhere, so it is no verdict: `Unknown`, which the caller fails
//!   closed on (the probe cache's stale-WRITE grace still applies).
//!
//! **Age.** The snapshot answers for up to its own TTL and the probe cache
//! then keeps the answer for the probe's: the two stack (about two hours
//! with the defaults), see [`super::probe`].

use super::probe::{names_a_lesser_role, Permission};
use crate::forge_repo_facts::installation::Answer;

/// Leg 1's outcome: `Ok(Some)` classified, `Ok(None)` unparseable, `Err`
/// the read failed.
pub(crate) type RepoLeg = Result<Option<Permission>, String>;

/// The permission `answer` settles, running leg 1 (`repo_leg`) only when it
/// must; `None` = the snapshot is switched off and the probe keeps both of
/// its pre-W8 legs.
pub(crate) fn from_snapshot(
    answer: &Answer,
    repo_leg: impl FnOnce() -> RepoLeg,
) -> Option<Permission> {
    match answer {
        Answer::Disabled => None,
        Answer::Listed(Some(_)) => Some(Permission::Write),
        Answer::Listed(None) => {
            Some(Permission::Insufficient("not in this App installation".into()))
        }
        Answer::PerRepo => Some(match repo_leg() {
            Ok(Some(p)) => p,
            Ok(None) => Permission::Unknown("unparseable permissions".into()),
            Err(e) => Permission::Unknown(e),
        }),
        Answer::Unavailable => Some(match repo_leg() {
            Ok(Some(Permission::Write)) => Permission::Write,
            Ok(Some(lesser)) if names_a_lesser_role(&lesser) => lesser,
            Ok(_) => Permission::Unknown(
                "installation snapshot unavailable and the repository role is not WRITE".into(),
            ),
            Err(e) => Permission::Unknown(format!("installation snapshot unavailable; {e}")),
        }),
    }
}

#[cfg(test)]
#[path = "probe_snapshot_tests.rs"]
mod tests;
