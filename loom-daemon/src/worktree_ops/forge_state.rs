//! The hygiene read path (W6): the REST issue and PR state reads behind the
//! worktree reaper, eager reclaim, `clean`, `--aggressive`, the legacy
//! checkpoint command and the primary-checkout reaper. Since W6 PR2 the
//! number-keyed probes of `clean` come through here too; what is held for a
//! pass, what is remembered across passes and the pre-removal confirm live
//! in [`super::hygiene_pass`]. The branch lookups with no item number
//! (`clean::check_pr_merged` and `check_pr_status_for_branch`, both last
//! resorts behind the REST listing) still name gh's own target.
//!
//! # Which repo
//!
//! Every read names the checkout's OWN repository explicitly: gh's base repo
//! for the root, resolved and verified by [`crate::forge_repo_facts`]
//! (`canonical(root, GhRepoEnv::Ignore)` — `set-default` > `upstream` >
//! `github` > `origin`, ambiguous roots cross-checked against gh, the
//! post-rename name). `LOOM_REPO` / `GH_REPO` are never read: a CLI process
//! that exported `LOOM_REPO` for some other repo (the merge commands do)
//! must not read issue `N` of that repo for this checkout's worktree. The
//! fallback is the one W3a uses: a root the facts cannot model
//! ([`Lookup::Legacy`]) keeps gh's `{owner}/{repo}` placeholder, run with
//! `GH_REPO` stripped and unconditional; a fact that cannot be established
//! right now ([`Lookup::Unavailable`]) is [`Read::Unknown`].
//!
//! # Conditional, fresh
//!
//! Single-item reads (`issues/{n}`, `pulls/{n}`) are conditional `GET`s on
//! the shared `view-` entry of [`crate::forge_cached_view`] (one `ETag` with
//! the agents' `--cached` views, dropped by `gh-cached --invalidate N` after
//! a write). A `304` is server-fresh: ADR-0021 allows a conditional read to
//! gate an action, since a lagging replica would serve an unconditional read
//! the same stale body. No answer is remembered here beyond the `ETag` and
//! its body: every call reaches the forge. `LOOM_HYGIENE_CONDITIONAL=0`
//! makes them unconditional (no `If-None-Match`, the entry neither read nor
//! written); the target, identity check, routing and breaker are unchanged.
//!
//! [`issue_facts_fresh`] / [`pull_facts_fresh`] are always unconditional
//! ([`Mode::Unconditional`]): the read [`super::hygiene_pass`] makes
//! immediately before a removal, which must be this request's own answer
//! and never a body served from the store.
//!
//! Branch listings (`pulls?head=`) stay unconditional: `gh-cached
//! --invalidate` drops only `view-` entries, so a stored listing could
//! outlive a merge or close made by this host.
//!
//! # Trust boundary
//!
//! A `304` serves the body stored next to the `ETag`, in the `0700`
//! per-user store ([`crate::forge_etag_store::private_dir`]). Any process of
//! the same uid can write that store — agents included — so a conditional
//! read trusts same-uid processes not to plant a forged body under a current
//! `ETag`. This is accepted explicitly: the same uid can already edit the
//! worktrees, the git config and the daemon's own state. Other users are
//! refused by `private_dir`.
//!
//! # Identity
//!
//! Every answered body must be the item that was asked for: its `number`,
//! and its repository — `base.repo.id` against the record's repo id when
//! both are known (rename-proof), else `base.repo.full_name` /
//! `repository_url` against the canonical name. A mismatch (a transferred
//! item, a followed redirect) is [`Read::Unknown`], counted as
//! [`IDENTITY_MISMATCH`]. A first-hand (`200`) body for the right number
//! that names another repo under the same id (or with no id to compare)
//! casts doubt on the repo record ([`crate::forge_repo_facts::observe`], as
//! of the request-sent time), so a renamed repo re-resolves on the next read
//! instead of failing every read forever; a `304` body, another number or
//! another repo id never does ([`should_observe`]).
//!
//! # Failure
//!
//! A shed, a timeout, a non-200/304, a parse failure or a mismatch is
//! [`Read::Unknown`]; a `404`/`410` is [`Read::Gone`]. Consumers map both to
//! KEEP — never to "closed" or "no PR".
//!
//! # Rate-limit breaker
//!
//! Item reads honour the global breaker as the pre-W6 `cached_read` issue
//! reads did: while it is cooling an item read makes no forge call and is
//! [`Read::Unknown`]; a failed read's stderr is reported to it
//! (`global_observe_failure`), so a rate-limit refusal here trips it.

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::forge_call_stats::ForgeOp;
use crate::forge_etag_store::{self as store, ConditionalRead, Target};
use crate::forge_repo_facts::{self as facts, Fact, GhRepoEnv, Lookup};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation, ReadClass};

