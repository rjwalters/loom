//! The closed-issues candidate fetch for `loom-daemon duplicate-scan`
//! (#9208): ONE time-bounded REST issue search, selected by relevance,
//! instead of `check-duplicate.sh`'s fixed recency count.
//!
//! # Why
//!
//! The closed pool used to be "the 20 most recently *created* closed issues"
//! — about one day of this repo's history — chosen before any similarity
//! scoring. A duplicate closed five weeks earlier (#6808, re-filed as #9167)
//! had long since scrolled out of it. Widening the count only moves the cliff
//! and pulls more full bodies per call; the pool has to be selected by
//! relevance inside a time bound, then scored exactly as before.
//!
//! # What this module does, and only this
//!
//! It turns the query's own keywords into one search request, parses the
//! answer into [`Candidate`]s, and hands them to the unchanged scan. The
//! search result is a candidate *set*, never a verdict: `--threshold` still
//! decides what is reported.
//!
//! # The query is built from keyword tokens only
//!
//! Issue titles and bodies are untrusted forge text. The search string is
//! assembled from the fixed qualifiers below plus tokens that survived
//! [`extract_keywords`] — lowercase ASCII alphanumerics, three characters or
//! longer — and [`build_query`] re-checks that shape itself rather than
//! trusting its caller. Raw title or body text never reaches the query, so a
//! title carrying `repo:other/thing`, a quote, `is:open` or an upper-case
//! boolean operator cannot change what is searched.
//!
//! # Cost
//!
//! The search bucket is 30 requests a minute, shared by everything on the
//! host's identity. So: one request per invocation, first page only
//! ([`PER_PAGE`]), pinned to one credential (a reader-then-writer retry would
//! be a second search), no sleep and no retry. When the search cannot answer,
//! the caller falls back to the recency list it used before.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::duplicate_scan::{extract_keywords, Candidate};

/// GitHub search allows at most five `AND`/`OR`/`NOT` operators in one query,
/// so six `OR`-joined terms is the ceiling.
pub(crate) const MAX_TERMS: usize = 6;

/// GitHub search rejects a query longer than this.
pub(crate) const MAX_QUERY_CHARS: usize = 256;

/// One page, no pagination. Every item carries its full body, so the page is
/// held to 50 (the compile-time check below) rather than the endpoint's 100.
pub(crate) const PER_PAGE: usize = 50;
const _: () = assert!(PER_PAGE <= 50);

/// `duplicate-scan` exits with this when the windowed search could not
/// answer: not a verdict (0/1) and not "could not run" (2). The caller falls
/// back to its recency list.
pub(crate) const EXIT_SEARCH_UNAVAILABLE: i32 = 3;

/// The deadline for the one search request.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Is `token` one [`extract_keywords`] could have produced? Checked again at
/// the point the query is assembled, so the "keywords only" property does not
/// rest on every future caller passing the right thing.
fn is_keyword_token(token: &str) -> bool {
    token.len() >= 3
        && token
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// `owner/name` with nothing a search qualifier could be smuggled through.
fn is_plain_slug(slug: &str) -> bool {
    let ok = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    matches!(slug.split_once('/'), Some((owner, name)) if ok(owner) && ok(name))
}

/// The search terms for a query: the title's keywords first, in the order
/// they appear in the title, then the body's — deduplicated and capped at
/// [`MAX_TERMS`].
///
/// Title first because a title is short and specific while a body is long and
/// full of subsystem jargon (the same reasoning as the scan's title
/// corroboration, #8591). Order of appearance rather than the alphabetical
/// order [`extract_keywords`] returns, so which six terms survive the cap is
/// decided by the text and not by the alphabet.
#[must_use]
pub(crate) fn search_terms(title: &str, body: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for text in [title, body] {
        // Membership in `extract_keywords`' output is the filter: the walk
        // below only restores the order that function sorts away.
        let keywords = extract_keywords(text);
        let lowered = text.to_lowercase();
        for word in lowered.split(|c: char| !c.is_ascii_alphanumeric()) {
            if terms.len() == MAX_TERMS {
                return terms;
            }
            let is_keyword = keywords.binary_search_by(|k| k.as_str().cmp(word)).is_ok();
            if is_keyword && !terms.iter().any(|t| t == word) {
                terms.push(word.to_string());
            }
        }
    }
    terms
}

/// The search query: fixed qualifiers plus `OR`-joined `terms`, or `None`
/// when there is nothing safe to search for (no usable term, or a repo slug
/// that is not a plain `owner/name`).
///
/// At most [`MAX_TERMS`] terms are used and the whole query stays within
/// [`MAX_QUERY_CHARS`]: a term that would overflow it is dropped, along with
/// every term after it.
#[must_use]
pub(crate) fn build_query(slug: &str, closed_since: &str, terms: &[String]) -> Option<String> {
    if !is_plain_slug(slug) {
        return None;
    }
    let mut query = format!("repo:{slug} is:issue is:closed closed:>={closed_since}");
    let mut used = 0usize;
    for term in terms.iter().filter(|t| is_keyword_token(t)).take(MAX_TERMS) {
        let joiner = if used == 0 { " " } else { " OR " };
        if query.len() + joiner.len() + term.len() > MAX_QUERY_CHARS {
            break;
        }
        query.push_str(joiner);
        query.push_str(term);
        used += 1;
    }
    (used > 0).then_some(query)
}

/// The window's lower bound as the `YYYY-MM-DD` a `closed:>=` qualifier
/// takes: `days` before `now`, in UTC.
#[must_use]
pub(crate) fn closed_since(now: DateTime<Utc>, days: u32) -> String {
    (now - chrono::Duration::days(i64::from(days)))
        .format("%Y-%m-%d")
        .to_string()
}

/// Percent-encode a query value (unreserved characters pass through).
fn url_encode(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The REST path for `query`: first page only, [`PER_PAGE`] items.
#[must_use]
pub(crate) fn search_path(query: &str) -> String {
    format!("search/issues?q={}&per_page={PER_PAGE}", url_encode(query))
}

/// The candidates in a search answer: its `items`, with pull requests
/// dropped (the issue search endpoint can return them) and a `null` body
/// read as empty. An item with no usable number is skipped, as the scan's
/// own pool parse skips it.
///
/// # Errors
/// The answer is not a JSON object carrying an `items` array — which is "the
/// search did not answer", never "the search found nothing".
pub(crate) fn parse_items(body: &str) -> Result<Vec<Candidate>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("the answer was not JSON ({e})"))?;
    let items = value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "the answer carried no `items` array".to_string())?;
    let text = |item: &serde_json::Value, key: &str| {
        item.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok(items
        .iter()
        .filter(|item| {
            item.get("pull_request")
                .is_none_or(serde_json::Value::is_null)
        })
        .filter_map(|item| {
            Some(Candidate {
                number: item.get("number")?.as_u64()?,
                title: text(item, "title"),
                body: text(item, "body"),
            })
        })
        .collect())
}

