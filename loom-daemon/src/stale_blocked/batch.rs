//! Batched evidence gathering for `check-stale-blocked` (issue #10480) and,
//! through [`gather_filtered`], `notify-cleared-blockers` (#10515).
//!
//! The per-artifact gatherer it replaced (over [`crate::dep_recheck::forge`])
//! spent 3–6 GraphQL `gh … view` calls per artifact and repeated them for
//! every artifact citing the same blocker: about 1,700 points for 300 parked
//! issues. This module reads the same facts in bulk:
//!
//! - **Candidates**: one REST + ETag `loom:blocked` listing, which returns
//!   issues *and* PRs and already carries each body, labels and comment count
//!   ([`crate::forge_listing::list_issues_cached_all_as`]).
//! - **Comments**: one REST + ETag walk per artifact whose count is non-zero.
//! - **Blocker states**: one REST + ETag `issues/{n}` read per distinct
//!   `(repo, number)` for the whole run. That endpoint answers for a PR number
//!   too, so the old `issue view` → `pr view` fallback is gone.
//! - **Closing PRs**: one aliased GraphQL query per [`CLOSING_BATCH`] issues
//!   (REST cannot tell a closing reference from a mention). It is the only
//!   GraphQL left, and it uses `gh`'s own arguments (`first: 100`, no
//!   `includeClosedPrs`) so the PR set matches `gh issue view --json
//!   closedByPullRequestsReferences` exactly.
//! - **A parked PR's own merge state**: REST `pulls/{n}`, read only when the
//!   PR would otherwise be reported stale and no label already supersedes it.
//!
//! Every warm re-run is free on the core bucket (all `304`s), and the GraphQL
//! cost is ⌈issues/100⌉ points.
//!
//! # Budget floor
//!
//! Before any evidence read the run's projected cost is checked against the
//! free budget probe, and re-checked between reads from the forge's own
//! answers; a run that would cross the floor stops and reports the rest as
//! unevaluated. See [`super::budget`]. What was spent is returned as a
//! [`ForgeCost`].
//!
//! # Fail safe, unchanged
//!
//! A read that did not answer leaves the artifacts that depend on it
//! *unevaluated*, never clear and never stale. In particular a failed or
//! partial closing-reference read is never taken to mean "no closing PRs":
//! that would silently drop classifier reason (c).
//!
//! The classifier ([`super::classify`]) and [`super::Evidence`] do not change;
//! this module only builds the same evidence more cheaply.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::budget::{self, Budget, Floor, ForgeCost, Guard, Meter};
use super::{classify, park_self_block, Artifact, Evidence, RemoteRef, Verdict};
use crate::dep_recheck::{extract, named, premise, recheck};
use crate::forge_call_stats::{ops, ForgeOp};
use crate::forge_etag_store as store;
use crate::forge_identity::FleetLogins;
use crate::forge_listing::RestIssue;
use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
use crate::proc_exec::Completion;

/// Every read here is recorded under this caller in `forge_call_stats`.
const CALLER: &str = "stale_blocked";

/// The closing-reference query's telemetry name.
const GRAPHQL_OP: &str = "stale_blocked.closing_refs_graphql";

/// Issues per aliased closing-reference query.
pub const CLOSING_BATCH: usize = 100;

/// `first:` on each `closedByPullRequestsReferences` — `gh`'s own value.
pub const CLOSING_FIRST: usize = 100;

/// Comment pages are read until one is short; the cap bounds a runaway.
const COMMENT_PAGE: usize = 100;
const COMMENT_MAX_PAGES: u32 = 50;

/// Deadline for one GraphQL batch.
const GRAPHQL_TIMEOUT: Duration = Duration::from_secs(120);

/// One referenced issue's or PR's state (`OPEN` / `CLOSED` / `MERGED`) and
/// label names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefState {
    pub state: String,
    pub labels: Vec<String>,
}

/// One PR declared to close an issue, as the batched query reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosingRef {
    pub number: i64,
    pub state: String,
}