use super::clean::{self, PrStatus};
use super::clean_owner::PrRowRest;
use super::gh;

/// Counter: an answered body named another item or another repo.
pub(crate) const IDENTITY_MISMATCH: &str = "hygiene.identity_mismatch";
/// Counter: an item read answered `404`/`410` (a health signal: a burst for
/// one repo means its reads are not reaching it).
pub(crate) const ITEM_GONE: &str = "hygiene.item_gone";
/// `0` makes single-item reads unconditional (the confirm semantics do not
/// change: every read still reaches the forge).
pub(crate) const CONDITIONAL_ENV: &str = "LOOM_HYGIENE_CONDITIONAL";

/// One hygiene read's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Read<T> {
    /// The forge answered for exactly this item.
    Ok(T),
    /// `404` / `410`: the item is not there (or not visible). KEEP.
    Gone,
    /// No usable answer. KEEP.
    Unknown,
}

impl<T> Read<T> {
    /// The answer, if the forge gave one.
    pub(crate) fn ok(self) -> Option<T> {
        match self {
            Read::Ok(v) => Some(v),
            Read::Gone | Read::Unknown => None,
        }
    }
}

/// An issue's REST `state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IssueState {
    Open,
    Closed,
}

/// `GET repos/{o}/{r}/issues/{n}`: state and `closed_at` from ONE read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueFacts {
    pub(crate) state: IssueState,
    /// Non-empty `closed_at`, when the forge reports one.
    pub(crate) closed_at: Option<String>,
}

/// `GET repos/{o}/{r}/pulls/{n}`: status (with `merged_at` / `closed_at`)
/// and the head commit the forge recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullFacts {
    pub(crate) status: PrStatus,
    pub(crate) head_sha: Option<String>,
}

/// Where a root's hygiene reads go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Where {
    /// gh's base repo for the root, as a verified fact.
    Repo(Fact),
    /// gh's `{owner}/{repo}` placeholder, `GH_REPO` stripped, unconditional.
    Placeholder,
    /// No repo can be established now: every read is [`Read::Unknown`].
    Unavailable,
}

/// Resolve `root`'s hygiene target ([`crate::forge_repo_facts`], `GH_REPO`
/// and `LOOM_REPO` ignored).
pub(crate) fn resolve(root: &Path) -> Where {
    match facts::canonical(root, GhRepoEnv::Ignore) {
        Lookup::Fact(f) => Where::Repo(f),
        Lookup::Legacy => Where::Placeholder,
        Lookup::Unavailable => Where::Unavailable,
    }
}

/// The explicit target a fact names: the canonical (post-redirect) repo.
pub(crate) fn target_of(fact: &Fact) -> Target {
    Target {
        repo: Some(fact.full_name()),
        host: Some(fact.host.clone()),
    }
}

/// How a single-item read reaches the forge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Conditional on the shared `view-` entry (unless
    /// `LOOM_HYGIENE_CONDITIONAL=0`): a `304` serves the stored body.
    Conditional,
    /// No `If-None-Match`, the entry neither read nor written: the answer
    /// is always this request's own `200`.
    Unconditional,
}

