//! Which agent `gh` passthroughs go to the forge gateway (2AMLogic/2am#4367 P4).
//!
//! The front already serves some reads from the ETag cache; those run
//! through the facade, whose router offers the gateway first
//! ([`crate::forge_gateway::route_read`]). Every other call is exec'd. This
//! module decides which of those execs is a read it can name the owner of,
//! and so may run under that owner's gateway read profile instead of the
//! session's credential (the writer).
//!
//! **Unclassified means direct.** A call is a read only when its shape says
//! so: `issue|pr view|list`, `pr checks|diff`, `repo view`,
//! `run|workflow view|list`, `label list`, and `gh api <path>` with no
//! method, field or input flag and a path that is not `graphql` — and only
//! when every flag is one that shape is known to take. `issue|pr status`,
//! and any call whose `--json`/`--jq`/`--template` names a `viewer*` value,
//! are answered for the caller and stay on it (#9872). Everything
//! else — every mutation, `gh api -X …`, `gh release`, `gh auth` (the git
//! credential helper), search, extensions, aliases and anything unknown —
//! keeps the session's credential, as before.
//!
//! **The owner must be certain.** A gateway key answers for one owner's
//! installation, so naming the wrong owner would turn a read into a 404.
//! The owner comes from the last `-R owner/repo` (attached `-Rowner/repo`
//! too), a `gh api repos/<o>/<r>/…` path, a positional `github.com` URL,
//! `GH_REPO`, or the checkout's one `origin` remote
//! ([`super::implicit_repo`]'s rule). Flag values are never read as URLs,
//! and when the sources disagree or cannot be read the call stays direct.

use std::path::PathBuf;

use super::classify::is_slug;

/// How the front routed one passthrough, for its ledger row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrontRoute {
    /// The gateway profile the call runs under, when it does.
    pub gateway: Option<PathBuf>,
    /// The call was classified as a read.
    pub read: bool,
}

/// The flags one read shape accepts: those taking a value, and the boolean
/// ones. A flag in neither list makes the call unclassified, so an argument
/// is never mistaken for a value (or the reverse) on a shape this does not
/// know.
struct Spec {
    valued: &'static [&'static str],
    bools: &'static [&'static str],
}

/// Accepted by every read shape.
const COMMON_VALUED: &[&str] = &["-R", "--repo", "--json", "-q", "--jq", "-t", "--template"];

/// Fields and filters naming these are answered for the caller (`viewer*`):
/// a gateway, which asks as its own App, would answer for itself (#9872).
const VIEWER: &str = "viewer";

/// `gh api` flags that change the method or send a body.
const API_WRITE_FLAGS: &[&str] = &[
    "-X",
    "--method",
    "-f",
    "--raw-field",
    "-F",
    "--field",
    "--input",
    "--hostname",
];

/// The flags of a classified read shape, or `None` for any other call.
fn spec(cmd: &str, verb: &str) -> Option<Spec> {
    let (valued, bools): (&'static [&'static str], &'static [&'static str]) = match (cmd, verb) {
        ("issue", "list") => (
            &[
                "-L",
                "--limit",
                "-s",
                "--state",
                "-l",
                "--label",
                "-a",
                "--assignee",
                "-A",
                "--author",
                "-S",
                "--search",
                "-m",
                "--milestone",
                "--mention",
                "--app",
            ],
            &[],
        ),
        ("pr", "list") => (
            &[
                "-L",
                "--limit",
                "-s",
                "--state",
                "-l",
                "--label",
                "-a",
                "--assignee",
                "-A",
                "--author",
                "-S",
                "--search",
                "-B",
                "--base",
                "-H",
                "--head",
                "--app",
            ],
            &["-d", "--draft"],
        ),
        ("issue" | "pr", "view") => (&[], &["-c", "--comments"]),
        ("pr", "checks") => (&["-i", "--interval"], &["--required", "--fail-fast"]),
        ("pr", "diff") => (&["--color"], &["--name-only", "--patch"]),
        ("repo", "view") => (&["-b", "--branch"], &[]),
        ("run", "list") => (
            &[
                "-L",
                "--limit",
                "-b",
                "--branch",
                "-u",
                "--user",
                "-e",
                "--event",
                "-s",
                "--status",
                "-c",
                "--commit",
                "--created",
                "-w",
                "--workflow",
            ],
            &["-a", "--all"],
        ),
        ("run", "view") => (
            &["-j", "--job", "--attempt"],
            &["--log", "--log-failed", "--exit-status", "-v", "--verbose"],
        ),
        ("workflow", "list") => (&["-L", "--limit"], &["-a", "--all"]),
        ("workflow", "view") => (&["-r", "--ref"], &["-y", "--yaml"]),
        ("label", "list") => (&["-L", "--limit", "-S", "--search", "--order", "--sort"], &[]),
        ("api", _) => (
            &["-H", "--header", "--cache", "-p", "--preview"],
            &[
                "-i",
                "--include",
                "--paginate",
                "--silent",
                "--verbose",
                "--slurp",
            ],
        ),
        _ => return None,
    };
    Some(Spec { valued, bools })
}

