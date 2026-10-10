//! The head check of the workspace resync (#10987): where every registered
//! repo's default branch is, asked once per pass.
//!
//! # One request, not one per repo
//!
//! [`live`] sends one GraphQL query per owner, with one alias per repo:
//!
//! ```text
//! query { r0: repository(owner: "acme", name: "app") {
//!           defaultBranchRef { name target { oid } } }
//!         r1: repository(owner: "acme", name: "lib") { … } }
//! ```
//!
//! Per owner because the credential is: an App installation token sees its
//! own installation's repos and no others. More than [`CHUNK`] repos of one
//! owner are split across queries. Each query runs under the writer
//! credential through the fleet store's transport
//! ([`GhTransport::graphql_as_writer`]), is accounted as
//! `git.default-branch-heads`, and is not sent while the rate-limit breaker
//! is open. GraphQL is a `POST`, so there is no ETag and no free `304`: a
//! query costs one point on the credential's GraphQL bucket.
//!
//! # What a caller does with the answer
//!
//! * [`Answer::At`]: the forge named the default branch and its head.
//! * [`Answer::Fault`]: the forge answered, and has no such repo for this
//!   credential (deleted, renamed, or not visible). A fact about one repo,
//!   never about the host's network.
//! * No entry: the query for that owner failed, or the breaker is open.
//!
//! The pass asks `git ls-remote` about a repo with no usable answer
//! ([`probe`], a few at a time), unless the breaker is open: then nothing is
//! asked this tick at all.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{git, Env, Memory, Scan, UNREACHABLE_IN_A_ROW};
use crate::fleet_store::gh::GhTransport;
use crate::forge_call_stats::ops;

/// Repos per query. Far below any GraphQL node limit; it bounds the size of
/// one `gh` argument.
pub const CHUNK: usize = 100;

/// `git ls-remote` children running at once in [`probe`].
pub const PROBE_PARALLEL: usize = 8;

/// Deadline for one query: a third of the pass's network budget.
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// What the whole head check may spend across owners, token minting
/// included: the pass's network budget. An owner not reached in time is
/// asked with `git ls-remote` like any repo the query gave no head for.
const HEADS_BUDGET: Duration = Duration::from_secs(45);

/// The rate-limit breaker's name for this job.
const CALLER: &str = "workspace_resync";

/// One workspace the head check asks about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadAsk {
    /// The registered root. It decides the credential.
    pub root: PathBuf,
    /// `OWNER/REPO`.
    pub nwo: String,
}

/// Why the forge has no default-branch head for a repo. Always about that
/// one repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// No such repo for this credential: deleted, renamed, or not visible.
    NotFound(String),
    /// The credential may not read it.
    Forbidden(String),
    /// The repo has no default branch (it is empty).
    NoDefaultBranch,
    /// The repository is archived: it cannot be pushed to.
    Archived,
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(why) => write!(f, "the forge has no such repository: {why}"),
            Self::Forbidden(why) => write!(f, "the forge refused to read the repository: {why}"),
            Self::NoDefaultBranch => f.write_str("the repository has no default branch"),
            Self::Archived => f.write_str("the repository is archived"),
        }
    }
}

/// What the forge said about one repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The default branch and the commit it is at.
    At {
        /// The default branch's name, as the forge has it.
        branch: String,
        /// Its head.
        commit: String,
    },
    /// The forge answered without a head.
    Fault(Fault),
}

/// One pass's head check.
#[derive(Debug, Default)]
pub struct Heads {
    /// By the root asked about. A root that is absent got no answer.
    pub answers: HashMap<PathBuf, Answer>,
    /// Forge requests sent.
    pub requests: u32,
    /// The rate-limit breaker is open: some or all of the check was not sent.
    pub breaker_open: bool,
    /// The check's time budget ran out between owners: later owners were not
    /// asked.
    pub out_of_budget: bool,
    /// Why a request got no usable answer, one entry per request.
    pub failures: Vec<String>,
}

/// `owner` and `name` of a slug whose parts can sit in a GraphQL string
/// literal: `[A-Za-z0-9._-]+` each.
fn owner_name(nwo: &str) -> Option<(&str, &str)> {
    nwo.split_once('/').filter(|(o, n)| {
        [*o, *n].iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
    })
}