/// What the batch gatherer needs from the forge. Production is
/// [`GhStaleBlockedForge`]; tests use a counting fake.
pub trait StaleBlockedForge {
    /// Every open item labelled `loom:blocked`, issues and PRs together.
    ///
    /// # Errors
    /// The listing did not answer completely.
    fn list_blocked(&mut self) -> Result<Vec<RestIssue>>;

    /// Every comment on an issue or PR, oldest first.
    ///
    /// # Errors
    /// The read failed.
    fn comments(&mut self, number: u32) -> Result<Vec<extract::Comment>>;

    /// One issue's or PR's state and labels; `repo` `None` is the invoking
    /// repo. `Ok(None)` when it does not exist.
    ///
    /// # Errors
    /// The read failed.
    fn ref_state(&mut self, repo: Option<&str>, number: i64) -> Result<Option<RefState>>;

    /// One open PR's `(mergeable, mergeStateStatus)` in GraphQL spelling.
    ///
    /// # Errors
    /// The read failed or the PR does not exist.
    fn pr_merge_state(&mut self, number: u32) -> Result<(String, String)>;

    /// The closing PRs of up to [`CLOSING_BATCH`] issues. An issue is present
    /// in the map only when its answer was complete.
    ///
    /// # Errors
    /// The whole batch did not answer.
    fn closing_refs_batch(&mut self, issues: &[u32]) -> Result<HashMap<u32, Vec<ClosingRef>>>;

    /// The live budget from the free `/rate_limit` + GraphQL `rateLimit`
    /// probe; `None` when it did not answer.
    fn budget(&mut self) -> Option<Budget>;

    /// Whether the rate-limit breaker is suppressing forge calls.
    fn breaker_open(&mut self) -> bool;

    /// What the reads above have spent so far.
    fn meter(&self) -> Meter;
}

/// One examined artifact and its evidence, or why it could not be evaluated.
#[derive(Debug, Clone)]
pub struct Gathered {
    pub kind: Artifact,
    pub number: i64,
    pub title: String,
    pub evidence: Result<Evidence, String>,
}

/// Which artifacts to examine.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Per population, applied after the issue/PR split.
    pub limit: u32,
    /// Skip the PR population.
    pub no_prs: bool,
    /// The remaining-points floor the run must not cross.
    pub floor: Floor,
}

/// One run's result: the examined artifacts, the enumeration failure when
/// the listing itself did not answer, and what the run spent.
#[derive(Debug, Clone)]
pub struct Gathering {
    pub items: Vec<Gathered>,
    pub enumerate_error: Option<String>,
    pub cost: ForgeCost,
}

/// One artifact while its evidence is being assembled.
struct Pending {
    kind: Artifact,
    row: RestIssue,
    body: String,
    prose: Vec<i64>,
    named: Vec<named::Dep>,
    declared: Vec<crate::park_record::BlockerRef>,
    closing: Vec<ClosingRef>,
    failed: Option<String>,
}

/// A blocker reference: `(repo, number)`, `None` meaning the invoking repo.
type RefKey = (Option<String>, i64);

/// Gather the evidence for every open `loom:blocked` artifact, within the
/// budget floor ([`super::budget`]).
pub fn gather_all(
    forge: &mut dyn StaleBlockedForge,
    fleet: &FleetLogins,
    opts: Options,
) -> Gathering {
    gather_filtered(forge, fleet, opts, &mut |_, _, _| true)
}