fn conditional() -> bool {
    std::env::var(CONDITIONAL_ENV)
        .ok()
        .is_none_or(|v| v.trim() != "0")
}

#[derive(Debug, serde::Deserialize)]
struct IssueBody {
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    closed_at: Option<String>,
    #[serde(default)]
    repository_url: Option<String>,
}

/// One issue's state and `closed_at`, read fresh, booked under `caller`.
pub(crate) fn issue_facts(root: &Path, issue: u32, caller: &'static str) -> Read<IssueFacts> {
    issue_facts_in(root, issue, caller, Mode::Conditional)
}

/// [`issue_facts`], unconditional: never a body served from the store.
pub(crate) fn issue_facts_fresh(root: &Path, issue: u32, caller: &'static str) -> Read<IssueFacts> {
    issue_facts_in(root, issue, caller, Mode::Unconditional)
}

fn issue_facts_in(root: &Path, issue: u32, caller: &'static str, mode: Mode) -> Read<IssueFacts> {
    let read = item_read(root, "issue", issue, caller, mode, |body| {
        let b: IssueBody = serde_json::from_str(body).ok()?;
        let repo = b
            .repository_url
            .as_deref()
            .and_then(facts::full_name_from_repository_url);
        Some((b.number, repo, None, b))
    });
    let b = match read {
        Read::Ok(b) => b,
        Read::Gone => return Read::Gone,
        Read::Unknown => return Read::Unknown,
    };
    let state = match b.state.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("open") => IssueState::Open,
        Some("closed") => IssueState::Closed,
        _ => return Read::Unknown,
    };
    let closed_at = b
        .closed_at
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Read::Ok(IssueFacts { state, closed_at })
}

/// One PR's status and head SHA, read fresh, booked under `caller`.
pub(crate) fn pull_facts(root: &Path, pr: u32, caller: &'static str) -> Read<PullFacts> {
    pull_facts_in(root, pr, caller, Mode::Conditional)
}

/// [`pull_facts`], unconditional: never a body served from the store.
pub(crate) fn pull_facts_fresh(root: &Path, pr: u32, caller: &'static str) -> Read<PullFacts> {
    pull_facts_in(root, pr, caller, Mode::Unconditional)
}

fn pull_facts_in(root: &Path, pr: u32, caller: &'static str, mode: Mode) -> Read<PullFacts> {
    let read = item_read(root, "pr", pr, caller, mode, |body| {
        let row: PrRowRest = serde_json::from_str(body).ok()?;
        let repo = row.base_full_name().map(str::to_string);
        let id = row.base.as_ref().and_then(|b| b.repo.as_ref()?.id);
        Some((row.number, repo, id, row))
    });
    let row = match read {
        Read::Ok(row) => row,
        Read::Gone => return Read::Gone,
        Read::Unknown => return Read::Unknown,
    };
    Read::Ok(PullFacts {
        status: clean::classify_pr_row(
            row.state.as_str(),
            row.merged_at.as_deref(),
            row.closed_at.as_deref(),
        ),
        head_sha: row
            .head
            .and_then(|h| h.sha)
            .filter(|s| !s.trim().is_empty()),
    })
}

/// `(number, repo full name, repo id, parsed body)` of an answered body.
type Parsed<B> = (Option<u64>, Option<String>, Option<u64>, B);

