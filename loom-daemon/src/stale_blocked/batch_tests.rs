//! Tests for the batched `check-stale-blocked` gatherer (#10480), against a
//! counting fake [`StaleBlockedForge`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{anyhow, Result};

use super::batch::{
    closing_refs_query, gather_all, parse_closing_refs, parse_comments, parse_pr_merge_state,
    parse_rate_limit, parse_ref_state, ClosingRef, Gathered, Options, RefState, StaleBlockedForge,
    CLOSING_BATCH,
};
use super::budget::{Budget, Floor, Meter};
use super::{classify, park_self_block, undeclared, Artifact, Verdict};
use crate::dep_recheck::{extract, recheck};
use crate::forge_identity::FleetLogins;
use crate::forge_listing::RestIssue;

type Key = (Option<String>, i64);

#[derive(Default)]
pub(super) struct Fake {
    /// The archived probe's answer (#10562): `Err` when it did not answer.
    pub(super) archived: Option<Result<bool, String>>,
    pub(super) archived_calls: usize,
    pub(super) rows: Vec<RestIssue>,
    pub(super) list_fails: bool,
    pub(super) comments: HashMap<u32, Vec<extract::Comment>>,
    pub(super) states: HashMap<Key, RefState>,
    pub(super) merge: HashMap<u32, (String, String)>,
    pub(super) closing: HashMap<u32, Vec<ClosingRef>>,
    /// Issues the closing query leaves out (a `null` alias / truncated list).
    pub(super) closing_missing: HashSet<u32>,
    pub(super) closing_fails: bool,
    /// The probe's answer, and whether the breaker is open.
    pub(super) budget: Option<Budget>,
    pub(super) breaker: bool,
    /// `rateLimit.remaining` each successive GraphQL batch reports, and the
    /// core `x-ratelimit-remaining` each successive REST read reports.
    pub(super) graphql_remaining: VecDeque<u64>,
    pub(super) core_remaining: VecDeque<u64>,
    pub(super) meter: Meter,
    // Call log.
    pub(super) list_calls: usize,
    pub(super) comment_calls: Vec<u32>,
    pub(super) state_calls: Vec<Key>,
    pub(super) merge_calls: Vec<u32>,
    pub(super) closing_calls: Vec<usize>,
    pub(super) budget_calls: usize,
}

impl StaleBlockedForge for Fake {
    fn archived(&mut self) -> Result<bool, String> {
        self.archived_calls += 1;
        self.meter.rest(true, None);
        self.archived.clone().unwrap_or(Ok(false))
    }

    fn list_blocked(&mut self) -> Result<Vec<RestIssue>> {
        self.list_calls += 1;
        if self.list_fails {
            return Err(anyhow!("HTTP 502"));
        }
        Ok(self.rows.clone())
    }

    fn comments(&mut self, number: u32) -> Result<Vec<extract::Comment>> {
        self.comment_calls.push(number);
        self.meter.rest(false, self.core_remaining.pop_front());
        self.comments
            .get(&number)
            .cloned()
            .ok_or_else(|| anyhow!("no comments fixture for #{number}"))
    }

    fn ref_state(&mut self, repo: Option<&str>, number: i64) -> Result<Option<RefState>> {
        let key = (repo.map(str::to_string), number);
        self.state_calls.push(key.clone());
        self.meter.rest(true, self.core_remaining.pop_front());
        Ok(self.states.get(&key).cloned())
    }

    fn pr_merge_state(&mut self, number: u32) -> Result<(String, String)> {
        self.merge_calls.push(number);
        self.meter.rest(false, self.core_remaining.pop_front());
        self.merge
            .get(&number)
            .cloned()
            .ok_or_else(|| anyhow!("no merge fixture for #{number}"))
    }

    fn closing_refs_batch(&mut self, issues: &[u32]) -> Result<HashMap<u32, Vec<ClosingRef>>> {
        self.closing_calls.push(issues.len());
        self.meter
            .graphql(Some(1), self.graphql_remaining.pop_front());
        if self.closing_fails {
            return Err(anyhow!("GraphQL: something went wrong"));
        }
        Ok(issues
            .iter()
            .filter(|n| !self.closing_missing.contains(n))
            .map(|n| (*n, self.closing.get(n).cloned().unwrap_or_default()))
            .collect())
    }