/// One argument of a classified shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok<'a> {
    /// A flag, with its value when it takes one (`--x v`, `--x=v`, `-xv`).
    Flag(&'a str, Option<&'a str>),
    /// A positional argument.
    Arg(&'a str),
}

/// Split `rest` (the argv after the command, and after the verb unless the
/// command is `api`) against `spec`. `Err` on anything it cannot place:
/// an unknown flag, a valued flag with no value, `--`, or bundled short
/// booleans.
fn tokenize<'a>(rest: &'a [String], spec: &Spec) -> Result<Vec<Tok<'a>>, ()> {
    let valued = |f: &str| COMMON_VALUED.contains(&f) || spec.valued.contains(&f);
    let known = |f: &str| valued(f) || spec.bools.contains(&f);
    let mut out = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let a = a.as_str();
        if a == "--" {
            return Err(());
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (&a[..2 + n.len()], Some(v)),
                None => (a, None),
            };
            if !known(name) {
                return Err(());
            }
            let value = match (valued(name), inline) {
                (true, Some(v)) => Some(v),
                (true, None) => Some(it.next().ok_or(())?.as_str()),
                (false, None) => None,
                (false, Some(_)) => return Err(()),
            };
            out.push(Tok::Flag(name, value));
        } else if a.len() >= 2 && a.starts_with('-') {
            let name = &a[..2];
            if !known(name) {
                return Err(());
            }
            let value = match (valued(name), a.len() > 2) {
                (true, true) => Some(&a[2..]),
                (true, false) => Some(it.next().ok_or(())?.as_str()),
                (false, false) => None,
                // `-ab`: bundled booleans are a shape this does not read.
                (false, true) => return Err(()),
            };
            out.push(Tok::Flag(name, value));
        } else {
            out.push(Tok::Arg(a));
        }
    }
    Ok(out)
}

/// The tokens after the command (and verb), for a classified shape.
fn parse(args: &[String]) -> Option<(&str, Vec<Tok<'_>>)> {
    let cmd = args.first()?.as_str();
    let verb = args.get(1).map_or("", String::as_str);
    let spec = spec(cmd, verb)?;
    let rest = if cmd == "api" { &args[1..] } else { &args[2..] };
    Some((cmd, tokenize(rest, &spec).ok()?))
}

/// Whether `args` (argv without the program) is a read this front may send
/// to the gateway: a classified shape, every flag understood, no `--web` /
/// `--help` / `--watch`, no field or filter that names a `viewer*` value,
/// and for `gh api` one path that is not `graphql` and no method, field or
/// input flag.
#[must_use]
pub fn is_read(args: &[String]) -> bool {
    if args
        .iter()
        .any(|a| ["-h", "--help", "--web", "--watch"].contains(&a.as_str()))
    {
        return false;
    }
    if args.first().is_some_and(|c| c == "api")
        && args[1..].iter().any(|a| {
            API_WRITE_FLAGS.iter().any(|f| {
                a == f
                    || a.starts_with(&format!("{f}="))
                    || (f.len() == 2 && a.len() > 2 && a.starts_with(f))
            })
        })
    {
        return false;
    }
    let Some((cmd, toks)) = parse(args) else {
        return false;
    };
    let viewer = toks.iter().any(|t| {
        matches!(t, Tok::Flag("--json" | "-q" | "--jq" | "-t" | "--template", Some(v))
            if v.to_ascii_lowercase().contains(VIEWER))
    });
    if viewer {
        return false;
    }
    if cmd == "api" {
        let paths: Vec<&str> = toks
            .iter()
            .filter_map(|t| if let Tok::Arg(a) = t { Some(*a) } else { None })
            .collect();
        return matches!(paths.as_slice(), [p] if p.trim_start_matches('/') != "graphql");
    }
    true
}

