//! The reader route for an untargeted read (W4-C).
//!
//! Most daemon reads are built with [`GhTarget::None`] and a working
//! directory: `gh` itself works out the repository from `-R`, from the
//! endpoint path, from `GH_REPO` (which the facade sets from `LOOM_REPO`),
//! or from the checkout's remotes. Before W4-C such a read never reached a
//! reader App ([`super::reader_route`] needs a slug), so every one of them
//! spent the writer's bucket.
//!
//! [`derive`] names the repository such a read is about **without changing
//! the invocation's target**. The slug only steers the *reader* attempt:
//!
//! - [`GhInvocation::reader_slug`] offers it to the router;
//! - the reader attempt's child gets `GH_REPO=<slug>`;
//! - the accounting row books it as `rp=<slug>`, `ro=derived`.
//!
//! The writer attempt (a read with no reader, or the writer fallback) still
//! runs with `target = None`, so its `GH_CONFIG_DIR` and `GH_REPO` are
//! exactly the pre-W4-C ones.
//!
//! # Authority
//!
//! Explicit repo, then `LOOM_REPO` / `GH_REPO`, then the sole local
//! resolution — the order `gh` itself applies, and the explicit-repo-wins
//! step of `forge_etag_store::resolve_target`:
//!
//! 1. an explicit `-R` / `--repo` (or `gh repo view OWNER/REPO`) on an
//!    `issue` / `pr` / `repo` subcommand;
//! 2. a `gh api` endpoint `repos/<owner>/<repo>[/…]` with a literal owner
//!    and repo;
//! 3. for a call `gh` resolves from its environment — a `{owner}/{repo}`
//!    placeholder, or an `issue|pr view|list|status` with no `-R` — the
//!    `GH_REPO` the child will see: `LOOM_REPO` when it is set (the facade
//!    exports it as `GH_REPO`), else an inherited `GH_REPO`;
//! 4. otherwise, the checkout's base repo
//!    ([`crate::forge_repo_facts::base_repo`]) — only when it is not
//!    ambiguous, not pinned to legacy, and equal to
//!    [`crate::forge_etag_store::remote_identity`]. Any disagreement keeps
//!    the writer and counts `facade.cwd_route.disagree`;
//! 5. anything else (`api graphql`, `search`, `run`, `release`, endpoints
//!    outside `repos/`, a URL argument, a non-github.com host, and any
//!    `gh api` call that is not a GET — some sites send a mutation through a
//!    read-intent helper) has no route and stays on the writer.
//!
//! Endpoints whose answer depends on who asks are never derived even when
//! a site forgot `.writer_identity()` ([`asker_dependent_path`]).
//!
//! # Kill switches
//!
//! `LOOM_FACADE_CWD_ROUTING=0` disables derivation; `LOOM_READ_ROUTING=legacy`
//! disables it with everything else W4 added. Both are read on every call.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use super::{AccessIntent, GhInvocation, GhTarget, OutputContract};
use crate::forge_identity::RoutingMode;
use crate::forge_repo_facts::GhRepoEnv;

/// `0` disables the derivation (every untargeted read stays on the writer).
pub const CWD_ROUTING_ENV: &str = "LOOM_FACADE_CWD_ROUTING";

/// The named counter a local disagreement bumps (W1's name).
pub const DISAGREE_COUNTER: &str = crate::forge_call_stats::buckets::CWD_ROUTE_DISAGREE;

/// What the derivation reads from the process environment, captured once
/// per call so a test can inject it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DeriveEnv {
    /// `LOOM_FACADE_CWD_ROUTING`.
    pub(crate) cwd_routing: Option<String>,
    /// `LOOM_READ_ROUTING` parsed.
    pub(crate) legacy: bool,
    /// `LOOM_REPO`, set or not (an empty value is "set").
    pub(crate) loom_repo: Option<OsString>,
    /// The daemon's own `GH_REPO`, which the child inherits when
    /// `LOOM_REPO` is unset.
    pub(crate) gh_repo: Option<OsString>,
}