    fn budget(&mut self) -> Option<Budget> {
        self.budget_calls += 1;
        self.budget
    }

    fn breaker_open(&mut self) -> bool {
        self.breaker
    }

    fn meter(&self) -> Meter {
        self.meter
    }
}

pub(super) fn row(number: u32, body: &str, comments: u32, pr: bool, labels: &[&str]) -> RestIssue {
    RestIssue {
        number,
        title: Some(format!("artifact {number}")),
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: None,
        updated_at: None,
        closed_at: None,
        state: "open".to_string(),
        body: Some(body.to_string()),
        author: None,
        author_association: None,
        is_pull_request: pr,
        comments,
    }
}

pub(super) fn issue(number: u32, body: &str) -> RestIssue {
    row(number, body, 0, false, &["loom:blocked"])
}

pub(super) fn state(s: &str) -> RefState {
    RefState {
        state: s.to_string(),
        labels: Vec::new(),
        is_pr: false,
    }
}

pub(super) fn comment(login: &str, body: &str) -> extract::Comment {
    extract::Comment {
        author: extract::Author {
            login: login.to_string(),
        },
        body: body.to_string(),
    }
}

pub(super) fn fleet() -> FleetLogins {
    FleetLogins::single(extract::DEFAULT_BOT_LOGIN)
}

fn run(fake: &mut Fake) -> (Vec<Gathered>, Option<String>) {
    let g = gather_all(
        fake,
        &fleet(),
        Options {
            limit: 1000,
            no_prs: false,
            floor: Floor::default(),
        },
    );
    (g.items, g.enumerate_error)
}

fn verdict_of(g: &Gathered) -> Verdict {
    classify(g.evidence.as_ref().expect("evaluated"))
}

fn find(out: &[Gathered], n: i64) -> &Gathered {
    out.iter()
        .find(|g| g.number == n)
        .expect("artifact present")
}

// --- call counts --------------------------------------------------------------

#[test]
fn two_hundred_fifty_issues_cost_three_graphql_queries_and_one_read_per_blocker() {
    let mut fake = Fake::default();
    // 250 issues over K = 5 distinct blockers; every 10th has comments.
    for n in 1..=250u32 {
        let blocker = 9000 + i64::from(n % 5);
        let comments = u32::from(n % 10 == 0);
        fake.rows.push(row(
            n,
            &format!("Blocked by #{blocker}"),
            comments,
            false,
            &["loom:blocked"],
        ));
        if comments > 0 {
            fake.comments
                .insert(n, vec![comment("a-human", "still waiting")]);
        }
    }
    for b in 9000..9005 {
        fake.states.insert((None, b), state("OPEN"));
    }
    let (out, err) = run(&mut fake);
    assert!(err.is_none());
    assert_eq!(out.len(), 250);
    assert!(out.iter().all(|g| g.evidence.is_ok()));

    assert_eq!(fake.closing_calls.len(), 250usize.div_ceil(CLOSING_BATCH), "⌈250/100⌉ queries");
    assert_eq!(fake.closing_calls, vec![100, 100, 50]);
    assert_eq!(fake.state_calls.len(), 5, "one state read per distinct blocker");
    assert_eq!(fake.comment_calls.len(), 25, "comments read only where the count is non-zero");
    assert!(fake.merge_calls.is_empty(), "no PR in the population, no PR read");
    let total = fake.closing_calls.len() + fake.state_calls.len() + fake.comment_calls.len() + 1;
    assert!(total <= 250usize.div_ceil(100) * 2 + 5 + 25 + 1, "total forge calls {total}");
}

#[test]
fn ten_issues_citing_one_blocker_read_it_once() {
    let mut fake = Fake::default();
    for n in 1..=10 {
        fake.rows.push(issue(n, "Blocked by #7"));
    }
    fake.states.insert((None, 7), state("CLOSED"));
    let (out, _) = run(&mut fake);
    assert_eq!(fake.state_calls, vec![(None, 7)]);
    assert!(out
        .iter()
        .all(|g| matches!(verdict_of(g), Verdict::Stale(_))));
}

