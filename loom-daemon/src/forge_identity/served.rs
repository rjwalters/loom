//! Coverage evidence for item-scoped reads (W6).
//!
//! A reader `404` on a whole-repo read means the reader's installation does
//! not include the repo ([`super::Failure::Coverage`]): the read re-runs on
//! the writer. A `404` on ONE item (`issues/{n}`, `pulls/{n}`) usually means
//! only that the item does not exist there (a number that is not a PR, a
//! deleted or transferred issue), and re-running it on the writer costs a
//! call that answers the same `404`.
//!
//! So a reader that recently answered a `200`/`304` for the same repo AND
//! the same endpoint family (`issues` vs `pulls`) is taken at its word on an
//! item-scoped `404`: no writer retry and no withdrawal. The family scope
//! matters: an App installed on the repo without pull-request read access
//! can answer `404` for every `pulls/{n}` while serving `issues/*` fine, and
//! must not have that read as "the PR is gone". Without evidence for the
//! family, the read keeps the Coverage path (writer retry).
//!
//! The answer a consumer gets from such a `404` is `Gone`, which every
//! hygiene consumer maps to KEEP — never to an action.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// How long one reader `200`/`304` counts as coverage evidence for its
/// `(repo, family)`: the repo-withdrawal window, so the evidence never
/// outlives the period a Coverage failure would have withdrawn the reader for.
pub const ITEM_SCOPED_EVIDENCE: Duration = super::REPO_WITHDRAWAL;

/// Counter: a reader `404` on an item-scoped read was accepted as the item's
/// answer (no writer retry) on the strength of recent coverage evidence.
pub const ITEM_SCOPED_404: &str = "forge.item_scoped_404";

type Key = (String, String, String);

fn served() -> &'static Mutex<HashMap<Key, SystemTime>> {
    static MAP: OnceLock<Mutex<HashMap<Key, SystemTime>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(app_id: &str, owner_repo: &str, family: &str) -> Key {
    (app_id.to_string(), owner_repo.to_ascii_lowercase(), family.to_ascii_lowercase())
}

/// The endpoint family of a REST path: the segment after `repos/<o>/<r>/`
/// (`issues`, `pulls`, …), with any query string dropped. `""` for a path
/// that is not under `repos/<o>/<r>/`.
#[must_use]
pub fn endpoint_family(url: &str) -> &str {
    let rest = url.trim_start_matches('/');
    let Some(rest) = rest.strip_prefix("repos/") else {
        return "";
    };
    let mut segs = rest.splitn(4, '/');
    let (_, _, tail) = (segs.next(), segs.next(), segs.next());
    let family = tail.unwrap_or("");
    family.split(['?', '/']).next().unwrap_or("")
}

/// Record that reader `app_id` answered a `200`/`304` for `owner_repo`'s
/// `family` at `now`.
pub fn note_reader_served(app_id: &str, owner_repo: &str, family: &str, now: SystemTime) {
    if owner_repo.is_empty() || family.is_empty() {
        return;
    }
    if let Ok(mut m) = served().lock() {
        m.insert(key(app_id, owner_repo, family), now);
    }
}

/// Whether reader `app_id` answered a `200`/`304` for `owner_repo`'s
/// `family` within [`ITEM_SCOPED_EVIDENCE`] before `now`.
#[must_use]
pub fn reader_recently_served(
    app_id: &str,
    owner_repo: &str,
    family: &str,
    now: SystemTime,
) -> bool {
    if owner_repo.is_empty() || family.is_empty() {
        return false;
    }
    let Ok(m) = served().lock() else {
        return false;
    };
    m.get(&key(app_id, owner_repo, family))
        .and_then(|at| now.duration_since(*at).ok())
        .is_some_and(|age| age <= ITEM_SCOPED_EVIDENCE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_is_the_segment_after_the_repo() {
        assert_eq!(endpoint_family("repos/acme/app/issues/7"), "issues");
        assert_eq!(endpoint_family("repos/acme/app/pulls/7"), "pulls");
        assert_eq!(endpoint_family("repos/acme/app/pulls?state=all&head=a:b"), "pulls");
        assert_eq!(endpoint_family("/repos/acme/app/issues"), "issues");
        assert_eq!(endpoint_family("repos/acme/app"), "");
        assert_eq!(endpoint_family("search/issues?q=x"), "");
    }

    #[test]
    fn evidence_is_scoped_by_app_repo_and_family_and_expires() {
        let now = SystemTime::now();
        note_reader_served("w6-served-1", "Acme/App-Served", "issues", now);
        assert!(reader_recently_served("w6-served-1", "acme/app-served", "issues", now));
        assert!(
            !reader_recently_served("w6-served-1", "acme/app-served", "pulls", now),
            "issues evidence never covers pulls"
        );
        assert!(!reader_recently_served("w6-served-2", "acme/app-served", "issues", now));
        assert!(!reader_recently_served("w6-served-1", "acme/other", "issues", now));
        let later = now + ITEM_SCOPED_EVIDENCE + Duration::from_secs(1);
        assert!(!reader_recently_served("w6-served-1", "acme/app-served", "issues", later));
        // Nothing is recorded or matched without a repo or a family.
        note_reader_served("w6-served-3", "", "issues", now);
        assert!(!reader_recently_served("w6-served-3", "", "issues", now));
    }
}
