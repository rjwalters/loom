//! The endpoint an `invoke github` span hit, as a template (#11407).
//!
//! `github.operation` names *why* a call was made; `github.route` names
//! *what* it hit, so the `api.rest` and `agent_gh_front` spend can be ranked
//! by endpoint. Three attributes, all read from argv:
//!
//! - `github.route`: for `gh api`, the path matched against [`ROUTE_TABLE`]
//!   (`/repos/{owner}/{repo}/pulls/{n}/reviews`), `/graphql` for GraphQL,
//!   [`ROUTE_UNKNOWN`] for a path the table lacks, [`ROUTE_OTHER_HOST`] for an
//!   absolute URL off `api.github.com` and [`ROUTE_NONE`] for no endpoint. A
//!   porcelain command (`gh pr list`) is [`ROUTE_PORCELAIN`]: it may send
//!   several requests, and `github.command` says which command it was.
//! - `http.request.method`: `gh api`'s method by gh's own rule, from a closed
//!   set; absent for porcelain.
//! - `github.command`: the command group (`pr view`, `api`, …). In the agent
//!   `gh` front it is the agent's own command ([`set_front_command`]), not the
//!   `gh api` the front sent for it.
//!
//! The vocabulary and the matching rules are gh-shim's (2AMLogic/2am
//! `infra/ops/internal/ghquota/args.go`, runbook `github-quota.md`
//! §"Route, resource and target"), so the two sources join on `route`. The
//! content boundary is also gh-shim's: every value is one of this file's
//! constants. No path segment, query string, body, header or flag value is
//! ever returned.

use std::sync::OnceLock;

/// GraphQL, by either spelling of the endpoint.
pub const ROUTE_GRAPHQL: &str = "/graphql";
/// A path the table does not hold.
pub const ROUTE_UNKNOWN: &str = "(unknown)";
/// An absolute URL that is not on `api.github.com`.
pub const ROUTE_OTHER_HOST: &str = "(other-host)";
/// `gh api` with no endpoint (gh refuses it; nothing is sent).
pub const ROUTE_NONE: &str = "(none)";
/// A porcelain command, not `gh api`: see `github.command`.
pub const ROUTE_PORCELAIN: &str = "(porcelain)";

/// `github.command` for an argv with no positional, and for one whose first
/// positional is not a gh command.
pub const COMMAND_NONE: &str = "(none)";
pub const COMMAND_OTHER: &str = "other";

#[path = "route_table.rs"]
mod table;
use table::{GROUPS, LEAVES, METHODS, ROUTE_TABLE, VALUE_FLAGS, VERBS};

/// What a span records about the endpoint (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteFacts {
    pub route: &'static str,
    /// `gh api` only.
    pub method: Option<&'static str>,
    pub command: &'static str,
}

impl RouteFacts {
    /// The span attributes these facts set.
    #[must_use]
    pub fn attributes(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = vec![
            ("github.route", self.route),
            ("github.command", self.command),
        ];
        if let Some(method) = self.method {
            out.push(("http.request.method", method));
        }
        out
    }
}

struct Flag<'a> {
    name: &'a str,
    value: Option<&'a str>,
}

/// gh's argv as positionals and named flags (gh-shim's `splitArgs`).
fn split(args: &[String]) -> (Vec<&str>, Vec<Flag<'_>>) {
    let mut pos = Vec::new();
    let mut flags = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        i += 1;
        if tok == "--" {
            pos.extend(args[i..].iter().map(String::as_str));
            break;
        }
        if tok.len() < 2 || !tok.starts_with('-') {
            pos.push(tok);
            continue;
        }
        let long = tok.starts_with("--");
        let name = if long {
            tok.split_once('=').map_or(tok, |(n, _)| n)
        } else {
            tok.get(..2).unwrap_or(tok)
        };
        let mut value = None;
        if VALUE_FLAGS.contains(&name) {
            if long && tok.contains('=') {
                value = tok.split_once('=').map(|(_, v)| v);
            } else if !long && tok.len() > 2 {
                value = tok.get(2..).map(|v| v.strip_prefix('=').unwrap_or(v));
            } else if i < args.len() {
                value = Some(args[i].as_str());
                i += 1;
            }
        }
        flags.push(Flag { name, value });
    }
    (pos, flags)
}

