//! The pure half of the gather: rows and facts to the classifier's inputs and
//! the draw's.

use super::*;
use crate::work_finder::step_item::{Step, Verdict};

pub(crate) fn row(number: u32, labels: &[&str], body: &str, pr: bool) -> RestIssue {
    RestIssue {
        number,
        title: Some(format!("item {number}")),
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: Some("2026-10-01T00:00:00Z".to_string()),
        updated_at: Some("2026-10-02T00:00:00Z".to_string()),
        closed_at: None,
        state: "open".to_string(),
        body: Some(body.to_string()),
        author: Some("someone".to_string()),
        author_association: Some("MEMBER".to_string()),
        is_pull_request: pr,
        comments: 0,
        open_sub_issues: None,
        labels_at: None,
        draft: false,
    }
}

fn classify_rows(rows: &[RestIssue], facts: &IssueFacts) -> Vec<Item> {
    let pulls = rows
        .iter()
        .filter(|r| r.is_pull_request)
        .map(|r| pull_from_row(r, PullMarkers::default()))
        .collect();
    let inputs = build_inputs(rows, pulls, facts, RepoInput::default(), HostInput::default());
    step_item::classify(&inputs)
}

#[test]
fn rows_join_and_classify_through_the_builder() {
    let rows = vec![
        row(1, &["loom:triage"], "", false),
        row(2, &["loom:curated"], "", false),
        row(3, &["loom:issue"], "", false),
        row(4, &["loom:issue"], "", false),
        row(40, &["loom:review-requested"], "Closes #4", true),
    ];
    let items = classify_rows(&rows, &IssueFacts::default());
    let steps: Vec<(Option<u32>, Option<u32>, Verdict)> =
        items.iter().map(|i| (i.issue, i.pr, i.verdict)).collect();
    assert_eq!(
        steps,
        vec![
            (Some(1), None, Verdict::Step(Step::Curator)),
            (Some(2), None, Verdict::Step(Step::ChampionPromote)),
            (Some(3), None, Verdict::Step(Step::Builder)),
            (Some(4), Some(40), Verdict::Step(Step::Judge)),
        ]
    );
}

#[test]
fn facts_reach_the_classifier() {
    let park = "<!-- loom:park Blocked by: #9 by=champion at=2026-10-01T00:00:00Z -->";
    let rows = vec![
        row(5, &["loom:issue"], "", false),
        row(6, &["loom:curated", "loom:blocked"], park, false),
    ];
    let facts = IssueFacts {
        landed: HashMap::from([(5, 50)]),
        red_fix: HashSet::new(),
    };
    let items = classify_rows(&rows, &facts);
    assert_eq!(items[0].verdict, Verdict::Step(Step::CuratorKeepOrClose));
    assert_eq!(items[1].verdict, Verdict::Step(Step::ChampionReevaluate));
    // Without the landed fact the issue is a plain Builder step. The
    // dependency park needs no fact: Champion re-checks its blockers on every
    // draw (#12078).
    let items = classify_rows(&rows, &IssueFacts::default());
    assert_eq!(items[0].verdict, Verdict::Step(Step::Builder));
    assert_eq!(items[1].verdict, Verdict::Step(Step::ChampionReevaluate));
}

#[test]
fn draw_inputs_read_the_target_row_and_its_stage_label() {
    let rows = vec![
        row(4, &["loom:issue", "loom:important"], "", false),
        row(40, &["loom:changes-requested"], "Closes #4", true),
        row(7, &["loom:epic"], "", false),
    ];
    let items = classify_rows(&rows, &IssueFacts::default());
    let mut asked = Vec::new();
    let inputs = draw_inputs(items, &rows, |r, label| {
        asked.push((r.number, label));
        None
    });
    // The Doctor step's age is its PR's `loom:changes-requested`; the epic
    // passes and is not asked about.
    assert_eq!(asked, vec![(40, "loom:changes-requested")]);
    assert_eq!(inputs[0].labels, vec!["loom:issue", "loom:important"]);
    assert!(inputs[0].created_at.is_some());
}

#[test]
fn the_ladder_and_the_red_fix_marker_are_read_from_bodies() {
    let ladder = "<!-- loom:reeval step=champion_merge rung=1 next=2026-10-20T00:00:00Z -->";
    let rows = vec![row(70, &["loom:pr"], ladder, true)];
    let due = ladder_due(&rows);
    assert_eq!(due.len(), 1);
    assert_eq!((due[0].0, due[0].1), (70, StepRole::ChampionMerge));
    assert!(has_red_fix_marker(Some("text\n<!-- loom:main-red-fix -->\n")));
    assert!(!has_red_fix_marker(Some("quoting <!-- loom:main-red-fix --> inline")));
}