/// The aliased query for `repos` (`OWNER/REPO` each): alias `r<i>` is the
/// `i`-th. A slug that cannot be quoted gets no alias, so it gets no answer.
#[must_use]
pub fn query(repos: &[&str]) -> String {
    let fields: String = repos
        .iter()
        .enumerate()
        .filter_map(|(i, nwo)| {
            let (owner, name) = owner_name(nwo)?;
            Some(format!(
                " r{i}: repository(owner: \"{owner}\", name: \"{name}\") {{ isArchived \
                 defaultBranchRef {{ name target {{ oid }} }} }}"
            ))
        })
        .collect();
    format!("query {{{fields} }}")
}

/// Read a [`query`] answer for `count` repos: the `i`-th entry is alias
/// `r<i>`'s. `None` when the body carries no `data` object at all (the whole
/// query failed). An alias that is `null` is a [`Fault`] only when the
/// answer's `errors` say why; otherwise it stays unanswered.
#[must_use]
pub fn parse(body: &str, count: usize) -> Option<Vec<Option<Answer>>> {
    let json: Value = serde_json::from_str(body.trim()).ok()?;
    let data = json.get("data")?.as_object()?;
    let errors = json.get("errors").and_then(Value::as_array);
    let fault = |alias: &str| {
        let error = errors?.iter().find(|e| {
            e.get("path")
                .and_then(Value::as_array)
                .and_then(|p| p.first())
                .and_then(Value::as_str)
                == Some(alias)
        })?;
        let why = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no detail")
            .to_string();
        match error.get("type").and_then(Value::as_str) {
            Some("NOT_FOUND") => Some(Fault::NotFound(why)),
            Some("FORBIDDEN") => Some(Fault::Forbidden(why)),
            _ => None,
        }
    };
    let one = |i: usize| {
        let alias = format!("r{i}");
        let repo = match data.get(&alias) {
            Some(repo) if repo.is_object() => repo,
            _ => return fault(&alias).map(Answer::Fault),
        };
        if repo["isArchived"].as_bool() == Some(true) {
            return Some(Answer::Fault(Fault::Archived));
        }
        let head = &repo["defaultBranchRef"];
        if head.is_null() {
            return Some(Answer::Fault(Fault::NoDefaultBranch));
        }
        let branch = head["name"].as_str().filter(|b| !b.is_empty())?;
        let commit = head["target"]["oid"]
            .as_str()
            .filter(|oid| oid.len() >= 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()))?;
        Some(Answer::At {
            branch: branch.to_string(),
            commit: commit.to_string(),
        })
    };
    Some((0..count).map(one).collect())
}

/// One query: up to [`CHUNK`] repos of one owner.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Batch<'a> {
    /// The owner, lowercased.
    pub(super) owner: String,
    /// A root of this owner's, and its repo: they pick the credential.
    pub(super) under: &'a HeadAsk,
    /// The repos asked about, `OWNER/REPO` each; alias `r<i>` is the `i`-th.
    pub(super) repos: Vec<&'a str>,
}

/// The queries one head check sends: `asks` grouped by owner (an owner's
/// name is not case-sensitive), each owner's repos in [`CHUNK`]s. Two clones
/// of one repo are one alias.
pub(super) fn batches(asks: &[HeadAsk]) -> Vec<Batch<'_>> {
    let mut owners: BTreeMap<String, Vec<&HeadAsk>> = BTreeMap::new();
    for ask in asks {
        let owner = ask.nwo.split('/').next().unwrap_or_default().to_lowercase();
        owners.entry(owner).or_default().push(ask);
    }
    let mut out = Vec::new();
    for (owner, group) in owners {
        let Some(under) = group.first().copied() else {
            continue;
        };
        let mut repos: Vec<&str> = group.iter().map(|ask| ask.nwo.as_str()).collect();
        repos.sort_unstable();
        repos.dedup();
        out.extend(repos.chunks(CHUNK).map(|chunk| Batch {
            owner: owner.clone(),
            under,
            repos: chunk.to_vec(),
        }));
    }
    out
}