impl DeriveEnv {
    /// The live environment (read on every call — no `OnceLock`).
    pub(crate) fn current() -> Self {
        Self {
            cwd_routing: std::env::var(CWD_ROUTING_ENV).ok(),
            legacy: RoutingMode::current() == RoutingMode::Legacy,
            loom_repo: std::env::var_os("LOOM_REPO"),
            gh_repo: std::env::var_os("GH_REPO"),
        }
    }

    fn disabled(&self) -> bool {
        self.legacy || self.cwd_routing.as_deref().is_some_and(|v| v.trim() == "0")
    }
}

/// What the checkout says, for step 4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CwdAnswer {
    /// The one repository every local source agrees on.
    Sole(String),
    /// Local sources disagree (or are ambiguous): keep the writer, count it.
    Disagree,
    /// Not a checkout, no usable remote, or repo facts are off.
    Unresolved,
}

/// Where a derived route came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Via {
    RepoFlag,
    Path,
    LoomRepo,
    GhRepo,
    Cwd,
}

/// The derivation's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Derivation {
    /// The reader attempt may serve this read for `slug`.
    Route { slug: String, via: Via },
    /// No route: the read stays on the writer, exactly as before W4-C.
    Writer,
    /// The checkout's local sources disagreed: the writer, and one
    /// `facade.cwd_route.disagree`.
    Disagree,
}

/// How `gh` will resolve the repository of this argv.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    /// The argv names it (`-R`, `repo view OWNER/REPO`, a literal path).
    Named(String, Via),
    /// `gh` resolves it from `GH_REPO`, then the checkout.
    FromEnv,
    /// `gh repo view` with no argument: the checkout only (it ignores
    /// `GH_REPO`).
    FromCheckout,
    /// No derivable repository.
    Unroutable,
}

/// `gh api` flags that take a value (so the value is not the endpoint).
const API_VALUED: &[&str] = &[
    "-H",
    "--header",
    "-f",
    "--raw-field",
    "-F",
    "--field",
    "-q",
    "--jq",
    "-t",
    "--template",
    "-X",
    "--method",
    "--hostname",
    "--input",
    "--cache",
    "-p",
    "--preview",
];

/// The endpoint of a `gh api` argv (`rest` = the words after `api`).
fn api_endpoint(rest: &[String]) -> Option<&str> {
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if API_VALUED.contains(&a) {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(a);
    }
    None
}

/// Whether a `gh api` call carries `--hostname` for a host other than
/// github.com (a reader App only holds github.com tokens).
fn foreign_hostname(rest: &[String]) -> bool {
    rest.windows(2)
        .any(|w| w[0] == "--hostname" && !w[1].eq_ignore_ascii_case("github.com"))
}

/// An endpoint whose answer depends on **who** asks: a reader App would
/// answer for itself. Never derived, whatever the site set.
#[must_use]
pub(crate) fn asker_dependent_path(path: &str) -> bool {
    let p = path.trim_start_matches('/').to_ascii_lowercase();
    let p = p.split('?').next().unwrap_or_default();
    let segs: Vec<&str> = p.split('/').collect();
    let first = segs.first().copied().unwrap_or_default();
    if matches!(first, "user" | "installation" | "app" | "viewer") {
        return true;
    }
    if first != "repos" {
        return false;
    }
    // repos/<o>/<r>/collaborators/<u>/permission, …/branches/<b>/protection…,
    // …/rulesets…, …/rules/branches/…, …/installation
    let tail = segs.get(3..).unwrap_or_default();
    match tail.first().copied() {
        Some("collaborators") => tail.last() == Some(&"permission"),
        Some("branches") => tail.contains(&"protection"),
        Some("rulesets" | "rules" | "installation") => true,
        _ => false,
    }
}

