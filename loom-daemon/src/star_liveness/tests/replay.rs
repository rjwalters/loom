//! The replay acceptance check against 2026-09-28 (#9244): with every
//! then-`loom:urgent` issue starred, one pass yields exactly one operator
//! escalation for each, and #9268 inherits #8191's star.

use serde_json::Value;

use super::fake::{repo_input, t, Host, World};
use crate::forge_listing::RestIssue;
use crate::star_liveness::inherit;
use crate::star_liveness::task::RepoInput;
use crate::types::LandingStage;
use crate::work_finder::WorkItem;

const FIXTURE: &str = include_str!("fixtures/replay_2026_09_28.json");

fn load() -> (World, Vec<RepoInput>, Value) {
    let fx: Value = serde_json::from_str(FIXTURE).unwrap();
    let world = World::default();
    let mut repos = Vec::new();
    for repo in fx["repos"].as_array().unwrap() {
        let slug = repo["slug"].as_str().unwrap();
        for it in repo["items"].as_array().unwrap() {
            let number = u32::try_from(it["number"].as_u64().unwrap()).unwrap();
            world.add(
                slug,
                RestIssue {
                    number,
                    title: None,
                    labels: it["labels"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|l| l.as_str().unwrap().to_string())
                        .collect(),
                    created_at: Some("2026-09-20T00:00:00Z".into()),
                    updated_at: Some("2026-09-28T01:00:00Z".into()),
                    closed_at: None,
                    state: "open".into(),
                    body: it["body"].as_str().map(str::to_string),
                    author: None,
                    is_pull_request: it["pr"].as_bool().unwrap_or(false),
                },
            );
        }
        for (n, bodies) in repo["comments"].as_object().unwrap() {
            for b in bodies.as_array().unwrap() {
                world.comment(slug, n.parse().unwrap(), b.as_str().unwrap());
            }
        }
        let mut input = repo_input(&format!("replay-{slug}"));
        input.slug = slug.to_string();
        if repo["pool_exhausted"].as_bool().unwrap() {
            input.pool = Some((
                "all 4 token(s) exhausted since 2026-09-28 06:10Z, next possible clear ~06:25Z"
                    .into(),
                "2026-09-28T06:10".into(),
            ));
        }
        repos.push(input);
    }
    (world, repos, fx)
}

#[test]
fn replay_2026_09_28_one_escalation_each_within_one_tick() {
    let (world, repos, fx) = load();
    let mut host = Host::new("host-75acf4b9");
    let report = host.pass(&world, &repos, Vec::new(), t(8, 0));

    let expected = fx["expect_escalations"].as_array().unwrap();
    for e in expected {
        let repo = e["repo"].as_str().unwrap();
        let issue = u32::try_from(e["issue"].as_u64().unwrap()).unwrap();
        let row = report
            .rows
            .iter()
            .find(|r| r.repo == repo && r.issue == issue)
            .unwrap_or_else(|| panic!("{repo}#{issue} has no landing row"));
        assert_eq!(row.stage, LandingStage::NeedsOperator, "{repo}#{issue}");
        let ask = row.ask.as_ref().unwrap();
        assert_eq!(ask.kind.as_str(), e["kind"].as_str().unwrap(), "{repo}#{issue}");
        for m in e["mentions"].as_array().unwrap() {
            assert!(ask.text.contains(m.as_str().unwrap()), "{repo}#{issue}: {}", ask.text);
        }
        let on_issue: Vec<_> = world
            .posted(repo)
            .into_iter()
            .filter(|(n, _)| *n == issue)
            .collect();
        assert_eq!(on_issue.len(), 1, "{repo}#{issue}: exactly one escalation");
    }
    let total: usize = ["rjwalters/loom", "2AMLogic/loom-ui", "2AMLogic/2am"]
        .iter()
        .map(|s| world.posted(s).len())
        .sum();
    assert_eq!(total, expected.len(), "no escalation beyond the expected ones");
    assert_eq!(report.escalations_posted, expected.len());

    for inh in fx["expect_inherited"].as_array().unwrap() {
        let repo = inh["repo"].as_str().unwrap();
        let issue = u32::try_from(inh["issue"].as_u64().unwrap()).unwrap();
        let from = u32::try_from(inh["from"].as_u64().unwrap()).unwrap();
        let row = report
            .rows
            .iter()
            .find(|r| r.repo == repo && r.issue == issue)
            .unwrap();
        assert_eq!(row.inherited_from, Some(from));
        assert!(row.ask.is_none(), "the incident itself is agent work (curation)");
        // …and the work finder sees it as starred, from Curator.
        let root = &repos.iter().find(|r| r.slug == repo).unwrap().root;
        let mut items: Vec<WorkItem> = Vec::new();
        inherit::apply(Some(root), &mut items);
        let item = items.iter().find(|i| i.number == issue).unwrap();
        assert!(item.is_operator_priority());
        assert_eq!(item.operator_priority_inherited_from, Some(from));
    }

    // The next tick adds nothing.
    let again = host.pass(&world, &repos, Vec::new(), t(8, 2));
    assert_eq!(again.escalations_posted, 0);
    let total_after: usize = ["rjwalters/loom", "2AMLogic/loom-ui", "2AMLogic/2am"]
        .iter()
        .map(|s| world.posted(s).len())
        .sum();
    assert_eq!(total_after, expected.len());
}
