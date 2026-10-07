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
pub const GROUPS_QUERY: &str = "query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    mergeQueue {
      entries(first: 100) {
        nodes { position headCommit { oid } pullRequest { number headRefOid } }
      }
    }
  }
}";

/// Decode [`GROUPS_QUERY`]. Pure. The group for the entry at position *k*
/// contains the entries at positions `1..=k`, oldest first. An entry with no
/// group commit yet (still being built) has no commit to fail and is skipped
/// as a group, but still counts as a member of the groups behind it. No
/// queue (`mergeQueue: null`) is no groups.
///
/// # Errors
///
/// A shape it cannot read, or an entry missing its PR.
pub fn parse_groups(data: &serde_json::Value) -> Result<Vec<MergeGroup>, String> {
    let repo = data
        .pointer("/data/repository")
        .ok_or("unexpected merge-queue shape")?;
    let Some(nodes) = repo
        .pointer("/mergeQueue/entries/nodes")
        .and_then(serde_json::Value::as_array)
    else {
        return if repo
            .get("mergeQueue")
            .is_some_and(serde_json::Value::is_null)
        {
            Ok(Vec::new())
        } else {
            Err("unexpected merge-queue shape".to_string())
        };
    };
    let mut entries: Vec<(u64, Member, Option<String>)> = Vec::new();
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
        entries.push((
            pos,
            Member {
                pr,
                head: head.to_string(),
            },
            commit,
        ));
    }
    entries.sort_by_key(|e| e.0);
    let members: Vec<Member> = entries.iter().map(|e| e.1.clone()).collect();
    Ok(entries
        .iter()
        .enumerate()
        .filter_map(|(k, e)| {
            e.2.as_ref().map(|c| MergeGroup {
                commit: c.clone(),
                members: members[..=k].to_vec(),
            })
        })
        .collect())
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
        let text = self.ok(
            "merge_queue.groups",
            AccessIntent::Read,
            &[
                "api".into(),
                "graphql".into(),
                "-f".into(),
                format!("query={GROUPS_QUERY}"),
                "-f".into(),
                format!("owner={}", self.owner()),
                "-f".into(),
                format!("name={}", self.repo_name()),
            ],
        )?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("unparseable queue: {e}"))?;
        if let Some(errs) = v.get("errors").filter(|e| !e.is_null()) {
            return Err(safe_detail(&errs.to_string()));
        }
        parse_groups(&v)
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
        let data = json!({"data": {"repository": {"mergeQueue": {"entries": {"nodes": [
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
        let data = json!({"data": {"repository": {"mergeQueue": {"entries": {"nodes": [
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
        let bad = json!({"data": {"repository": {"mergeQueue": {"entries": {"nodes": [
            {"position": 1, "pullRequest": {}}]}}}}});
        assert!(parse_groups(&bad).is_err());
    }
}
