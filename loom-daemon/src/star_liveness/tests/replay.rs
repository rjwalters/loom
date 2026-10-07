//! The replay acceptance check against 2026-09-28 (#9244): with every
//! then-`loom:urgent` issue starred, the first pass yields exactly one
//! operator escalation for each (the pools-exhausted one after its grace
//! window), and #9268 inherits #8191's star.
//!
//! PR #9276's comments are its real ones (see the fixture's `_comment`).
//! Nothing on the PR names #9268; the refusal comment names only #9115, a
//! merged PR. #9268 is found because it is an open issue quoting the
//! refusal's failure signature, and when it is not open (it was closed at
//! 08:14Z) the correct result is a `needs-operator` ask carrying the 405 text.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::fake::{bot, issue, repo_input, t, Host, World};
use crate::forge_listing::RestIssue;
use crate::star_liveness::forge::ForgeComment;
use crate::star_liveness::inherit;
use crate::star_liveness::task::RepoInput;
use crate::types::{AskKind, LandingStage, StarLivenessReport};
use crate::work_finder::WorkItem;

const FIXTURE: &str = include_str!("fixtures/replay_2026_09_28.json");
const SLUGS: [&str; 3] = ["rjwalters/loom", "2AMLogic/loom-ui", "2AMLogic/2am"];

fn ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

/// The fixture world. `all_comments` also loads the comments posted after
/// `snapshot_at`.
fn load_with(all_comments: bool) -> (World, Vec<RepoInput>, Value) {
    let fx: Value = serde_json::from_str(FIXTURE).unwrap();
    let snapshot = ts(fx["snapshot_at"].as_str().unwrap());
    let world = World::default();
    let mut repos = Vec::new();
    for repo in fx["repos"].as_array().unwrap() {
        let slug = repo["slug"].as_str().unwrap();
        for it in repo["items"].as_array().unwrap() {
            let number = u32::try_from(it["number"].as_u64().unwrap()).unwrap();
            world.add(
                slug,
                RestIssue {
                    comments: 0,
                    number,
                    title: it["title"].as_str().map(str::to_string),
                    labels: it["labels"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|l| l.as_str().unwrap().to_string())
                        .collect(),
                    created_at: Some("2026-09-20T00:00:00Z".into()),
                    updated_at: Some("2026-09-28T01:00:00Z".into()),
                    closed_at: None,
                    state: it["state"].as_str().unwrap_or("open").into(),
                    body: it["body"].as_str().map(str::to_string),
                    author: it["author"].as_str().map(str::to_string),
                    author_association: None,
                    is_pull_request: it["pr"].as_bool().unwrap_or(false),
                },
            );
            if let Some(a) = it["association"].as_str() {
                world.repo(slug).associations.insert(number, a.to_string());
            }
        }
        for (n, list) in repo["comments"].as_object().unwrap() {
            for c in list.as_array().unwrap() {
                let at = c["created_at"].as_str().unwrap();
                if !all_comments && ts(at) > snapshot {
                    continue;
                }
                world.comment_full(
                    slug,
                    n.parse().unwrap(),
                    ForgeComment {
                        body: c["body"].as_str().unwrap().to_string(),
                        created_at: Some(at.to_string()),
                        updated_at: Some(at.to_string()),
                        author: c["author"].as_str().map(str::to_string),
                        author_association: c["association"].as_str().map(str::to_string),
                    },
                );
            }
        }
        let mut input = repo_input(&format!("replay-{slug}"));
        input.slug = slug.to_string();
        if repo["pool_exhausted"].as_bool().unwrap() {
            input.pool = Some(
                "all 4 token(s) exhausted since 2026-09-28 06:10Z, next possible clear ~06:25Z"
                    .into(),
            );
        }
        repos.push(input);
    }
    (world, repos, fx)
}

fn total_posted(world: &World) -> usize {
    SLUGS.iter().map(|s| world.posted(s).len()).sum()
}

fn row<'a>(
    report: &'a StarLivenessReport,
    repo: &str,
    issue: u32,
) -> &'a crate::types::StarLandingRow {
    report
        .rows
        .iter()
        .find(|r| r.repo == repo && r.issue == issue)
        .unwrap_or_else(|| panic!("{repo}#{issue} has no landing row"))
}

