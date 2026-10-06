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
//! # What is walked
//!
//! Only an argv this module fully understands ([`plan`]): a captured
//! `gh api <endpoint> --paginate` with nothing but `--jq`, header, preview
//! and hostname flags. GraphQL (cursor pagination), a request with fields or
//! a body, an explicit method, `--slurp`, a template, an argv that already
//! asks for `--include`, and any flag not named here keep the single
//! `--paginate` execution exactly as before (booked `pu`, or counted from
//! its own status blocks when it passed `--include`).
//!
//! `LOOM_GH_PAGE_WALK=0` turns the walk off (every call is the single
//! `--paginate` execution again).

use super::{GhInvocation, OutputContract};
use crate::proc_exec::{Completion, ExecError};
use std::ffi::OsString;
use std::process::Output;
use std::time::{Duration, Instant};

/// `0` disables the walk.
pub const WALK_ENV: &str = "LOOM_GH_PAGE_WALK";

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
    if std::env::var(WALK_ENV).is_ok_and(|v| v.trim() == "0") {
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

/// The argv of one page: `--paginate` becomes `--include` where it stood —
/// so every other argument keeps the position the site gave it — and the
/// endpoint is replaced by `next` (the previous page's `rel="next"` URL).
fn page_args(args: &[OsString], endpoint: usize, next: Option<&str>) -> Vec<OsString> {
    args.iter()
        .enumerate()
        .map(|(i, arg)| match next {
            _ if arg == "--paginate" => OsString::from("--include"),
            Some(url) if i == endpoint => OsString::from(url),
            _ => arg.clone(),
        })
        .collect()
}

/// One page's stdout, split at the end of its header block. A stdout that
/// does not start with a status line has no header block (`gh` failed before
/// the request, or a test stub that prints a body only): it is all body.
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

/// The `rel="next"` URL of a header block's `Link` header, if any. Only an
/// `https://` URL is followed.
pub(super) fn next_link(head: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(head);
    let value = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("link").then_some(value)
    })?;
    let mut rest = value;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let close = after.find('>')?;
        let url = &after[..close];
        let params = &after[close + 1..];
        let params_end = params.find('<').unwrap_or(params.len());
        if params[..params_end].contains(r#"rel="next""#) {
            return url.starts_with("https://").then(|| url.to_string());
        }
        rest = &params[params_end..];
    }
    None
}

/// What `gh api --paginate` prints for these page bodies (see the module
/// docs). A single page is returned untouched.
fn join(bodies: &[Vec<u8>], filtered: bool) -> Vec<u8> {
    if let [only] = bodies {
        return only.clone();
    }
    let inner_of = |body: &Vec<u8>| -> Option<Vec<u8>> {
        let trimmed = body.trim_ascii();
        let inner = trimmed.strip_prefix(b"[")?.strip_suffix(b"]")?;
        Some(inner.trim_ascii().to_vec())
    };
    let inners: Option<Vec<Vec<u8>>> = bodies.iter().map(inner_of).collect();
    match inners.filter(|_| !filtered) {
        Some(inners) => {
            let mut out = vec![b'['];
            for inner in inners.iter().filter(|i| !i.is_empty()) {
                if out.len() > 1 {
                    out.push(b',');
                }
                out.extend_from_slice(inner);
            }
            out.push(b']');
            out
        }
        None => bodies.concat(),
    }
}

/// Walk `inv`'s pages, running each through `run_page` (one ordinary facade
/// execution: span, accounting row, credential), and return the completion
/// the single `--paginate` execution would have produced.
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
                stdout: join(&bodies, filtered),
                stderr,
            });
        };
        let mut page = inv.clone();
        page.args = page_args(&inv.args, endpoint, next.as_deref());
        page.contract = OutputContract::Captured { timeout: left };
        match run_page(page)? {
            Completion::TimedOut {
                stdout,
                stderr: page_err,
            } => {
                bodies.push(split(&stdout).1.to_vec());
                stderr.extend_from_slice(&page_err);
                return Ok(Completion::TimedOut {
                    stdout: join(&bodies, filtered),
                    stderr,
                });
            }
            Completion::Exited(out) => {
                let (head, body) = split(&out.stdout);
                bodies.push(body.to_vec());
                stderr.extend_from_slice(&out.stderr);
                // A `Link` loop is ended by `MAX_PAGES` and the deadline.
                match head.and_then(next_link).filter(|_| out.status.success()) {
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
    Err(ExecError::Collect(std::io::Error::other(format!(
        "gh api page walk did not end within {MAX_PAGES} pages; the listing is incomplete"
    ))))
}

#[cfg(test)]
#[path = "paged_tests.rs"]
mod tests;