/// The HTTP method a `gh api` argv sends: `-X` / `--method` when given,
/// else `POST` when it carries a body (`-f`, `-F`, `--field`,
/// `--raw-field`, `--input` — `gh api`'s own default), else `GET`.
fn api_method(rest: &[String]) -> String {
    let mut method = None;
    let mut body = false;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if a == "-X" || a == "--method" {
            method = rest.get(i + 1).map(|m| m.to_ascii_uppercase());
            i += 2;
            continue;
        }
        if let Some(m) = a.strip_prefix("--method=") {
            method = Some(m.to_ascii_uppercase());
        } else if let Some(m) = a.strip_prefix("-X").filter(|m| !m.is_empty()) {
            method = Some(m.to_ascii_uppercase());
        } else if matches!(a, "-f" | "-F" | "--field" | "--raw-field" | "--input")
            || a.starts_with("--field=")
            || a.starts_with("--raw-field=")
            || a.starts_with("--input=")
        {
            body = true;
        }
        i += 1;
    }
    method.unwrap_or_else(|| if body { "POST" } else { "GET" }.to_string())
}

/// `owner/repo` from a `[HOST/]OWNER/REPO` value, when the host (if any) is
/// github.com and the value is not a URL.
fn github_nwo(value: &str) -> Option<String> {
    let v = value.trim();
    if v.contains("://") || v.contains(':') || v.contains(char::is_whitespace) {
        return None;
    }
    let segs: Vec<&str> = v.trim_matches('/').split('/').collect();
    match segs.as_slice() {
        [o, r] if !o.is_empty() && !r.is_empty() => Some(format!("{o}/{r}")),
        [h, o, r] if h.eq_ignore_ascii_case("github.com") && !o.is_empty() && !r.is_empty() => {
            Some(format!("{o}/{r}"))
        }
        _ => None,
    }
}

/// `repos/<owner>/<repo>[/…|?…]` with a literal owner and repo.
fn literal_repos_path(path: &str) -> Option<String> {
    let p = path.trim_start_matches('/');
    let rest = p.strip_prefix("repos/")?;
    let rest = rest.split('?').next().unwrap_or_default();
    let mut segs = rest.split('/');
    let (owner, repo) = (segs.next()?, segs.next()?);
    let literal = |s: &str| !s.is_empty() && !s.contains('{') && !s.contains('}');
    (literal(owner) && literal(repo)).then(|| format!("{owner}/{repo}"))
}

/// Whether `path` is a `repos/…` endpoint with a `{owner}` / `{repo}`
/// placeholder in its owner or repo segment.
fn placeholder_repos_path(path: &str) -> bool {
    let p = path.trim_start_matches('/');
    let Some(rest) = p.strip_prefix("repos/") else {
        return false;
    };
    let mut segs = rest.split('/');
    let (owner, repo) = (segs.next().unwrap_or(""), segs.next().unwrap_or(""));
    owner == "{owner}" && repo == "{repo}"
}

/// The value of `-R` / `--repo` (`-R x`, `--repo x`, `--repo=x`, `-Rx`).
fn repo_flag(rest: &[String]) -> Option<Option<&str>> {
    let mut found = None;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if a == "-R" || a == "--repo" {
            found = Some(rest.get(i + 1).map(String::as_str));
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("--repo=") {
            found = Some(Some(v));
        } else if let Some(v) = a.strip_prefix("-R").filter(|v| !v.is_empty()) {
            found = Some(Some(v));
        }
        i += 1;
    }
    found
}

/// The positional words after the subcommand pair (flags and their values
/// skipped as well as an unknown flag set allows: a word starting with `-`
/// is a flag; `--json`, `--jq`, `-q`, `--template`, `-t`, `-L`, `--limit`,
/// `-s`, `--state`, `-l`, `--label`, `-A`, `--author`, `-S`, `--search`,
/// `-B`, `--base`, `-H`, `--head`, `-a`, `--assignee`, `-R`, `--repo` take
/// a value).
fn positionals(rest: &[String]) -> Vec<&str> {
    const VALUED: &[&str] = &[
        "--json",
        "--jq",
        "-q",
        "--template",
        "-t",
        "-L",
        "--limit",
        "-s",
        "--state",
        "-l",
        "--label",
        "-A",
        "--author",
        "-S",
        "--search",
        "-B",
        "--base",
        "-H",
        "--head",
        "-a",
        "--assignee",
        "-R",
        "--repo",
        "--app",
        "-m",
        "--milestone",
        "--mention",
    ];
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if VALUED.contains(&a) {
            i += 2;
            continue;
        }
        if !a.starts_with('-') {
            out.push(a);
        }
        i += 1;
    }
    out
}