fn is_command_word(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// The command group of `pos`, as one of this file's constants.
fn command_of(pos: &[&str]) -> &'static str {
    let Some(first) = pos.first() else {
        return COMMAND_NONE;
    };
    if !is_command_word(first) {
        return COMMAND_OTHER;
    }
    if let Some(group) = GROUPS.iter().find(|g| **g == *first) {
        if let Some(verb) = pos.get(1) {
            let full = format!("{group} {verb}");
            if let Some(known) = VERBS.iter().find(|v| **v == full) {
                return known;
            }
        }
        return group;
    }
    LEAVES
        .iter()
        .find(|l| **l == *first)
        .copied()
        .unwrap_or(COMMAND_OTHER)
}

/// `gh api`'s method: `-X`/`--method` if given, else `POST` when any field
/// or `--input` is given, else `GET`. Anything outside the set is `_OTHER` (OTel semconv).
fn api_method(flags: &[Flag<'_>]) -> &'static str {
    let explicit = flags
        .iter()
        .rfind(|f| f.name == "-X" || f.name == "--method");
    if let Some(flag) = explicit {
        let Some(value) = flag.value else {
            return "_OTHER";
        };
        let upper = value.to_ascii_uppercase();
        return METHODS
            .iter()
            .find(|m| **m == upper)
            .copied()
            .unwrap_or("_OTHER");
    }
    let fields = ["-f", "--raw-field", "-F", "--field", "--input"];
    if flags.iter().any(|f| fields.contains(&f.name)) {
        "POST"
    } else {
        "GET"
    }
}

/// A `gh api` endpoint as path segments: `None` for an absolute URL off
/// `api.github.com`; the query and fragment dropped; an empty or malformed
/// path is no segments.
fn endpoint_segments(endpoint: &str) -> Option<Vec<&str>> {
    if endpoint == "graphql" {
        return Some(vec!["graphql"]);
    }
    let mut path = endpoint;
    if let Some((scheme, rest)) = endpoint.split_once("://") {
        let (host, rest_path) = rest.split_once('/').unwrap_or((rest, ""));
        let web = scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http");
        if !web || !host.eq_ignore_ascii_case("api.github.com") {
            return None;
        }
        path = rest_path;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let path = path.strip_prefix('/').unwrap_or(path);
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return Some(Vec::new());
    }
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() > 32 || segs.iter().any(|s| s.is_empty()) {
        return Some(Vec::new());
    }
    Some(segs)
}

struct Template {
    segs: Vec<&'static str>,
    greedy: bool,
    text: &'static str,
    literals: usize,
}

fn templates() -> &'static [Template] {
    static TEMPLATES: OnceLock<Vec<Template>> = OnceLock::new();
    TEMPLATES.get_or_init(|| {
        ROUTE_TABLE
            .iter()
            .map(|t| {
                let greedy = t.ends_with("+}");
                let trimmed: &'static str = if greedy {
                    Box::leak(format!("{}}}", &t[..t.len() - 2]).into_boxed_str())
                } else {
                    t
                };
                let segs: Vec<&'static str> = trimmed.split('/').collect();
                let literals = segs.iter().filter(|s| !s.starts_with('{')).count();
                Template {
                    text: Box::leak(format!("/{trimmed}").into_boxed_str()),
                    segs,
                    greedy,
                    literals,
                }
            })
            .collect()
    })
}

