//! The one place the daemon posts an issue/PR comment — and the dashboard
//! link every posted comment therefore carries (#9772).
//!
//! # The contract
//!
//! Every comment the daemon posts ends with a small visible link to the
//! comment's own dashboard page:
//!
//! ```text
//! {body}\n\n[loom dashboard](https://dashboard.2amlogic.com/github.com/<owner>/<repo>/(issues|pull)/<N>)\n<!-- loom:dashboard-link -->\n
//! ```
//!
//! loom-ui resolves that URL shape to the right page (2AMLogic/loom-ui#632,
//! merged `3ad60e7`: `/issues/N` and `/pull/N` share one number sequence and
//! land on the issue page), so the link is the "always a small clickable
//! link in all text content" affordance: any comment is one click from its
//! fleet view. The base URL comes from `LOOM_DASHBOARD_URL` (default
//! [`DEFAULT_DASHBOARD_BASE_URL`], trailing slash(es) trimmed).
//!
//! # Why a chokepoint, not a per-site append
//!
//! Before this module ~a dozen modules composed a body and POSTed it to
//! `repos/{n}/issues/{n}/comments` ad hoc. Appending per site would be a
//! dozen chances to forget; the same reasoning that puts `loom:verdict-sha`
//! in `post-verdict.sh` and `loom:provenance` in `create-pr.sh` puts the
//! footer here: a caller cannot post through this module *without* the
//! footer, so omission is structurally impossible instead of a matter of
//! remembering. The footer is idempotent on [`FOOTER_MARKER`], so a body
//! that already carries one (e.g. a re-posted verdict) is not double-linked.
//!
//! The shell twin (`forge_gh_comment_rl_safe` + `lib/dashboard-link.sh`,
//! #9774) pins itself to the format [`build_dashboard_footer`] produces —
//! if you change the format here, that test breaks on the shell side too.
//! Change both or neither.
//!
//! # Why `--input -` JSON, not `-f body=…`
//!
//! Writes go to `gh api --input -` as `{"body": …}` JSON rather than
//! repeated `-f key=value` flags: every body here is multi-line markdown,
//! and `-f`'s shell-adjacent quoting has already corrupted comment bodies
//! elsewhere in this repo (`merge_pr/redate.rs` documents the same choice).
//! `serde_json` escaping is the only encoder in the path.

use std::ffi::OsStr;
use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::credential_preflight::apply_gh_config_for_root;

/// The production dashboard origin, used when `LOOM_DASHBOARD_URL` is unset.
pub const DEFAULT_DASHBOARD_BASE_URL: &str = "https://dashboard.2amlogic.com";

/// The hidden marker a footer is idempotent on — also what the shell twin
/// and the `loom:dashboard-link` grep-based acceptance checks match on.
pub const FOOTER_MARKER: &str = "<!-- loom:dashboard-link -->";

const ENV_DASHBOARD_BASE_URL: &str = "LOOM_DASHBOARD_URL";

/// The configured dashboard origin: `LOOM_DASHBOARD_URL` when set (and not
/// blank), else [`DEFAULT_DASHBOARD_BASE_URL`]. Trailing slash(es) trimmed so
/// a configured `https://d.example.com/` still builds `…/github.com/o/r/…`.
#[must_use]
pub fn dashboard_base_url() -> String {
    let raw = std::env::var(ENV_DASHBOARD_BASE_URL).unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEFAULT_DASHBOARD_BASE_URL.to_string();
    }
    trimmed.trim_end_matches('/').to_string()
}

/// The dashboard page for `number` in `nwo`: `…/pull/N` for a PR, `…/issues/N`
/// otherwise. loom-ui's resolver lands both on the same issue page; the link
/// itself stays truthful about which object it names.
#[must_use]
pub fn forge_dashboard_url(nwo: &str, number: impl fmt::Display, is_pr: bool) -> String {
    format!(
        "{base}/github.com/{nwo}/{kind}/{number}",
        base = dashboard_base_url(),
        kind = if is_pr { "pull" } else { "issues" },
    )
}

