//! The daemon's **single comment chokepoint** (Issue #9772).
//!
//! Every issue/PR comment loom-daemon publishes goes through this module, and
//! every comment it publishes therefore ends with a link to that issue/PR's
//! fleet-dashboard page:
//!
//! ```text
//! <body>
//!
//! [loom dashboard](https://dashboard.2amlogic.com/github.com/<owner>/<repo>/issues/<N>)
//! <!-- loom:dashboard-link -->
//! ```
//!
//! ## Why a chokepoint rather than a footer at each site
//!
//! Before this module ~12 modules composed a body and POSTed it to
//! `repos/{nwo}/issues/{n}/comments` themselves. Appending the footer at each
//! of those sites would have been 12 chances to forget it and 12 copies to
//! keep byte-identical — the failure mode the repo's other structural
//! guarantees (`loom:verdict-sha` in `post-verdict.sh`, `loom:provenance` in
//! `create-pr.sh`) exist to avoid. So the URL, the `-f body=` field and the
//! footer are constructed in exactly one place: [`post_command`]. Callers
//! choose how to *spawn* that `Command` (timeouts, logging, error mapping all
//! differ), but they cannot choose the body or the endpoint.
//!
//! There is deliberately **no `--no-footer` escape hatch**. A caller that
//! genuinely needs a bare body is not a comment poster and should not be
//! reaching for this module.
//!
//! ## One endpoint for issues and PRs
//!
//! A PR *is* an issue as far as comments are concerned, so
//! `POST /repos/{nwo}/issues/{n}/comments` serves both and `is_pr` only ever
//! affects the *link* (`pull/N` vs `issues/N`) — kept truthful even though
//! loom-ui resolves both shapes to the same page (2AMLogic/loom-ui#632).
//!
//! ## Format fixture
//!
//! [`FOOTER_FIXTURE`] (`src/forge_comment/fixtures/footer.txt`) pins the exact
//! bytes of one rendered comment. A shell-side twin (`post-comment.sh`) can
//! assert against that same file so the two implementations cannot drift; the
//! Rust side asserts it in [`tests`].
//!
//! ## Marker-carrying comments
//!
//! The footer is appended to machine-readable comments too (lease records,
//! roster heartbeats, verdict-SHA anchors, auto-merge disarm notices). That is
//! safe by construction: every parser in the fleet matches its marker either
//! on the body's **literal first line**
//! (`defaults/docs/lease-record.md`, `role_shard::roster`) or as a
//! **substring** (`verdict-staleness-guard.sh`'s `MARKER_TEST` /
//! `MARKER_CAPTURE`, `merge_pr::redate::already_redated`). None of them
//! anchors on the body's *end*, so a trailing visible line changes nothing.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

/// The hidden marker the footer always carries. Its presence is what makes
/// [`with_footer`] idempotent, and it is what a reader greps for to tell a
/// footer apart from an author-written dashboard link.
pub const DASHBOARD_MARKER: &str = "<!-- loom:dashboard-link -->";

/// Env var overriding the dashboard base URL (a self-hosted `loom-ui`, or a
/// throwaway base in a test).
pub const DASHBOARD_BASE_ENV: &str = "LOOM_DASHBOARD_URL";

/// The fleet dashboard this repo's own footers point at.
pub const DEFAULT_DASHBOARD_BASE: &str = "https://dashboard.2amlogic.com";

/// The forge host segment of the dashboard's URL space. loom-ui keys its routes
/// by forge host; only `github.com` is modelled today, and a Gitea deployment
/// would add a segment here rather than a second footer format.
const DASHBOARD_FORGE_HOST: &str = "github.com";

/// The visible link text. Deliberately short — this line is appended to every
/// comment the fleet writes, including one-line machine notices.
const LINK_TEXT: &str = "loom dashboard";

/// The byte-exact rendering of one commented body + footer, for a shell twin to
/// pin against. See the module docs.
pub const FOOTER_FIXTURE: &str = include_str!("forge_comment/fixtures/footer.txt");

/// The dashboard base URL: [`DASHBOARD_BASE_ENV`] when set to something
/// non-blank, else [`DEFAULT_DASHBOARD_BASE`]. Trailing slashes are trimmed so
/// the caller never has to care whether the operator wrote one.
#[must_use]
pub fn dashboard_base() -> String {
    let raw = std::env::var(DASHBOARD_BASE_ENV).unwrap_or_default();
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        DEFAULT_DASHBOARD_BASE.to_string()
    } else {
        trimmed.to_string()
    }
}

