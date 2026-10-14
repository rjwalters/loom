//! The REST page walk: one ledger row per page of a `gh api --paginate` read
//! (W5 of the forge API reduction plan).
//!
//! # Why
//!
//! `gh api --paginate` is one process and — before this — one ledger row,
//! but GitHub charges one request per page. The row could only say "pages
//! unknown" (`pu`), so the per-bucket "charged" figure was a lower bound.
//!
//! Asking the same process for `--include` does not fix that safely: the
//! header blocks of every page land in the same stdout as the bodies, and a
//! `--jq` body is arbitrary text (a comment body, a file name), so a line that
//! merely *looks* like an HTTP status line could not be told from a real one —
//! stripping it would let forge content edit what a gating read returns.
//!
//! So the facade walks the pages itself. Each page is its own
//! `gh api <url> --include` execution: its stdout starts with exactly one
//! header block, ended by the first blank line, so the split never depends on
//! the body. Each page is a normal facade execution — its own `invoke github`
//! span and its own accounting row, with the response's status and the free
//! `x-ratelimit-*` headers — and the caller gets back one completion whose
//! stdout is what `gh api --paginate` would have printed:
//!
//! - with `--jq`: each page's filtered output, concatenated (`gh` runs the
//!   filter once per page);
//! - without: one JSON array when every page is an array (`gh` merges them),
//!   else the bodies concatenated.
//!
//! The first failing page ends the walk with that page's status and stderr,
//! as `gh` does. The caller's deadline bounds the whole walk.
//!
//! # Off by default
//!
//! The walk is **opt-in**: it runs only when `LOOM_GH_PAGE_WALK` is exactly
//! `1` ([`walk_enabled`]). Unset, or any other value, every call is the
//! single `--paginate` execution it always was (booked `pu`). It changes how
//! reads that gate decisions are fetched, so it is validated on one host
//! before it is the default: turn it on there and compare, per site, the
//! `gh` spawn count (the `invoke github` spans per operation) and the
//! `x-ratelimit-used` delta of each bucket against what the ledger charged
//! (`loom-daemon forge calls --by caller|bucket`). With the walk on, each
//! page's free `x-ratelimit-*` reading also feeds
//! [`crate::forge_bucket_book::observe`], as any `--include` response does.
//!
//! # A walk never looks complete when it is not
//!
//! The merged stdout is returned as a success only when every page was read
//! and the last one named no next page. Otherwise:
//!
//! - a successful page with no header block (stdout not starting `HTTP/`),
//!   or a `rel="next"` link that is not `https://`, is an error — the
//!   listing's end cannot be told, so page 1 is never passed off as all of
//!   it;
//! - a deadline that expires mid-walk is `TimedOut` with what a killed
//!   `gh api --paginate` leaves behind: a merged array that is opened and
//!   never closed (or the per-page `--jq` output so far) — never a
//!   well-formed array;
//! - a walk that passes [`MAX_PAGES`] is an error.
//!
//! # Page size
//!
//! `gh` adds `per_page=100` to a REST request only under `--paginate`, which
//! the page argv no longer carries. Page 1 therefore adds it exactly as `gh`
//! does ([`with_per_page`]): unless the endpoint's query already sets one.
//! Later pages follow the `rel="next"` URL, which carries it.
//!
//! # What is walked
//!
//! Only an argv this module fully understands ([`plan`]): a captured
//! `gh api <endpoint> --paginate` with nothing but `--jq`, header, preview
//! and hostname flags. GraphQL (cursor pagination), a request with fields or
//! a body, an explicit method, `--slurp`, a template, an argv that already
//! asks for `--include`, and any flag not named here keep the single
//! `--paginate` execution exactly as before (booked `pu`, or counted from
//! its own status blocks when it passed `--include`).

use super::{GhInvocation, OutputContract};
use crate::proc_exec::{Completion, ExecError};
use std::ffi::OsString;
use std::process::Output;
use std::time::{Duration, Instant};

/// The opt-in switch: exactly `1` enables the walk (see [`walk_enabled`]).
pub const WALK_ENV: &str = "LOOM_GH_PAGE_WALK";

/// `gh`'s own page size under `--paginate`.
const PER_PAGE: u32 = 100;

/// Whether `value` (the [`WALK_ENV`] setting) turns the walk on: exactly
/// `1`. Unset, empty, `0`, `true`, ` 1` — everything else — is off.
#[must_use]
pub fn walk_enabled(value: Option<&str>) -> bool {
    value == Some("1")
}

#[cfg(not(test))]
fn switch_on() -> bool {
    walk_enabled(std::env::var(WALK_ENV).ok().as_deref())
}

/// Test builds never read the process environment: the walk is off unless
/// the current test thread opts in ([`set_test_walk`]).
#[cfg(test)]
fn switch_on() -> bool {
    walk_enabled(TEST_WALK.with(|v| v.borrow().clone()).as_deref())
}