/// `https://github.com/<o>/<r>/…` → `o/r`; any other URL is `Err`.
fn url_repo(url: &str) -> Result<String, ()> {
    let rest = url.strip_prefix("https://github.com/").ok_or(())?;
    let mut parts = rest.split(['/', '?', '#']);
    let slug = format!("{}/{}", parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if is_slug(&slug) {
        Ok(slug)
    } else {
        Err(())
    }
}

/// The `owner/repo` a read names on its own argv, `Ok(None)` when it names
/// none (gh then uses `GH_REPO` or the checkout). Only `-R`/`--repo` values
/// (the last one wins, as in gh), positional URLs, a `repo view` slug and a
/// `repos/<o>/<r>` API path count; a value of another flag (`--search`,
/// `--jq`, …) is never read as a URL. `Err` for anything ambiguous: a
/// non-slug `-R`, a URL on another host, or two sources that disagree.
fn argv_repo(args: &[String]) -> Result<Option<String>, ()> {
    let (cmd, toks) = parse(args).ok_or(())?;
    let mut named: Vec<String> = Vec::new();
    if let Some(v) = toks.iter().rev().find_map(|t| match t {
        Tok::Flag("-R" | "--repo", v) => *v,
        _ => None,
    }) {
        if !is_slug(v) {
            return Err(());
        }
        named.push(v.to_string());
    }
    let positionals = toks.iter().filter_map(|t| match t {
        Tok::Arg(a) => Some(*a),
        Tok::Flag(..) => None,
    });
    for p in positionals {
        if p.contains("://") {
            named.push(url_repo(p)?);
        } else if cmd == "repo" {
            named.push(if is_slug(p) {
                p.to_string()
            } else {
                return Err(());
            });
        } else if cmd == "api" {
            let path = p.trim_start_matches('/');
            let Some(rest) = path.strip_prefix("repos/") else {
                // `/user`, `orgs/…`, `rate_limit`: no repo, so no owner.
                return Err(());
            };
            let mut parts = rest.split(['/', '?']);
            let (o, r) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            if o != "{owner}" {
                let slug = format!("{o}/{r}");
                named.push(if is_slug(&slug) { slug } else { return Err(()) });
            }
        }
    }
    let first = named.first().cloned();
    if named
        .iter()
        .any(|n| !n.eq_ignore_ascii_case(first.as_deref().unwrap_or("")))
    {
        return Err(());
    }
    Ok(first)
}

/// The owner of the repo a read targets, if certain. `gh_repo` is
/// `GH_REPO`; `remote` is the checkout's implicit repo (asked last).
#[must_use]
pub fn target_owner(
    args: &[String],
    gh_repo: Option<&str>,
    remote: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let repo = match argv_repo(args) {
        Err(()) => return None,
        Ok(Some(r)) => r,
        Ok(None) => match gh_repo.filter(|r| !r.is_empty()) {
            Some(r) => is_slug(r).then(|| r.to_string())?,
            None => remote()?,
        },
    };
    let owner = crate::credential_preflight::owner_of_nwo(&repo);
    (!owner.is_empty()).then(|| owner.to_ascii_lowercase())
}

/// How to run `args`, with the gateway lookup injected.
#[must_use]
pub fn plan(
    args: &[String],
    gh_repo: Option<&str>,
    remote: impl FnOnce() -> Option<String>,
    lookup: &dyn Fn(&str) -> Option<PathBuf>,
) -> FrontRoute {
    if !is_read(args) {
        return FrontRoute::default();
    }
    let gateway = target_owner(args, gh_repo, remote).and_then(|o| lookup(&o));
    FrontRoute {
        gateway,
        read: true,
    }
}

#[cfg(test)]
#[path = "read_route_tests.rs"]
mod tests;