/// The dashboard URL for one issue/PR. `is_pr` picks `pull/N` over `issues/N`.
#[must_use]
pub fn dashboard_url(nwo: &str, number: u32, is_pr: bool) -> String {
    let kind = if is_pr { "pull" } else { "issues" };
    format!(
        "{base}/{host}/{nwo}/{kind}/{number}",
        base = dashboard_base(),
        host = DASHBOARD_FORGE_HOST,
        nwo = nwo.trim().trim_matches('/'),
    )
}

/// The footer as appended to a non-empty body: a blank separator line, the
/// visible link, the hidden marker, and a trailing newline.
///
/// Returns the empty string when `nwo` is blank — there is no URL to link to,
/// so there is no footer to write. That is an absence of the required input,
/// **not** an opt-out; see [`resolve_nwo`] for how callers that only know a
/// working directory obtain one.
#[must_use]
pub fn dashboard_footer(nwo: &str, number: u32, is_pr: bool) -> String {
    if nwo.trim().is_empty() {
        return String::new();
    }
    format!(
        "\n\n[{LINK_TEXT}]({url})\n{DASHBOARD_MARKER}\n",
        url = dashboard_url(nwo, number, is_pr)
    )
}

/// `body` with [`dashboard_footer`] appended.
///
/// - **Idempotent**: a body that already carries [`DASHBOARD_MARKER`] is
///   returned untouched, so passing a comment through this twice (or editing a
///   previously-footered comment, as the roster heartbeat's `PATCH` does)
///   never double-appends.
/// - Trailing whitespace on `body` is trimmed first, so the separator is always
///   exactly one blank line however the caller's `format!` ended.
/// - An empty `body` yields the footer with no leading blank lines.
#[must_use]
pub fn with_footer(nwo: &str, number: u32, is_pr: bool, body: &str) -> String {
    if body.contains(DASHBOARD_MARKER) {
        return body.to_string();
    }
    let footer = dashboard_footer(nwo, number, is_pr);
    if footer.is_empty() {
        return body.to_string();
    }
    let base = body.trim_end();
    if base.is_empty() {
        return footer.trim_start_matches('\n').to_string();
    }
    format!("{base}{footer}")
}

/// Resolve `owner/repo` for a comment posted from `cwd`.
///
/// `LOOM_REPO` first (the override every other daemon forge call honors), then
/// the `origin` remote of `cwd`. `None` when neither answers — the caller still
/// posts (the REST path falls back to `gh`'s own `{owner}/{repo}`
/// placeholders), just without a footer it has no URL for.
#[must_use]
pub fn resolve_nwo(cwd: &Path) -> Option<String> {
    if let Ok(repo) = std::env::var("LOOM_REPO") {
        let repo = repo.trim();
        if !repo.is_empty() {
            return Some(repo.to_string());
        }
    }
    crate::credential_preflight::nwo_from_git_remote(cwd)
}