/// `body` with the dashboard footer appended — the canonical format, pinned
/// by the shell twin's test (#9774). Idempotent: a body already carrying
/// [`FOOTER_MARKER`] is returned unchanged.
#[must_use]
pub fn build_dashboard_footer(
    base: &str,
    nwo: &str,
    number: impl fmt::Display,
    is_pr: bool,
    body: &str,
) -> String {
    if body.contains(FOOTER_MARKER) {
        return body.to_string();
    }
    let base = base.trim_end_matches('/');
    format!(
        "{body}\n\n[loom dashboard]({base}/github.com/{nwo}/{kind}/{number})\n{FOOTER_MARKER}\n",
        kind = if is_pr { "pull" } else { "issues" },
    )
}

/// [`build_dashboard_footer`] at the configured base URL — the form every
/// in-daemon caller uses.
#[must_use]
pub fn append_dashboard_footer(
    nwo: &str,
    number: impl fmt::Display,
    is_pr: bool,
    body: &str,
) -> String {
    build_dashboard_footer(&dashboard_base_url(), nwo, number, is_pr, body)
}

/// [`append_dashboard_footer`] when the `owner/repo` slug resolved, else the
/// body unchanged — for the sites whose `gh` invocation keeps bespoke
/// semantics (a dispatch-path timeout, a `LOOM_REPO` override) and only
/// shares the footer format: a link to nowhere is worse than no link, so an
/// unresolvable slug posts the caller's body untouched rather than a wrong
/// URL. The slug resolution to prefer is `LOOM_REPO` first, then a
/// `resolve_owner_repo` over the site's own root.
#[must_use]
pub fn footer_or_body(
    nwo: Option<&str>,
    number: impl fmt::Display,
    is_pr: bool,
    body: &str,
) -> String {
    match nwo {
        Some(nwo) => append_dashboard_footer(nwo, number, is_pr, body),
        None => body.to_string(),
    }
}