/// [`gather_all`], keeping only the artifacts `keep` accepts once their text
/// is read (#10515). A rejected artifact costs no closing-PR or blocker-state
/// read and is not in the result. An artifact whose text read **failed** is
/// always kept, unevaluated: it is never filtered away as "not cited".
pub fn gather_filtered(
    forge: &mut dyn StaleBlockedForge,
    fleet: &FleetLogins,
    opts: Options,
    keep: &mut dyn FnMut(Artifact, i64, &extract::Input) -> bool,
) -> Gathering {
    let mut cost = ForgeCost {
        floor: opts.floor,
        ..ForgeCost::default()
    };
    let rows = match forge.list_blocked() {
        Ok(rows) => rows,
        Err(e) => {
            return Gathering {
                items: Vec::new(),
                enumerate_error: Some(format!("loom:blocked listing failed: {e}")),
                cost,
            }
        }
    };
    let limit = opts.limit as usize;
    let mut selected: Vec<(Artifact, RestIssue)> = Vec::new();
    for (kind, want_pr) in [(Artifact::Issue, false), (Artifact::Pr, true)] {
        if want_pr && opts.no_prs {
            continue;
        }
        let rows = rows
            .iter()
            .filter(|r| r.is_pull_request == want_pr)
            .take(limit);
        selected.extend(rows.map(|row| (kind, row.clone())));
    }

    // Nothing to gather, nothing to probe.
    if !selected.is_empty() {
        cost.projected = budget::project(&selected);
        let refused = if forge.breaker_open() {
            Some("rate-limit breaker is suppressing forge calls — not evaluated this run".into())
        } else {
            cost.budget_before = forge.budget();
            cost.budget_before
                .and_then(|b| budget::refusal(&b, cost.projected, opts.floor))
        };
        if let Some(why) = refused {
            let items = selected
                .into_iter()
                .map(|(kind, row)| Gathered {
                    kind,
                    number: i64::from(row.number),
                    title: row.title.unwrap_or_default(),
                    evidence: Err(why.clone()),
                })
                .collect();
            cost.budget_refused = Some(why);
            cost.meter = forge.meter();
            return Gathering {
                items,
                enumerate_error: None,
                cost,
            };
        }
    }

    let mut guard = Guard::new(opts.floor);
    let mut pending: Vec<Pending> = Vec::new();
    for (kind, row) in selected {
        let (p, input) = read_text(forge, fleet, kind, row, &mut guard);
        if input.is_none_or(|i| keep(kind, i64::from(p.row.number), &i)) {
            pending.push(p);
        }
    }
    read_closing(forge, &mut pending, &mut guard);
    let states = read_states(forge, &pending, &mut guard);
    let items = pending
        .into_iter()
        .map(|p| assemble(forge, p, &states, &mut guard))
        .collect();
    cost.meter = forge.meter();
    cost.budget_stopped = guard.stopped();
    Gathering {
        items,
        enumerate_error: None,
        cost,
    }
}

/// Body (from the listing row) and comments (read only when there are any),
/// and every reference the text cites. The text is returned too, `None` when
/// its read failed.
fn read_text(
    forge: &mut dyn StaleBlockedForge,
    fleet: &FleetLogins,
    kind: Artifact,
    row: RestIssue,
    guard: &mut Guard,
) -> (Pending, Option<extract::Input>) {
    let body = row.body.clone().unwrap_or_default();
    let mut p = Pending {
        kind,
        body: body.clone(),
        prose: Vec::new(),
        named: Vec::new(),
        declared: Vec::new(),
        closing: Vec::new(),
        failed: None,
        row,
    };
    let comments = if p.row.comments == 0 {
        Vec::new()
    } else {
        if let Err(why) = guard.core(&forge.meter()) {
            p.failed = Some(why);
            return (p, None);
        }
        match forge.comments(p.row.number) {
            Ok(c) => c,
            Err(e) => {
                p.failed = Some(format!(
                    "comment read for {} #{} failed — cannot assess a park from a failed read \
                     (fail safe: never guess 'no refs' on missing data): {e}",
                    kind.label(),
                    p.row.number
                ));
                return (p, None);
            }
        }
    };
    let input = extract::Input { body, comments };
    // A qualified park-record blocker is read in its own repo via `declared`,
    // never as a local `#N` (#10443).
    let masked = extract::Input {
        body: crate::park_record::mask_qualified(&input.body),
        comments: input.comments.clone(),
    };
    p.prose = extract::extract_with(&masked, fleet)
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    // The park record is read from the BODY only (#8925).
    p.declared = crate::park_record::blockers(&p.body);
    if kind == Artifact::Issue {
        p.named = named::parse_entries(&p.body);
    }
    (p, Some(input))
}

