//! Agent-facing cached issue/PR listing (Issue #5056).
//!
//! # Why this exists
//!
//! The daemon's polling loops already read issue lists nearly for free via
//! [`crate::forge_listing::list_issues_cached`] — a REST `GET` with
//! `If-None-Match` that a matching ETag answers with a **304 at zero
//! rate-limit cost**. Agent role prompts never had that: their `gh issue list`
//! / `gh pr list` invocations are **GraphQL**, which has no conditional-request
//! mechanism, so every role tick on every host burned the shared GraphQL pool
//! even when the queue was unchanged (measured: `graphql` at 1378/5000 in ~16
//! minutes while REST `core` sat at 19/5000).
//!
//! This module exposes that same ETag/REST/304 mechanism to agents as
//! `loom-daemon forge issue list --cached …` / `forge pr list --cached …`,
//! backed by the **disk-persistent** variant
//! [`crate::forge_listing::list_issues_cached_persistent`] (agents run each
//! query as a fresh short-lived process, so the ETag must survive process
//! exit for the second reader to get a free `304`).
//!
//! # Scope — what routes here vs. declines to `gh`
//!
//! Only the label/state listing shape the REST issues endpoint can serve is
//! handled. Anything else **declines** (exit [`DECLINED`], no stdout) so the
//! caller (`gh-cached`) falls back to plain `gh`, keeping every existing call
//! site correct:
//!
//! - **Requires `--json`** with a field subset the REST row can supply
//!   ({number,title,state,body,labels,createdAt,updatedAt,closedAt,author}).
//!   A bare `gh issue list` (human table) or a PR-only field (`mergedAt`,
//!   `files`) declines — we cannot reproduce those.
//! - **`--search`** is honored only for pure `label:` / `-label:` terms
//!   (client-side include/exclude); any other search token declines.
//! - **`pr list`** is served only for `--state open` (the
//!   `loom:review-requested` queue); merged/closed PR listings need the pulls
//!   endpoint and decline.
//! - A **possibly-truncated** full page (>= `PER_PAGE` rows) declines rather
//!   than silently serve a partial set.
//!
//! Because declining is always safe (the caller re-runs the identical `gh`),
//! repointing a prompt to the cached helper can never change results — only
//! cost.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::forge_cmd::{detect_forge, ForgeType};
use crate::forge_listing::{list_issues_cached_persistent, CachedListing};

/// Exit code signalling "this shape is not cacheable; fall back to `gh`".
/// Shares the numeric value of [`crate::forge_cmd::EX_FORGE_DECLINED`].
pub const DECLINED: i32 = crate::forge_cmd::EX_FORGE_DECLINED;

/// The `--json` fields a REST issue row can supply, mapped to their gh names.
const SUPPORTED_JSON_FIELDS: &[&str] = &[
    "number",
    "title",
    "state",
    "body",
    "labels",
    "createdAt",
    "updatedAt",
    "closedAt",
    "author",
];

/// A parsed, cacheable `list` query. Returned by [`parse_query`]; `None` there
/// means "decline — not a shape we can serve".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedQuery {
    /// Positive labels (REST `labels=` AND filter). Comma-joined for the URL.
    pub positive_labels: Vec<String>,
    /// Labels to exclude client-side (from `--search "-label:X"`).
    pub negative_labels: Vec<String>,
    /// `open` | `closed` | `all`.
    pub state: String,
    /// Requested `--json` fields (already validated against
    /// [`SUPPORTED_JSON_FIELDS`]).
    pub json_fields: Vec<String>,
    /// Optional `--jq` expression, applied after projection.
    pub jq: Option<String>,
    /// Optional `--limit` (applied client-side after filtering).
    pub limit: Option<usize>,
    /// Optional `--repo owner/name` override.
    pub repo: Option<String>,
}

/// Is this a `forge <issue|pr> list --cached …` request?
#[must_use]
pub fn is_cached_list(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some("list") && args.iter().any(|a| a == "--cached")
}

/// Entry point from `forge_cmd::dispatch`. Serves the cached listing to stdout
/// and exits `0`, or exits [`DECLINED`] (no stdout) when the shape is not
/// cacheable / the lookup failed, so the caller falls back to `gh`. Never
/// returns.
pub fn handle(entity: &str, args: &[String]) -> ! {
    // Gitea has no REST-ETag path here; decline to the shell/gh fallback.
    if detect_forge(None) == ForgeType::Gitea {
        std::process::exit(DECLINED);
    }
    match build_output(entity, args, &default_fetcher) {
        Some(output) => {
            print!("{output}");
            std::process::exit(0);
        }
        None => {
            eprintln!(
                "loom-daemon forge {entity} list --cached: not a cacheable shape (or lookup \
                 failed); caller should fall back to gh"
            );
            std::process::exit(DECLINED);
        }
    }
}

