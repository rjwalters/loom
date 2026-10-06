//! One forge-call ledger row per agent `gh` passthrough (W5 of the forge API
//! reduction plan).
//!
//! The daemon's own `gh` calls are booked by the facade
//! ([`crate::gh_invocation::accounting`]); the reads this front serves from
//! the ETag cache are booked by the modules that serve them. Everything else
//! an agent session runs — every mutation, every `gh api`, every shape the
//! front does not serve — was exec'd with no row at all, so that spend showed
//! up only as the gap between the ledger and the `x-ratelimit-used` header.
//!
//! [`book`] closes it: one row, written **before** the exec (the exec
//! replaces this process, so there is no "after"). Because of that the row
//! records intent, not outcome — it is booked `ok`, i.e. charged, and a
//! `--paginate` call is flagged pages-unknown.
//!
//! What a row carries, all of it coarse and none of it an argument value
//! beyond the repository slug the facade's rows already carry:
//!
//! - `caller`: `agent.gh.<command>` from a fixed table ([`caller_of`]) —
//!   never the argv;
//! - the billed resource, from the same argv classifier the facade uses
//!   ([`crate::gh_invocation::accounting::static_pool`]): `gh api` is `core`
//!   unless it is `api graphql` or a search path, `gh issue|pr|repo|project`
//!   are `graphql`;
//! - the role, as `agent-<LOOM_ROLE>` (`agent-session` outside a role), in
//!   the row's role field — so `forge calls --by role` lists agent spend
//!   beside the daemon's `reader` / `writer` rows, on older binaries too;
//! - the credential: the session's `GH_CONFIG_DIR` (or env token) read by
//!   path shape exactly as the facade reads its own
//!   ([`crate::gh_invocation::accounting::cred_of_with`]).
//!
//! Booking is local I/O only and can never fail or hold up the agent's
//! call: one sink append, and — only when neither the argv nor `GH_REPO`
//! names the repository — one local `git remote get-url origin`, killed
//! after [`REMOTE_BUDGET`] ([`bounded_remote`]). This process is about to be
//! replaced by the real `gh`, so nothing is memoised and nothing may wait: a
//! `git` that does not answer in time leaves the row without a repository,
//! never the agent without its call. The sink swallows its own errors and a
//! panic here is caught.
//!
//! The row goes to the **host** sink, not the session's: the spawner exports
//! the directory it resolved as `LOOM_FORGE_CALL_STATS_DIR`
//! ([`crate::agent_session::isolation`]), because the session's private
//! `TMPDIR` would otherwise move the default somewhere no rollup reads.
//!
//! A call the facade already booked ([`crate::gh_invocation::BOOKED_ENV`] —
//! a `loom-daemon` command run inside the session, whose `gh` resolves to
//! this front) is not booked again; the spawner blanks that marker in the
//! session's own environment, so only a facade child ever carries it. Commands that never reach the API
//! (`gh auth` — the git credential helper —, `gh config`, help, version,
//! completion, aliases, extensions) are not booked.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::classify::is_slug;
use crate::forge_call_stats::{self, CallAttribution, CallIdentity, Outcome, Pool};
use crate::gh_invocation::accounting::{self, CredAttr};

/// The role label of a session with no `LOOM_ROLE`.
const NO_ROLE: &str = "session";

/// Longest role kept in a label.
const ROLE_MAX: usize = 32;

/// Longest the front waits for the local `git` read of `origin`. A healthy
/// read takes a few milliseconds.
const REMOTE_BUDGET: Duration = Duration::from_millis(500);

/// `cwd`'s `origin` remote as `owner/repo`, read with `git` under `budget`:
/// the child (and its process group) is killed at the deadline and the
/// answer is `None`. Local only — never a forge call.
fn bounded_remote(git: &str, cwd: &Path, budget: Duration) -> Option<String> {
    let mut cmd = Command::new(git);
    cmd.args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    match crate::proc_exec::run_bounded(cmd, budget) {
        Ok(crate::proc_exec::Completion::Exited(out)) if out.status.success() => {
            crate::forge_etag_store::parse_remote_url(&String::from_utf8_lossy(&out.stdout))
                .map(|(_, nwo)| nwo)
        }
        _ => None,
    }
}

/// The ledger `caller` for a `gh <command> …`, or `None` for a command that
/// never spends API budget. Fixed names only: an unlisted command (an alias,
/// an extension) is `agent.gh.other`.
#[must_use]
pub fn caller_of(command: &str) -> Option<&'static str> {
    Some(match command {
        "help" | "version" | "--version" | "--help" | "-h" | "completion" | "config" | "alias"
        | "extension" | "auth" => return None,
        "api" => "agent.gh.api",
        "issue" => "agent.gh.issue",
        "pr" => "agent.gh.pr",
        "repo" => "agent.gh.repo",
        "project" => "agent.gh.project",
        "search" => "agent.gh.search",
        "release" => "agent.gh.release",
        "run" => "agent.gh.run",
        "workflow" => "agent.gh.workflow",
        "label" => "agent.gh.label",
        "secret" => "agent.gh.secret",
        "variable" => "agent.gh.variable",
        "cache" => "agent.gh.cache",
        "gist" => "agent.gh.gist",
        "status" => "agent.gh.status",
        "browse" => "agent.gh.browse",
        "ruleset" => "agent.gh.ruleset",
        "org" => "agent.gh.org",
        _ => "agent.gh.other",
    })
}