/// The issue/PR number in a forge *reference*: either a bare number
/// (`"1234"`, `"#1234"`) or the issue/PR URL `create-issue.sh` and `gh` print
/// (`https://github.com/o/r/issues/1234`, with or without a trailing slash or
/// an `#issuecomment-…` fragment).
///
/// `gh issue comment` accepted both shapes, so every call site this module
/// replaced could be handed either. [`post_command`] needs an actual number to
/// build the REST path and the footer URL, so the widening happens here once
/// rather than as a bare `parse::<u32>()` at each site — which would silently
/// fail on exactly the URL form production uses (`watchdog::peer_coord`'s
/// sentinel records what `create-issue.sh` printed: a URL).
#[must_use]
pub fn issue_number(reference: &str) -> Option<u32> {
    let trimmed = reference.trim().trim_end_matches('/');
    // Last path segment first (`…/issues/1234` -> `1234`), THEN the `#`
    // handling — doing it the other way round turns a bare `#1234` into the
    // empty string.
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let digits = last.trim_start_matches('#');
    let digits = digits.split('#').next().unwrap_or(digits).trim();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// The REST path of an issue/PR's comment collection.
///
/// `{owner}/{repo}` placeholders when `nwo` is blank — `gh api` expands those
/// from its working directory's remote, which is what the pre-#9772 call sites
/// relied on.
#[must_use]
pub fn comments_path(nwo: &str, number: u32) -> String {
    let nwo = nwo.trim();
    let slug = if nwo.is_empty() {
        "{owner}/{repo}".to_string()
    } else {
        nwo.to_string()
    };
    format!("repos/{slug}/issues/{number}/comments")
}

/// The `gh api -f` field for a comment POST, footer already applied.
///
/// For the callers that must post through their **own** metered `gh` wrapper
/// rather than [`post_command`] — `star_liveness::forge`'s rate-limit-aware
/// `api()` and `role_runner::roster`'s `PATCH` of an existing comment. They
/// still cannot compose the body or name the endpoint themselves
/// ([`comments_path`]), which is what the chokepoint is for; what they keep is
/// the transport, because losing their breaker/ETag/credential handling to gain
/// one shared `Command` builder would be a net regression.
#[must_use]
pub fn body_field(nwo: &str, number: u32, is_pr: bool, body: &str) -> String {
    format!("body={}", with_footer(nwo, number, is_pr, body))
}

/// The one comment-POST `Command` in the daemon: `gh api
/// repos/<nwo>/issues/<N>/comments --method POST -f body=<body + footer>`.
///
/// `cwd` (when given) becomes the child's working directory and selects the
/// forge credentials for that repository root, matching what every migrated
/// call site did by hand. stdout is piped and stderr is piped; the caller owns
/// spawning, timeouts and error reporting — everything except *what* is posted
/// and *where*.
pub fn post_command(
    gh: impl AsRef<OsStr>,
    cwd: Option<&Path>,
    nwo: &str,
    number: u32,
    is_pr: bool,
    body: &str,
) -> Command {
    let mut cmd = Command::new(gh);
    cmd.arg("api")
        .arg(comments_path(nwo, number))
        .arg("--method")
        .arg("POST")
        .arg("-f")
        .arg(format!("body={}", with_footer(nwo, number, is_pr, body)));
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// [`post_command`] with `owner/repo` resolved from `cwd` ([`resolve_nwo`]) —
/// the shape the call sites that only ever knew a repository root use, and the
/// one that keeps a migrated site to a single line.
pub fn post_command_in(
    gh: impl AsRef<OsStr>,
    cwd: &Path,
    number: u32,
    is_pr: bool,
    body: &str,
) -> Command {
    let nwo = resolve_nwo(cwd).unwrap_or_default();
    post_command(gh, Some(cwd), &nwo, number, is_pr, body)
}

/// Post a comment, returning the forge's response body on success and a
/// human-readable message on failure. The straightforward wrapper around
/// [`post_command`] for callers with no special process handling.
pub fn post(
    gh: impl AsRef<OsStr>,
    nwo: &str,
    number: u32,
    is_pr: bool,
    body: &str,
) -> Result<String, String> {
    post_in(gh, None, nwo, number, is_pr, body)
}

/// [`post`] with an explicit working directory (credentials + `gh`'s own repo
/// resolution follow it).
pub fn post_in(
    gh: impl AsRef<OsStr>,
    cwd: Option<&Path>,
    nwo: &str,
    number: u32,
    is_pr: bool,
    body: &str,
) -> Result<String, String> {
    let out = post_command(gh, cwd, nwo, number, is_pr, body)
        .output()
        .map_err(|e| format!("could not exec gh api: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(format!(
        "gh api {path} exited {code}: {stderr}",
        path = comments_path(nwo, number),
        code = out
            .status
            .code()
            .map_or_else(|| "by signal".to_string(), |c| c.to_string()),
    ))
}

/// Post a comment resolving `owner/repo` from `cwd` ([`resolve_nwo`]) — the
/// shape the call sites that only ever knew a repository root use.
pub fn post_from(
    gh: impl AsRef<OsStr>,
    cwd: &Path,
    number: u32,
    is_pr: bool,
    body: &str,
) -> Result<String, String> {
    let nwo = resolve_nwo(cwd).unwrap_or_default();
    post_in(gh, Some(cwd), &nwo, number, is_pr, body)
}

#[cfg(test)]
mod tests;