/// One single-item read: resolve, fetch, check identity, parse.
///
/// Breaker-aware like every daemon forge poll (and like the
/// `forge_etag_store::cached_read` path the issue reads used before W6):
/// while the global rate-limit breaker is cooling the read makes no forge
/// call and is [`Read::Unknown`], and a failed read's stderr is reported to
/// the breaker so a rate-limit refusal here trips it.
fn item_read<B>(
    root: &Path,
    entity: &str,
    number: u32,
    caller: &'static str,
    mode: Mode,
    parse: impl Fn(&str) -> Option<Parsed<B>>,
) -> Read<B> {
    // Before `resolve`, which may itself read the repo from the forge.
    if crate::rate_limit_breaker::global_skip_pass(caller) {
        return Read::Unknown;
    }
    let op = view_op(entity);
    let (body, fact, first_hand) = match resolve(root) {
        Where::Unavailable => return Read::Unknown,
        Where::Placeholder => match placeholder_item(root, entity, number, caller, op) {
            Read::Ok(body) => (body, None, None),
            Read::Gone => return Read::Gone,
            Read::Unknown => return Read::Unknown,
        },
        Where::Repo(fact) => {
            let target = target_of(&fact);
            let dir = (mode == Mode::Conditional && conditional())
                .then(store::daemon_store_dir)
                .flatten();
            let site = ConditionalRead::new(caller, op).item_scoped();
            let gh = PathBuf::from(crate::gh_invocation::gh_bin());
            let sent_at = chrono::Utc::now().timestamp();
            let read = crate::forge_cached_view::fetch_view_for(
                site,
                &gh,
                Some(root),
                dir.as_deref(),
                entity,
                number,
                &target,
            );
            use crate::forge_cached_view::ViewStatus as S;
            match (read.status, read.body) {
                (S::Fresh, Some(body)) => (body, Some(fact), Some(sent_at)),
                (S::NotModified, Some(body)) => (body, Some(fact), None),
                (S::NotFound | S::Gone, _) => return gone(),
                (S::Failed, _) => {
                    crate::rate_limit_breaker::global_observe_failure(&read.stderr, caller);
                    return Read::Unknown;
                }
                _ => return Read::Unknown,
            }
        }
    };
    let Some((got_number, got_repo, got_id, parsed)) = parse(&body) else {
        return Read::Unknown;
    };
    let seen = Seen {
        number: got_number,
        repo: got_repo.as_deref(),
        id: got_id,
    };
    if let (Some(f), Some(sent_at)) = (fact.as_ref(), first_hand) {
        observe_repo(f, number, &seen, sent_at);
    }
    if identity_matches(fact.as_ref(), number, &seen) {
        Read::Ok(parsed)
    } else {
        Read::Unknown
    }
}

/// What an answered body says it is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Seen<'a> {
    pub(crate) number: Option<u64>,
    pub(crate) repo: Option<&'a str>,
    pub(crate) id: Option<u64>,
}

fn gone<T>() -> Read<T> {
    crate::forge_call_stats::counters::bump(ITEM_GONE);
    Read::Gone
}

fn view_op(entity: &str) -> ForgeOp {
    if entity == "pr" {
        crate::forge_call_stats::ops::PR_VIEW_STATE
    } else {
        crate::forge_call_stats::ops::ISSUE_VIEW_STATE
    }
}

/// Is the answered body the item that was asked for? The number always; the
/// repository when a fact named it (by id when both sides carry one).
pub(crate) fn identity_matches(fact: Option<&Fact>, number: u32, seen: &Seen<'_>) -> bool {
    let number_ok = seen.number == Some(u64::from(number));
    let repo_ok = fact.is_none_or(|f| match (f.repo_id, seen.id) {
        (Some(want), Some(got)) => want == got,
        _ => seen
            .repo
            .is_some_and(|r| r.eq_ignore_ascii_case(&f.full_name())),
    });
    if number_ok && repo_ok {
        return true;
    }
    let n = crate::forge_call_stats::counters::bump(IDENTITY_MISMATCH);
    log::warn!(
        "forge_state: asked for #{number} of {}, the forge answered #{} of {}; treating it as \
         unknown ({IDENTITY_MISMATCH}={n})",
        fact.map_or_else(|| "{owner}/{repo}".to_string(), Fact::full_name),
        seen.number
            .map_or_else(|| "?".to_string(), |v| v.to_string()),
        seen.repo.unwrap_or("<none>"),
    );
    false
}