/// A fetcher abstraction so tests can inject a fake listing without a real
/// `gh`. Production uses [`default_fetcher`].
type Fetcher<'a> = dyn Fn(&str, &str, Option<&str>) -> Option<CachedListing> + 'a;

fn default_fetcher(labels: &str, state: &str, repo: Option<&str>) -> Option<CachedListing> {
    let gh_bin = std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string());
    // #7275: the actual `gh api` child process already inherits this process's
    // cwd whenever `cwd` is `None` here (Command::current_dir is simply never
    // called, so the OS default applies) — so passing it through explicitly
    // changes nothing about *what gh requests*. What it fixes is the
    // disk-persistent cache KEY: `list_issues_cached_persistent` needs the
    // real cwd to resolve which repo's git remote (and therefore which repo)
    // this query is actually for, so two hosts/repos sharing a label
    // convention never collide on the same on-disk cache file.
    let cwd = std::env::current_dir().ok();
    list_issues_cached_persistent(Path::new(&gh_bin), cwd.as_deref(), repo, labels, state).ok()
}

/// Core, side-effect-free (given the `fetch` closure) pipeline: parse → fetch →
/// filter → project → optional jq. Returns `None` to decline.
pub fn build_output(entity: &str, args: &[String], fetch: &Fetcher) -> Option<String> {
    let want_pr = match entity {
        "issue" => false,
        "pr" => true,
        _ => return None,
    };
    let q = parse_query(entity, args)?;

    let labels_joined = q.positive_labels.join(",");
    let listing = fetch(&labels_joined, &q.state, q.repo.as_deref())?;
    // Never serve a possibly-truncated page — decline so gh returns the full set.
    if listing.truncated {
        return None;
    }

    let mut rows: Vec<Value> = listing
        .issues
        .into_iter()
        .filter(|it| it.is_pull_request == want_pr)
        .filter(|it| {
            // Client-side exclusion for `-label:X` search terms.
            !q.negative_labels
                .iter()
                .any(|neg| it.labels.iter().any(|l| l == neg))
        })
        .map(|it| project_row(&it, &q.json_fields))
        .collect();

    if let Some(limit) = q.limit {
        rows.truncate(limit);
    }

    let array = Value::Array(rows);
    match &q.jq {
        Some(expr) => apply_jq(&array, expr),
        // gh prints `--json` output as pretty JSON with a trailing newline.
        None => serde_json::to_string_pretty(&array)
            .ok()
            .map(|s| format!("{s}\n")),
    }
}

/// Build a gh-`--json`-shaped object with only the requested fields.
fn project_row(it: &crate::forge_listing::RestIssue, fields: &[String]) -> Value {
    let mut obj = serde_json::Map::new();
    for f in fields {
        let v = match f.as_str() {
            "number" => json!(it.number),
            "title" => json!(it.title.clone().unwrap_or_default()),
            // gh emits state uppercase (OPEN/CLOSED); REST gives lowercase.
            "state" => json!(it.state.to_ascii_uppercase()),
            "body" => json!(it.body.clone().unwrap_or_default()),
            "labels" => Value::Array(
                it.labels
                    .iter()
                    .map(|name| json!({ "name": name }))
                    .collect(),
            ),
            "createdAt" => json!(it.created_at.clone().unwrap_or_default()),
            "updatedAt" => json!(it.updated_at.clone().unwrap_or_default()),
            "closedAt" => json!(it.closed_at.clone().unwrap_or_default()),
            "author" => json!({ "login": it.author.clone().unwrap_or_default() }),
            _ => Value::Null,
        };
        obj.insert(f.clone(), v);
    }
    Value::Object(obj)
}

