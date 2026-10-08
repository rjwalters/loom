//! The `merge_group` check runner (#10256, Phase B6 of #9978): the piece that
//! turns [`group_check`] into a posted `loom/merge-authorization` status.
//!
//! [`run_group_check`] finds the live merge group for a commit, reads the
//! state of the other required contexts, evaluates every member, and posts the
//! result as the commit status. Fail-closed rules:
//!
//! - a group that cannot be found, or whose discovery read fails, posts
//!   nothing success-shaped (an unfound group posts `failure`; a failed read
//!   posts nothing, and a context that never reports blocks the merge);
//! - a status write that is not confirmed is reported, never assumed;
//! - only [`GroupConclusion::Success`] ever posts `success`.
//!
//! Required-context states ([`states_from`]) count only `success`, `neutral`
//! and `skipped` as passed (GitHub's own rule for required checks), pick the
//! newest run per name, and treat a truncated listing as unreadable.

use serde_json::Value;

use super::authz::{AuthzFacts, GrantStore, REQUIRED_CHECK_CONTEXT};
use super::gh_lifecycle::GhLifecycleForge;
use super::group_authz::{group_check, status_for, CheckState, GroupConclusion, MergeGroup};
use super::group_github::GroupForge;
use crate::gh_invocation::AccessIntent;

/// What happened to one `group-check` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The conclusion was reached and its status confirmed posted.
    Posted(GroupConclusion),
    /// The conclusion was reached but the status write was not confirmed.
    /// Without a posted `success` GitHub cannot merge the group.
    PostFailed(GroupConclusion, String),
    /// The merge groups could not be read; nothing was posted.
    Unreadable(String),
}

/// Reads the state of each required context on a commit.
pub trait ContextStates {
    /// # Errors
    ///
    /// The commit's check runs or statuses could not be read completely.
    fn states(&self, commit: &str) -> Result<Vec<(String, CheckState)>, String>;
}

/// Evaluate and post the check for the merge group built at `commit`.
pub fn run_group_check(
    store: &dyn GrantStore,
    forge: &dyn GroupForge,
    states: &dyn ContextStates,
    facts_for: &dyn Fn(u32) -> Result<AuthzFacts, String>,
    commit: &str,
) -> RunOutcome {
    let groups = match forge.groups() {
        Ok(g) => g,
        Err(e) => return RunOutcome::Unreadable(e),
    };
    let conclusion = match groups
        .iter()
        .find(|g| g.commit.eq_ignore_ascii_case(commit))
    {
        Some(g) => group_check(store, g, states.states(commit), facts_for),
        None => {
            // Not a live group (rebuilt or dequeued): it must not authorize.
            let g = MergeGroup {
                commit: commit.to_string(),
                members: Vec::new(),
            };
            group_check(store, &g, Ok(Vec::new()), facts_for)
        }
    };
    let (state, desc) = status_for(&conclusion);
    match forge.post_status(commit, state, &desc) {
        Ok(()) => RunOutcome::Posted(conclusion),
        Err(e) => RunOutcome::PostFailed(conclusion, e),
    }
}

fn rank(s: CheckState) -> u8 {
    match s {
        CheckState::Success => 0,
        CheckState::Pending => 1,
        CheckState::Failure => 2,
    }
}

fn run_state(status: &str, conclusion: Option<&str>) -> CheckState {
    match (status, conclusion) {
        ("completed", Some("success" | "neutral" | "skipped")) => CheckState::Success,
        ("completed", _) => CheckState::Failure,
        _ => CheckState::Pending,
    }
}

fn status_state(s: &str) -> CheckState {
    match s {
        "success" => CheckState::Success,
        "pending" => CheckState::Pending,
        _ => CheckState::Failure,
    }
}

