//! The routed `gh` entry points for the script-helper family (#13744).
//!
//! `run_gh` used to spawn `gh` (or the `gh-cached` wrapper) with whatever
//! credential was ambient in the daemon's environment: no access intent, no
//! reader routing, no shed, no `invoke github` span — so a helper read could
//! land on the writer and nobody could account for it (the 2026-11-10
//! incident: the `rjwalters` writer budget exhausted ~35 minutes of every
//! hour while the daemon's own spans booked only a fraction of the spend).
//!
//! Every call this family makes now goes through
//! [`crate::gh_invocation::GhInvocation`] with a **declared** intent:
//!
//! - [`gh_read`] / [`gh_read_json`] — [`AccessIntent::Read`] **and**
//!   [`GhInvocation::reader_only`] (#12555): a reader App or the read gateway
//!   serves it, or it is shed. On a host with no reader pool and no gateway
//!   at all it still runs on the only credential there is — the facade's
//!   documented no-pool branch — which is what keeps an attended session
//!   with a single login working exactly as before.
//! - [`gh_write`] — [`AccessIntent::Write`]: the writer, with the facade's
//!   write sequencing (`own_writes`, the write-scope gate) applied.
//!
//! Each call names a stable [`Operation`] (`github.operation` on its span,
//! the accounting row's caller), runs in `repo_root` with the same `gh`
//! resolver ladder and the same 30 s deadline the raw helper had, and — when
//! `use_cache` — pins the repo's `gh-cached` read wrapper exactly as before.
//!
//! # The declaration is checked, twice
//!
//! The declared intent must match what the argv provably is, or the call is
//! **refused before anything is spawned** ([`argv_access`]):
//!
//! - a read-shaped argv ([`gh_write`] on `gh api` without a write method, a
//!   `… view` / `… list`, a GraphQL `query` document) can never be labelled
//!   a write — that label is how a read would sneak onto the writer;
//! - a write-shaped argv (`-X POST/PUT/PATCH/DELETE`, `-f` fields on a REST
//!   path, `… edit` / `… comment` / `… create`, a GraphQL `mutation`) can
//!   never be labelled a read — a mutation routed reader-only would spend a
//!   reader's budget on a doomed call and skip write sequencing.
//!
//! An argv the classifier cannot prove either way ([`ArgvAccess::Opaque`])
//! runs exactly as declared: the guard refuses only what it can prove, so a
//! future `gh` subcommand cannot be mis-blocked by an stale allowlist.
//!
//! The runtime guard is the hard boundary (it sees the assembled argv,
//! however a call site built it); `tests/script_helper_intent_guard.rs` is
//! the CI half — a source lint that fails on a mislabelled **literal** argv
//! at review time, plus a ban on the retired raw entry-point names.
//!
//! # Operations that stay on the writer
//!
//! None, through this module: a read whose answer depends on **who** asks
//! (the workflow-scope `gh api -i user` probe) is an asker-dependent probe —
//! it is built directly on the facade with
//! [`GhInvocation::writer_identity`] and listed, with its reason, in
//! `tests/writer_read_guard.rs` (`READ_ON_WRITER`). It cannot use
//! [`gh_read`]: `reader_only()` ignores a writer pin by construction.

use std::path::Path;

use crate::cmd_out::{decode_json, CmdOutcome, Query, Unavailable, DEFAULT_TIMEOUT};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// What an assembled `gh` argv provably does to the forge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgvAccess {
    /// Fetches state: a REST GET, a `… view` / `… list` porcelain, a GraphQL
    /// `query` document.
    Read,
    /// Mutates state: an explicit write method, request fields on a REST
    /// path, a mutating porcelain subcommand, a GraphQL `mutation`.
    Write,
    /// The classifier cannot prove either way; the call runs as declared.
    Opaque,
}