/// Send `asks`' [`batches`] with `send` (which returns a query's response
/// body) and collect the answers. `breaker_open` is read before every
/// request, because a refusal on the last one may have opened it; once it
/// says yes nothing more is sent. It is read once more after the last
/// request: that one may have opened it, and then the caller must neither
/// fall back to git nor claim. `out_of_budget` is read before each request
/// after the first; once it says yes the remaining owners are not asked.
pub(super) fn gather(
    asks: &[HeadAsk],
    breaker_open: &dyn Fn() -> bool,
    out_of_budget: &dyn Fn() -> bool,
    send: &dyn Fn(&Batch<'_>) -> anyhow::Result<String>,
) -> Heads {
    let mut heads = Heads::default();
    for batch in batches(asks) {
        if breaker_open() {
            heads.breaker_open = true;
            return heads;
        }
        if heads.requests > 0 && out_of_budget() {
            heads.out_of_budget = true;
            break;
        }
        heads.requests += 1;
        let parsed = send(&batch).and_then(|body| {
            parse(&body, batch.repos.len()).ok_or_else(|| {
                let shown: String = body.chars().take(200).collect();
                anyhow::anyhow!("the answer carries no data: {shown}")
            })
        });
        let found = match parsed {
            Ok(found) => found,
            Err(e) => {
                heads.failures.push(format!("{}: {e:#}", batch.owner));
                continue;
            }
        };
        for (nwo, answer) in batch.repos.iter().zip(found) {
            let Some(answer) = answer else {
                continue;
            };
            for ask in asks.iter().filter(|ask| ask.nwo == *nwo) {
                heads.answers.insert(ask.root.clone(), answer.clone());
            }
        }
    }
    if breaker_open() {
        heads.breaker_open = true;
    }
    heads
}

/// The production head check: one query per owner (per [`CHUNK`] repos),
/// under that owner's writer credential.
pub(super) fn live(asks: &[HeadAsk]) -> Heads {
    let started = Instant::now();
    gather(
        asks,
        &|| crate::rate_limit_breaker::global_skip_pass(CALLER),
        &|| started.elapsed() >= HEADS_BUDGET,
        &|batch| {
            // Minting the owner's token happens inside this call, so the
            // clock above covers it; the query gets what is left.
            let left = HEADS_BUDGET.saturating_sub(started.elapsed());
            GhTransport::new(&batch.under.root, &batch.under.nwo).graphql_as_writer(
                ops::GIT_DEFAULT_BRANCH_HEADS,
                &query(&batch.repos),
                QUERY_TIMEOUT.min(left.max(Duration::from_secs(1))),
            )
        },
    )
}

/// Ask each of `roots`' remotes for its default branch's head with
/// `git ls-remote`, all at once: the caller hands over at most
/// [`PROBE_PARALLEL`]. The answers are in `roots`' order.
fn probe(roots: &[&Path]) -> Vec<anyhow::Result<String>> {
    std::thread::scope(|scope| {
        let running: Vec<_> = roots
            .iter()
            .map(|root| scope.spawn(move || probe_one(root)))
            .collect();
        running
            .into_iter()
            .map(|thread| {
                thread
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("the remote probe panicked")))
            })
            .collect()
    })
}

fn probe_one(root: &Path) -> anyhow::Result<String> {
    let branch = git::default_branch(root).ok_or_else(|| {
        anyhow::anyhow!("no usable default branch ref (origin/HEAD or origin/main)")
    })?;
    git::remote_head(root, &branch)
}

// ============================================================================
// The check, as a pass runs it
// ============================================================================

/// What the head check learned about one workspace this pass.
pub(super) enum Asked {
    /// Nothing: the pass is offline, the repo is backing off, or the check
    /// did not get to it. The workspace is classified from its clone.
    No,
    /// The forge named the default branch and its head.
    Forge {
        /// The default branch, as the forge has it.
        branch: String,
        /// Its head.
        commit: String,
    },
    /// The remote was asked directly for the head of the branch the clone
    /// follows.
    Remote(anyhow::Result<String>),
    /// Nothing to resync, and nothing wrong: the repo is empty or archived.
    /// Skipped quietly, without a refusal or a warning.
    Skip(String),
}

