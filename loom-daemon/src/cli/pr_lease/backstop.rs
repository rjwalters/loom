//! The PR-lease half of the Judge no-verdict backstop (#11855): `release`
//! records a Judge pass that ended without a verdict, and `claim` refuses a
//! Judge a PR those records hold. The rules live in
//! [`loom_daemon::judge_backstop`].

use chrono::{DateTime, Utc};
use loom_daemon::forge_call_stats::ops;
use loom_daemon::gh_invocation::AccessIntent;
use loom_daemon::judge_backstop::{self as backstop, CiRead, CiState};

use super::{Ctx, Gh, Role, Row};

/// `claim`: a Judge refused by the backstop. Like 4 and 5, the prompt's
/// `[[ $? =~ ^[02]$ ]]` reads it as "stand down; next PR".
pub(crate) const EXIT_BACKSTOP: i32 = 6;

impl Gh {
    /// `(open, head sha, labels)` for `pr`; `None` when unreadable.
    pub(crate) fn pr_head(&self, pr: u64) -> Option<(bool, String, Vec<String>)> {
        let out = self
            .inv("pr_lease.pr_head", AccessIntent::Read, ops::PR_VIEW_STATE)
            .args([
                "api",
                &format!("repos/{{owner}}/{{repo}}/pulls/{pr}"),
                "--jq",
                "{state: .state, head: .head.sha, labels: [.labels[].name]}",
            ])
            .run();
        let v: serde_json::Value = serde_json::from_slice(&out.ok_output()?.stdout).ok()?;
        let open = v.get("state")?.as_str()? == "open";
        let head = v.get("head")?.as_str()?.to_string();
        let labels = v
            .get("labels")?
            .as_array()?
            .iter()
            .filter_map(|l| l.as_str().map(str::to_string))
            .collect();
        Some((open, head, labels))
    }

    /// `sha`'s CI as the backstop sees it ([`backstop::ci_state`]).
    pub(crate) fn ci_state(&self, sha: &str) -> CiState {
        backstop::ci_state(sha, Utc::now(), |kind, path| {
            let (name, op) = match kind {
                CiRead::CheckRuns => ("pr_lease.checks_pending", ops::CI_CHECK_RUNS_FOR_SHA),
                CiRead::CommitStatus => ("pr_lease.commit_status", ops::CI_COMMIT_STATUS),
            };
            let out = self
                .inv(name, AccessIntent::Read, op)
                .args(["api", &format!("repos/{{owner}}/{{repo}}/{path}")])
                .run();
            serde_json::from_slice(&out.ok_output()?.stdout).ok()
        })
    }
}

/// The fields a Judge release appends to its marker: the no-verdict record
/// when the PR is still waiting for review with no verdict, else nothing (a
/// verdict landed, the PR closed, or its state could not be read). A pass that
/// deferred because the head's checks are still running (`ci_exempts`, asked
/// only once a record is otherwise due; [`CiState::exempts`]) is not a
/// blocked pass: CI settling changes neither the head nor the labels, so a
/// record would hold the PR from Judge long after CI went green.
pub(crate) fn release_fields(
    role: Role,
    state: Option<(bool, String, Vec<String>)>,
    ci_exempts: impl FnOnce(&str) -> bool,
) -> String {
    match state {
        Some((open, head, labels))
            if role == Role::Judge
                && backstop::ended_without_verdict(open, &labels)
                && !ci_exempts(&head) =>
        {
            backstop::released_fields(&head, &labels)
        }
        _ => String::new(),
    }
}

/// The fields the reap of a terminated Judge appends (#11931): the no-verdict
/// record, `reaped=<reason>` added, when the PR is still waiting for review
/// with no verdict; else nothing. Unlike [`release_fields`] there is no CI
/// exemption: a pass the role runner killed at its timeout ceiling spent the
/// whole ceiling, and is exactly the re-run the backstop exists to bound.
pub(crate) fn reaped_release_fields(
    role: Role,
    state: Option<(bool, String, Vec<String>)>,
    reason: &str,
) -> String {
    match state {
        Some((open, head, labels))
            if role == Role::Judge && backstop::ended_without_verdict(open, &labels) =>
        {
            backstop::reaped_fields(&head, &labels, reason)
        }
        _ => String::new(),
    }
}