/// Whether a body may cast doubt on the repo record: a FIRST-HAND (`200`)
/// answer for exactly the item asked for, naming another repository — and,
/// when both sides carry a repo id, under the SAME id (a rename). A `304`
/// serves a stored body, not a response to this request; a different number
/// is some other item (a transferred issue answers with its new repo's
/// number), and a different id is some other repo, not this one renamed —
/// neither says anything about this record, and feeding them in would mark
/// it suspect on every pass.
pub(crate) fn should_observe(fact: &Fact, number: u32, seen: &Seen<'_>) -> bool {
    let number_ok = seen.number == Some(u64::from(number));
    let id_ok = match (fact.repo_id, seen.id) {
        (Some(want), Some(got)) => want == got,
        _ => true,
    };
    let renamed = seen
        .repo
        .is_some_and(|r| !r.eq_ignore_ascii_case(&fact.full_name()));
    number_ok && id_ok && renamed
}

/// Feed a first-hand body's repository to [`facts::observe`] at the moment
/// the request was sent, when [`should_observe`] allows it. The record is
/// never rewritten from the body; it is only re-resolved before its next use.
fn observe_repo(fact: &Fact, number: u32, seen: &Seen<'_>, sent_at: i64) {
    if let (true, Some(r)) = (should_observe(fact, number, seen), seen.repo) {
        facts::observe(&fact.host, &fact.configured_nwo, r, sent_at);
    }
}

/// The placeholder fallback for one item: today's unconditional `gh api
/// repos/{owner}/{repo}/…`, with `GH_REPO` stripped.
fn placeholder_item(
    root: &Path,
    entity: &str,
    number: u32,
    caller: &'static str,
    op: ForgeOp,
) -> Read<String> {
    let url = crate::forge_cached_view::build_view_url(entity, None, number);
    let Some(out) = hygiene_get(caller, Some(op), root, &url, None, ReadClass::Hygiene) else {
        return Read::Unknown;
    };
    if out.status.success() {
        return Read::Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("HTTP 404") || stderr.contains("HTTP 410") {
        gone()
    } else {
        crate::rate_limit_breaker::global_observe_failure(&stderr, caller);
        Read::Unknown
    }
}

/// One bounded `gh api <path>` hygiene read from `root`: `GH_REPO` stripped
/// (the path names the repo, or gh resolves the checkout's own), `--hostname`
/// when the fact's host is not github.com, booked under `caller` (and `op`
/// when given). `None` = no answer (timeout, shed, spawn failure).
pub(crate) fn hygiene_get(
    caller: &'static str,
    op: Option<ForgeOp>,
    root: &Path,
    path: &str,
    fact: Option<&Fact>,
    class: ReadClass,
) -> Option<Output> {
    let mut inv = GhInvocation::new(
        Operation::new(caller),
        AccessIntent::Read,
        GhTarget::None,
        gh::GH_PROBE_TIMEOUT,
    )
    .args(["api", path])
    .current_dir(root)
    .read_class(class)
    .strip_env("GH_REPO");
    if let Some(op) = op {
        inv = inv.forge_op(op);
    }
    if let Some(host) = fact.map(|f| f.host.as_str()).filter(|h| *h != "github.com") {
        inv = inv.arg("--hostname").arg(host);
    }
    gh::bounded_via(inv)
}

/// The `pulls?state=all&head=<owner>:<branch>` path: the fact's canonical
/// repo when the owner came from one, else gh's placeholder.
pub(crate) fn pulls_by_head_path(fact: Option<&Fact>, owner: &str, branch: &str) -> String {
    let repo = fact.map_or_else(|| "{owner}/{repo}".to_string(), Fact::full_name);
    format!("repos/{repo}/pulls?state=all&head={owner}:{branch}&per_page=30")
}

#[cfg(test)]
#[path = "forge_state_tests.rs"]
mod tests;