#[cfg(test)]
thread_local! {
    static TEST_WALK: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Set THIS test thread's [`WALK_ENV`] value (`None` = unset, i.e. off).
#[cfg(test)]
pub(super) fn set_test_walk(value: Option<&str>) {
    TEST_WALK.with(|v| *v.borrow_mut() = value.map(str::to_string));
}

/// A walk longer than this is a `Link` loop, not a listing.
const MAX_PAGES: usize = 1000;

/// Flags the walk carries to every page, each with its value.
const CARRIED_VALUED: &[&str] = &[
    "--jq",
    "-q",
    "-H",
    "--header",
    "-p",
    "--preview",
    "--hostname",
];

/// The index of the endpoint in `inv`'s argv when the facade walks this
/// call's pages itself; `None` runs it as the single execution it always was.
pub(super) fn plan(inv: &GhInvocation) -> Option<usize> {
    if !matches!(inv.contract, OutputContract::Captured { .. }) {
        return None;
    }
    if !switch_on() {
        return None;
    }
    walkable_endpoint(&inv.args)
}

/// Pure core of [`plan`]: `gh api <endpoint> --paginate` plus only
/// [`CARRIED_VALUED`] flags, where the endpoint is not `graphql`.
pub(super) fn walkable_endpoint(args: &[OsString]) -> Option<usize> {
    let words: Vec<&str> = args
        .iter()
        .map(|a| a.to_str())
        .collect::<Option<Vec<_>>>()?;
    if words.first() != Some(&"api") {
        return None;
    }
    let (mut endpoint, mut paginate) = (None, false);
    let mut i = 1;
    while i < words.len() {
        let word = words[i];
        if word == "--paginate" {
            paginate = true;
        } else if CARRIED_VALUED.contains(&word) {
            i += 1;
            words.get(i)?;
        } else if word.starts_with('-') || endpoint.is_some() {
            return None;
        } else {
            endpoint = Some(i);
        }
        i += 1;
    }
    let endpoint = endpoint.filter(|_| paginate)?;
    (words[endpoint].trim_start_matches('/') != "graphql").then_some(endpoint)
}

/// `endpoint` with `per_page=100` added the way `gh api --paginate` adds it
/// (its `addPerPage`): `?per_page=100`, or `&per_page=100` after an existing
/// query — and nothing when the query already sets a `per_page`.
fn with_per_page(endpoint: &str) -> String {
    let Some((_, query)) = endpoint.split_once('?') else {
        return format!("{endpoint}?per_page={PER_PAGE}");
    };
    let already = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .any(|(key, value)| key == "per_page" && !value.is_empty());
    if already {
        endpoint.to_string()
    } else {
        format!("{endpoint}&per_page={PER_PAGE}")
    }
}

/// The argv of one page: `--paginate` becomes `--include` where it stood —
/// so every other argument keeps the position the site gave it. Page 1
/// (`next` is `None`) keeps the site's endpoint with `gh`'s page size
/// ([`with_per_page`]); a later page's endpoint is `next`, the previous
/// page's `rel="next"` URL, untouched.
fn page_args(args: &[OsString], endpoint: usize, next: Option<&str>) -> Vec<OsString> {
    args.iter()
        .enumerate()
        .map(|(i, arg)| match next {
            _ if arg == "--paginate" => OsString::from("--include"),
            Some(url) if i == endpoint => OsString::from(url),
            None if i == endpoint => arg
                .to_str()
                .map_or_else(|| arg.clone(), |path| OsString::from(with_per_page(path))),
            _ => arg.clone(),
        })
        .collect()
}

/// One page's stdout, split at the end of its header block. A stdout that
/// does not start with a status line has no header block (`gh` failed before
/// the request): it is all body, and [`walk`] refuses it on a successful
/// page.
fn split(stdout: &[u8]) -> (Option<&[u8]>, &[u8]) {
    if !stdout.starts_with(b"HTTP/") {
        return (None, stdout);
    }
    let find = |sep: &[u8]| {
        stdout
            .windows(sep.len())
            .position(|w| w == sep)
            .map(|at| (at, at + sep.len()))
    };
    // The earliest blank line wins, so a CRLF header block is never split
    // at a later LF-only blank line inside the body.
    match [find(b"\r\n\r\n"), find(b"\n\n")]
        .into_iter()
        .flatten()
        .min()
    {
        Some((head_end, body_start)) => (Some(&stdout[..head_end]), &stdout[body_start..]),
        None => (Some(stdout), &[]),
    }
}

/// What a header block says about the page after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Next {
    /// No `rel="next"`: this was the last page.
    End,
    /// The `https://` URL of the next page.
    Url(String),
    /// A `rel="next"` this walk will not follow (not `https://`, or a
    /// `Link` value it cannot read). There may be more pages: the listing
    /// is NOT known to be complete.
    Unfollowable,
}