// ------------------------------------------------- the Judge CI hold (#12042)

mod ci_hold {
    use super::*;
    use chrono::TimeZone;
    use serde_json::{json, Value};

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
    }

    /// One check run in `status`, started `hours_ago`.
    fn checks(status: &str, hours_ago: i64) -> Value {
        let started = (now() - chrono::Duration::hours(hours_ago)).to_rfc3339();
        json!({"total_count": 1, "check_runs": [{"status": status, "started_at": started}]})
    }

    /// `pr_planning::ci_pending` (the #12003 rule) over a fixed forge.
    fn forge(page: Value) -> impl FnMut(&str) -> bool {
        move |sha: &str| {
            crate::pr_planning::ci_pending(sha, now(), |_, path| {
                Some(if path.contains("/status?") {
                    json!({"state": "success", "statuses": []})
                } else {
                    page.clone()
                })
            })
        }
    }

    /// A review-requested PR (a Judge step) and a curated issue.
    fn judge_and_issue() -> Vec<Item> {
        let rows = vec![
            row(5, &["loom:review-requested"], "", true),
            row(2, &["loom:curated"], "", false),
        ];
        let items = classify_rows(&rows, &IssueFacts::default());
        assert!(items.iter().any(|i| i.step() == Some(Step::Judge)), "{items:?}");
        items
    }

    fn judge(items: &[Item]) -> Verdict {
        items.iter().find(|i| i.pr == Some(5)).unwrap().verdict
    }

    fn heads() -> HashMap<u32, String> {
        HashMap::from([(5, HEAD.to_string())])
    }

    #[test]
    fn a_judge_step_with_ci_running_is_not_drawn() {
        let mut items = judge_and_issue();
        let held =
            hold_ci_pending(&mut items, &heads(), 4, |_| None, forge(checks("in_progress", 1)));
        assert_eq!(held, 1);
        assert_eq!(judge(&items), Verdict::Pass(PassReason::CiPending));
        assert_eq!(PassReason::CiPending.as_str(), "ci_pending");
        assert!(
            items
                .iter()
                .any(|i| i.issue == Some(2) && i.step().is_some()),
            "only the Judge step is held"
        );
    }

    #[test]
    fn it_is_drawn_once_ci_concludes() {
        let mut items = judge_and_issue();
        let held =
            hold_ci_pending(&mut items, &heads(), 4, |_| None, forge(checks("completed", 1)));
        assert_eq!(held, 0);
        assert_eq!(judge(&items), Verdict::Step(Step::Judge));
    }

    #[test]
    fn ci_pending_past_the_six_hour_cap_is_drawn() {
        let mut items = judge_and_issue();
        let held = hold_ci_pending(&mut items, &heads(), 4, |_| None, forge(checks("queued", 7)));
        assert_eq!(held, 0, "a run stuck past 6 h no longer holds the PR");
        assert_eq!(judge(&items), Verdict::Step(Step::Judge));
    }

    #[test]
    fn a_cached_answer_is_used_and_reads_are_budgeted() {
        let mut items = judge_and_issue();
        let held = hold_ci_pending(&mut items, &heads(), 4, |_| Some(true), |_| panic!("no read"));
        assert_eq!(held, 1, "a cached pending answer holds without a read");

        let mut items = judge_and_issue();
        let held = hold_ci_pending(&mut items, &heads(), 0, |_| None, |_| panic!("no read"));
        assert_eq!(held, 0, "past the budget the step is drawn unread");
        assert_eq!(judge(&items), Verdict::Step(Step::Judge));

        let mut items = judge_and_issue();
        let held = hold_ci_pending(&mut items, &HashMap::new(), 4, |_| None, |_| panic!("no read"));
        assert_eq!(held, 0, "a PR with no known head is drawn unread");
    }
}

/// #12074: a failed open-PR listing keeps the last good PR set but never
/// vouches for a Builder step (the flag stays false); with nothing cached it
/// is empty, as before.
#[test]
fn a_failed_pull_listing_keeps_the_last_good_set_unvouched() {
    let slug = format!("acme/pulls-last-good-{}", std::process::id());
    let (none, ok) = pulls_or_last_good(&slug, Err(anyhow::anyhow!("shed")));
    assert!(none.is_empty() && !ok);
    let pull = RestPull {
        number: 7,
        ..RestPull::default()
    };
    let (fresh, ok) = pulls_or_last_good(&slug, Ok(vec![pull.clone()]));
    assert_eq!((fresh.len(), ok), (1, true));
    let (kept, ok) = pulls_or_last_good(&slug, Err(anyhow::anyhow!("shed")));
    assert_eq!(kept, vec![pull]);
    assert!(!ok, "a last good PR listing never vouches for a Builder step");
}