/// Derive each `required` context's state from the commit's check-runs and
/// commit-statuses JSON. Pure. A required context with no report yet is
/// `Pending`; [`REQUIRED_CHECK_CONTEXT`] itself is never listed.
///
/// # Errors
///
/// A shape it cannot read, or a listing shorter than its own `total_count`
/// (incomplete evidence is an outage, not a pass).
pub fn states_from(
    required: &[String],
    check_runs: &Value,
    statuses: &Value,
) -> Result<Vec<(String, CheckState)>, String> {
    let runs = check_runs
        .get("check_runs")
        .and_then(Value::as_array)
        .ok_or("unexpected check-runs shape")?;
    let total = check_runs
        .get("total_count")
        .and_then(Value::as_u64)
        .ok_or("check-runs without total_count")?;
    if usize::try_from(total).map_or(true, |t| t > runs.len()) {
        return Err(format!("check-runs listing incomplete ({} of {total})", runs.len()));
    }
    let sts = statuses
        .get("statuses")
        .and_then(Value::as_array)
        .ok_or("unexpected statuses shape")?;
    let st_total = statuses
        .get("total_count")
        .and_then(Value::as_u64)
        .ok_or("statuses without total_count")?;
    if usize::try_from(st_total).map_or(true, |t| t > sts.len()) {
        return Err(format!("statuses listing incomplete ({} of {st_total})", sts.len()));
    }
    let mut out = Vec::new();
    for ctx in required.iter().filter(|c| *c != REQUIRED_CHECK_CONTEXT) {
        // Newest check run (highest id) and newest status (first listed) for
        // the context; the worse of the two decides.
        let run = runs
            .iter()
            .filter(|r| r.get("name").and_then(Value::as_str) == Some(ctx))
            .max_by_key(|r| r.get("id").and_then(Value::as_u64).unwrap_or(0))
            .map(|r| {
                run_state(
                    r.get("status").and_then(Value::as_str).unwrap_or(""),
                    r.get("conclusion").and_then(Value::as_str),
                )
            });
        let st = sts
            .iter()
            .find(|s| s.get("context").and_then(Value::as_str) == Some(ctx))
            .map(|s| status_state(s.get("state").and_then(Value::as_str).unwrap_or("")));
        let state = match (run, st) {
            (None, None) => CheckState::Pending,
            (a, b) => [a, b]
                .into_iter()
                .flatten()
                .max_by_key(|s| rank(*s))
                .unwrap_or(CheckState::Pending),
        };
        out.push((ctx.clone(), state));
    }
    Ok(out)
}

/// GitHub [`ContextStates`]: reads the commit's check runs and statuses for
/// the ruleset's required contexts.
pub struct GhContextStates<'a> {
    pub forge: &'a GhLifecycleForge,
    /// Required contexts from the capability preflight.
    pub required: Vec<String>,
}

impl ContextStates for GhContextStates<'_> {
    fn states(&self, commit: &str) -> Result<Vec<(String, CheckState)>, String> {
        if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("not a commit sha: {commit:?}"));
        }
        let get = |op: &'static str, tail: &str| -> Result<Value, String> {
            let text = self.forge.ok(
                op,
                AccessIntent::Read,
                &[
                    "api".into(),
                    format!("repos/{}/commits/{commit}/{tail}?per_page=100", self.forge.nwo()),
                ],
            )?;
            serde_json::from_str(&text).map_err(|e| format!("{op}: unparseable: {e}"))
        };
        let runs = get("merge_queue.check_runs", "check-runs")?;
        let sts = get("merge_queue.statuses", "status")?;
        states_from(&self.required, &runs, &sts)
    }
}