#[test]
fn replay_2026_09_28_one_escalation_each_within_one_tick() {
    let (world, repos, fx) = load_with(false);
    let mut host = Host::new("host-75acf4b9");
    let first = host.pass(&world, &repos, Vec::new(), t(8, 0));
    // The pools-exhausted ask waits out its grace window (a peer host with
    // capacity may claim the issue first); the second pass is past it.
    let second = host.pass(&world, &repos, Vec::new(), t(8, 10));

    let expected = fx["expect_escalations"].as_array().unwrap();
    for e in expected {
        let repo = e["repo"].as_str().unwrap();
        let issue = u32::try_from(e["issue"].as_u64().unwrap()).unwrap();
        let after_grace = e["after_grace"].as_bool().unwrap_or(false);
        if after_grace {
            let early = row(&first, repo, issue);
            assert_eq!(early.stage, LandingStage::NoCapacity, "{repo}#{issue} in grace");
        }
        let report = if after_grace { &second } else { &first };
        let r = row(report, repo, issue);
        assert_eq!(r.stage, LandingStage::NeedsOperator, "{repo}#{issue}");
        let ask = r.ask.as_ref().unwrap();
        assert_eq!(ask.kind.as_str(), e["kind"].as_str().unwrap(), "{repo}#{issue}");
        for m in e["mentions"].as_array().unwrap() {
            assert!(ask.text.contains(m.as_str().unwrap()), "{repo}#{issue}: {}", ask.text);
        }
        let on_issue = world
            .posted(repo)
            .into_iter()
            .filter(|(n, _)| *n == issue)
            .count();
        assert_eq!(on_issue, 1, "{repo}#{issue}: exactly one escalation");
    }
    assert_eq!(total_posted(&world), expected.len(), "no escalation beyond the expected ones");
    assert_eq!(first.escalations_posted + second.escalations_posted, expected.len());

    for inh in fx["expect_inherited"].as_array().unwrap() {
        let repo = inh["repo"].as_str().unwrap();
        let issue = u32::try_from(inh["issue"].as_u64().unwrap()).unwrap();
        let from = u32::try_from(inh["from"].as_u64().unwrap()).unwrap();
        let r = row(&first, repo, issue);
        assert_eq!(r.inherited_from, Some(from));
        assert!(r.ask.is_none(), "the incident itself is agent work (curation)");
        // …and the work finder sees it as starred, from Curator.
        let root = &repos.iter().find(|r| r.slug == repo).unwrap().root;
        let mut items: Vec<WorkItem> = Vec::new();
        inherit::apply(Some(root), &mut items);
        let item = items.iter().find(|i| i.number == issue).unwrap();
        assert!(item.is_operator_priority());
        assert_eq!(item.operator_priority_inherited_from, Some(from));
    }

    // The next tick adds nothing.
    let again = host.pass(&world, &repos, Vec::new(), t(8, 12));
    assert_eq!(again.escalations_posted, 0);
    assert_eq!(total_posted(&world), expected.len());
}

#[test]
fn without_an_open_incident_the_refusal_escalates_with_the_raw_405_text() {
    // #9268 as it was from 08:14Z: closed. It must not inherit, and nothing
    // else may take its place; the ask carries the forge's words instead.
    let (world, repos, _) = load_with(false);
    world
        .repo("rjwalters/loom")
        .items
        .get_mut(&9268)
        .unwrap()
        .state = "closed".into();
    let report = Host::new("host-a").pass(&world, &repos, Vec::new(), t(8, 20));
    let r = row(&report, "rjwalters/loom", 8191);
    let ask = r.ask.as_ref().unwrap();
    assert_eq!(ask.kind, AskKind::MergeRefused);
    assert!(ask.text.contains("No open incident issue tracks it"), "{}", ask.text);
    assert!(
        ask.text
            .contains("Merge commits are not allowed on this repository"),
        "{}",
        ask.text
    );
    assert!(ask.text.contains(r#""status":"405""#), "{}", ask.text);
    assert!(
        report.rows.iter().all(|r| r.inherited_from.is_none()),
        "no inheritance without an open incident"
    );
    // #9115, the only issue number the refusal names, is a merged PR.
    assert!(report.rows.iter().all(|r| r.issue != 9115));
}

#[test]
fn a_later_bot_comment_naming_an_open_issue_does_not_give_it_the_star() {
    // Every real comment, including the later re-date and verdict-stale
    // notices (#8508, #8248, #5686), with those issues open, plus a later bot
    // note naming an unrelated open issue.
    let (world, repos, _) = load_with(true);
    let slug = "rjwalters/loom";
    for n in [5686, 8508, 8248, 7777] {
        world.add(slug, issue(n, &["loom:issue"]));
    }
    world.comment_full(slug, 9276, bot("Retrying the merge after #7777 lands."));
    let report = Host::new("host-a").pass(&world, &repos, Vec::new(), t(8, 30));
    let inherited: Vec<u32> = report
        .rows
        .iter()
        .filter(|r| r.inherited_from.is_some())
        .map(|r| r.issue)
        .collect();
    assert_eq!(inherited, vec![9268], "only the signature-matched incident");
    let mut items: Vec<WorkItem> = [5686, 8508, 8248, 7777]
        .iter()
        .map(|n| WorkItem::new(*n, vec!["loom:issue".into()]))
        .collect();
    let root = &repos.iter().find(|r| r.slug == slug).unwrap().root;
    inherit::apply(Some(root), &mut items);
    assert!(items
        .iter()
        .filter(|i| i.number != 9268)
        .all(|i| !i.is_operator_priority()));
}
