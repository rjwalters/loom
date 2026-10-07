//! GitHub adapters for merge-group authorization and the group-aware
//! revocation entry point (#10256, Phase B5 of #9978).
//!
//! [`super::group_authz::revoke_refail_dequeue`] needs two forge reads and
//! writes it could not make on its own: *which* merge-group commits contain a
//! PR, and the commit-status write that re-fails
//! [`REQUIRED_CHECK_CONTEXT`] on each. [`GroupForge`] is that seam;
//! [`GhLifecycleForge`] is the GitHub implementation, and
//! [`revoke_for_transition_groups`] is what the Loom-owned transitions now call
//! instead of the single-PR [`super::lifecycle::revoke_for_transition`].
//!
//! Over-failing only ever withdraws authority, so every error path here
//! leaves the PR *less* mergeable, never more. Direct mode returns before the
//! first forge call.

use super::authz::{Revocation, REQUIRED_CHECK_CONTEXT};
use super::gh_lifecycle::GhLifecycleForge;
use super::github::safe_detail;
use super::grants::{reason_token, CommentGrantStore, GrantRecord};
use super::group_authz::{
    revoke_refail_dequeue, GroupRevocation, Member, MergeGroup, StatusApi, StatusState,
};
use super::lifecycle::{transition_line, Ctx};
use super::mode::MergeMode;
use crate::gh_invocation::AccessIntent;

/// Group discovery plus the status write.
pub trait GroupForge: StatusApi {
    /// Every live merge group on the default branch's queue, with its members.
    ///
    /// # Errors
    ///
    /// The queue could not be read or parsed.
    fn groups(&self) -> Result<Vec<MergeGroup>, String>;
}

/// Entries carry the group commit (`headCommit`) built for them.
pub const GROUPS_QUERY: &str = "query($owner: String!, $name: String!, $after: String) {
  repository(owner: $owner, name: $name) {
    mergeQueue {
      entries(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { position headCommit { oid } pullRequest { number headRefOid } }
      }
    }
  }
}";

/// Upper bound on pages read (100 entries each). A queue this deep, or a
/// cursor that never advances, is reported as an error rather than read
/// forever.
const MAX_PAGES: usize = 50;

/// One queue entry: position, member, and its group commit if built yet.
type Entry = (u64, Member, Option<String>);

/// Decode one page of [`GROUPS_QUERY`]: its entries and the cursor of the
/// next page (`None` on the last). No queue (`mergeQueue: null`) is an empty
/// last page.
///
/// # Errors
///
/// A shape it cannot read, an entry missing its PR, or a `pageInfo` that does
/// not say whether more entries follow (completeness cannot be confirmed).
fn parse_page(data: &serde_json::Value) -> Result<(Vec<Entry>, Option<String>), String> {
    let repo = data
        .pointer("/data/repository")
        .ok_or("unexpected merge-queue shape")?;
    let Some(entries) = repo.pointer("/mergeQueue/entries") else {
        return if repo
            .get("mergeQueue")
            .is_some_and(serde_json::Value::is_null)
        {
            Ok((Vec::new(), None))
        } else {
            Err("unexpected merge-queue shape".to_string())
        };
    };
    let nodes = entries
        .get("nodes")
        .and_then(serde_json::Value::as_array)
        .ok_or("unexpected merge-queue shape")?;
    let has_next = entries
        .pointer("/pageInfo/hasNextPage")
        .and_then(serde_json::Value::as_bool)
        .ok_or("merge queue page without pageInfo; cannot confirm all groups were read")?;
    let next = if has_next {
        Some(
            entries
                .pointer("/pageInfo/endCursor")
                .and_then(serde_json::Value::as_str)
                .ok_or("merge queue page has more entries but no cursor")?
                .to_string(),
        )
    } else {
        None
    };
    let mut out: Vec<Entry> = Vec::new();
    for n in nodes {
        let pr = n
            .pointer("/pullRequest/number")
            .and_then(serde_json::Value::as_u64)
            .and_then(|x| u32::try_from(x).ok())
            .ok_or("queue entry without a PR")?;
        let head = n
            .pointer("/pullRequest/headRefOid")
            .and_then(serde_json::Value::as_str)
            .ok_or("queue entry without a head")?;
        let pos = n
            .get("position")
            .and_then(serde_json::Value::as_u64)
            .ok_or("queue entry without a position")?;
        let commit = n
            .pointer("/headCommit/oid")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        out.push((
            pos,
            Member {
                pr,
                head: head.to_string(),
            },
            commit,
        ));
    }
    Ok((out, next))
}

/// Build the groups from every entry. The group for the entry at position
/// *k* contains the entries at positions `1..=k`, oldest first. An entry with
/// no group commit yet (still being built) has no commit to fail and is
/// skipped as a group, but still counts as a member of the groups behind it.
fn build_groups(mut entries: Vec<Entry>) -> Vec<MergeGroup> {
    entries.sort_by_key(|e| e.0);
    let members: Vec<Member> = entries.iter().map(|e| e.1.clone()).collect();
    entries
        .iter()
        .enumerate()
        .filter_map(|(k, e)| {
            e.2.as_ref().map(|c| MergeGroup {
                commit: c.clone(),
                members: members[..=k].to_vec(),
            })
        })
        .collect()
}