fn digits(s: &str, max_len: usize) -> bool {
    (1..=max_len).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

fn issue_number(s: &str) -> bool {
    digits(s, 10)
        && s.parse::<u64>()
            .is_ok_and(|n| (1..=i32::MAX as u64).contains(&n))
}

fn login(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=39).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
}

fn repo_name(s: &str) -> bool {
    (1..=100).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

fn seg_match(tmpl: &str, seg: &str) -> bool {
    if !tmpl.starts_with('{') {
        return tmpl == seg;
    }
    match tmpl {
        "{n}" => issue_number(seg),
        "{id}" => digits(seg, 20),
        "{owner}" | "{org}" | "{username}" => login(seg) || seg == "{owner}" || seg == ":owner",
        "{repo}" => repo_name(seg) || seg == "{repo}" || seg == ":repo",
        _ => !seg.is_empty(),
    }
}

/// The most specific template the path fits (most literal segments, an exact
/// length over a greedy tail), or [`ROUTE_UNKNOWN`].
fn match_route(segs: &[&str]) -> &'static str {
    if segs == ["graphql"] {
        return ROUTE_GRAPHQL;
    }
    if segs.is_empty() {
        return ROUTE_UNKNOWN;
    }
    let mut best = ROUTE_UNKNOWN;
    let mut best_score: Option<usize> = None;
    for t in templates() {
        let fits_len = if t.greedy {
            segs.len() >= t.segs.len()
        } else {
            segs.len() == t.segs.len()
        };
        if !fits_len || !t.segs.iter().zip(segs).all(|(t, s)| seg_match(t, s)) {
            continue;
        }
        let score = t.literals * 2 + usize::from(!t.greedy);
        if best_score.is_none_or(|b| score > b) {
            best = t.text;
            best_score = Some(score);
        }
    }
    best
}

/// The endpoint facts of a `gh` argv (without the program).
#[must_use]
pub fn classify(args: &[String]) -> RouteFacts {
    let (pos, flags) = split(args);
    let command = command_of(&pos);
    if command != "api" {
        return RouteFacts {
            route: ROUTE_PORCELAIN,
            method: None,
            command,
        };
    }
    let method = Some(api_method(&flags));
    let route = match pos.get(1) {
        None => ROUTE_NONE,
        Some(endpoint) => endpoint_segments(endpoint).map_or(ROUTE_OTHER_HOST, |s| match_route(&s)),
    };
    RouteFacts {
        route,
        method,
        command,
    }
}

/// Every value [`classify`] can return as a route, for vocabulary checks.
#[must_use]
pub fn is_known_route(route: &str) -> bool {
    [
        ROUTE_GRAPHQL,
        ROUTE_UNKNOWN,
        ROUTE_OTHER_HOST,
        ROUTE_NONE,
        ROUTE_PORCELAIN,
    ]
    .contains(&route)
        || templates().iter().any(|t| t.text == route)
}

/// Record the command the agent `gh` front is serving, so the front's own
/// `gh api` reads are attributed to it (`pr view`, not `api`). Called once,
/// before the front sends anything.
pub fn set_front_command(args: &[String]) {
    let (pos, _) = split(args);
    let command = command_of(&pos);
    #[cfg(not(test))]
    {
        let _ = FRONT_COMMAND.set(command);
    }
    #[cfg(test)]
    TEST_FRONT_COMMAND.with(|c| c.set(Some(command)));
}

/// The agent's command when this process is the `gh` front.
#[must_use]
pub fn front_command() -> Option<&'static str> {
    #[cfg(not(test))]
    {
        FRONT_COMMAND.get().copied()
    }
    #[cfg(test)]
    {
        TEST_FRONT_COMMAND.with(std::cell::Cell::get)
    }
}

#[cfg(not(test))]
static FRONT_COMMAND: OnceLock<&'static str> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_FRONT_COMMAND: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// Clear THIS test thread's front command.
#[cfg(test)]
pub(crate) fn clear_test_front_command() {
    TEST_FRONT_COMMAND.with(|c| c.set(None));
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;