/// The `outcome=` field a Champion release records (#12616): the step's
/// `CHAMPION-STEP` outcome word, or `none` when it wrote no line (a step that
/// died). A merge leaves the release as it was. Anything but `[A-Za-z0-9_-]`
/// is dropped so the word cannot break the marker.
pub(crate) fn champion_outcome_fields(role: Role, outcome: Option<&str>) -> String {
    if role != Role::Champion {
        return String::new();
    }
    let word: String = outcome
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(32)
        .collect();
    match word.as_str() {
        "merged" => String::new(),
        "" => " outcome=none".to_string(),
        w => format!(" outcome={w}"),
    }
}

/// When the backstop's hold on this PR ends, from its trusted lease `rows`
/// and its live `(open, head, labels)`; `None` when not held. Pure.
pub(crate) fn hold(
    rows: &[Row],
    state: Option<(bool, String, Vec<String>)>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let (open, head, labels) = state?;
    if !open {
        return None;
    }
    let records: Vec<_> = rows
        .iter()
        .filter_map(|r| backstop::parse(&r.body).map(|nv| (r.updated_at, nv)))
        .collect();
    backstop::hold_until(&records, &head, &backstop::label_digest(&labels), now)
}

impl Ctx {
    /// `claim`'s backstop check: `Some(exit)` when this Judge must stand down.
    pub(crate) fn backstop_refusal(&self, rows: &[Row]) -> Option<i32> {
        if self.me.role != Role::Judge || !backstop::enabled() {
            return None;
        }
        let until = hold(rows, self.gh.pr_head(self.pr()), (self.now)())?;
        self.say(&format!(
            "BACKSTOP: Judge passes at this head and label set already ended without a verdict \
             (#11855); held until {} — stand down, next PR (a push or label change releases \
             it; {}=0 disables)",
            until.to_rfc3339(),
            backstop::DISABLE_ENV
        ));
        Some(EXIT_BACKSTOP)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// [`backstop::ci_state`] with `Gh::ci_state`'s op names and full paths.
    fn ci_state(
        sha: &str,
        now: DateTime<Utc>,
        mut get: impl FnMut(&str, &str) -> Option<serde_json::Value>,
    ) -> CiState {
        backstop::ci_state(sha, now, |kind, path| {
            let op = match kind {
                CiRead::CheckRuns => "pr_lease.checks_pending",
                CiRead::CommitStatus => "pr_lease.commit_status",
            };
            get(op, &format!("repos/{{owner}}/{{repo}}/{path}"))
        })
    }

    fn ci_pending(
        sha: &str,
        now: DateTime<Utc>,
        get: impl FnMut(&str, &str) -> Option<serde_json::Value>,
    ) -> bool {
        ci_state(sha, now, get) == CiState::Young
    }

    const HEAD: &str = "3333333333333333333333333333333333333333";

    fn state(labels: &[&str]) -> Option<(bool, String, Vec<String>)> {
        Some((true, HEAD.into(), labels.iter().map(|l| (*l).to_string()).collect()))
    }

    fn row(id: u64, at: DateTime<Utc>, fields: &str) -> Row {
        Row {
            id,
            updated_at: at,
            body: format!("<!-- loom:pr-lease-released role=judge host=h sweep=s{id}{fields} -->"),
        }
    }

    #[test]
    fn only_a_judge_release_without_a_verdict_is_stamped() {
        let waiting = ["loom:review-requested", "loom:reviewing"];
        let stamped = release_fields(Role::Judge, state(&waiting), |_| false);
        assert_eq!(stamped, format!(" head={HEAD} verdict=none labels=review-requested"));
        assert_eq!(release_fields(Role::Doctor, state(&waiting), |_| false), "");
        assert_eq!(release_fields(Role::Judge, state(&["loom:pr"]), |_| false), "");
        assert_eq!(release_fields(Role::Judge, None, |_| false), "");
    }

    #[test]
    fn a_pass_deferred_on_pending_checks_is_not_stamped() {
        let waiting = ["loom:review-requested", "loom:reviewing"];
        let mut asked = None;
        let fields = release_fields(Role::Judge, state(&waiting), |head| {
            asked = Some(head.to_string());
            true
        });
        assert_eq!(fields, "");
        assert_eq!(asked.as_deref(), Some(HEAD));
        // Never asked when no record would be written anyway.
        let fields = release_fields(Role::Judge, state(&["loom:pr"]), |_| panic!("asked"));
        assert_eq!(fields, "");
    }

    #[test]
    fn stamped_releases_hold_the_pr_and_plain_ones_do_not() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
        let fields = release_fields(Role::Judge, state(&["loom:review-requested"]), |_| false);
        let earlier = now - chrono::Duration::minutes(10);
        let plain = [row(1, earlier, "")];
        assert_eq!(hold(&plain, state(&["loom:review-requested"]), now), None);
        let stamped = [row(1, earlier, &fields)];
        assert_eq!(
            hold(&stamped, state(&["loom:review-requested", "loom:reviewing"]), now),
            Some(earlier + chrono::Duration::hours(1))
        );
        // A closed PR, or one whose state is unreadable, is never held.
        assert_eq!(hold(&stamped, None, now), None);
        let closed = Some((false, HEAD.to_string(), vec!["loom:review-requested".to_string()]));
        assert_eq!(hold(&stamped, closed, now), None);
    }