/// Read every page via `fetch` (given the cursor to resume after, `None` for
/// the first page) and build the groups from the complete queue. Discovery is
/// all-or-nothing: a failed or unconfirmable later page is an error, never a
/// shorter group list, because [`GroupRevocation::passed_checks_withdrawn`]
/// reads an empty list as "nothing left to withdraw".
///
/// # Errors
///
/// A page that cannot be fetched or parsed, a cursor that repeats, or a queue
/// deeper than [`MAX_PAGES`] pages.
pub fn fetch_all_groups(
    fetch: &dyn Fn(Option<&str>) -> Result<serde_json::Value, String>,
) -> Result<Vec<MergeGroup>, String> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let (page, next) = parse_page(&fetch(cursor.as_deref())?)?;
        entries.extend(page);
        match next {
            None => return Ok(build_groups(entries)),
            Some(c) if cursor.as_deref() == Some(c.as_str()) => {
                return Err("merge queue cursor did not advance".to_string());
            }
            Some(c) => cursor = Some(c),
        }
    }
    Err(format!(
        "merge queue deeper than {MAX_PAGES} pages; cannot confirm all groups were read"
    ))
}

/// Decode a single-page [`GROUPS_QUERY`] response. Pure.
///
/// # Errors
///
/// As [`parse_page`], and a page that says more entries follow (use
/// [`fetch_all_groups`] to read them).
pub fn parse_groups(data: &serde_json::Value) -> Result<Vec<MergeGroup>, String> {
    let (entries, next) = parse_page(data)?;
    if next.is_some() {
        return Err("merge queue has more pages; single-page parse is incomplete".to_string());
    }
    Ok(build_groups(entries))
}

impl StatusApi for GhLifecycleForge {
    fn post_status(
        &self,
        commit: &str,
        state: StatusState,
        description: &str,
    ) -> Result<(), String> {
        if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("refusing to post a status to non-sha {commit:?}"));
        }
        let state = match state {
            StatusState::Success => "success",
            StatusState::Pending => "pending",
            StatusState::Failure => "failure",
        };
        let desc: String = description.chars().take(140).collect();
        self.ok(
            "merge_queue.post_status",
            AccessIntent::Write,
            &[
                "api".into(),
                "-X".into(),
                "POST".into(),
                format!("repos/{}/statuses/{commit}", self.nwo()),
                "-f".into(),
                format!("state={state}"),
                "-f".into(),
                format!("context={REQUIRED_CHECK_CONTEXT}"),
                "-f".into(),
                format!("description={desc}"),
            ],
        )
        .map(|_| ())
    }
}

impl GroupForge for GhLifecycleForge {
    fn groups(&self) -> Result<Vec<MergeGroup>, String> {
        fetch_all_groups(&|after| {
            let mut args: Vec<String> = vec![
                "api".into(),
                "graphql".into(),
                "-f".into(),
                format!("query={GROUPS_QUERY}"),
                "-f".into(),
                format!("owner={}", self.owner()),
                "-f".into(),
                format!("name={}", self.repo_name()),
            ];
            if let Some(c) = after {
                args.push("-f".into());
                args.push(format!("after={c}"));
            }
            let text = self.ok("merge_queue.groups", AccessIntent::Read, &args)?;
            let v: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| format!("unparseable queue: {e}"))?;
            if let Some(errs) = v.get("errors").filter(|e| !e.is_null()) {
                return Err(safe_detail(&errs.to_string()));
            }
            Ok(v)
        })
    }
}

/// Revoke, re-fail every live merge group containing `pr`, then dequeue,
/// before a Loom-owned transition. `None` in direct mode (nothing read or
/// written). Group discovery or a status write failing is reported through
/// [`GroupRevocation::refailed`], never swallowed.
#[must_use]
pub fn revoke_for_transition_groups(
    ctx: &Ctx<'_>,
    groups: &dyn GroupForge,
    pr: u32,
    reason: &str,
) -> Option<GroupRevocation> {
    if ctx.mode == MergeMode::Direct {
        return None;
    }
    let token = format!("revoked-{}", reason_token(reason));
    let store = CommentGrantStore::new(ctx.forge, &token, ctx.now);
    let before = store.record(pr).unwrap_or(GrantRecord::None);
    let rev = revoke_refail_dequeue(
        ctx.mode,
        ctx.execution_enabled,
        ctx.queue,
        &store,
        groups,
        &|| groups.groups(),
        pr,
    );
    if rev.revocation.grant_revoked.is_ok() {
        ctx.removed_event(pr, &before, token, None);
    }
    Some(rev)
}