/// Check every workspace's default-branch head for one pass: the batched
/// query first, then `git ls-remote` for each repo it gave no head for.
/// `names` is `roots`' `OWNER/REPO`s. Only called by a pass that is online.
///
/// With the rate-limit breaker open nothing is asked, the pass goes offline
/// and the result is empty.
pub(super) fn check(
    env: &Env<'_>,
    roots: &[PathBuf],
    names: &[Option<String>],
    memory: &mut Memory,
    scan: &mut Scan,
) -> HashMap<PathBuf, Asked> {
    let now = (env.clock)();
    let asks: Vec<HeadAsk> = roots
        .iter()
        .zip(names)
        .filter_map(|(root, nwo)| {
            let nwo = nwo.clone()?;
            // A repo that is backing off is not looked at this pass.
            let waiting = memory
                .backoff
                .get(root)
                .is_some_and(|b| b.next_attempt > now);
            (!waiting && !crate::init::is_loom_source_repo(root)).then(|| HeadAsk {
                root: root.clone(),
                nwo,
            })
        })
        .collect();
    let mut found = HashMap::new();
    if asks.is_empty() {
        return found;
    }
    let heads = (env.heads)(&asks);
    scan.head_queries = heads.requests;
    let news = memory.note_breaker(heads.breaker_open);
    if heads.breaker_open {
        if news {
            log::warn!(
                "workspace_resync: the forge rate-limit breaker is open; no default-branch head \
                 is checked, and nothing is resynced, until it closes (reported once)"
            );
        }
        scan.breaker_open = true;
        scan.online = false;
        return found;
    }
    if news {
        log::info!("workspace_resync: the forge rate-limit breaker closed; checking heads again");
    }
    // Folded into the host's record at the end of the pass, once it is known
    // whether any remote answered.
    scan.head_failure = (!heads.failures.is_empty()).then(|| heads.failures.join("; "));
    // The repos to ask directly: the ones the query gave no head for.
    let mut direct: Vec<&HeadAsk> = Vec::new();
    for ask in &asks {
        match heads.answers.get(&ask.root) {
            Some(Answer::At { branch, commit }) => {
                let head = Asked::Forge {
                    branch: branch.clone(),
                    commit: commit.clone(),
                };
                found.insert(ask.root.clone(), head);
            }
            Some(Answer::Fault(fault @ (Fault::NoDefaultBranch | Fault::Archived))) => {
                found.insert(ask.root.clone(), Asked::Skip(fault.to_string()));
            }
            Some(Answer::Fault(_)) | None => direct.push(ask),
        }
    }
    let mut silent = 0;
    for group in direct.chunks(PROBE_PARALLEL) {
        if silent >= UNREACHABLE_IN_A_ROW || (env.spent)() {
            // The rest stay unasked and are classified from their clones.
            break;
        }
        let roots: Vec<&Path> = group.iter().map(|ask| ask.root.as_path()).collect();
        scan.probes += u32::try_from(group.len()).unwrap_or(u32::MAX);
        for (ask, head) in group.iter().zip(probe(&roots)) {
            let fault = match heads.answers.get(&ask.root) {
                Some(Answer::Fault(fault)) => Some(fault),
                _ => None,
            };
            let head = match (head, fault) {
                (Ok(head), Some(_)) => {
                    // A credential that cannot see the repo is the query's
                    // problem, not the repo's: its own remote answers.
                    if memory.noted.insert(format!("blind:{}", ask.root.display())) {
                        log::warn!(
                            "workspace_resync: {}: the head query's credential cannot see this \
                             repo, so its remote is asked with git ls-remote every tick \
                             (reported once)",
                            ask.nwo
                        );
                    }
                    Ok(head)
                }
                // git could get no credential at all: the host's failure, not
                // the repo's, whatever the forge said. Kept as it is so the
                // pass reports one `credential-helper` alert for the host
                // instead of a refusal (and `repo-access`) per repo.
                (Err(e), Some(_)) if e.downcast_ref::<git::Credential>().is_some() => Err(e),
                // The forge says it has no such repo and the remote gave no
                // head either: the repo's failure, however git worded it.
                (Err(e), Some(fault)) => Err(git::Refused(format!("{fault} ({e:#})")).into()),
                (head, None) => head,
            };
            let unreachable = head
                .as_ref()
                .is_err_and(|e| e.downcast_ref::<git::Unreachable>().is_some());
            silent = if unreachable { silent + 1 } else { 0 };
            found.insert(ask.root.clone(), Asked::Remote(head));
        }
    }
    found
}
