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
//!
//! Since the writer no longer confirms anything (#12394), this evidence is
//! the ONLY thing that lets a reader's refusal stand as an answer
//! ([`judge_coverage`]). Without it a 403/404 is no answer: the read chains
//! try the next reader and shed the read when none is left — never
//! "absent". A 403, or any refusal from a reader with no evidence for the
//! repo at all, also withdraws that reader from the repo; a plain 404 from
//! one that recently served another family of the repo only sheds that
//! read, so one legitimately missing item cannot take a covering reader off
//! the whole repo.

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

/// Whether `url` names one item under its endpoint family: a segment after
/// `repos/<o>/<r>/<family>/` (`issues/7`, `pulls/5/files`, `git/ref/heads/x`),
/// not the family's listing (`pulls?state=open`) or the repo itself.
#[must_use]
pub fn is_item_path(url: &str) -> bool {
    let rest = url.trim_start_matches('/');
    let Some(rest) = rest.strip_prefix("repos/") else {
        return false;
    };
    let path = rest.split('?').next().unwrap_or("");
    let mut segs = path.splitn(5, '/');
    let (_, _, family, item) = (segs.next(), segs.next(), segs.next(), segs.next());
    family.is_some_and(|f| !f.is_empty()) && item.is_some_and(|i| !i.is_empty())
}

/// What a reader's coverage refusal (a 403/404, [`super::Failure::Coverage`])
/// calls for, now that the writer confirms nothing (#12394).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageRefusal {
    /// W6: the item's own answer; return it, withdraw nothing.
    Answer,
    /// A plain `404` from a reader that recently served another family of
    /// the repo: it sees the repo, so the item may well be missing, but
    /// nothing proves it. No answer — the read is shed — and the reader is
    /// NOT taken off the repo: one legitimately missing item must not move a
    /// covering reader's every read of the repo (and then the next reader's).
    Shed,
    /// A `403`, a "not accessible by integration" / "could not resolve to a
    /// repository", or any refusal from a reader with no evidence it sees the
    /// repo: the reader is blind to it. Withdraw it for the repo (the
    /// coverage scope) and try the next reader.
    Withdraw,
}

/// Judge a reader's coverage refusal of `owner_repo` (see
/// [`CoverageRefusal`]). The one case it is an answer (W6): an item-scoped
/// read, an HTTP `404` (never a `403`, and never GitHub's "not accessible by
/// integration" / "could not resolve to a repository", which are the
/// reader's blindness, not the item's absence), from a reader that recently
/// served the same repo's same endpoint `family`; that counts
/// [`ITEM_SCOPED_404`]. `http_status` is the response's status, resolved by
/// the caller (headers, else the stderr's `HTTP nnn`).
#[must_use]
pub fn judge_coverage(
    app_id: &str,
    owner_repo: &str,
    family: &str,
    item_scoped: bool,
    http_status: Option<u16>,
    stderr: &str,
    now: SystemTime,
) -> CoverageRefusal {
    let s = stderr.to_ascii_lowercase();
    let blind = s.contains("resource not accessible by integration")
        || s.contains("could not resolve to a repository");
    let plain_404 = http_status == Some(404) && !blind;
    if !plain_404 {
        return CoverageRefusal::Withdraw;
    }
    if item_scoped && reader_recently_served(app_id, owner_repo, family, now) {
        crate::forge_call_stats::counters::bump(ITEM_SCOPED_404);
        return CoverageRefusal::Answer;
    }
    if reader_recently_served_repo(app_id, owner_repo, now) {
        CoverageRefusal::Shed
    } else {
        CoverageRefusal::Withdraw
    }
}

/// Whether reader `app_id` answered a `200`/`304` for ANY endpoint family of
/// `owner_repo` within [`ITEM_SCOPED_EVIDENCE`] before `now`.
#[must_use]
pub fn reader_recently_served_repo(app_id: &str, owner_repo: &str, now: SystemTime) -> bool {
    if owner_repo.is_empty() {
        return false;
    }
    let repo = owner_repo.to_ascii_lowercase();
    let Ok(m) = served().lock() else {
        return false;
    };
    m.iter().any(|((a, r, _), at)| {
        a == app_id
            && *r == repo
            && now
                .duration_since(*at)
                .is_ok_and(|age| age <= ITEM_SCOPED_EVIDENCE)
    })
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
    fn an_item_path_has_a_segment_past_its_family() {
        assert!(is_item_path("repos/acme/app/issues/7"));
        assert!(is_item_path("/repos/acme/app/pulls/5/files"));
        assert!(is_item_path("repos/{owner}/{repo}/issues/7"));
        assert!(!is_item_path("repos/acme/app/pulls?state=open"));
        assert!(!is_item_path("repos/acme/app/pulls/"));
        assert!(!is_item_path("repos/acme/app"));
        assert!(!is_item_path("search/issues?q=x"));
    }

    #[test]
    fn only_a_covered_item_scoped_404_is_an_answer() {
        use CoverageRefusal::{Answer, Shed, Withdraw};
        let now = SystemTime::now();
        let (app, repo) = ("w6-answer-1", "acme/app-answer");
        let j = |family: &str, item: bool, status: Option<u16>, stderr: &str| {
            judge_coverage(app, repo, family, item, status, stderr, now)
        };
        // No evidence at all: the reader is blind to the repo.
        assert_eq!(j("pulls", true, Some(404), ""), Withdraw);
        note_reader_served(app, repo, "pulls", now);
        assert_eq!(j("pulls", true, Some(404), ""), Answer);
        // A plain 404 from a reader that sees the repo, but no proof for
        // this read: no answer, and the reader stays on the repo.
        assert_eq!(j("pulls", false, Some(404), ""), Shed, "not item-scoped");
        assert_eq!(j("issues", true, Some(404), ""), Shed, "another family");
        // A 403, an unknown status or the integration refusals: blind,
        // whatever the evidence.
        assert_eq!(j("pulls", true, Some(403), ""), Withdraw, "a 403");
        assert_eq!(j("pulls", true, None, ""), Withdraw, "no status");
        let blind = "gh: Resource not accessible by integration (HTTP 404)";
        assert_eq!(j("pulls", true, Some(404), blind), Withdraw);
        let gql = "GraphQL: Could not resolve to a Repository with the name 'acme/app-answer'.";
        assert_eq!(j("pulls", true, Some(404), gql), Withdraw);
        // Another repo, another app, stale evidence: none.
        assert_eq!(judge_coverage(app, "acme/other", "pulls", true, Some(404), "", now), Withdraw);
        assert_eq!(
            judge_coverage("w6-answer-2", repo, "pulls", true, Some(404), "", now),
            Withdraw
        );
        let later = now + ITEM_SCOPED_EVIDENCE + Duration::from_secs(1);
        assert_eq!(judge_coverage(app, repo, "pulls", true, Some(404), "", later), Withdraw);
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