/// Audit line for a transition comment, including the group re-fail result
/// and the residual window when a passed check could not be withdrawn.
#[must_use]
pub fn group_transition_line(rev: &GroupRevocation) -> String {
    let base = transition_line(&Revocation {
        grant_revoked: rev.revocation.grant_revoked.clone(),
        dequeue: rev.revocation.dequeue.clone(),
    });
    let tail = if rev.passed_checks_withdrawn() {
        format!("{rev}")
    } else {
        format!("{rev}; a fully passed group could still merge it (residual window)")
    };
    format!("{base} Merge groups: {tail}.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(pos: u64, pr: u64, commit: Option<&str>) -> serde_json::Value {
        json!({"position": pos, "headCommit": commit.map(|c| json!({"oid": c})),
               "pullRequest": {"number": pr, "headRefOid": format!("{pr:040x}")}})
    }

    #[test]
    fn group_k_contains_entries_one_through_k_in_position_order() {
        let c1 = "1".repeat(40);
        let c2 = "2".repeat(40);
        let data = json!({"data": {"repository": {"mergeQueue": {"entries": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [
            node(2, 20, Some(&c2)), node(1, 10, Some(&c1))]}}}}});
        let g = parse_groups(&data).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].commit, c1);
        assert_eq!(g[0].members.iter().map(|m| m.pr).collect::<Vec<_>>(), [10]);
        assert_eq!(g[1].members.iter().map(|m| m.pr).collect::<Vec<_>>(), [10, 20]);
    }

    #[test]
    fn entry_without_commit_is_a_member_but_not_a_group() {
        let c2 = "2".repeat(40);
        let data = json!({"data": {"repository": {"mergeQueue": {"entries": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [
            node(1, 10, None), node(2, 20, Some(&c2))]}}}}});
        let g = parse_groups(&data).unwrap();
        assert_eq!(g.len(), 1);
        assert!(g[0].contains(10) && g[0].contains(20));
    }

    #[test]
    fn no_queue_is_no_groups_and_garbage_is_an_error() {
        let none = json!({"data": {"repository": {"mergeQueue": null}}});
        assert!(parse_groups(&none).unwrap().is_empty());
        assert!(parse_groups(&json!({"data": {}})).is_err());
        assert!(parse_groups(&json!({"data": {"repository": {"mergeQueue": {}}}})).is_err());
        let bad = json!({"data": {"repository": {"mergeQueue": {"entries": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [
            {"position": 1, "pullRequest": {}}]}}}}});
        assert!(parse_groups(&bad).is_err());
    }

    fn page(nodes: &[serde_json::Value], next: Option<&str>) -> serde_json::Value {
        json!({"data": {"repository": {"mergeQueue": {"entries": {
            "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next},
            "nodes": nodes}}}}})
    }

    #[test]
    fn multi_page_queue_yields_cumulative_groups_across_pages() {
        let (c1, c2, c3) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
        let p1 = page(&[node(1, 10, Some(&c1)), node(2, 20, Some(&c2))], Some("cur1"));
        let p2 = page(&[node(3, 30, Some(&c3))], None);
        let seen = std::cell::RefCell::new(Vec::new());
        let g = fetch_all_groups(&|after| {
            seen.borrow_mut().push(after.map(str::to_string));
            Ok(if after.is_none() {
                p1.clone()
            } else {
                p2.clone()
            })
        })
        .unwrap();
        assert_eq!(*seen.borrow(), [None, Some("cur1".to_string())]);
        assert_eq!(g.len(), 3);
        assert_eq!(g[2].members.iter().map(|m| m.pr).collect::<Vec<_>>(), [10, 20, 30]);
        assert!(g[2].contains(30));
    }

    #[test]
    fn later_page_failure_is_an_error_not_a_shorter_group_list() {
        let c1 = "1".repeat(40);
        let p1 = page(&[node(1, 10, Some(&c1))], Some("cur1"));
        let r = fetch_all_groups(&|after| match after {
            None => Ok(p1.clone()),
            Some(_) => Err("boom".to_string()),
        });
        assert_eq!(r.unwrap_err(), "boom");
    }

    #[test]
    fn unconfirmable_or_runaway_pagination_is_an_error() {
        let c1 = "1".repeat(40);
        // Single-page parse refuses a page that says more follow.
        assert!(parse_groups(&page(&[node(1, 10, Some(&c1))], Some("c"))).is_err());
        // Missing pageInfo cannot confirm completeness.
        let no_info = json!({"data": {"repository": {"mergeQueue": {"entries": {
            "nodes": [node(1, 10, Some(&c1))]}}}}});
        assert!(fetch_all_groups(&|_| Ok(no_info.clone())).is_err());
        // A cursor that never advances.
        let stuck = page(&[node(1, 10, Some(&c1))], Some("same"));
        let e = fetch_all_groups(&|_| Ok(stuck.clone())).unwrap_err();
        assert!(e.contains("did not advance"), "{e}");
        // hasNextPage without a cursor.
        let nocur = json!({"data": {"repository": {"mergeQueue": {"entries": {
            "pageInfo": {"hasNextPage": true, "endCursor": null}, "nodes": []}}}}});
        assert!(fetch_all_groups(&|_| Ok(nocur.clone())).is_err());
    }
}