/// The closing PRs of every still-evaluable issue, [`CLOSING_BATCH`] per
/// query, serially (loom#9191 secondary limits).
fn read_closing(forge: &mut dyn StaleBlockedForge, pending: &mut [Pending], guard: &mut Guard) {
    let issues: Vec<u32> = pending
        .iter()
        .filter(|p| p.kind == Artifact::Issue && p.failed.is_none())
        .map(|p| p.row.number)
        .collect();
    let mut answers: HashMap<u32, Vec<ClosingRef>> = HashMap::new();
    let mut batch_errors: HashMap<u32, String> = HashMap::new();
    for chunk in issues.chunks(CLOSING_BATCH) {
        // Re-checked between batches, from the previous batch's own
        // `rateLimit.remaining`.
        let answer = match guard.graphql(&forge.meter()) {
            Ok(()) => forge.closing_refs_batch(chunk),
            Err(why) => Err(anyhow!(why)),
        };
        match answer {
            Ok(map) => answers.extend(map),
            Err(e) => {
                for n in chunk {
                    batch_errors.insert(*n, e.to_string());
                }
            }
        }
    }
    for p in pending
        .iter_mut()
        .filter(|p| p.kind == Artifact::Issue && p.failed.is_none())
    {
        let n = p.row.number;
        match answers.remove(&n) {
            Some(refs) => p.closing = refs,
            None => {
                let why = batch_errors
                    .remove(&n)
                    .unwrap_or_else(|| "no complete answer for this issue".to_string());
                p.failed = Some(format!(
                    "closing-PR read for issue #{n} failed — never guess 'no closing PRs' on a \
                     failed read: {why}"
                ));
            }
        }
    }
}

/// Whether every closing PR is merged or closed (classifier reason (c)).
fn all_resolved(closing: &[ClosingRef]) -> bool {
    !closing.is_empty()
        && closing
            .iter()
            .all(|c| c.state == "MERGED" || c.state == "CLOSED")
}

/// Every distinct blocker the evaluable artifacts need, read once each.
///
/// A closing PR is read only when every closing PR of its issue is already
/// resolved: its labels feed only that reason's rendered line, and an open
/// closing PR makes the reason not fire at all.
fn read_states(
    forge: &mut dyn StaleBlockedForge,
    pending: &[Pending],
    guard: &mut Guard,
) -> HashMap<RefKey, Result<RefState, String>> {
    let mut keys: BTreeSet<RefKey> = BTreeSet::new();
    for p in pending.iter().filter(|p| p.failed.is_none()) {
        keys.extend(p.prose.iter().map(|n| (None, *n)));
        keys.extend(
            p.declared
                .iter()
                .filter_map(|b| Some((b.repo.clone()?, i64::try_from(b.number).ok()?)))
                .map(|(r, n)| (Some(r), n)),
        );
        keys.extend(
            p.named
                .iter()
                .filter(|d| !d.checked)
                .map(|d| (d.repo.clone(), d.number)),
        );
        if all_resolved(&p.closing) {
            keys.extend(p.closing.iter().map(|c| (None, c.number)));
        }
    }
    keys.into_iter()
        .map(|key| {
            if let Err(why) = guard.core(&forge.meter()) {
                return (key, Err(why));
            }
            let state = match forge.ref_state(key.0.as_deref(), key.1) {
                Ok(Some(s)) if !s.state.is_empty() => Ok(s),
                Ok(Some(_)) => Err("the forge returned no state".to_string()),
                Ok(None) => Err("not found (HTTP 404)".to_string()),
                Err(e) => Err(e.to_string()),
            };
            (key, state)
        })
        .collect()
}