/// Classify an assembled `gh` argv (subcommand onward — never the program).
///
/// Proves only what is provable from the argv itself; the HTTP transport
/// alone never decides it (`gh api graphql` POSTs a `query` document, and
/// that is a read).
#[must_use]
pub fn argv_access(args: &[&str]) -> ArgvAccess {
    let Some((&sub, rest)) = args.split_first() else {
        return ArgvAccess::Opaque;
    };
    match sub {
        "api" => api_access(rest),
        "issue" | "pr" | "repo" | "release" | "label" => noun_access(sub, rest),
        _ => ArgvAccess::Opaque,
    }
}

/// `gh api …`: the method flag decides, else request fields imply POST, and
/// a `graphql` endpoint is classified by its document (`query`/`{` reads,
/// `mutation` writes; an invisible document stays [`ArgvAccess::Opaque`]).
fn api_access(rest: &[&str]) -> ArgvAccess {
    let mut method: Option<String> = None;
    let mut fields = false;
    let mut endpoint: Option<&str> = None;
    let mut query_doc: Option<&str> = None;
    let value_flags = [
        "-X",
        "--method",
        "-H",
        "--header",
        "-f",
        "-F",
        "--field",
        "--raw-field",
        "--input",
        "--hostname",
        "-t",
        "--template",
        "-q",
        "--jq",
        "--cache",
    ];
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i];
        if let Some(v) = a.strip_prefix("--method=") {
            method = Some(v.to_ascii_uppercase());
            i += 1;
            continue;
        }
        if let Some(v) = a.strip_prefix("-X") {
            if !v.is_empty() {
                method = Some(v.to_ascii_uppercase());
                i += 1;
                continue;
            }
        }
        if let Some(v) = a.strip_prefix("--field=") {
            fields = true;
            if let Some(doc) = v.strip_prefix("query=") {
                query_doc = Some(doc);
            }
            i += 1;
            continue;
        }
        if value_flags.contains(&a) {
            if matches!(a, "-f" | "-F" | "--field" | "--raw-field" | "--input") {
                fields = true;
                if let Some(v) = rest.get(i + 1) {
                    if let Some(doc) = v.strip_prefix("query=") {
                        query_doc = Some(doc);
                    }
                }
            }
            // A value flag consumes the next token; `-X POST` lands here too.
            if let Some(v) = rest.get(i + 1) {
                if a == "-X" || a == "--method" {
                    method = Some(v.to_ascii_uppercase());
                }
            }
            i += 2;
            continue;
        }
        // `--flag=value` self-contained; unknown flags and bare endpoints.
        if a.starts_with('-') {
            if a.contains('=') {
                i += 1;
                continue;
            }
            i += 1;
            continue;
        }
        if endpoint.is_none() {
            endpoint = Some(a);
        }
        i += 1;
    }
    if endpoint == Some("graphql") {
        return match query_doc.map(str::trim_start) {
            Some(doc) if doc.starts_with("mutation") => ArgvAccess::Write,
            Some(doc) if doc.starts_with("query") || doc.starts_with('{') => ArgvAccess::Read,
            _ => ArgvAccess::Opaque,
        };
    }
    match method.as_deref() {
        Some("GET" | "HEAD") => ArgvAccess::Read,
        Some(_) => ArgvAccess::Write,
        // `gh api` defaults to GET, or POST once fields are supplied.
        None if fields => ArgvAccess::Write,
        None => ArgvAccess::Read,
    }
}

/// The porcelain nouns this family uses. `view` / `list` (and `pr diff`,
/// `pr checks`, `pr status`, `release download`) read; the mutating
/// subcommands write; an unknown subcommand is [`ArgvAccess::Opaque`].
fn noun_access(noun: &str, rest: &[&str]) -> ArgvAccess {
    let Some((&verb, _)) = rest.split_first() else {
        return ArgvAccess::Opaque;
    };
    let reads = match noun {
        "issue" | "repo" | "label" => matches!(verb, "view" | "list"),
        "pr" => matches!(verb, "view" | "list" | "diff" | "checks" | "status"),
        "release" => matches!(verb, "view" | "list" | "download"),
        _ => return ArgvAccess::Opaque,
    };
    if reads {
        return ArgvAccess::Read;
    }
    let writes = matches!(
        verb,
        "edit"
            | "comment"
            | "create"
            | "close"
            | "reopen"
            | "merge"
            | "ready"
            | "lock"
            | "unlock"
            | "pin"
            | "unpin"
            | "review"
            | "delete"
            | "rename"
            | "archive"
            | "unarchive"
            | "upload"
            | "transfer"
    );
    if writes {
        ArgvAccess::Write
    } else {
        ArgvAccess::Opaque
    }
}