/// How `gh` resolves the repository for `args`.
fn shape(args: &[OsString]) -> Shape {
    let words: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let (Some(cmd), rest) = (words.first().map(String::as_str), words.get(1..).unwrap_or_default())
    else {
        return Shape::Unroutable;
    };
    match cmd {
        "api" => {
            if foreign_hostname(rest) {
                return Shape::Unroutable;
            }
            // A mutation is never a read, whatever intent the site declared
            // (a reader App would refuse it, and the 403 would withdraw the
            // reader for the repo): only GET / HEAD are derived.
            if !matches!(api_method(rest).as_str(), "GET" | "HEAD") {
                return Shape::Unroutable;
            }
            let Some(path) = api_endpoint(rest) else {
                return Shape::Unroutable;
            };
            if asker_dependent_path(path) {
                return Shape::Unroutable;
            }
            if let Some(slug) = literal_repos_path(path) {
                return Shape::Named(slug, Via::Path);
            }
            if placeholder_repos_path(path) {
                return Shape::FromEnv;
            }
            Shape::Unroutable
        }
        "issue" | "pr" | "repo" => {
            let sub = rest.first().map(String::as_str).unwrap_or_default();
            let after = rest.get(1..).unwrap_or_default();
            // A URL argument names its own repo (gh ignores `-R` / GH_REPO
            // for it): not modelled, so the writer keeps it.
            if positionals(after).iter().any(|p| p.contains("://")) {
                return Shape::Unroutable;
            }
            if cmd == "repo" {
                // Only `repo view` reads one repository; it takes the repo
                // as a positional and ignores GH_REPO.
                if sub != "view" {
                    return Shape::Unroutable;
                }
                return match positionals(after).first() {
                    Some(arg) => github_nwo(arg)
                        .map_or(Shape::Unroutable, |s| Shape::Named(s, Via::RepoFlag)),
                    None => Shape::FromCheckout,
                };
            }
            match repo_flag(after) {
                Some(Some(v)) => {
                    github_nwo(v).map_or(Shape::Unroutable, |s| Shape::Named(s, Via::RepoFlag))
                }
                Some(None) => Shape::Unroutable,
                None if matches!(sub, "view" | "list" | "status") => Shape::FromEnv,
                None => Shape::Unroutable,
            }
        }
        _ => Shape::Unroutable,
    }
}

/// Whether `inv` may have a route derived at all (the preconditions).
fn eligible(inv: &GhInvocation) -> bool {
    inv.intent == AccessIntent::Read
        && matches!(inv.contract, OutputContract::Captured { .. })
        && inv.target == GhTarget::None
        && inv.cwd.is_some()
        && inv.config_dir.is_none()
        && inv.role.is_none()
        && !inv.strip_token_env
        && !inv.writer_only
}

/// The derivation for `inv` under `env`, consulting `checkout` for step 4.
pub(crate) fn derive_with(
    inv: &GhInvocation,
    env: &DeriveEnv,
    checkout: &dyn Fn(&Path, GhRepoEnv) -> CwdAnswer,
) -> Derivation {
    if env.disabled() || !eligible(inv) {
        return Derivation::Writer;
    }
    let Some(cwd) = inv.cwd.as_deref() else {
        return Derivation::Writer;
    };
    let gh_repo_stripped = inv.stripped_env.contains(&"GH_REPO");
    let from_checkout = |gh_env: GhRepoEnv| match checkout(cwd, gh_env) {
        CwdAnswer::Sole(slug) => Derivation::Route {
            slug,
            via: Via::Cwd,
        },
        CwdAnswer::Disagree => Derivation::Disagree,
        CwdAnswer::Unresolved => Derivation::Writer,
    };
    match shape(&inv.args) {
        Shape::Named(slug, via) => Derivation::Route { slug, via },
        Shape::Unroutable => Derivation::Writer,
        Shape::FromCheckout => from_checkout(GhRepoEnv::Ignore),
        Shape::FromEnv if gh_repo_stripped => from_checkout(GhRepoEnv::Ignore),
        Shape::FromEnv => {
            // What the child's GH_REPO will be: the facade exports
            // LOOM_REPO whenever it is set (even empty), else the child
            // inherits the daemon's GH_REPO. Empty means "resolve from the
            // checkout", as it does for gh.
            let (value, via) = match &env.loom_repo {
                Some(v) => (Some(v), Via::LoomRepo),
                None => (env.gh_repo.as_ref(), Via::GhRepo),
            };
            match value.map(|v| v.to_string_lossy().trim().to_string()) {
                Some(v) if !v.is_empty() => github_nwo(&v)
                    .map_or(Derivation::Writer, |slug| Derivation::Route { slug, via }),
                _ => from_checkout(GhRepoEnv::Honour),
            }
        }
    }
}