/// `LOOM_REPO` when set, else `cwd`'s `origin` remote — a local read, no
/// forge call.
fn repo_slug(cwd: &Path) -> Option<String> {
    std::env::var("LOOM_REPO")
        .ok()
        .filter(|r| !r.trim().is_empty())
        .or_else(|| loom_daemon::credential_preflight::nwo_from_git_remote(cwd))
}

/// Run the one search request for `path`, recorded under the inventoried
/// `issue.search` operation.
///
/// Pinned to the writer credential: the facade's reader-then-writer shape
/// would answer a refused reader with a second search on the writer, and the
/// contract here is one request.
fn run_search(cwd: &Path, slug: &str, path: &str) -> Result<String, String> {
    use loom_daemon::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    let outcome = GhInvocation::new(
        Operation::new("duplicate_scan.closed_search"),
        AccessIntent::Read,
        GhTarget::None,
        SEARCH_TIMEOUT,
    )
    .forge_op(loom_daemon::forge_call_stats::ops::ISSUE_SEARCH)
    .identity_scope(None, Some(slug))
    .writer_identity()
    .current_dir(cwd)
    .args(["api", path])
    .run();
    match outcome {
        loom_daemon::cmd_out::CmdOutcome::Ran(out) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        loom_daemon::cmd_out::CmdOutcome::Ran(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // One line is enough to name a 403/429; the rest of a forge error
            // body is noise in a warning.
            let first = stderr.lines().next().unwrap_or_default().trim();
            Err(format!("the forge refused the search: {first}"))
        }
        loom_daemon::cmd_out::CmdOutcome::Unavailable(why) => {
            Err(format!("the search did not complete: {why}"))
        }
    }
}

/// What the windowed fetch produced.
#[derive(Debug)]
pub(crate) enum ClosedSearch {
    /// The search answered: these are the closed pool (possibly empty).
    Candidates(Vec<Candidate>),
    /// The query has no keywords, so there is nothing to search for and no
    /// request was made. The scan would bail on the same condition.
    NoKeywords,
    /// The search could not answer, for the reason given. The caller falls
    /// back to the recency list.
    Unavailable(String),
}

/// Fetch the closed-issues pool for `title`/`body`: issues closed within
/// `days` of `now` that the forge's search ranks against the query's
/// keywords. At most one forge request, and none at all when the query has
/// no keywords or the forge is not GitHub.
#[must_use]
pub(crate) fn fetch(
    cwd: &Path,
    title: &str,
    body: &str,
    days: u32,
    now: DateTime<Utc>,
) -> ClosedSearch {
    let terms = search_terms(title, body);
    if terms.is_empty() {
        // Decided before anything touches git or the forge.
        return ClosedSearch::NoKeywords;
    }
    let github = loom_daemon::forge_cmd::detect_forge(Some(cwd))
        == loom_daemon::forge_cmd::ForgeType::GitHub;
    let slug = repo_slug(cwd);
    let since = closed_since(now, days);
    select(&terms, github, slug.as_deref(), &since, |slug, path| {
        run_search(cwd, slug, path)
    })
}

/// [`fetch`] with the environment resolved and the request injected: the
/// decision of whether to search at all, and what a failed search becomes.
/// `search` is called at most once.
pub(crate) fn select(
    terms: &[String],
    github: bool,
    slug: Option<&str>,
    closed_since: &str,
    search: impl FnOnce(&str, &str) -> Result<String, String>,
) -> ClosedSearch {
    if terms.is_empty() {
        return ClosedSearch::NoKeywords;
    }
    if !github {
        return ClosedSearch::Unavailable("this forge has no GitHub issue search".to_string());
    }
    let Some(slug) = slug else {
        return ClosedSearch::Unavailable("the repository could not be resolved".to_string());
    };
    let Some(query) = build_query(slug, closed_since, terms) else {
        return ClosedSearch::Unavailable(format!(
            "no safe query could be built for repository `{slug}`"
        ));
    };
    match search(slug, &search_path(&query)).and_then(|answer| parse_items(&answer)) {
        Ok(candidates) => ClosedSearch::Candidates(candidates),
        Err(why) => ClosedSearch::Unavailable(why),
    }
}

#[cfg(test)]
#[path = "duplicate_closed_search_tests.rs"]
mod tests;
