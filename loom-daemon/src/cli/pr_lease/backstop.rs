//! The PR-lease half of the Judge no-verdict backstop (#11855): `release`
//! records a Judge pass that ended without a verdict, and `claim` refuses a
//! Judge a PR those records hold. The rules live in
//! [`loom_daemon::judge_backstop`].

use chrono::{DateTime, Utc};
use loom_daemon::forge_call_stats::ops;
use loom_daemon::gh_invocation::AccessIntent;
use loom_daemon::judge_backstop as backstop;

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
}

/// The fields a Judge release appends to its marker: the no-verdict record
/// when the PR is still waiting for review with no verdict, else nothing (a
/// verdict landed, the PR closed, or its state could not be read).
pub(crate) fn release_fields(role: Role, state: Option<(bool, String, Vec<String>)>) -> String {
    match state {
        Some((open, head, labels))
            if role == Role::Judge && backstop::ended_without_verdict(open, &labels) =>
        {
            backstop::released_fields(&head, &labels)
        }
        _ => String::new(),
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
        let stamped = release_fields(Role::Judge, state(&waiting));
        assert_eq!(stamped, format!(" head={HEAD} verdict=none labels=review-requested"));
        assert_eq!(release_fields(Role::Doctor, state(&waiting)), "");
        assert_eq!(release_fields(Role::Judge, state(&["loom:pr"])), "");
        assert_eq!(release_fields(Role::Judge, None), "");
    }

    #[test]
    fn stamped_releases_hold_the_pr_and_plain_ones_do_not() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
        let fields = release_fields(Role::Judge, state(&["loom:review-requested"]));
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
}
