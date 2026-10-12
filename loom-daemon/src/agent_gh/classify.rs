//! Route an agent `gh` argv: ETag-servable or passthrough (#10331).
//!
//! Pure: no environment, no filesystem. This decides *ETag-servable*, not
//! *read-only*: a call is routed to the conditional-request cache only when
//! the in-repo ETag modules can reproduce `gh`'s output exactly. Everything
//! else — every mutation, every `api` call, every shape or flag this does not
//! recognise — is [`Route::Passthrough`], which execs the next `gh` with the
//! argv untouched. The fail-safe direction is always "pass through".
//!
//! The served shapes are deliberately the parse rules of the modules that
//! serve them ([`crate::forge_cached_view::parse_query`],
//! [`crate::forge_cached_list::parse_query`]), narrowed further for `list`,
//! whose projection is not `gh`-identical for every field (see
//! [`LIST_PARITY_FIELDS`]), and of [`super::pr_checks::parse`] for
//! `pr checks` (#10516). A module that still declines at run time (a
//! number that is really a PR, a truncated page, a non-200/304 answer) sends
//! the call down the passthrough path too.

use super::pr_checks;
use crate::{forge_cached_list, forge_cached_view};

/// `gh issue …` or `gh pr …`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entity {
    Issue,
    Pr,
}

impl Entity {
    /// The entity name the ETag modules take.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Entity::Issue => "issue",
            Entity::Pr => "pr",
        }
    }
}

/// What the front does with one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `issue|pr view <N> --json …` through [`crate::forge_cached_view`].
    EtagView(Entity),
    /// `issue|pr list … --json …` through [`crate::forge_cached_list`]. The
    /// argv to serve with: `gh`'s implicit `--limit 30` made explicit, since
    /// the listing module applies no default limit.
    EtagList(Entity, Vec<String>),
    /// `pr checks <N> [--json …]` through [`super::pr_checks`].
    EtagChecks,
    /// Exec the next `gh` with the argv byte-identical.
    Passthrough,
}

/// `gh`'s default `--limit` for `issue list` / `pr list`.
const GH_DEFAULT_LIST_LIMIT: &str = "30";

/// `--json` fields whose `list` projection is identical to `gh`'s. Excluded:
/// `labels` (the listing row keeps names only; `gh` emits
/// `{id,name,description,color}`), `author` (`gh` adds `id`/`name`/`is_bot`),
/// `closedAt` (`gh` prints a zero time for an open item, not `""`).
const LIST_PARITY_FIELDS: &[&str] = &["body", "createdAt", "number", "state", "title", "updatedAt"];

/// Flags that make a call unservable wherever they appear: help, a browser,
/// and the ETag modules' own `--cached` (which plain `gh` rejects, so it must
/// reach `gh` to fail the way it always has).
const NEVER_SERVED: &[&str] = &["-h", "--help", "--web", "-w", "--cached"];

/// Classify `args` (argv without the program name).
#[must_use]
pub fn classify(args: &[String]) -> Route {
    let (Some(command), Some(verb)) = (args.first(), args.get(1)) else {
        return Route::Passthrough;
    };
    let entity = match command.as_str() {
        "issue" => Entity::Issue,
        "pr" => Entity::Pr,
        _ => return Route::Passthrough,
    };
    if args.iter().any(|a| NEVER_SERVED.contains(&a.as_str())) || !repos_are_slugs(args) {
        return Route::Passthrough;
    }
    let rest = &args[1..];
    match verb.as_str() {
        "view" if forge_cached_view::parse_query(entity.as_str(), rest).is_some() => {
            Route::EtagView(entity)
        }
        "list" => classify_list(entity, rest),
        "checks" if entity == Entity::Pr && pr_checks::parse(&rest[1..]).is_some() => {
            Route::EtagChecks
        }
        _ => Route::Passthrough,
    }
}

/// `list`: the module's shapes, minus `--search` (`gh` sends it to the search
/// API, whose order differs from the listing's) and minus any field outside
/// [`LIST_PARITY_FIELDS`].
fn classify_list(entity: Entity, rest: &[String]) -> Route {
    if rest
        .iter()
        .any(|a| is_flag(a, "--search") || is_flag(a, "-S"))
    {
        return Route::Passthrough;
    }
    let Some(q) = forge_cached_list::parse_query(entity.as_str(), rest) else {
        return Route::Passthrough;
    };
    if !q
        .json_fields
        .iter()
        .all(|f| LIST_PARITY_FIELDS.contains(&f.as_str()))
    {
        return Route::Passthrough;
    }
    // `gh` prints `--json` keys sorted; the listing projects them in the
    // order asked for. Ask in sorted order.
    let mut fields = q.json_fields.clone();
    fields.sort();
    fields.dedup();
    let mut served = Vec::with_capacity(rest.len() + 2);
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--json" {
            served.extend(["--json".to_string(), fields.join(",")]);
            i += 2;
            continue;
        }
        if rest[i].starts_with("--json=") {
            served.push(format!("--json={}", fields.join(",")));
        } else {
            served.push(rest[i].clone());
        }
        i += 1;
    }
    if q.limit.is_none() {
        served.extend(["--limit".to_string(), GH_DEFAULT_LIST_LIMIT.to_string()]);
    }
    Route::EtagList(entity, served)
}

/// `a` is `flag` or `flag=…`.
fn is_flag(a: &str, flag: &str) -> bool {
    a == flag || a.strip_prefix(flag).is_some_and(|v| v.starts_with('='))
}

/// Every `-R`/`--repo` value is a plain `owner/name` (`gh` also accepts
/// `HOST/OWNER/REPO` and URLs, which the ETag modules do not resolve).
fn repos_are_slugs(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let value = if a == "-R" || a == "--repo" {
            i += 1;
            args.get(i).map(String::as_str)
        } else {
            a.strip_prefix("--repo=").or_else(|| a.strip_prefix("-R="))
        };
        if (a == "-R" || a == "--repo" || value.is_some()) && !value.is_some_and(is_slug) {
            return false;
        }
        i += 1;
    }
    true
}

/// `owner/name`: exactly one `/`, both halves non-empty and URL-safe.
#[must_use]
pub fn is_slug(s: &str) -> bool {
    let ok = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    matches!(s.split_once('/'), Some((o, n)) if ok(o) && ok(n))
}

#[cfg(test)]
#[path = "classify_tests.rs"]
mod tests;