#[test]
fn an_open_closing_pr_is_not_read_individually() {
    let mut fake = Fake::default();
    fake.rows.push(issue(201, "See the linked PR."));
    fake.closing.insert(
        201,
        vec![ClosingRef {
            number: 4744,
            state: "OPEN".to_string(),
        }],
    );
    let (out, _) = run(&mut fake);
    assert!(fake.state_calls.is_empty(), "{:?}", fake.state_calls);
    assert_eq!(verdict_of(&out[0]), Verdict::Undocumented);
}

// --- fail-safe ------------------------------------------------------------------

#[test]
fn a_missing_blocker_leaves_only_its_citers_unevaluated() {
    let mut fake = Fake::default();
    fake.rows.push(issue(1, "Blocked by #404"));
    fake.rows.push(issue(2, "Blocked by #7"));
    fake.states.insert((None, 7), state("CLOSED"));
    let (out, _) = run(&mut fake);
    let why = find(&out, 1).evidence.as_ref().unwrap_err();
    assert!(why.contains("#404"), "{why}");
    assert!(matches!(verdict_of(find(&out, 2)), Verdict::Stale(_)));
}

#[test]
fn a_missing_closing_answer_is_unevaluated_not_no_closing_prs() {
    let mut fake = Fake::default();
    fake.rows.push(issue(1, "nothing cited"));
    fake.rows.push(issue(2, "nothing cited"));
    fake.closing_missing.insert(1);
    let (out, _) = run(&mut fake);
    let why = find(&out, 1).evidence.as_ref().unwrap_err();
    assert!(why.contains("never guess 'no closing PRs'"), "{why}");
    // Its batch-mate's complete answer still counts.
    assert_eq!(verdict_of(find(&out, 2)), Verdict::Undocumented);
}

#[test]
fn a_failed_closing_batch_leaves_the_whole_batch_unevaluated() {
    let mut fake = Fake {
        closing_fails: true,
        ..Fake::default()
    };
    fake.rows.push(issue(1, "nothing cited"));
    fake.rows
        .push(row(2, "nothing cited", 0, true, &["loom:blocked"]));
    let (out, _) = run(&mut fake);
    assert!(find(&out, 1).evidence.is_err(), "never read as Undocumented");
    // A PR has no closing-PR arm, so it is unaffected.
    assert_eq!(verdict_of(find(&out, 2)), Verdict::Undocumented);
}

#[test]
fn a_failed_comment_read_is_unevaluated_and_skips_the_closing_query() {
    let mut fake = Fake::default();
    fake.rows
        .push(row(1, "nothing cited", 3, false, &["loom:blocked"]));
    let (out, _) = run(&mut fake);
    assert!(out[0].evidence.is_err());
    assert!(fake.closing_calls.is_empty());
}

#[test]
fn a_failed_listing_is_an_enumeration_error() {
    let mut fake = Fake {
        list_fails: true,
        ..Fake::default()
    };
    let (out, err) = run(&mut fake);
    assert!(out.is_empty());
    assert!(err.unwrap().contains("HTTP 502"));
}

// --- population ------------------------------------------------------------------

#[test]
fn no_prs_and_limit_apply_per_population() {
    let mut fake = Fake::default();
    for n in 1..=5 {
        fake.rows.push(issue(n, "nothing cited"));
        fake.rows
            .push(row(100 + n, "nothing cited", 0, true, &["loom:blocked"]));
    }
    let opts = Options {
        limit: 2,
        no_prs: false,
        floor: Floor::default(),
    };
    let out = gather_all(&mut fake, &fleet(), opts).items;
    let kinds: Vec<(Artifact, i64)> = out.iter().map(|g| (g.kind, g.number)).collect();
    assert_eq!(
        kinds,
        vec![
            (Artifact::Issue, 1),
            (Artifact::Issue, 2),
            (Artifact::Pr, 101),
            (Artifact::Pr, 102)
        ]
    );
    let opts = Options {
        limit: 100,
        no_prs: true,
        floor: Floor::default(),
    };
    let out = gather_all(&mut fake, &fleet(), opts).items;
    assert!(out.iter().all(|g| g.kind == Artifact::Issue));
    assert_eq!(out.len(), 5);
}

// --- parity with the per-artifact gatherer's fixtures ------------------------------