/// The `rel="next"` of a header block's `Link` header. Only an `https://`
/// URL is followed; anything else that names a next page is
/// [`Next::Unfollowable`], never [`Next::End`].
pub(super) fn next_link(head: &[u8]) -> Next {
    let head = String::from_utf8_lossy(head);
    let Some(value) = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("link").then_some(value)
    }) else {
        return Next::End;
    };
    let mut rest = value;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else {
            return Next::Unfollowable;
        };
        let url = &after[..close];
        let params = &after[close + 1..];
        let params_end = params.find('<').unwrap_or(params.len());
        if params[..params_end].contains(r#"rel="next""#) {
            return if url.starts_with("https://") {
                Next::Url(url.to_string())
            } else {
                Next::Unfollowable
            };
        }
        rest = &params[params_end..];
    }
    Next::End
}

/// The elements of every page when each body is a JSON array (`None` when
/// one is not), joined with commas and with no enclosing brackets.
fn array_elements(bodies: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for body in bodies {
        let inner = body
            .trim_ascii()
            .strip_prefix(b"[")?
            .strip_suffix(b"]")?
            .trim_ascii();
        if !inner.is_empty() {
            if !out.is_empty() {
                out.push(b',');
            }
            out.extend_from_slice(inner);
        }
    }
    Some(out)
}

/// What `gh api --paginate` prints for these page bodies (see the module
/// docs). A single page is returned untouched.
fn join(bodies: &[Vec<u8>], filtered: bool) -> Vec<u8> {
    if let [only] = bodies {
        return only.clone();
    }
    match array_elements(bodies).filter(|_| !filtered) {
        Some(elements) => [b"[".as_slice(), &elements, b"]".as_slice()].concat(),
        None => bodies.concat(),
    }
}

/// What a `gh api --paginate` killed at its deadline leaves on stdout after
/// these complete pages: with `--jq`, each page's output so far; without,
/// the merged array opened and never closed. Never a well-formed listing —
/// when the pages are not arrays there is nothing safe to print, so nothing
/// is.
fn cut_short(bodies: &[Vec<u8>], filtered: bool) -> Vec<u8> {
    if filtered {
        return bodies.concat();
    }
    match array_elements(bodies).filter(|_| !bodies.is_empty()) {
        Some(elements) => [b"[".as_slice(), &elements].concat(),
        None => Vec::new(),
    }
}

fn incomplete(why: &str) -> ExecError {
    ExecError::Collect(std::io::Error::other(format!(
        "REST page walk: {why}; the listing is incomplete"
    )))
}

/// Walk `inv`'s pages, running each through `run_page` (one ordinary facade
/// execution: span, accounting row, credential), and return the completion
/// the single `--paginate` execution would have produced.
///
/// # Errors
///
/// A page's own [`ExecError`]; and [`ExecError::Collect`] when the listing
/// cannot be known to be complete — a successful page without a header
/// block, a next link that is not followed, or more than [`MAX_PAGES`]
/// pages. A deadline is `Ok(Completion::TimedOut)` with [`cut_short`]
/// stdout.
pub(super) fn walk(
    inv: &GhInvocation,
    endpoint: usize,
    timeout: Duration,
    mut run_page: impl FnMut(GhInvocation) -> Result<Completion, ExecError>,
) -> Result<Completion, ExecError> {
    let started = Instant::now();
    let filtered = inv.args.iter().any(|a| a == "--jq" || a == "-q");
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut next: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let Some(left) = timeout
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
        else {
            return Ok(Completion::TimedOut {
                stdout: cut_short(&bodies, filtered),
                stderr,
            });
        };
        let mut page = inv.clone();
        page.args = page_args(&inv.args, endpoint, next.as_deref());
        page.contract = OutputContract::Captured { timeout: left };
        match run_page(page)? {
            // The killed page's partial stdout is dropped: only whole pages
            // are ever part of what the caller sees.
            Completion::TimedOut {
                stdout: _,
                stderr: page_err,
            } => {
                stderr.extend_from_slice(&page_err);
                return Ok(Completion::TimedOut {
                    stdout: cut_short(&bodies, filtered),
                    stderr,
                });
            }
            Completion::Exited(out) => {
                let (head, body) = split(&out.stdout);
                bodies.push(body.to_vec());
                stderr.extend_from_slice(&out.stderr);
                // A failed page ends the walk with its own status, as `gh`
                // does; the caller sees a failed read.
                let following = if out.status.success() {
                    match head.map(next_link) {
                        // No header block: where the listing ends is
                        // unknown, so this page is never "all of it".
                        None => return Err(incomplete("a page had no HTTP header block")),
                        Some(Next::Unfollowable) => {
                            return Err(incomplete("a next link was not an https URL"));
                        }
                        Some(Next::Url(url)) => Some(url),
                        Some(Next::End) => None,
                    }
                } else {
                    None
                };
                // A `Link` loop is ended by `MAX_PAGES` and the deadline.
                match following {
                    Some(url) => next = Some(url),
                    None => {
                        return Ok(Completion::Exited(Output {
                            status: out.status,
                            stdout: join(&bodies, filtered),
                            stderr,
                        }));
                    }
                }
            }
        }
    }
    Err(incomplete(&format!("it did not end within {MAX_PAGES} pages")))
}

#[cfg(test)]
#[path = "paged_tests.rs"]
mod tests;