/// The one comment POST. Appends the dashboard footer (nothing can skip it)
/// and POSTs `{"body": …}` to `repos/{nwo}/issues/{number}/comments` via
/// `gh api --input -` — a PR *is* an issue for comments, so one endpoint
/// serves both. `root`, when given, selects the owner-partitioned
/// `GH_CONFIG_DIR` for cross-owner managed repos (#5401 — a no-op for
/// single-owner fleets). Returns the response body (the comment JSON, whose
/// `html_url` is the new comment) on success.
///
/// # Errors
///
/// When `gh` cannot be spawned, cannot be written to, exits non-zero, or
/// produces undecodable output. The error text carries `gh`'s stderr so a
/// rate-limit message reaches the caller's log.
pub fn post_comment(
    gh_bin: impl AsRef<OsStr>,
    root: Option<&Path>,
    nwo: &str,
    number: impl fmt::Display,
    is_pr: bool,
    body: &str,
) -> Result<String, String> {
    let full_body = append_dashboard_footer(nwo, &number, is_pr, body);
    let payload = serde_json::json!({ "body": full_body }).to_string();

    let mut cmd = Command::new(gh_bin.as_ref());
    cmd.arg("api")
        .arg(format!("repos/{nwo}/issues/{number}/comments"))
        .arg("--input")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(root) = root {
        apply_gh_config_for_root(&mut cmd, root);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not exec gh api (comment): {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "gh api stdin was not piped".to_string())?
        .write_all(payload.as_bytes())
        .map_err(|e| format!("could not write the comment request body: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("gh api (comment) failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "gh api (comment on {nwo}#{number}) failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("gh api (comment) output was not UTF-8: {e}"))
}

/// Parse a forge issue reference into `(owner/repo, number)`, where the slug
/// is `None` when the reference is a bare number against the ambient repo.
///
/// Accepts the shapes the daemon actually stores: `N`, `owner/repo#N`,
/// `owner/repo/issues/N`, and the full URL forms. `None` for anything else —
/// an unparseable reference cannot build a truthful dashboard link, so the
/// caller posts without a footer rather than a wrong one.
#[must_use]
pub fn parse_issue_ref(issue_ref: &str) -> Option<(Option<String>, u64)> {
    let reference = issue_ref.trim();
    if reference.is_empty() {
        return None;
    }
    // A bare number: the ambient repo's own issue.
    if let Ok(number) = reference.parse::<u64>() {
        return Some((None, number));
    }
    // Strip a scheme/host prefix so `https://github.com/o/r/issues/3`,
    // `github.com/o/r/issues/3`, and `o/r/issues/3` share one tail parse.
    let tail = reference
        .split_once("github.com/")
        .map_or(reference, |(_, tail)| tail);
    let segments: Vec<&str> = tail.split(['#', '/']).filter(|s| !s.is_empty()).collect();
    if segments.len() < 3 {
        return None;
    }
    // `o/r#N`, `o/r/issues/N`, `o/r/pull/N`: the number is the final segment.
    let number = segments[segments.len() - 1].parse::<u64>().ok()?;
    Some((Some(format!("{}/{}", segments[0], segments[1])), number))
}

/// Arguments for the `forge comment` verb ([`cli_entrypoint`]).
pub struct CommentArgs {
    /// Issue/PR number for the post path; `None` when `patch_created` is set.
    pub number: Option<u64>,
    /// `owner/repo`; `None` resolves from the current checkout's origin remote.
    pub repo: Option<String>,
    pub body: Option<String>,
    /// Path to read the body from; `-` means stdin.
    pub body_file: Option<std::path::PathBuf>,
    /// The number names a pull request (link says `/pull/N`).
    pub is_pr: bool,
    /// `--patch-created <URL|owner/repo#N>`: instead of posting a new comment,
    /// append the footer to the CREATED object's existing body (idempotent) —
    /// the post-create step for create-issue.sh / create-pr.sh (#9774), where
    /// the number exists only after the create. Best-effort by contract: the
    /// caller warns and moves on when this fails.
    pub patch_created: Option<String>,
}

/// The `loom-daemon forge comment` verb (#9772): the same chokepoint the
/// daemon's internal sites use, exposed so shell callers and agents
/// (#9774's `post-comment.sh`) post through it instead of around it.
///
/// # Errors
///
/// When the body is missing or double-specified, the repo cannot be
/// resolved, or the POST fails.
pub fn cli_entrypoint(args: CommentArgs) -> anyhow::Result<()> {
    if let Some(created_ref) = &args.patch_created {
        return patch_created_entrypoint(created_ref);
    }
    let number = args
        .number
        .ok_or_else(|| anyhow::anyhow!("a NUMBER is required (or --patch-created <URL|owner/repo#N>)"))?;
    let body = match (&args.body, &args.body_file) {
        (Some(text), None) => text.clone(),
        (None, Some(path)) => {
            if path.as_os_str() == "-" {
                let mut buffer = String::new();
                std::io::stdin()
                    .read_to_string(&mut buffer)
                    .map_err(|e| anyhow::anyhow!("could not read the body from stdin: {e}"))?;
                buffer
            } else {
                std::fs::read_to_string(path).map_err(|e| {
                    anyhow::anyhow!("could not read the body from {}: {e}", path.display())
                })?
            }
        }
        (Some(_), Some(_)) => {
            anyhow::bail!("--body and --body-file are mutually exclusive");
        }
        (None, None) => {
            anyhow::bail!("one of --body TEXT or --body-file PATH (- = stdin) is required");
        }
    };
    let nwo = match &args.repo {
        Some(nwo) => nwo.clone(),
        None => crate::worktree_ops::gh::resolve_owner_repo(Path::new("."))
            .map(|(owner, name)| format!("{owner}/{name}"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot resolve owner/repo from the origin remote of the working directory — pass --repo OWNER/REPO"
                )
            })?,
    };
    let response = post_comment(
        crate::forge_cmd::gh_bin(),
        Some(Path::new(".")),
        &nwo,
        number,
        args.is_pr,
        &body,
    )
    .map_err(anyhow::Error::msg)?;
    // The response is the new comment's JSON; `html_url` is the friendliest
    // one-line confirmation. A shape change must never fail the POST, so a
    // parse miss just echoes the raw response.
    match serde_json::from_str::<serde_json::Value>(response.trim()) {
        Ok(value) => {
            if let Some(url) = value["html_url"].as_str() {
                println!("{url}");
            } else {
                println!("{response}");
            }
        }
        Err(_) => println!("{response}"),
    }
    Ok(())
}

/// `forge comment --patch-created <URL|owner/repo#N>` (#9774): fetch the
/// created object's body, append the dashboard footer (idempotent), PATCH it
/// back. The create scripts call this right after a successful create — the
/// number exists only then — and treat failure as a logged note, never as a
/// reason to un-file.
///
/// # Errors
///
/// When the reference does not parse as a created GitHub object, or the fetch
/// or PATCH fails.
fn patch_created_entrypoint(created_ref: &str) -> anyhow::Result<()> {
    let trimmed = created_ref.trim();
    // A GitHub URL is the shape `gh`/`forge dashboard-link` emit; a bare
    // `owner/repo#N` is accepted for tests.
    let (nwo, number, is_pr) =
        if let Some((Some(nwo), number)) = parse_issue_ref(trimmed) {
            let is_pr = trimmed.contains("/pull/");
            (nwo, number, is_pr)
        } else {
            anyhow::bail!(
                "--patch-created expects a GitHub object URL or owner/repo#N, got {trimmed:?}"
            );
        };
    let gh = crate::forge_cmd::gh_bin();
    let current =
        gh_api_get(gh.as_str(), &format!("repos/{nwo}/issues/{number}")).map_err(anyhow::Error::msg)?;
    let body = serde_json::from_str::<serde_json::Value>(current.trim())
        .map_err(|e| anyhow::anyhow!("could not parse the created object's JSON: {e}"))?
        ["body"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let updated = build_dashboard_footer(&dashboard_base_url(), &nwo, number, is_pr, &body);
    if updated == body {
        // Already carries the marker: nothing to do, and re-POSTing an
        // unchanged body would burn the PATCH for nothing.
        return Ok(());
    }
    gh_api_patch(
        gh.as_str(),
        &format!("repos/{nwo}/issues/{number}"),
        &serde_json::json!({ "body": updated }).to_string(),
    )
    .map_err(anyhow::Error::msg)?;
    Ok(())
}

/// `gh api <path>` (GET) — the raw response body on success, `gh`'s stderr on
/// failure. Same no-cache plain-`gh` semantics `forge_get_pr_nocache` uses.
fn gh_api_get(gh_bin: &str, path: &str) -> Result<String, String> {
    let out = std::process::Command::new(gh_bin)
        .arg("api")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not exec gh api: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "gh api {path} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("gh api {path} output was not UTF-8: {e}"))
}

/// `gh api <path> -X PATCH --input -` with a JSON request body — the same
/// stdin-JSON discipline as [`post_comment`] (multi-line markdown never goes
/// through `-f`).
fn gh_api_patch(gh_bin: &str, path: &str, json: &str) -> Result<String, String> {
    let mut child = std::process::Command::new(gh_bin)
        .arg("api")
        .arg(path)
        .arg("-X")
        .arg("PATCH")
        .arg("--input")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not exec gh api: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "gh api stdin was not piped".to_string())?
        .write_all(json.as_bytes())
        .map_err(|e| format!("could not write the gh api request body: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("gh api (PATCH {path}) failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "gh api (PATCH {path}) failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("gh api output was not UTF-8: {e}"))
}

#[cfg(test)]
mod tests;