    fn runs(total: usize, page: usize, pending_at: Option<&str>) -> serde_json::Value {
        let mut rows: Vec<serde_json::Value> = (0..page)
            .map(|_| serde_json::json!({"status": "completed", "started_at": "2026-10-14T05:00:00Z"}))
            .collect();
        if let Some(at) = pending_at {
            rows.pop();
            rows.push(serde_json::json!({"status": "in_progress", "started_at": at}));
        }
        serde_json::json!({"total_count": total, "check_runs": rows})
    }

    fn status(statuses: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"state": "pending", "statuses": statuses})
    }

    /// Answers `ci_pending`'s reads from `pages` (check-run page N at index
    /// N-1) and `combined`, recording every path asked.
    fn read_ci(
        pages: &[serde_json::Value],
        combined: Option<serde_json::Value>,
        now: DateTime<Utc>,
    ) -> (bool, Vec<String>) {
        let mut asked = Vec::new();
        let pending = ci_pending(HEAD, now, |op, path| {
            asked.push(format!("{op} {path}"));
            if path.contains("/status?") {
                return combined.clone();
            }
            let page = path
                .split("&page=")
                .nth(1)
                .map_or(1, |n| n.parse::<usize>().unwrap());
            pages.get(page - 1).cloned()
        });
        (pending, asked)
    }

    #[test]
    fn a_pending_run_past_the_first_hundred_is_read() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
        let pages = [
            runs(150, 100, None),
            runs(150, 50, Some("2026-10-14T05:30:00Z")),
        ];
        let (pending, asked) = read_ci(&pages, None, now);
        assert!(pending, "the in-progress run on page 2 exempts the pass");
        let base = format!("repos/{{owner}}/{{repo}}/commits/{HEAD}/check-runs?per_page=100");
        assert_eq!(
            asked,
            vec![
                format!("pr_lease.checks_pending {base}"),
                format!("pr_lease.checks_pending {base}&page=2"),
            ],
            "stops at the exempting page, before the statuses"
        );
        // All 150 completed: both pages read, then the statuses; not pending.
        let pages = [runs(150, 100, None), runs(150, 50, None)];
        let (pending, asked) = read_ci(&pages, Some(status(serde_json::json!([]))), now);
        assert!(!pending);
        assert_eq!(asked.len(), 3);
        assert!(asked[2].starts_with("pr_lease.commit_status "));
        assert!(asked[2].ends_with(&format!("commits/{HEAD}/status?per_page=100")));
    }

    #[test]
    fn check_run_paging_is_bounded() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
        let pages: Vec<_> = (0..20).map(|_| runs(2000, 100, None)).collect();
        let (pending, asked) = read_ci(&pages, None, now);
        assert!(!pending);
        assert_eq!(asked.len(), backstop::MAX_CHECK_RUN_PAGES + 1, "capped pages + statuses");
        // A short page ends the paging even when total_count says more.
        let (_, asked) = read_ci(&[runs(500, 40, None)], None, now);
        assert_eq!(asked.len(), 2);
    }

    /// #11972: stuck CI (pending, but all of it past the window or
    /// unstamped) is told apart from settled CI, and only a `ci-pending`
    /// skip is exempted by settled CI.
    #[test]
    fn ci_state_tells_stuck_from_settled() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
        let one =
            |run: serde_json::Value| serde_json::json!({"total_count": 1, "check_runs": [run]});
        let state = |page: serde_json::Value, combined: Option<serde_json::Value>| {
            ci_state(HEAD, now, |_, path| {
                if path.contains("/status?") {
                    combined.clone()
                } else {
                    Some(page.clone())
                }
            })
        };
        let queued = one(serde_json::json!({"status": "queued", "started_at": null}));
        assert_eq!(state(queued, None), CiState::Stuck);
        let stale =
            one(serde_json::json!({"status": "in_progress", "started_at": "2026-10-13T20:00:00Z"}));
        assert_eq!(state(stale, None), CiState::Stuck);
        let young =
            one(serde_json::json!({"status": "in_progress", "started_at": "2026-10-14T05:30:00Z"}));
        assert_eq!(state(young, None), CiState::Young);
        assert_eq!(state(runs(1, 1, None), None), CiState::Settled);
        // A stale pending legacy status is stuck too.
        let stale_status = status(serde_json::json!([
            {"state": "pending", "context": "b", "created_at": "2026-10-13T20:00:00Z"},
        ]));
        assert_eq!(state(runs(1, 1, None), Some(stale_status)), CiState::Stuck);
        for skipped in [false, true] {
            assert!(CiState::Young.exempts(skipped));
            assert!(!CiState::Stuck.exempts(skipped));
            assert_eq!(CiState::Settled.exempts(skipped), skipped);
        }
    }

    #[test]
    fn a_pending_legacy_status_exempts_the_pass() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
        let done = [runs(1, 1, None)];
        let young = status(serde_json::json!([
            {"state": "success", "context": "a", "created_at": "2026-10-14T05:00:00Z"},
            {"state": "pending", "context": "b", "created_at": "2026-10-14T05:45:00Z"},
        ]));
        assert!(read_ci(&done, Some(young), now).0);
        // A pending status stuck past the window, or none pending at all,
        // does not exempt; nor does the bare combined `pending` GitHub
        // reports for a commit with no statuses.
        let stale = status(serde_json::json!([
            {"state": "pending", "context": "b", "created_at": "2026-10-13T20:00:00Z"},
        ]));
        assert!(!read_ci(&done, Some(stale), now).0);
        assert!(!read_ci(&done, Some(status(serde_json::json!([]))), now).0);
        // Unreadable check runs still consult the statuses.
        let young = status(serde_json::json!([
            {"state": "pending", "context": "b", "created_at": "2026-10-14T05:45:00Z"},
        ]));
        assert!(read_ci(&[], Some(young), now).0);
        assert!(!read_ci(&[], None, now).0);
    }
}