#[test]
fn verdicts_match_the_existing_fixtures() {
    let mut fake = Fake::default();
    // Stale prose reference (T1a).
    fake.rows
        .push(issue(178, "Blocked by #7 (user authentication)."));
    fake.states.insert((None, 7), state("CLOSED"));
    // Cross-repo named dependency (#8502), all resolved.
    fake.rows.push(issue(
        179,
        "## Dependencies\n\n- [ ] other/repo#176: rating\n- [ ] #177: votes\n",
    ));
    fake.states
        .insert((Some("other/repo".to_string()), 176), state("CLOSED"));
    fake.states.insert((None, 177), state("MERGED"));
    // Merged closing PR (T1j), labels from the REST read.
    fake.rows.push(issue(190, "No prose blocker here."));
    fake.closing.insert(
        190,
        vec![ClosingRef {
            number: 4743,
            state: "MERGED".to_string(),
        }],
    );
    fake.states.insert(
        (None, 4743),
        RefState {
            state: "MERGED".to_string(),
            labels: vec!["loom:blocked".to_string()],
            is_pr: true,
        },
    );
    // Undocumented (T2a), and a bot-only reference (T2f) over REST's `[bot]`.
    fake.rows.push(issue(180, "This needs more thought."));
    fake.rows
        .push(row(183, "Nothing cited.", 1, false, &["loom:blocked"]));
    let bot_page = r#"[{"user":{"login":"loom-fleet-dispatch[bot]","type":"Bot"},"body":"Blocked by #7, per the last pass."}]"#;
    fake.comments.insert(183, parse_comments(bot_page).unwrap());
    // Prose-only park on a live blocker (T3i).
    fake.rows
        .push(issue(202, "Blocked by #9 — noted here, never declared."));
    fake.states.insert((None, 9), state("OPEN"));
    // Parked PRs: operator-held (T6e), conflicting, and landable (T6a).
    let park = "<!-- loom:park Blocked by: #8322 by=doctor at=2026-09-19T12:09:00Z -->";
    fake.states.insert((None, 8322), state("CLOSED"));
    fake.rows
        .push(row(8314, park, 0, true, &["loom:blocked", "loom:operator-only"]));
    fake.rows.push(row(8315, park, 0, true, &["loom:blocked"]));
    fake.merge
        .insert(8315, ("CONFLICTING".to_string(), "DIRTY".to_string()));
    fake.rows
        .push(row(8316, park, 0, true, &["loom:blocked", "loom:changes-requested"]));
    fake.merge
        .insert(8316, ("MERGEABLE".to_string(), "CLEAN".to_string()));

    let (out, err) = run(&mut fake);
    assert!(err.is_none());

    match verdict_of(find(&out, 178)) {
        Verdict::Stale(r) => assert!(r[0].contains("7:CLOSED"), "{r:?}"),
        v => panic!("178: {v:?}"),
    }
    // #9274: resolved refs on UNCHECKED boxes are Unticked, never Stale.
    assert_eq!(
        verdict_of(find(&out, 179)),
        Verdict::Unticked {
            resolved_refs: vec!["other/repo#176".to_string(), "#177".to_string()],
            unparsed: 0
        }
    );
    // #9274: a merged closing PR is not a blocker reference.
    assert_eq!(verdict_of(find(&out, 190)), Verdict::Undocumented);
    assert_eq!(verdict_of(find(&out, 180)), Verdict::Undocumented);
    assert_eq!(
        verdict_of(find(&out, 183)),
        Verdict::Undocumented,
        "a [bot] comment never counts"
    );
    let g202 = find(&out, 202);
    assert_eq!(verdict_of(g202), Verdict::StillBlocked);
    assert!(undeclared(g202.evidence.as_ref().unwrap()));
    match verdict_of(find(&out, 8314)) {
        Verdict::Superseded { block, .. } => assert!(block.contains("loom:operator-only")),
        v => panic!("8314: {v:?}"),
    }
    match verdict_of(find(&out, 8315)) {
        Verdict::Superseded { block, .. } => assert!(block.contains("CONFLICTING"), "{block}"),
        v => panic!("8315: {v:?}"),
    }
    assert!(matches!(verdict_of(find(&out, 8316)), Verdict::Stale(_)));
    // The label-held PR needed no merge-state read; the other two did.
    assert_eq!(fake.merge_calls, vec![8315, 8316]);
    // Exactly one GraphQL batch for the 6 issues; PRs are not in it.
    assert_eq!(fake.closing_calls, vec![6]);
}