/// Step 4 in production: [`crate::forge_repo_facts::base_repo`], agreeing
/// with `remote_identity`, unambiguous and not legacy-pinned.
pub(crate) fn checkout_answer(cwd: &Path, gh_env: GhRepoEnv) -> CwdAnswer {
    if !crate::forge_repo_facts::enabled() {
        return CwdAnswer::Unresolved;
    }
    let Some(base) = crate::forge_repo_facts::base_repo(cwd, gh_env) else {
        return CwdAnswer::Unresolved;
    };
    if base.ambiguous
        || !base.host.eq_ignore_ascii_case("github.com")
        || crate::forge_repo_facts::is_legacy_pinned(cwd, gh_env)
    {
        return CwdAnswer::Disagree;
    }
    match crate::forge_etag_store::remote_identity(cwd) {
        Some((host, nwo))
            if host.eq_ignore_ascii_case("github.com") && nwo.eq_ignore_ascii_case(&base.nwo) =>
        {
            CwdAnswer::Sole(base.nwo)
        }
        _ => CwdAnswer::Disagree,
    }
}

/// Count one disagreement and warn once per root.
fn note_disagreement(cwd: &Path, op: &str) {
    // The named counter only: the writer row this read now books carries
    // W1's own `rd` flag (and bumps W1's row count) when its `origin` repo
    // disagrees, so bumping that here too would count one call twice.
    let n = crate::forge_call_stats::counters::bump(DISAGREE_COUNTER);
    static WARNED: OnceLock<Mutex<HashSet<std::path::PathBuf>>> = OnceLock::new();
    let first = WARNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut w| w.insert(cwd.to_path_buf()))
        .unwrap_or(false);
    if first {
        log::warn!(
            "gh_invocation: {} resolves to different repositories locally ({op}); its \
             untargeted reads stay on the writer ({DISAGREE_COUNTER}={n})",
            cwd.display()
        );
    }
}

impl GhInvocation {
    /// Derive this read's reader route (see the module docs). Never changes
    /// `target`; sets only the private `route_slug`.
    #[must_use]
    pub(super) fn with_derived_route(self) -> Self {
        self.with_derived_route_in(&DeriveEnv::current(), &checkout_answer)
    }

    /// [`GhInvocation::with_derived_route`] with the environment and the
    /// checkout resolver injected.
    #[must_use]
    pub(crate) fn with_derived_route_in(
        mut self,
        env: &DeriveEnv,
        checkout: &dyn Fn(&Path, GhRepoEnv) -> CwdAnswer,
    ) -> Self {
        match derive_with(&self, env, checkout) {
            Derivation::Route { slug, via } => {
                log::trace!(
                    "gh_invocation: {} reads {slug} on the reader route ({via:?})",
                    self.operation.as_str()
                );
                self.route_slug = Some(slug);
            }
            Derivation::Disagree => {
                if let Some(cwd) = self.cwd.as_deref() {
                    note_disagreement(cwd, self.operation.as_str());
                }
            }
            Derivation::Writer => {}
        }
        self
    }
}

#[cfg(test)]
#[path = "cwd_route_tests.rs"]
mod tests;