/// Build one artifact's [`Evidence`] from the run-wide reads.
fn assemble(
    forge: &mut dyn StaleBlockedForge,
    p: Pending,
    states: &HashMap<RefKey, Result<RefState, String>>,
    guard: &mut Guard,
) -> Gathered {
    let evidence = match p.failed.clone() {
        Some(why) => Err(why),
        None => evidence_for(forge, &p, states, guard),
    };
    Gathered {
        kind: p.kind,
        number: i64::from(p.row.number),
        title: p.row.title.clone().unwrap_or_default(),
        evidence,
    }
}

fn evidence_for(
    forge: &mut dyn StaleBlockedForge,
    p: &Pending,
    states: &HashMap<RefKey, Result<RefState, String>>,
    guard: &mut Guard,
) -> Result<Evidence, String> {
    let lookup = |key: RefKey, what: String| -> Result<RefState, String> {
        match states.get(&key) {
            Some(Ok(s)) => Ok(s.clone()),
            Some(Err(why)) => Err(format!("could not read state for {what}: {why}")),
            None => Err(format!("could not read state for {what}: never requested")),
        }
    };
    let prose = p
        .prose
        .iter()
        .map(|&n| {
            lookup((None, n), format!("reference #{n}")).map(|s| premise::Ref {
                number: n,
                state: s.state,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let remote =
        p.declared
            .iter()
            .filter_map(|b| Some((b.repo.clone()?, i64::try_from(b.number).ok()?)))
            .map(|(repo, number)| {
                lookup((Some(repo.clone()), number), format!("cross-repo blocker {repo}#{number}"))
                    .map(|s| RemoteRef {
                        repo,
                        number,
                        state: s.state,
                    })
            })
            .collect::<Result<Vec<_>, String>>()?;
    let named = p
        .named
        .iter()
        .map(|d| {
            if d.checked {
                return Ok(d.clone());
            }
            let s =
                lookup((d.repo.clone(), d.number), format!("named dependency {}", d.reference()))?;
            Ok(named::Dep {
                state: Some(s.state),
                ..d.clone()
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let closing = if all_resolved(&p.closing) {
        p.closing
            .iter()
            .map(|c| {
                let s = lookup((None, c.number), format!("closing PR #{}", c.number))?;
                Ok(recheck::Pr {
                    number: c.number,
                    state: s.state,
                    labels: s.labels,
                    ..recheck::Pr::default()
                })
            })
            .collect::<Result<Vec<_>, String>>()?
    } else {
        p.closing
            .iter()
            .map(|c| recheck::Pr {
                number: c.number,
                state: c.state.clone(),
                ..recheck::Pr::default()
            })
            .collect()
    };
    let mut evidence = Evidence {
        named,
        prose,
        closing,
        declared: p.declared.clone(),
        remote,
        self_block: None,
    };
    if p.kind == Artifact::Pr && matches!(classify(&evidence), Verdict::Stale(_)) {
        guard.core(&forge.meter())?;
        evidence.self_block = self_block(forge, &p.row)?;
    }
    Ok(evidence)
}

/// A parked PR's own superseding block. Labels come from the listing row; the
/// merge state is read only when no label already decides it.
fn self_block(
    forge: &mut dyn StaleBlockedForge,
    row: &RestIssue,
) -> Result<Option<String>, String> {
    let mut pr = recheck::Pr {
        number: i64::from(row.number),
        state: "OPEN".to_string(),
        labels: row.labels.clone(),
        ..recheck::Pr::default()
    };
    if let Some(block) = park_self_block(&pr) {
        return Ok(Some(block));
    }
    let (mergeable, merge_state_status) = forge.pr_merge_state(row.number).map_err(|e| {
        format!("PR #{} merge-state read failed — cannot assess its own block: {e}", row.number)
    })?;
    pr.mergeable = mergeable;
    pr.merge_state_status = merge_state_status;
    Ok(park_self_block(&pr))
}

// ============================================================================
// Production forge: REST + ETag through `forge_etag_store`, one GraphQL query
// per batch through `GhInvocation`.
// ============================================================================

/// [`StaleBlockedForge`] over `gh`, for the repo checked out at `root` (or the
/// explicit `repo`).
pub struct GhStaleBlockedForge {
    gh_bin: PathBuf,
    root: PathBuf,
    /// The explicit `--repo` / `LOOM_REPO`, when given.
    repo: Option<String>,
    /// `owner/name` for URLs: `repo`, else `root`'s remote, else `gh`'s
    /// `{owner}/{repo}` placeholder.
    slug: String,
    /// What this run's evidence reads have spent.
    meter: Meter,
}

impl GhStaleBlockedForge {
    #[must_use]
    pub fn new(root: &Path, repo: Option<&str>) -> Self {
        let repo = repo.map(str::to_string).or_else(|| {
            std::env::var("LOOM_REPO")
                .ok()
                .filter(|r| !r.trim().is_empty())
        });
        let slug = store::resolve_target(Some(root), repo.as_deref())
            .repo
            .unwrap_or_else(|| "{owner}/{repo}".to_string());
        Self {
            gh_bin: PathBuf::from(crate::gh_invocation::gh_bin()),
            root: root.to_path_buf(),
            repo,
            slug,
            meter: Meter::default(),
        }
    }

    /// A conditional GET through the shared ETag store. `repo` overrides the
    /// invoking repo (a cross-repo named dependency).
    fn get(&mut self, op: ForgeOp, repo: Option<&str>, url: &str) -> Result<Option<String>> {
        let read = store::cached_read(
            store::ConditionalRead::new(CALLER, op),
            &self.gh_bin,
            Some(&self.root),
            repo.or(self.repo.as_deref()),
            url,
            "stale-",
        )?;
        self.meter.rest(read.not_modified, read.core_remaining);
        Ok(read.body)
    }
}

impl StaleBlockedForge for GhStaleBlockedForge {
    fn list_blocked(&mut self) -> Result<Vec<RestIssue>> {
        crate::forge_listing::list_issues_cached_all_as(
            CALLER,
            &self.gh_bin,
            Some(&self.root),
            self.repo.as_deref(),
            "loom:blocked",
            "open",
        )
    }

    fn comments(&mut self, number: u32) -> Result<Vec<extract::Comment>> {
        let mut all = Vec::new();
        for page in 1..=COMMENT_MAX_PAGES {
            let url = format!(
                "repos/{}/issues/{number}/comments?per_page={COMMENT_PAGE}&page={page}",
                self.slug
            );
            let body = self
                .get(ops::COMMENT_LIST, None, &url)?
                .ok_or_else(|| anyhow!("gh api {url} failed: HTTP 404"))?;
            let rows = parse_comments(&body).map_err(|e| anyhow!("parse {url}: {e}"))?;
            let short = rows.len() < COMMENT_PAGE;
            all.extend(rows);
            if short {
                return Ok(all);
            }
        }
        Err(anyhow!(
            "more than {} comments on #{number}",
            COMMENT_PAGE * COMMENT_MAX_PAGES as usize
        ))
    }

    fn ref_state(&mut self, repo: Option<&str>, number: i64) -> Result<Option<RefState>> {
        let url = format!("repos/{}/issues/{number}", repo.unwrap_or(&self.slug));
        match self.get(ops::ISSUE_VIEW_STATE, repo, &url)? {
            Some(body) => parse_ref_state(&body).map(Some),
            None => Ok(None),
        }
    }

    fn pr_merge_state(&mut self, number: u32) -> Result<(String, String)> {
        let url = format!("repos/{}/pulls/{number}", self.slug);
        let body = self
            .get(ops::PR_VIEW_STATE, None, &url)?
            .ok_or_else(|| anyhow!("gh api {url} failed: HTTP 404"))?;
        parse_pr_merge_state(&body)
    }

    fn closing_refs_batch(&mut self, issues: &[u32]) -> Result<HashMap<u32, Vec<ClosingRef>>> {
        if crate::rate_limit_breaker::global_skip_pass(CALLER) {
            return Err(anyhow!("rate-limit breaker is suppressing forge calls"));
        }
        let (owner, name) = owner_name(&self.slug).ok_or_else(|| {
            anyhow!("cannot name the repository for the closing-PR query ({})", self.slug)
        })?;
        let field = format!("query={}", closing_refs_query(owner, name, issues));
        let inv = GhInvocation::new(
            Operation::new(GRAPHQL_OP),
            AccessIntent::Read,
            GhTarget::None,
            GRAPHQL_TIMEOUT,
        )
        .forge_op(ops::ISSUE_CLOSED_BY_PULL_REQUESTS)
        .identity_scope(None, Some(&self.slug))
        .program(&self.gh_bin)
        .current_dir(&self.root)
        .args(["api", "graphql", "-f", field.as_str()]);
        let out = match inv.execute() {
            Ok(GhCompletion::Captured(Completion::Exited(out))) => out,
            Ok(_) => {
                // It may still have been charged.
                self.meter.graphql(None, None);
                return Err(anyhow!("gh api graphql timed out"));
            }
            Err(e) => return Err(anyhow!("gh api graphql could not run: {e:?}")),
        };
        // `gh` exits non-zero when the answer carries any `errors` entry, but
        // still prints the partial `data`: parse first, judge per alias.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let (cost, remaining) = parse_rate_limit(&stdout);
        self.meter.graphql(cost, remaining);
        parse_closing_refs(&stdout, issues).ok_or_else(|| {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            crate::rate_limit_breaker::global_observe_failure(&stderr, CALLER);
            anyhow!("gh api graphql (closing PRs) failed: {stderr}")
        })
    }

    fn budget(&mut self) -> Option<Budget> {
        let ctx = crate::rate_limit_breaker::report::FailureContext::for_root(
            &self.root,
            self.gh_bin.to_string_lossy().into_owned(),
        );
        crate::rate_limit_breaker::forge::probe_budget_ctx(&ctx, chrono::Utc::now()).map(|b| {
            Budget {
                core_remaining: b.core_remaining,
                graphql_remaining: b.graphql_remaining,
            }
        })
    }

    fn breaker_open(&mut self) -> bool {
        crate::rate_limit_breaker::global_skip_pass(CALLER)
    }

    fn meter(&self) -> Meter {
        self.meter
    }
}

/// `owner/name`, each part `[A-Za-z0-9._-]+` so it can sit in a GraphQL
/// string literal.
fn owner_name(slug: &str) -> Option<(&str, &str)> {
    slug.split_once('/').filter(|(o, n)| {
        [*o, *n].iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
    })
}

/// One aliased query (`i<N>: issue(number: N)`), with `gh`'s own
/// `closedByPullRequestsReferences` arguments. Modelled on
/// `ci_telemetry::story::closing_refs_query`, which asks the mirror relation.
#[must_use]
pub fn closing_refs_query(owner: &str, name: &str, issues: &[u32]) -> String {
    let fields: String = issues
        .iter()
        .map(|n| {
            format!(
                " i{n}: issue(number: {n}) {{ closedByPullRequestsReferences(first: \
                 {CLOSING_FIRST}) {{ totalCount nodes {{ number state }} }} }}"
            )
        })
        .collect();
    format!(
        "query {{ rateLimit {{ cost remaining }} repository(owner: \"{owner}\", name: \
         \"{name}\") {{{fields} }} }}"
    )
}

/// The `rateLimit { cost remaining }` a [`closing_refs_query`] answer carries:
/// `(cost, remaining)`, each `None` when absent (a refused or unparseable
/// answer).
#[must_use]
pub fn parse_rate_limit(body: &str) -> (Option<u64>, Option<u64>) {
    let json: serde_json::Value = match serde_json::from_str(body.trim()) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let r = &json["data"]["rateLimit"];
    (r["cost"].as_u64(), r["remaining"].as_u64())
}

/// Parse a [`closing_refs_query`] answer. `None` when there is no
/// `data.repository` at all (the whole batch failed). An issue is in the map
/// only when its alias parsed completely and `totalCount` fits in one page; a
/// `null` alias (an `errors` entry) or a truncated list leaves it out, so it is
/// reported unevaluated rather than as having no closing PRs.
#[must_use]
pub fn parse_closing_refs(body: &str, issues: &[u32]) -> Option<HashMap<u32, Vec<ClosingRef>>> {
    let json: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let repository = json.get("data")?.get("repository")?;
    if !repository.is_object() {
        return None;
    }
    let mut out = HashMap::new();
    for n in issues {
        let field = &repository[format!("i{n}")]["closedByPullRequestsReferences"];
        let Some(total) = field["totalCount"].as_u64() else {
            continue;
        };
        let Some(nodes) = field["nodes"].as_array() else {
            continue;
        };
        let refs: Option<Vec<ClosingRef>> = nodes
            .iter()
            .map(|node| {
                Some(ClosingRef {
                    number: node["number"].as_i64()?,
                    state: node["state"].as_str()?.to_string(),
                })
            })
            .collect();
        match refs {
            Some(refs) if usize::try_from(total).ok() == Some(refs.len()) => {
                out.insert(*n, refs);
            }
            _ => {}
        }
    }
    Some(out)
}

#[derive(Deserialize)]
struct RawLabel {
    name: String,
}

/// `GET issues/{n}` → state + labels. A PR number answers too: a non-null
/// `pull_request.merged_at` is `MERGED`, else the uppercased `state` — the
/// same derivation as `forge_cached_view`'s PR state.
///
/// # Errors
/// The body is not an issue object.
pub fn parse_ref_state(body: &str) -> Result<RefState> {
    #[derive(Deserialize)]
    struct RawPr {
        #[serde(default)]
        merged_at: Option<String>,
    }
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        state: String,
        #[serde(default)]
        labels: Vec<RawLabel>,
        #[serde(default)]
        pull_request: Option<RawPr>,
    }
    let raw: Raw = serde_json::from_str(body.trim())?;
    let merged = raw.pull_request.is_some_and(|p| p.merged_at.is_some());
    Ok(RefState {
        state: if merged {
            "MERGED".to_string()
        } else {
            raw.state.to_ascii_uppercase()
        },
        labels: raw.labels.into_iter().map(|l| l.name).collect(),
    })
}

/// `GET pulls/{n}` → `(mergeable, mergeStateStatus)` in GraphQL spelling:
/// `mergeable` `true`/`false`/`null` → `MERGEABLE`/`CONFLICTING`/`UNKNOWN`,
/// `mergeable_state` uppercased (`dirty` → `DIRTY`).
///
/// # Errors
/// The body is not a PR object.
pub fn parse_pr_merge_state(body: &str) -> Result<(String, String)> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        mergeable: Option<bool>,
        #[serde(default)]
        mergeable_state: Option<String>,
    }
    let raw: Raw = serde_json::from_str(body.trim())?;
    let mergeable = match raw.mergeable {
        Some(true) => "MERGEABLE",
        Some(false) => "CONFLICTING",
        None => "UNKNOWN",
    };
    let state = raw
        .mergeable_state
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_default();
    Ok((mergeable.to_string(), state))
}

/// A REST comments page → [`extract::Comment`]s.
///
/// REST names an App's bot as `name[bot]` where the GraphQL `gh` shape this
/// vocabulary was built on says `name`: the suffix is stripped here so a bot's
/// comment is recognised exactly as before, whichever spelling the fleet
/// matcher is given.
///
/// # Errors
/// The body is not a comment array.
pub fn parse_comments(body: &str) -> Result<Vec<extract::Comment>> {
    #[derive(Deserialize)]
    struct RawUser {
        #[serde(default)]
        login: Option<String>,
    }
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        user: Option<RawUser>,
    }
    let rows: Vec<Raw> = serde_json::from_str(body.trim())?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let login = r.user.and_then(|u| u.login).unwrap_or_default();
            let login = login.strip_suffix("[bot]").unwrap_or(&login).to_string();
            extract::Comment {
                author: extract::Author { login },
                body: r.body.unwrap_or_default(),
            }
        })
        .collect())
}