/// `agent-<role>`: the role lowercased and reduced to `[a-z0-9_-]`,
/// `agent-session` when there is none.
#[must_use]
pub fn role_label(role: Option<&str>) -> String {
    let kept: String = role
        .unwrap_or_default()
        .trim()
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
        .take(ROLE_MAX)
        .collect();
    format!("agent-{}", if kept.is_empty() { NO_ROLE } else { &kept })
}

/// The value of `-R` / `--repo`, as written (it may not be a plain slug).
fn repo_flag(words: &[String]) -> Option<String> {
    words.iter().enumerate().find_map(|(i, a)| {
        if a == "-R" || a == "--repo" {
            words.get(i + 1).cloned()
        } else {
            a.strip_prefix("--repo=").map(str::to_string)
        }
    })
}

/// The `owner/repo` of a `gh api repos/<owner>/<repo>/…` path, when it is a
/// literal slug (not gh's `{owner}/{repo}` placeholder).
fn repo_in_api_path(words: &[String]) -> Option<String> {
    if words.first().map(String::as_str) != Some("api") {
        return None;
    }
    words.iter().skip(1).find_map(|a| {
        let rest = a.trim_start_matches('/').strip_prefix("repos/")?;
        let mut parts = rest.split(['/', '?']);
        let slug = format!("{}/{}", parts.next()?, parts.next()?);
        is_slug(&slug).then_some(slug)
    })
}

/// What the front knows about the session, read once per call.
#[derive(Debug, Clone, Default)]
pub struct Session {
    /// `LOOM_ROLE`.
    pub role: Option<String>,
    /// `GH_HOST`.
    pub host: Option<String>,
    /// `GH_REPO`.
    pub gh_repo: Option<String>,
}

/// One row, ready to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub caller: &'static str,
    pub identity: CallIdentity,
    pub pool: Pool,
    pub attribution: CallAttribution,
}

/// The row for one passthrough, or `None` for a call that is not booked.
/// Pure but for `remote`, which is only asked when the argv and `GH_REPO`
/// name no repository.
#[must_use]
pub fn plan(
    args: &[OsString],
    session: &Session,
    cred: &CredAttr,
    remote: impl FnOnce() -> Option<String>,
) -> Option<Row> {
    let words: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let caller = caller_of(words.first()?)?;
    if words.iter().any(|a| a == "--help" || a == "-h") {
        return None;
    }
    let pool = accounting::static_pool(args);
    let (pg, pu) = accounting::pages(args, false, None);
    // A `--repo` gh resolves some other way (a URL, `HOST/OWNER/REPO`) names
    // a repo this row cannot: it is left unattributed, never guessed from
    // the checkout.
    let (repo, origin) = if let Some(flag) = repo_flag(&words) {
        match Some(flag).filter(|r| is_slug(r)) {
            Some(repo) => (Some(repo), "target"),
            None => (None, "none"),
        }
    } else if let Some(repo) =
        repo_in_api_path(&words).or_else(|| session.gh_repo.clone().filter(|r| is_slug(r)))
    {
        (Some(repo), "target")
    } else if let Some(repo) = remote() {
        (Some(repo), "remote")
    } else {
        (None, "none")
    };
    let host = session.host.as_deref().filter(|h| !h.trim().is_empty());
    let mut identity = CallIdentity::default()
        .with_provider("github")
        .with_origin(host.unwrap_or("github.com"))
        .with_role(&role_label(session.role.as_deref()));
    if let Some(repo) = &repo {
        identity = identity.with_repo(repo);
    }
    let attribution = CallAttribution {
        ro: Some(origin.to_string()),
        ca: forge_call_stats::sanitize(&cred.account),
        co: cred.owner.as_deref().and_then(forge_call_stats::sanitize),
        tk: Some(cred.kind.to_string()),
        rr: Some(pool.as_str().to_string()),
        pg,
        pu,
        rd: None,
        // `gh api rate_limit` is a request GitHub does not charge.
        fr: (caller == "agent.gh.api" && pool == Pool::Other).then_some(true),
    };
    Some(Row {
        caller,
        identity,
        pool,
        attribution,
    })
}

/// Book the passthrough of `args` (see the module docs). Never fails, never
/// panics into the caller.
pub fn book(args: &[OsString], cwd: Option<&Path>) {
    let _ = std::panic::catch_unwind(|| {
        if std::env::var_os(crate::gh_invocation::BOOKED_ENV).is_some_and(|v| v == "1") {
            return;
        }
        let var = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let session = Session {
            role: var("LOOM_ROLE"),
            host: var("GH_HOST"),
            gh_repo: var("GH_REPO"),
        };
        let dir = std::env::var_os("GH_CONFIG_DIR")
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from);
        let env_token = ["GH_TOKEN", "GITHUB_TOKEN"]
            .iter()
            .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
        let cred = accounting::cred_of_with(dir.as_deref(), env_token);
        let remote = || cwd.and_then(|dir| bounded_remote("git", dir, REMOTE_BUDGET));
        if let Some(row) = plan(args, &session, &cred, remote) {
            forge_call_stats::record_attributed(
                row.caller,
                &row.identity,
                row.pool,
                Outcome::Ok,
                None,
                &row.attribution,
            );
        }
    });
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
