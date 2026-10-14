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
//! so: `issue|pr view|list|status`, `pr checks|diff`, `repo view`,
//! `run|workflow view|list`, `label list`, and `gh api <path>` with no
//! method, field or input flag and a path that is not `graphql`. Everything
//! else — every mutation, `gh api -X …`, `gh release`, `gh auth` (the git
//! credential helper), search, extensions, aliases and anything unknown —
//! keeps the session's credential, as before.
//!
//! **The owner must be certain.** A gateway key answers for one owner's
//! installation, so naming the wrong owner would turn a read into a 404.
//! The owner comes from a plain `-R owner/repo`, a `gh api repos/<o>/<r>/…`
//! path, a `github.com` URL argument, `GH_REPO`, or the checkout's one
//! `origin` remote ([`super::implicit_repo`]'s rule); when none of these is
//! unambiguous the call stays direct.

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

/// Flags that make any call unclassified.
const NEVER: &[&str] = &["-h", "--help", "--web", "-w"];

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

/// `gh api` flags that take a value (skipped when finding the path).
const API_VALUED: &[&str] = &[
    "-H",
    "--header",
    "-q",
    "--jq",
    "-t",
    "--template",
    "--cache",
    "-p",
    "--preview",
];

/// Whether `args` (argv without the program) is a read this front may send
/// to the gateway.
#[must_use]
pub fn is_read(args: &[String]) -> bool {
    let (Some(cmd), Some(verb)) = (args.first(), args.get(1)) else {
        return false;
    };
    if args.iter().any(|a| NEVER.contains(&a.as_str())) {
        return false;
    }
    match (cmd.as_str(), verb.as_str()) {
        ("issue", "view" | "list" | "status")
        | ("pr", "view" | "list" | "status" | "checks" | "diff")
        | ("repo", "view")
        | ("run" | "workflow", "view" | "list")
        | ("label", "list") => true,
        ("api", _) => api_path(&args[1..]).is_some_and(|p| p.trim_start_matches('/') != "graphql"),
        _ => false,
    }
}

/// The endpoint of a `gh api` argv tail, or `None` when a flag makes the
/// call a non-GET (or the argv is not understood).
fn api_path(rest: &[String]) -> Option<&str> {
    let mut path = None;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        let flag = a.split('=').next().unwrap_or(a);
        // `-XPOST`, `-fbody=x`: a short flag with its value attached.
        let attached = ["-X", "-f", "-F"]
            .iter()
            .any(|f| a.len() > 2 && a.starts_with(f));
        if API_WRITE_FLAGS.contains(&flag) || attached {
            return None;
        }
        if API_VALUED.contains(&a) {
            i += 2;
            continue;
        }
        if !a.starts_with('-') {
            if path.is_some() {
                return None; // two positionals: not a shape gh accepts
            }
            path = Some(a);
        }
        i += 1;
    }
    path
}

/// The `owner/repo` a read names on its own argv: `-R`/`--repo`, a
/// `repos/<o>/<r>` API path, a `github.com` URL, or `repo view <o>/<r>`.
/// `Err` when the argv names a repo this cannot resolve (a non-slug `-R`, a
/// URL on another host), which keeps the call direct.
fn argv_repo(args: &[String]) -> Result<Option<String>, ()> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let value = if a == "-R" || a == "--repo" {
            i += 1;
            Some(args.get(i).map_or("", String::as_str))
        } else {
            a.strip_prefix("--repo=").or_else(|| a.strip_prefix("-R="))
        };
        if let Some(v) = value {
            return if is_slug(v) {
                Ok(Some(v.to_string()))
            } else {
                Err(())
            };
        }
        i += 1;
    }
    for a in &args[1..] {
        if a.contains("://") {
            let rest = a.strip_prefix("https://github.com/").ok_or(())?;
            let mut parts = rest.split('/');
            let slug = format!("{}/{}", parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            return if is_slug(&slug) {
                Ok(Some(slug))
            } else {
                Err(())
            };
        }
    }
    if args.first().map(String::as_str) == Some("api") {
        let Some(path) = api_path(&args[1..]) else {
            return Err(());
        };
        let path = path.trim_start_matches('/');
        let Some(rest) = path.strip_prefix("repos/") else {
            // `/user`, `orgs/…`, `rate_limit`: no repo, so no owner.
            return Err(());
        };
        let mut parts = rest.split(['/', '?']);
        let (o, r) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if o == "{owner}" {
            return Ok(None); // gh fills it from the checkout, as below
        }
        let slug = format!("{o}/{r}");
        return if is_slug(&slug) {
            Ok(Some(slug))
        } else {
            Err(())
        };
    }
    if args.first().map(String::as_str) == Some("repo") {
        if let Some(p) = args[2..].iter().find(|a| !a.starts_with('-')) {
            return if is_slug(p) {
                Ok(Some(p.clone()))
            } else {
                Err(())
            };
        }
    }
    Ok(None)
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
