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
        blockers_closed: HashSet::from([6]),
        red_fix: HashSet::new(),
    };
    let items = classify_rows(&rows, &facts);
    assert_eq!(items[0].verdict, Verdict::Step(Step::CuratorKeepOrClose));
    assert_eq!(items[1].verdict, Verdict::Step(Step::ChampionReevaluate));
    // Unread facts pass the same items.
    let items = classify_rows(&rows, &IssueFacts::default());
    assert_eq!(items[0].verdict, Verdict::Step(Step::Builder));
    assert!(matches!(items[1].verdict, Verdict::Pass(_)));
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