#[test]
fn a_still_blocked_pr_is_never_read_for_merge_state() {
    let mut fake = Fake::default();
    let park = "<!-- loom:park Blocked by: #9 by=doctor at=2026-09-19T12:09:00Z -->";
    fake.rows.push(row(50, park, 0, true, &["loom:blocked"]));
    fake.states.insert((None, 9), state("OPEN"));
    let (out, _) = run(&mut fake);
    assert_eq!(verdict_of(&out[0]), Verdict::StillBlocked);
    assert!(fake.merge_calls.is_empty());
}

// --- parsers ------------------------------------------------------------------------

#[test]
fn rest_bot_logins_are_normalised_to_the_graphql_spelling() {
    let page = r#"[
        {"user":{"login":"github-actions[bot]","type":"Bot"},"body":"a"},
        {"user":{"login":"a-human","type":"User"},"body":"b"},
        {"user":null,"body":null}
    ]"#;
    let c = parse_comments(page).unwrap();
    let logins: Vec<&str> = c.iter().map(|c| c.author.login.as_str()).collect();
    assert_eq!(logins, vec!["github-actions", "a-human", ""]);
    assert_eq!(c[2].body, "");
}

#[test]
fn pr_merge_state_maps_rest_to_graphql_spelling() {
    let conflicting = parse_pr_merge_state(r#"{"mergeable":false,"mergeable_state":"dirty"}"#);
    assert_eq!(conflicting.unwrap(), ("CONFLICTING".to_string(), "DIRTY".to_string()));
    let unknown =
        parse_pr_merge_state(r#"{"mergeable":null,"mergeable_state":"unknown"}"#).unwrap();
    assert_eq!(unknown, ("UNKNOWN".to_string(), "UNKNOWN".to_string()));
    let pr = recheck::Pr {
        number: 1,
        state: "OPEN".to_string(),
        mergeable: unknown.0,
        merge_state_status: unknown.1,
        ..recheck::Pr::default()
    };
    assert_eq!(park_self_block(&pr), None, "UNKNOWN is not superseding, as before");
    let clean = parse_pr_merge_state(r#"{"mergeable":true,"mergeable_state":"clean"}"#).unwrap();
    assert_eq!(clean, ("MERGEABLE".to_string(), "CLEAN".to_string()));
}

#[test]
fn ref_state_reads_issues_and_prs_from_one_endpoint() {
    let issue = parse_ref_state(r#"{"state":"closed","labels":[{"name":"x"}]}"#).unwrap();
    assert_eq!(issue.state, "CLOSED");
    assert_eq!(issue.labels, vec!["x".to_string()]);
    let merged = parse_ref_state(
        r#"{"state":"closed","pull_request":{"merged_at":"2026-01-01T00:00:00Z"}}"#,
    )
    .unwrap();
    assert_eq!(merged.state, "MERGED");
    assert!(!issue.is_pr && merged.is_pr);
    let closed_pr =
        parse_ref_state(r#"{"state":"closed","pull_request":{"merged_at":null}}"#).unwrap();
    // #10556: a closed-unmerged PR is told apart from a closed issue.
    assert_eq!(closed_pr.state, "CLOSED_UNMERGED");
    assert!(closed_pr.is_pr);
    let open_pr = parse_ref_state(r#"{"state":"open","pull_request":{}}"#).unwrap();
    assert_eq!(open_pr.state, "OPEN");
}

#[test]
fn closing_query_uses_gh_own_arguments() {
    let q = closing_refs_query("o", "r", &[1, 23]);
    assert!(q.contains("i1: issue(number: 1)"), "{q}");
    assert!(q.contains("i23: issue(number: 23)"), "{q}");
    assert!(q.contains("closedByPullRequestsReferences(first: 100)"), "{q}");
    assert!(!q.contains("includeClosedPrs"), "gh does not pass it: {q}");
    assert!(
        q.starts_with("query { rateLimit { cost remaining } repository(owner: \"o\", name: \"r\")"),
        "{q}"
    );
}

#[test]
fn closing_answer_reports_its_rate_limit_cost() {
    let body = r#"{"data":{"rateLimit":{"cost":2,"remaining":4870},"repository":{}}}"#;
    assert_eq!(parse_rate_limit(body), (Some(2), Some(4870)));
    assert_eq!(parse_rate_limit(r#"{"errors":[{"message":"x"}]}"#), (None, None));
    assert_eq!(parse_rate_limit("not json"), (None, None));
}

#[test]
fn closing_parse_keeps_only_complete_aliases() {
    let body = r#"{"data":{"repository":{
        "i1":{"closedByPullRequestsReferences":{"totalCount":1,"nodes":[{"number":5,"state":"MERGED"}]}},
        "i2":null,
        "i3":{"closedByPullRequestsReferences":{"totalCount":101,"nodes":[{"number":6,"state":"OPEN"}]}},
        "i4":{"closedByPullRequestsReferences":{"totalCount":0,"nodes":[]}}
    }},"errors":[{"message":"Could not resolve to an Issue with the number of 2."}]}"#;
    let map = parse_closing_refs(body, &[1, 2, 3, 4]).unwrap();
    assert_eq!(
        map.get(&1).unwrap(),
        &vec![ClosingRef {
            number: 5,
            state: "MERGED".to_string()
        }]
    );
    assert!(!map.contains_key(&2), "a null alias is not 'no closing PRs'");
    assert!(!map.contains_key(&3), "a truncated list is not complete");
    assert_eq!(map.get(&4).unwrap(), &Vec::<ClosingRef>::new());
    assert!(parse_closing_refs(r#"{"errors":[{"message":"rate limited"}]}"#, &[1]).is_none());
    assert!(parse_closing_refs(r#"{"data":{"repository":null}}"#, &[1]).is_none());
    assert!(parse_closing_refs("not json", &[1]).is_none());
}

/// #9274 (Judge, round 3): a checklist line is judged by the checklist rule
/// alone. `item_re` accepts `- [ ] Blocked by #N`, and the prose extractor reads
/// the same phrase; without masking, the prose rule turned an unticked box into
/// Stale. Gather to classify, end to end.
#[test]
fn checklist_lines_are_never_also_read_as_prose() {
    let mut fake = Fake::default();
    // Supported checklist syntax with a dependency phrase, ref merged.
    fake.rows.push(issue(
        186,
        "## Dependencies\n\n- [ ] Blocked by #187: ratification remains pending\n",
    ));
    fake.states.insert((None, 187), state("MERGED"));
    // An unparseable unchecked line that still contains `Requires #N`.
    fake.rows.push(issue(
        256,
        "## Dependencies\n\n- [ ] vendor sign-off on the pinout. Requires #268 first\n",
    ));
    fake.states.insert((None, 268), state("CLOSED"));
    // An independent prose reference OUTSIDE the checklist is still prose.
    fake.rows.push(issue(
        257,
        "Blocked by #7 (auth).\n\n## Dependencies\n\n- [ ] Blocked by #9: still open\n",
    ));
    fake.states.insert((None, 7), state("CLOSED"));
    fake.states.insert((None, 9), state("OPEN"));

    let (out, err) = run(&mut fake);
    assert!(err.is_none());

    let g186 = find(&out, 186);
    assert!(g186.evidence.as_ref().unwrap().prose.is_empty());
    assert_eq!(
        verdict_of(g186),
        Verdict::Unticked {
            resolved_refs: vec!["#187".to_string()],
            unparsed: 0
        }
    );
    let g256 = find(&out, 256);
    assert!(g256.evidence.as_ref().unwrap().prose.is_empty());
    assert_eq!(
        verdict_of(g256),
        Verdict::Unticked {
            resolved_refs: vec![],
            unparsed: 1
        }
    );
    match verdict_of(find(&out, 257)) {
        Verdict::Stale(r) => {
            assert_eq!(r.len(), 1, "{r:?}");
            assert!(r[0].contains("7:CLOSED") && !r[0].contains("9:OPEN"), "{r:?}");
        }
        v => panic!("257: {v:?}"),
    }
}