/// Apply a `--jq` expression via the system `jq` (compact + raw, matching gh's
/// `--jq` output). Returns `None` (decline) when `jq` is missing or errors.
fn apply_jq(array: &Value, expr: &str) -> Option<String> {
    let input = serde_json::to_string(array).ok()?;
    let mut child = Command::new("jq")
        .arg("-c")
        .arg("-r")
        .arg(expr)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    use std::io::Write;
    child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Parse the `list` argument vector into a cacheable [`CachedQuery`], or `None`
/// to decline. Supports both `--flag value` and `--flag=value` forms.
pub fn parse_query(entity: &str, args: &[String]) -> Option<CachedQuery> {
    // args[0] is "list"; skip it.
    let mut positive_labels: Vec<String> = Vec::new();
    let mut negative_labels: Vec<String> = Vec::new();
    let mut state: Option<String> = None;
    let mut json_fields: Vec<String> = Vec::new();
    let mut jq: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut repo: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        // Split `--flag=value`.
        let (flag, inline_val): (&str, Option<&str>) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f, Some(v)),
            _ => (arg.as_str(), None),
        };
        // Fetch the value for a `--flag value` (or inline) flag; advances `i`.
        let take_val = |i: &mut usize| -> Option<String> {
            if let Some(v) = inline_val {
                Some(v.to_string())
            } else {
                *i += 1;
                args.get(*i).cloned()
            }
        };
        match flag {
            "--cached" => {}
            "--label" | "-l" => positive_labels.push(take_val(&mut i)?),
            "--state" | "-s" => state = Some(take_val(&mut i)?),
            "--limit" | "-L" => limit = Some(take_val(&mut i)?.parse().ok()?),
            "--repo" | "-R" => repo = Some(take_val(&mut i)?),
            "--json" => {
                let raw = take_val(&mut i)?;
                for field in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if !SUPPORTED_JSON_FIELDS.contains(&field) {
                        return None; // a field we cannot supply → decline
                    }
                    json_fields.push(field.to_string());
                }
            }
            "--jq" | "-q" => jq = Some(take_val(&mut i)?),
            "--search" | "-S" => {
                let raw = take_val(&mut i)?;
                for tok in raw.split_whitespace() {
                    if let Some(v) = tok.strip_prefix("-label:") {
                        negative_labels.push(v.to_string());
                    } else if let Some(v) = tok.strip_prefix("label:") {
                        positive_labels.push(v.to_string());
                    } else {
                        return None; // unsupported search term → decline
                    }
                }
            }
            // Any other flag / positional → not a shape we can serve.
            _ => return None,
        }
        i += 1;
    }

    // gh list without --json is a human table we cannot reproduce.
    if json_fields.is_empty() {
        return None;
    }

    let state = state.unwrap_or_else(|| "open".to_string());
    // PRs: only the open queue is serviceable from the issues endpoint.
    if entity == "pr" && state != "open" {
        return None;
    }
    if !matches!(state.as_str(), "open" | "closed" | "all") {
        return None;
    }

    Some(CachedQuery {
        positive_labels,
        negative_labels,
        state,
        json_fields,
        jq,
        limit,
        repo,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::forge_listing::RestIssue;

    fn s(v: &str) -> String {
        v.to_string()
    }
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| s(p)).collect()
    }

    fn issue(number: u32, labels: &[&str], is_pr: bool) -> RestIssue {
        RestIssue {
            number,
            title: Some(format!("issue {number}")),
            labels: labels.iter().map(|l| s(l)).collect(),
            created_at: Some(s("2026-08-03T00:00:00Z")),
            updated_at: Some(s("2026-08-03T01:00:00Z")),
            closed_at: None,
            state: s("open"),
            body: Some(s("body")),
            author: Some(s("octocat")),
            is_pull_request: is_pr,
        }
    }

    // ===== is_cached_list =====

    #[test]
    fn detects_cached_list_only_with_flag_and_list_verb() {
        assert!(is_cached_list(&argv(&["list", "--cached", "--json", "number"])));
        assert!(!is_cached_list(&argv(&["list", "--json", "number"])));
        assert!(!is_cached_list(&argv(&["view", "42", "--cached"])));
    }

    // ===== parse_query decline rules =====

    #[test]
    fn declines_without_json() {
        assert!(
            parse_query("issue", &argv(&["list", "--cached", "--label", "loom:issue"])).is_none()
        );
    }

    #[test]
    fn declines_unsupported_json_field() {
        // mergedAt / files are PR-endpoint fields we cannot supply.
        assert!(
            parse_query("pr", &argv(&["list", "--cached", "--json", "number,mergedAt"])).is_none()
        );
        assert!(parse_query("pr", &argv(&["list", "--cached", "--json", "files"])).is_none());
    }

    #[test]
    fn declines_unsupported_search_term() {
        assert!(parse_query(
            "pr",
            &argv(&[
                "list",
                "--cached",
                "--json",
                "number",
                "--search",
                "head:docs/x"
            ])
        )
        .is_none());
        assert!(parse_query(
            "issue",
            &argv(&[
                "list",
                "--cached",
                "--json",
                "number",
                "--search",
                "foo in:body"
            ])
        )
        .is_none());
    }

    #[test]
    fn declines_unknown_flag_or_positional() {
        assert!(parse_query(
            "issue",
            &argv(&["list", "--cached", "--json", "number", "--assignee", "me"])
        )
        .is_none());
        assert!(
            parse_query("issue", &argv(&["list", "--cached", "--json", "number", "42"])).is_none()
        );
    }

    #[test]
    fn declines_pr_non_open_state() {
        assert!(parse_query(
            "pr",
            &argv(&["list", "--cached", "--json", "number", "--state", "merged"])
        )
        .is_none());
    }

    // ===== parse_query success shapes =====

    #[test]
    fn parses_labels_state_search_json_forms() {
        let q = parse_query(
            "issue",
            &argv(&[
                "list",
                "--cached",
                "--label",
                "loom:issue",
                "--label=tier:goal-supporting",
                "--search",
                "-label:loom:building",
                "--state=open",
                "--json",
                "number,labels",
            ]),
        )
        .unwrap();
        assert_eq!(q.positive_labels, vec!["loom:issue", "tier:goal-supporting"]);
        assert_eq!(q.negative_labels, vec!["loom:building"]);
        assert_eq!(q.state, "open");
        assert_eq!(q.json_fields, vec!["number", "labels"]);
    }

    #[test]
    fn defaults_state_to_open() {
        let q = parse_query("issue", &argv(&["list", "--cached", "--json", "number"])).unwrap();
        assert_eq!(q.state, "open");
    }

    // ===== build_output: filtering + projection =====

    fn fetch_fixture(
        issues: Vec<RestIssue>,
        truncated: bool,
    ) -> impl Fn(&str, &str, Option<&str>) -> Option<CachedListing> {
        move |_labels: &str, _state: &str, _repo: Option<&str>| {
            Some(CachedListing {
                issues: issues.clone(),
                truncated,
            })
        }
    }

    #[test]
    fn issue_list_excludes_prs_and_applies_negative_labels() {
        let fixture = fetch_fixture(
            vec![
                issue(1, &["loom:issue"], false),
                issue(2, &["loom:issue", "loom:building"], false), // excluded by -label
                issue(3, &["loom:issue"], true),                   // a PR — excluded for issue list
            ],
            false,
        );
        let out = build_output(
            "issue",
            &argv(&[
                "list",
                "--cached",
                "--label",
                "loom:issue",
                "--search",
                "-label:loom:building",
                "--json",
                "number",
            ]),
            &fixture,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let nums: Vec<u64> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["number"].as_u64().unwrap())
            .collect();
        assert_eq!(nums, vec![1]);
    }

    #[test]
    fn pr_list_keeps_only_prs() {
        let fixture = fetch_fixture(
            vec![
                issue(10, &["loom:review-requested"], true),
                issue(11, &["loom:review-requested"], false), // an issue — excluded
            ],
            false,
        );
        let out = build_output(
            "pr",
            &argv(&[
                "list",
                "--cached",
                "--label",
                "loom:review-requested",
                "--json",
                "number",
            ]),
            &fixture,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["number"].as_u64().unwrap(), 10);
    }

    #[test]
    fn truncated_page_declines() {
        let fixture = fetch_fixture(vec![issue(1, &["loom:issue"], false)], true);
        assert!(
            build_output("issue", &argv(&["list", "--cached", "--json", "number"]), &fixture)
                .is_none()
        );
    }

    #[test]
    fn projects_gh_json_shape() {
        let fixture = fetch_fixture(vec![issue(7, &["loom:issue"], false)], false);
        let out = build_output(
            "issue",
            &argv(&[
                "list",
                "--cached",
                "--json",
                "number,title,state,labels,author,closedAt",
            ]),
            &fixture,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let row = &v[0];
        assert_eq!(row["number"].as_u64().unwrap(), 7);
        assert_eq!(row["title"].as_str().unwrap(), "issue 7");
        assert_eq!(row["state"].as_str().unwrap(), "OPEN"); // uppercased for gh parity
        assert_eq!(row["labels"][0]["name"].as_str().unwrap(), "loom:issue");
        assert_eq!(row["author"]["login"].as_str().unwrap(), "octocat");
        assert_eq!(row["closedAt"].as_str().unwrap(), ""); // null → empty string
                                                           // Only requested fields are present.
        assert!(row.get("body").is_none());
    }

    #[test]
    fn limit_truncates_result() {
        let fixture = fetch_fixture(
            vec![
                issue(1, &["loom:issue"], false),
                issue(2, &["loom:issue"], false),
                issue(3, &["loom:issue"], false),
            ],
            false,
        );
        let out = build_output(
            "issue",
            &argv(&["list", "--cached", "--json", "number", "--limit", "2"]),
            &fixture,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
    }

    #[test]
    fn jq_expression_applied_when_jq_present() {
        // Skip if jq is unavailable in the test environment.
        if Command::new("jq").arg("--version").output().is_err() {
            return;
        }
        let fixture = fetch_fixture(
            vec![
                issue(5, &["loom:issue"], false),
                issue(6, &["loom:issue"], false),
            ],
            false,
        );
        let out = build_output(
            "issue",
            &argv(&["list", "--cached", "--json", "number", "--jq", ".[].number"]),
            &fixture,
        )
        .unwrap();
        let nums: Vec<&str> = out.lines().collect();
        assert_eq!(nums, vec!["5", "6"]);
    }
}