/// The typed target when the argv names one (`--repo owner/repo` on a
/// porcelain call): routing then asks the **target** owner's readers, not
/// the checkout's, which is what a cross-repo read (`dep_classify` walking
/// `other/repo#N` nodes) needs. A `{owner}/{repo}` placeholder is not a
/// target — `gh` resolves it from the cwd, so the derived route is the
/// honest one.
fn target_of(args: &[&str]) -> GhTarget {
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        let value = if let Some(v) = a.strip_prefix("--repo=") {
            Some(v)
        } else if a == "--repo" {
            args.get(i + 1).copied()
        } else {
            None
        };
        if let Some(v) = value {
            if !v.contains('{') {
                if let Ok(t) = GhTarget::repo(v) {
                    return t;
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    GhTarget::None
}

/// What a refusal hands back: nothing ran, so nothing had side effects, and
/// no quota was spent. "No answer", never a negative one — the same class as
/// a spawn failure, which every caller of this family already maps to
/// could-not-run.
fn refused(op: &str, intent: AccessIntent, shape: ArgvAccess) -> CmdOutcome {
    log::error!(
        "gh_route: {op} refused: an argv that is provably a {shape:?} was declared \
         {intent:?} (#13744); nothing was sent"
    );
    CmdOutcome::Unavailable(Unavailable::Spawn(format!(
        "gh_route refused {op}: argv is provably a {shape:?} but the call declared \
         {intent:?} (#13744)"
    )))
}

/// Build the facade invocation for a declared call, or refuse it.
fn guarded(
    op: &'static str,
    intent: AccessIntent,
    args: &[&str],
    repo_root: &Path,
    use_cache: bool,
) -> Result<GhInvocation, CmdOutcome> {
    match (intent, argv_access(args)) {
        (AccessIntent::Write, ArgvAccess::Read) => {
            return Err(refused(op, intent, ArgvAccess::Read))
        }
        (AccessIntent::Read, ArgvAccess::Write) => {
            return Err(refused(op, intent, ArgvAccess::Write));
        }
        _ => {}
    }
    let mut inv = GhInvocation::new(Operation::new(op), intent, target_of(args), DEFAULT_TIMEOUT)
        .current_dir(repo_root)
        .args(args);
    if use_cache {
        // Same selection as the raw helper: the repo's `gh-cached` read
        // wrapper when it exists and answers `--version`, else the resolver.
        inv = inv.program(super::gh_cmd(repo_root));
    }
    if intent == AccessIntent::Read {
        inv = inv.reader_only();
    }
    Ok(match forge_op_for(op) {
        Some(fop) => inv.forge_op(fop),
        None => inv,
    })
}

/// A read, routed: a reader App or the read gateway serves it, or it is
/// shed — never the ambient writer (#12555). `None` is deliberately absent
/// from this table's semantics: an unreadable answer stays
/// [`Query::Unavailable`] / a non-zero exit stays [`CmdOutcome::Ran`],
/// exactly as the raw helper classified them.
pub fn gh_read(op: &'static str, args: &[&str], repo_root: &Path, use_cache: bool) -> CmdOutcome {
    match guarded(op, AccessIntent::Read, args, repo_root, use_cache) {
        Ok(inv) => inv.run(),
        Err(out) => out,
    }
}

/// A JSON-decoding read ([`crate::cmd_out::decode_json`] over [`gh_read`]).
/// Callers pass `--json <fields>` and **no `--jq`**.
pub fn gh_read_json<T, F>(
    op: &'static str,
    args: &[&str],
    repo_root: &Path,
    use_cache: bool,
    is_empty: F,
) -> Query<T>
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(&T) -> bool,
{
    decode_json(gh_read(op, args, repo_root, use_cache), is_empty)
}

/// A write: the writer credential, through the facade's write sequencing.
pub fn gh_write(op: &'static str, args: &[&str], repo_root: &Path, use_cache: bool) -> CmdOutcome {
    match guarded(op, AccessIntent::Write, args, repo_root, use_cache) {
        Ok(inv) => inv.run(),
        Err(out) => out,
    }
}

/// The inventoried forge operation (#9831) a telemetry name built through
/// this module serves, so its `forge_call_stats` row is not `unknown`.
/// `None` is deliberate for the porcelain views whose entity kind is chosen
/// at runtime (`validate_phase.*` issue-or-PR reads) and the PR-list
/// searches — the inventory has no matching row, mirroring the `None` arms
/// of `claim_reconciliation::gh_call::forge_op_for`.
fn forge_op_for(op: &str) -> Option<crate::forge_call_stats::ForgeOp> {
    use crate::forge_call_stats::ops as inv;
    Some(match op {
        "trust.comment_listing" | "premise.graphql_comments" => inv::COMMENT_LIST,
        "trust.issue_object"
        | "dep.issue_view"
        | "dep.node_view"
        | "recheck.issue_body"
        | "recheck.body_comments"
        | "premise.issue_view"
        | "champion_hold.labels"
        | "verdict.labels" => inv::ISSUE_VIEW_STATE,
        "recheck.pr_view" | "verdict.handback_read" => inv::PR_VIEW_STATE,
        "recheck.issue_view" | "notify.close_targets" => inv::ISSUE_CLOSED_BY_PULL_REQUESTS,
        "latency.timeline" => inv::TIMELINE_READ,
        "champion_hold.ratify_files" | "validate_phase.pr_files" => inv::PR_DIFF_AND_FILES,
        "curator.repo_view" | "dep.repo_view" | "premise.repo_view" => inv::REPO_VIEW,
        "validate_phase.fail_labels"
        | "dep.add_labels"
        | "dep.remove_label"
        | "verdict.add_labels"
        | "verdict.remove_label"
        | "verdict.handback_add"
        | "verdict.handback_remove"
        | "notify.unpark"
        | "curator.release_claim" => inv::ISSUE_EDIT_LABELS,
        "validate_phase.fail_comment" | "dep.comment" | "notify.comment" => inv::COMMENT_CREATE,
        "dep.edit_body" => inv::ISSUE_EDIT_BODY,
        "verdict.withdraw_comment" => inv::COMMENT_EDIT_DELETE,
        _ => return None,
    })
}

#[cfg(test)]
#[path = "gh_route_tests.rs"]
mod tests;

/// A read invocation exactly as [`gh_read`] builds it, for the
/// reader-chain proof in `gh_invocation/reader_only_tests.rs` — the same
/// chain the helper's own calls ride in production.
#[cfg(test)]
pub(crate) fn test_read_invocation(
    op: &'static str,
    args: &[&str],
    repo_root: &Path,
) -> GhInvocation {
    match guarded(op, AccessIntent::Read, args, repo_root, false) {
        Ok(inv) => inv,
        Err(out) => panic!("a correctly labelled read does not refuse: {out:?}"),
    }
}

/// A write invocation exactly as [`gh_write`] builds it (tests).
#[cfg(test)]
pub(crate) fn test_write_invocation(
    op: &'static str,
    args: &[&str],
    repo_root: &Path,
) -> GhInvocation {
    match guarded(op, AccessIntent::Write, args, repo_root, false) {
        Ok(inv) => inv,
        Err(out) => panic!("a correctly labelled write does not refuse: {out:?}"),
    }
}