/// Daemon pass: evaluate every live merge group so a group whose other
/// checks finished since the last look gets its verdict without waiting for
/// a workflow. Queue mode only. This is an accelerator: the enforcement is
/// the required context itself, which stays unreported (blocking) if this
/// never runs.
pub fn tick_groups(gh: &str, forge: &GhLifecycleForge) {
    use super::grants::CommentGrantStore;
    let required = match super::preflight::github_preflight(gh, &forge.nwo(), None) {
        Ok(c) => c.required_checks,
        Err(e) => {
            log::warn!("merge-queue: group pass skipped, required contexts unreadable: {e}");
            return;
        }
    };
    let groups = match forge.groups() {
        Ok(g) => g,
        Err(e) => {
            log::warn!("merge-queue: group pass skipped, queue unreadable: {e}");
            return;
        }
    };
    let store = CommentGrantStore::new(forge, "group-check", chrono::Utc::now());
    let states = GhContextStates { forge, required };
    for g in groups {
        match run_group_check(
            &store,
            forge,
            &states,
            &|pr| super::forge::read_facts(forge, pr),
            &g.commit,
        ) {
            RunOutcome::Posted(c) => log::info!("merge-queue: group {} -> {c:?}", g.commit),
            RunOutcome::PostFailed(c, e) => {
                log::warn!("merge-queue: group {} {c:?}, status unconfirmed: {e}", g.commit);
            }
            RunOutcome::Unreadable(e) => log::warn!("merge-queue: group pass: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::cell::RefCell;

    use super::super::authz::{Grant, MemoryGrantStore};
    use super::super::group_authz::{Member, StatusApi, StatusState};
    use super::*;
    use serde_json::json;

    const HA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const GC: &str = "9999999999999999999999999999999999999999";

    struct Fake {
        groups: Result<Vec<MergeGroup>, String>,
        post_ok: bool,
        posted: RefCell<Vec<(String, StatusState)>>,
    }
    impl StatusApi for Fake {
        fn post_status(&self, c: &str, s: StatusState, _d: &str) -> Result<(), String> {
            self.posted.borrow_mut().push((c.to_string(), s));
            if self.post_ok {
                Ok(())
            } else {
                Err("status api down".into())
            }
        }
    }
    impl GroupForge for Fake {
        fn groups(&self) -> Result<Vec<MergeGroup>, String> {
            self.groups.clone()
        }
    }
    struct States(Result<Vec<(String, CheckState)>, String>);
    impl ContextStates for States {
        fn states(&self, _c: &str) -> Result<Vec<(String, CheckState)>, String> {
            self.0.clone()
        }
    }

    fn fake(groups: Result<Vec<MergeGroup>, String>, post_ok: bool) -> Fake {
        Fake {
            groups,
            post_ok,
            posted: RefCell::default(),
        }
    }
    fn group() -> MergeGroup {
        MergeGroup {
            commit: GC.into(),
            members: vec![Member {
                pr: 1,
                head: HA.into(),
            }],
        }
    }
    fn granted() -> MemoryGrantStore {
        let s = MemoryGrantStore::default();
        s.put(Grant {
            pr: 1,
            approved_sha: HA.into(),
        })
        .unwrap();
        s
    }
    fn ci(s: CheckState) -> States {
        States(Ok(vec![("ci".into(), s)]))
    }
    fn good(_: u32) -> Result<AuthzFacts, String> {
        Ok(AuthzFacts::approved(HA))
    }
    fn last(f: &Fake) -> StatusState {
        f.posted.borrow().last().unwrap().1
    }

    #[test]
    fn authorized_group_with_green_ci_posts_success() {
        let f = fake(Ok(vec![group()]), true);
        let r = run_group_check(&granted(), &f, &ci(CheckState::Success), &good, GC);
        assert_eq!(r, RunOutcome::Posted(GroupConclusion::Success));
        assert_eq!(last(&f), StatusState::Success);
    }

    #[test]
    fn pending_ci_posts_pending_and_never_reads_facts() {
        let f = fake(Ok(vec![group()]), true);
        let boom = |_: u32| -> Result<AuthzFacts, String> { panic!("facts read while pending") };
        run_group_check(&granted(), &f, &ci(CheckState::Pending), &boom, GC);
        assert_eq!(last(&f), StatusState::Pending);
    }

    #[test]
    fn revoked_grant_posts_failure_even_with_green_ci() {
        let s = granted();
        s.revoke(1).unwrap();
        let f = fake(Ok(vec![group()]), true);
        run_group_check(&s, &f, &ci(CheckState::Success), &good, GC);
        assert_eq!(last(&f), StatusState::Failure);
    }

    #[test]
    fn hold_added_after_grant_posts_failure() {
        let f = fake(Ok(vec![group()]), true);
        let held = |_: u32| {
            let mut x = AuthzFacts::approved(HA);
            x.human_hold = super::super::authz::Fact::Known(true);
            Ok(x)
        };
        run_group_check(&granted(), &f, &ci(CheckState::Success), &held, GC);
        assert_eq!(last(&f), StatusState::Failure);
    }

    #[test]
    fn group_discovery_outage_posts_nothing() {
        let f = fake(Err("graphql 502".into()), true);
        let r = run_group_check(&granted(), &f, &ci(CheckState::Success), &good, GC);
        assert!(matches!(r, RunOutcome::Unreadable(_)));
        assert!(f.posted.borrow().is_empty());
    }

    #[test]
    fn unknown_commit_never_posts_success() {
        let f = fake(Ok(vec![group()]), true);
        let other = "8888888888888888888888888888888888888888";
        run_group_check(&granted(), &f, &ci(CheckState::Success), &good, other);
        assert_eq!(last(&f), StatusState::Failure);
    }

    #[test]
    fn unreadable_context_states_never_post_success() {
        let f = fake(Ok(vec![group()]), true);
        run_group_check(&granted(), &f, &States(Err("down".into())), &good, GC);
        assert_ne!(last(&f), StatusState::Success);
    }

    #[test]
    fn unconfirmed_post_is_reported() {
        let f = fake(Ok(vec![group()]), false);
        let r = run_group_check(&granted(), &f, &ci(CheckState::Success), &good, GC);
        assert!(matches!(r, RunOutcome::PostFailed(GroupConclusion::Success, _)));
    }

    fn req() -> Vec<String> {
        vec!["ci".into(), REQUIRED_CHECK_CONTEXT.into()]
    }

    #[test]
    fn states_newest_run_wins_and_missing_is_pending() {
        let runs = json!({"total_count": 2, "check_runs": [
            {"id": 1, "name": "ci", "status": "completed", "conclusion": "failure"},
            {"id": 2, "name": "ci", "status": "completed", "conclusion": "success"}]});
        let st = json!({"total_count": 0, "statuses": []});
        assert_eq!(
            states_from(&req(), &runs, &st).unwrap(),
            vec![("ci".to_string(), CheckState::Success)]
        );
        let none = json!({"total_count": 0, "check_runs": []});
        assert_eq!(states_from(&req(), &none, &st).unwrap()[0].1, CheckState::Pending);
    }

    #[test]
    fn states_worse_of_run_and_status_and_cancelled_fails() {
        let runs = json!({"total_count": 1, "check_runs": [
            {"id": 1, "name": "ci", "status": "completed", "conclusion": "success"}]});
        let st = json!({"total_count": 1, "statuses": [{"context": "ci", "state": "failure"}]});
        assert_eq!(states_from(&req(), &runs, &st).unwrap()[0].1, CheckState::Failure);
        let cancelled = json!({"total_count": 1, "check_runs": [
            {"id": 1, "name": "ci", "status": "completed", "conclusion": "cancelled"}]});
        let empty = json!({"total_count": 0, "statuses": []});
        assert_eq!(states_from(&req(), &cancelled, &empty).unwrap()[0].1, CheckState::Failure);
    }

    #[test]
    fn truncated_listing_is_an_error() {
        let runs = json!({"total_count": 150, "check_runs": [
            {"id": 1, "name": "ci", "status": "completed", "conclusion": "success"}]});
        let st = json!({"total_count": 0, "statuses": []});
        assert!(states_from(&req(), &runs, &st).is_err());
        assert!(states_from(&req(), &json!({}), &st).is_err());
    }
}
