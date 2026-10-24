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
    let next = DateTime::parse_from_rfc3339("2026-10-20T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let now = next - chrono::Duration::minutes(30);
    let due = ladder_due(&rows, now);
    assert_eq!(due.len(), 1);
    assert_eq!((due[0].0, due[0].1, due[0].2), (70, StepRole::ChampionMerge, next));
    // #12513: four days out is past rung 1's 1 h, a pre-#12513 marker: due now.
    let early = next - chrono::Duration::days(4);
    assert_eq!(ladder_due(&rows, early)[0].2, early);
    assert!(has_red_fix_marker(Some("text\n<!-- loom:main-red-fix -->\n")));
    assert!(!has_red_fix_marker(Some("quoting <!-- loom:main-red-fix --> inline")));
}

/// #12918: the rung and the raw `next` of the marker, for a PR's merge ladder
/// and an issue's promote ladder; a row with no marker has no entry.
#[test]
fn ladder_state_reads_rung_and_next_for_both_artifacts() {
    let pr_next = "2026-10-20T00:00:00Z";
    let issue_next = "2026-10-20T06:00:00Z";
    let rows = vec![
        row(
            70,
            &["loom:pr"],
            &format!("<!-- loom:reeval step=champion_merge rung=2 next={pr_next} -->"),
            true,
        ),
        row(
            71,
            &["loom:curated"],
            &format!("body\n<!-- loom:reeval step=champion_promote rung=3 next={issue_next} -->\n"),
            false,
        ),
        row(72, &["loom:pr"], "", true),
        // A promote marker on a PR is not that PR's ladder.
        row(
            73,
            &["loom:pr"],
            &format!("<!-- loom:reeval step=champion_promote rung=1 next={pr_next} -->"),
            true,
        ),
    ];
    let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
    let state = ladder_state(&rows, t(pr_next) - chrono::Duration::minutes(30));
    assert_eq!((state[&70].rung, state[&70].next), (2, t(pr_next)));
    assert_eq!((state[&71].rung, state[&71].next), (3, t(issue_next)));
    assert!(!state.contains_key(&72));
    assert!(!state.contains_key(&73));
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

    fn extras(
        state: Option<&str>,
        hours_ago: i64,
    ) -> std::collections::BTreeMap<u32, crate::discovery::ItemExtras> {
        let checks = state.map(|s| crate::discovery::Checks {
            state: s.to_string(),
            started_at: Some((now() - chrono::Duration::hours(hours_ago)).to_rfc3339()),
        });
        std::collections::BTreeMap::from([(
            5,
            crate::discovery::ItemExtras {
                checks,
                ..Default::default()
            },
        )])
    }

    /// Run the gather's wiring: the snapshot's verdict first, a counted read
    /// only for a head with no summary (#12317).
    fn with_snapshot(
        items: &mut [Item],
        summaries: &std::collections::BTreeMap<u32, crate::discovery::ItemExtras>,
    ) -> (usize, usize) {
        let verdicts = summary_verdicts(&heads(), summaries, now());
        let reads = std::cell::Cell::new(0usize);
        let held = hold_ci_pending(
            items,
            &heads(),
            4,
            |sha| verdicts.get(sha).copied(),
            |_| {
                reads.set(reads.get() + 1);
                false
            },
        );
        (held, reads.get())
    }

    #[test]
    fn a_pending_summary_holds_the_judge_step_without_a_read() {
        let mut items = judge_and_issue();
        let (held, reads) = with_snapshot(&mut items, &extras(Some("pending"), 1));
        assert_eq!((held, reads), (1, 0));
        assert_eq!(judge(&items), Verdict::Pass(PassReason::CiPending));
    }

    #[test]
    fn a_success_or_failure_summary_admits_the_judge_step_without_a_read() {
        for state in ["success", "failure"] {
            let mut items = judge_and_issue();
            let (held, reads) = with_snapshot(&mut items, &extras(Some(state), 1));
            assert_eq!((held, reads), (0, 0), "{state}");
            assert_eq!(judge(&items), Verdict::Step(Step::Judge));
        }
    }

    #[test]
    fn a_pending_summary_older_than_six_hours_is_drawn() {
        let mut items = judge_and_issue();
        let (held, reads) = with_snapshot(&mut items, &extras(Some("pending"), 7));
        assert_eq!((held, reads), (0, 0));
        assert_eq!(judge(&items), Verdict::Step(Step::Judge));
    }

    #[test]
    fn a_missing_summary_falls_back_to_one_counted_read() {
        let mut items = judge_and_issue();
        let (held, reads) = with_snapshot(&mut items, &extras(None, 0));
        assert_eq!((held, reads), (0, 1));
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

mod held_at_head {
    use super::*;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    /// PR 5, held (`loom:pr` + `loom:blocked`, a standing hold), so it
    /// classifies as a `ChampionHoldReevaluate` step.
    fn held_pr() -> Vec<Item> {
        let rows = vec![row(5, &["loom:pr", "loom:blocked"], "", true)];
        let markers = PullMarkers {
            champion_hold: true,
            ..PullMarkers::default()
        };
        let pulls = vec![pull_from_row(&rows[0], markers)];
        let inputs = build_inputs(
            &rows,
            pulls,
            &IssueFacts::default(),
            RepoInput::default(),
            HostInput::default(),
        );
        let items = step_item::classify(&inputs);
        assert_eq!(items[0].step(), Some(Step::ChampionHoldReevaluate), "{items:?}");
        items
    }

    fn map(v: &str) -> HashMap<u32, String> {
        HashMap::from([(5, v.to_string())])
    }

    fn run(
        heads: &HashMap<u32, String>,
        hold: &HashMap<u32, String>,
        released: &[u32],
    ) -> (usize, Verdict) {
        let mut items = held_pr();
        let n = hold_at_recorded_head(
            &mut items,
            heads,
            hold,
            &HashMap::new(),
            None,
            &released.iter().copied().collect(),
        );
        (n, items[0].verdict)
    }

    /// At its held head, with `recorded` the digest the hold wrote (`None`
    /// for a legacy hold) and `current` the repo's digest now.
    fn run_auth(recorded: Option<&str>, current: Option<&str>) -> usize {
        let mut items = held_pr();
        let auths: HashMap<u32, String> =
            recorded.map(|a| (5, a.to_string())).into_iter().collect();
        hold_at_recorded_head(&mut items, &map(HEAD), &map(HEAD), &auths, current, &HashSet::new())
    }

    #[test]
    fn an_unchanged_digest_keeps_the_hold() {
        assert_eq!(run_auth(Some("aa11"), Some("aa11")), 1);
    }

    #[test]
    fn a_changed_digest_is_drawn_once() {
        assert_eq!(run_auth(Some("aa11"), Some("bb22")), 0);
    }

    #[test]
    fn a_hold_with_no_recorded_digest_stays_held_at_its_head() {
        // A merge-risk hold never records `auth=` (#13008).
        assert_eq!(run_auth(None, Some("bb22")), 1);
    }

    #[test]
    fn no_current_digest_keeps_the_head_only_rule() {
        assert_eq!(run_auth(Some("aa11"), None), 1);
        assert_eq!(run_auth(None, None), 1);
    }

    #[test]
    fn a_pr_at_its_held_head_is_not_drawn() {
        let (n, v) = run(&map(HEAD), &map(HEAD), &[]);
        assert_eq!((n, v), (1, Verdict::Pass(PassReason::HeldAtHead)));
        assert_eq!(PassReason::HeldAtHead.as_str(), "held_at_head");
    }

    #[test]
    fn an_abbreviated_recorded_sha_still_matches() {
        assert_eq!(run(&map(HEAD), &map(&HEAD[..7]), &[]).0, 1);
    }

    #[test]
    fn a_push_makes_it_drawable() {
        let (n, v) = run(&map("fedcba9876543210"), &map(HEAD), &[]);
        assert_eq!((n, v), (0, Verdict::Step(Step::ChampionHoldReevaluate)));
    }

    #[test]
    fn auto_merge_ok_makes_it_drawable() {
        assert_eq!(run(&map(HEAD), &map(HEAD), &[5]).0, 0);
    }

    #[test]
    fn it_fails_open_without_a_recorded_or_current_head() {
        assert_eq!(run(&map(HEAD), &HashMap::new(), &[]).0, 0);
        assert_eq!(run(&HashMap::new(), &map(HEAD), &[]).0, 0);
    }
}

mod author_gate_held {
    use super::*;
    use crate::promotion_author::{revision, Stamp};

    const TRUST: &str = "0123456789abcdef";

    /// Issue 7, `loom:curated` only: a `ChampionPromote` step.
    fn curated() -> Vec<Item> {
        let rows = vec![row(7, &["loom:curated"], "body", false)];
        let items = classify_rows(&rows, &IssueFacts::default());
        assert_eq!(items[0].step(), Some(Step::ChampionPromote), "{items:?}");
        items
    }

    fn stamp(body: &str, trust: &str) -> Stamp {
        Stamp {
            rev: revision("item 7", body),
            trust: trust.to_string(),
        }
    }

    fn run(stamps: Vec<Stamp>, body: &str, trust: Option<&str>) -> (usize, Verdict) {
        let mut items = curated();
        let gates = HashMap::from([(7, stamps)]);
        let revs = HashMap::from([(7, revision("item 7", body))]);
        let n = hold_author_gated(&mut items, &gates, &revs, trust);
        (n, items[0].verdict)
    }

    #[test]
    fn a_gated_issue_with_unchanged_inputs_is_not_drawn() {
        let (n, v) = run(vec![stamp("body", TRUST)], "body", Some(TRUST));
        assert_eq!((n, v), (1, Verdict::Pass(PassReason::AuthorGateHeld)));
        assert_eq!(PassReason::AuthorGateHeld.as_str(), "author_gate_held");
    }

    #[test]
    fn a_body_edit_after_the_gate_makes_it_drawable() {
        assert_eq!(run(vec![stamp("body", TRUST)], "edited", Some(TRUST)).0, 0);
    }

    #[test]
    fn a_trust_change_makes_it_drawable() {
        assert_eq!(run(vec![stamp("body", TRUST)], "body", Some("fedcba9876543210")).0, 0);
    }

    #[test]
    fn an_older_matching_stamp_still_holds_an_edit_reverted() {
        let stamps = vec![stamp("body", TRUST), stamp("edited", TRUST)];
        assert_eq!(run(stamps, "body", Some(TRUST)).0, 1);
    }

    #[test]
    fn no_stamp_no_digest_or_no_marker_read_fails_open() {
        // A legacy notice parses to no stamp; an unreadable read has no entry.
        assert_eq!(run(vec![], "body", Some(TRUST)).0, 0);
        assert_eq!(run(vec![stamp("body", TRUST)], "body", None).0, 0);
        let mut items = curated();
        let revs = HashMap::from([(7, revision("item 7", "body"))]);
        assert_eq!(hold_author_gated(&mut items, &HashMap::new(), &revs, Some(TRUST)), 0);
        assert_eq!(items[0].step(), Some(Step::ChampionPromote));
    }

    #[test]
    fn only_promote_steps_are_held() {
        let rows = vec![row(7, &["loom:issue"], "body", false)];
        let mut items = classify_rows(&rows, &IssueFacts::default());
        let gates = HashMap::from([(7, vec![stamp("body", TRUST)])]);
        let revs = HashMap::from([(7, revision("item 7", "body"))]);
        assert_eq!(hold_author_gated(&mut items, &gates, &revs, Some(TRUST)), 0);
    }
}
