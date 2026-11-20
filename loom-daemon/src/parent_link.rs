//! Creation-time parent link (#10012 §4): what `create-issue.sh --parent N`
//! does, as daemon logic the script only passes a flag to.
//!
//! Two steps around the create:
//!
//! 1. [`child_body`] (before): append `<!-- loom:parent #N -->` to the child's
//!    body, and copy the parent's `<!-- loom:main-red-fix -->` marker when it
//!    has one (a body marker, not a label). Idempotent.
//! 2. [`link_created`] (after): link the child as a native sub-issue of the
//!    parent. Best effort; a failure there never un-files.
//!
//! Moved out of the removed star-liveness subsystem (#11104). The post-create
//! step no longer stars a child of a starred parent: star inheritance is gone
//! with that subsystem.

use std::path::Path;

use anyhow::Result;

use crate::claim_reconciliation::gh_call;

/// The `gh` program, from the shared resolver.
fn gh_program() -> std::path::PathBuf {
    std::path::PathBuf::from(crate::gh_invocation::gh_bin())
}

/// The red-main fix marker the work finder reads (at the start of a line).
pub const RED_MAIN_MARKER: &str = "<!-- loom:main-red-fix -->";

/// The parent marker for `parent`.
#[must_use]
pub fn parent_marker(parent: u32) -> String {
    format!("<!-- loom:parent #{parent} -->")
}

fn has_red_main(body: &str) -> bool {
    body.lines()
        .any(|l| l.trim_start().starts_with(RED_MAIN_MARKER))
}

/// `body` with the parent marker (and the parent's red-main marker, when
/// `parent_body` carries one) appended. A marker already present is not
/// repeated, so re-running a filing is harmless.
#[must_use]
pub fn child_body(body: &str, parent: u32, parent_body: Option<&str>) -> String {
    fn append(out: &mut String, marker: &str) {
        if !out.contains(marker) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(marker);
        }
    }
    let mut out = body.trim_end().to_string();
    append(&mut out, &parent_marker(parent));
    if parent_body.is_some_and(has_red_main) && !has_red_main(&out) {
        append(&mut out, RED_MAIN_MARKER);
    }
    out.push('\n');
    out
}

/// `gh api` arguments that create the native sub-issue link, given the
/// child's REST database id (the endpoint takes the id, not the number).
#[must_use]
pub fn sub_issue_link_args(slug: &str, parent: u32, child_db_id: u64) -> Vec<String> {
    vec![
        "api".into(),
        "-X".into(),
        "POST".into(),
        format!("repos/{slug}/issues/{parent}/sub_issues"),
        "-F".into(),
        format!("sub_issue_id={child_db_id}"),
    ]
}

/// The parent's body, `None` when it cannot be read (a rate limit must not
/// stop the filing: the marker is added regardless, only the red-main copy is
/// skipped).
#[must_use]
pub fn fetch_parent_body(root: &Path, slug: &str, parent: u32) -> Option<String> {
    let path = format!("repos/{slug}/issues/{parent}");
    let inv = gh_call::read("parent_link.parent_body", &gh_program(), root).args([
        "api",
        &path,
        "--jq",
        ".body // \"\"",
    ]);
    let out = gh_call::ok_stdout(inv)?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Post-create step: link `child` natively as a sub-issue of `parent`. Never
/// un-files.
///
/// # Errors
/// The child's id could not be read, or the link write failed.
pub fn link_created(root: &Path, slug: &str, parent: u32, child: u32) -> Result<()> {
    let gh = gh_program();
    let path = format!("repos/{slug}/issues/{child}");
    let id_inv =
        gh_call::read("parent_link.child_id", &gh, root).args(["api", &path, "--jq", ".id"]);
    let out = gh_call::output(id_inv)?;
    if !out.status.success() {
        anyhow::bail!("sub-issue link: could not read #{child}: {}", gh_call::stderr(&out));
    }
    let id: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("sub-issue link: #{child} has no numeric id"))?;
    let args = sub_issue_link_args(slug, parent, id);
    let link = gh_call::write("parent_link.sub_issue", &gh, root).args(&args);
    let out = gh_call::output(link)?;
    if out.status.success() {
        return Ok(());
    }
    let err = gh_call::stderr(&out);
    // Already linked is the goal state.
    if err.contains("already") {
        return Ok(());
    }
    anyhow::bail!("sub-issue link: {err}")
}

/// What a parent's children say about whether the parent is still work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitState {
    /// No child is known: not a split parent (or undecomposed). Never a
    /// completion signal.
    NoChildren,
    /// At least one child is still open: the parent legitimately waits.
    OpenChildren(usize),
    /// Children exist and every one is closed: the children carry the code,
    /// so the parent must not be built or promoted to `loom:issue` (#13635).
    AllClosed(Vec<u32>),
}

/// Classify `(number, is_closed)` pairs. Duplicates (the same child found by
/// both the marker search and the sub-issue list) collapse by number.
#[must_use]
pub fn classify_children(children: &[(u32, bool)]) -> SplitState {
    let mut seen: std::collections::BTreeMap<u32, bool> = std::collections::BTreeMap::new();
    for &(n, closed) in children {
        // A child seen open anywhere is open.
        let e = seen.entry(n).or_insert(closed);
        *e = *e && closed;
    }
    if seen.is_empty() {
        return SplitState::NoChildren;
    }
    let open = seen.values().filter(|c| !**c).count();
    if open > 0 {
        SplitState::OpenChildren(open)
    } else {
        SplitState::AllClosed(seen.keys().copied().collect())
    }
}

/// Parse `"<number> <STATE>"` lines (as emitted by the `--jq` filters below).
fn parse_child_lines(text: &str) -> Option<Vec<(u32, bool)>> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (n, state) = line.split_once(' ')?;
        let closed = match state.to_ascii_uppercase().as_str() {
            "CLOSED" | "MERGED" => true,
            "OPEN" => false,
            _ => return None,
        };
        out.push((n.parse().ok()?, closed));
    }
    Some(out)
}

/// Page size of both child reads: a full page means the list may be longer
/// than what was read, which is treated as unreadable (fail closed).
const CHILD_PAGE: usize = 100;

/// `--limit` of the marker search. Like `CHILD_PAGE`, a raw result that
/// reaches it may be truncated (#13670).
const MARKER_LIMIT: usize = CHILD_PAGE * 2;

/// Parse the marker search output: a leading `RAW <n>` line carrying the
/// **unfiltered** search count, then `"<number> <STATE>"` rows that survived
/// the jq marker filter. Fails closed when the raw count reaches
/// [`MARKER_LIMIT`], even if the filter dropped rows (the dropped rows may
/// have hidden an unread child).
fn parse_marker_output(text: &str, parent: u32) -> Result<Vec<(u32, bool)>> {
    let malformed = || anyhow::anyhow!("malformed marker-child list for #{parent}");
    let trimmed = text.trim_start();
    let (head, rest) = trimmed.split_once('\n').unwrap_or((trimmed, ""));
    let raw: usize = head
        .trim()
        .strip_prefix("RAW ")
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(malformed)?;
    if raw >= MARKER_LIMIT {
        anyhow::bail!("marker-child search of #{parent} may be truncated");
    }
    parse_child_lines(rest).ok_or_else(malformed)
}

/// Enumerate `parent`'s children from the native sub-issue list and from
/// `<!-- loom:parent #N -->` markers, and classify them.
///
/// # Errors
/// Either read failed, was malformed, or filled a whole page. An unreadable
/// list is never evidence that the parent is split and finished.
pub fn split_state(root: &Path, slug: &str, parent: u32) -> Result<SplitState> {
    split_state_with_gh(&gh_program(), root, slug, parent)
}

fn split_state_with_gh(gh: &Path, root: &Path, slug: &str, parent: u32) -> Result<SplitState> {
    let mut all: Vec<(u32, bool)> = Vec::new();

    let path = format!("repos/{slug}/issues/{parent}/sub_issues?per_page={CHILD_PAGE}&state=all");
    let sub = gh_call::read("parent_link.sub_issues", gh, root).args([
        "api",
        &path,
        "--jq",
        r#".[] | "\(.number) \(.state | ascii_upcase)""#,
    ]);
    let out = gh_call::ok_stdout(sub)
        .ok_or_else(|| anyhow::anyhow!("could not read sub-issues of #{parent}"))?;
    let rows = parse_child_lines(&String::from_utf8_lossy(&out))
        .ok_or_else(|| anyhow::anyhow!("malformed sub-issue list for #{parent}"))?;
    if rows.len() >= CHILD_PAGE {
        anyhow::bail!("sub-issue list of #{parent} may be truncated");
    }
    all.extend(rows);

    // The search is a loose text match; the jq select re-checks the exact
    // marker so #12 never matches #123.
    let query = format!("\"loom:parent #{parent}\" in:body");
    let jq = format!(
        r#"length as $n | "RAW \($n)", (.[] | select(.body | contains("{}")) | "\(.number) \(.state)")"#,
        parent_marker(parent)
    );
    let limit = MARKER_LIMIT.to_string();
    let marker = gh_call::read("parent_link.marker_children", gh, root).args([
        "issue",
        "list",
        "--repo",
        slug,
        "--state",
        "all",
        "--limit",
        &limit,
        "--search",
        &query,
        "--json",
        "number,state,body",
        "--jq",
        &jq,
    ]);
    let out = gh_call::graphql_output(marker)
        .map_err(|_| anyhow::anyhow!("could not search marker children of #{parent}"))?;
    let rows = match out {
        Some(out) if out.status.success() => {
            parse_marker_output(&String::from_utf8_lossy(&out.stdout), parent)?
        }
        None => rest_marker_children(gh, root, slug, parent, &query)?,
        Some(_) => anyhow::bail!("could not search marker children of #{parent}"),
    };
    all.extend(rows.into_iter().filter(|&(n, _)| n != parent));
    Ok(classify_children(&all))
}

#[derive(serde::Deserialize)]
struct RestMarkerPage {
    total_count: usize,
    incomplete_results: bool,
    items: Vec<RestMarkerItem>,
}

#[derive(serde::Deserialize)]
struct RestMarkerItem {
    number: u32,
    state: RestIssueState,
    // A null body is normal. A missing or non-string body could hide a marker.
    body: serde_json::Value,
    repository_url: String,
    #[serde(default, rename = "pull_request", deserialize_with = "field_present")]
    has_pull_request: bool,
}

fn field_present<'de, D: serde::Deserializer<'de>>(value: D) -> Result<bool, D::Error> {
    let _: serde::de::IgnoredAny = serde::Deserialize::deserialize(value)?;
    Ok(true)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum RestIssueState {
    Open,
    Closed,
}

fn rest_marker_children(
    gh: &Path,
    root: &Path,
    slug: &str,
    parent: u32,
    query: &str,
) -> Result<Vec<(u32, bool)>> {
    let query_arg = format!("q={query} repo:{slug} is:issue");
    let per_page = format!("per_page={CHILD_PAGE}");
    let marker = parent_marker(parent);
    let mut first_total = None;
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for page in 1..=MARKER_LIMIT.div_ceil(CHILD_PAGE) {
        let page_arg = format!("page={page}");
        // The scoped query carries its repo in q, not in the REST path.
        // Name the target explicitly so the normal reader routing sees it.
        let read = gh_call::read("parent_link.marker_children_rest", gh, root)
            .reader_repo(slug)
            .args([
                "api",
                "search/issues",
                "--method",
                "GET",
                "-f",
                &query_arg,
                "-F",
                &per_page,
                "-F",
                &page_arg,
            ]);
        let reply = gh_call::ok_stdout(read)
            .ok_or_else(|| anyhow::anyhow!("could not search REST marker children of #{parent}"))?;
        let malformed = || anyhow::anyhow!("malformed REST marker-child list for #{parent}");
        let response: RestMarkerPage = serde_json::from_slice(&reply).map_err(|_| malformed())?;
        if response.incomplete_results
            || response.total_count >= MARKER_LIMIT
            || first_total.is_some_and(|total| total != response.total_count)
            || response.items.len()
                != response
                    .total_count
                    .saturating_sub(seen.len())
                    .min(CHILD_PAGE)
        {
            anyhow::bail!("REST marker-child search of #{parent} may be truncated");
        }
        for item in response.items {
            let scoped = item
                .repository_url
                .rsplit_once("/repos/")
                .is_some_and(|(_, repo)| repo.eq_ignore_ascii_case(slug));
            if item.number == 0 || !scoped || item.has_pull_request || !seen.insert(item.number) {
                return Err(malformed());
            }
            if item.body.is_null() {
                continue;
            }
            if item.body.as_str().ok_or_else(malformed)?.contains(&marker) {
                rows.push((item.number, matches!(item.state, RestIssueState::Closed)));
            }
        }
        if seen.len() == response.total_count {
            return Ok(rows);
        }
        first_total = Some(response.total_count);
    }
    anyhow::bail!("REST marker-child search of #{parent} may be truncated");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_no_children_is_not_a_split_parent() {
        assert_eq!(classify_children(&[]), SplitState::NoChildren);
    }

    #[test]
    fn classify_all_closed_dedups_and_sorts() {
        assert_eq!(
            classify_children(&[(9, true), (7, true), (9, true)]),
            SplitState::AllClosed(vec![7, 9])
        );
    }

    #[test]
    fn classify_any_open_child_wins_even_if_seen_closed_elsewhere() {
        assert_eq!(
            classify_children(&[(7, true), (8, false), (8, true)]),
            SplitState::OpenChildren(1)
        );
    }

    #[test]
    fn child_lines_parse_and_reject_garbage() {
        assert_eq!(
            parse_child_lines("7 CLOSED\n8 OPEN\n\n9 MERGED\n"),
            Some(vec![(7, true), (8, false), (9, true)])
        );
        assert_eq!(parse_child_lines(""), Some(vec![]));
        assert_eq!(parse_child_lines("7 WEIRD"), None);
        assert_eq!(parse_child_lines("x CLOSED"), None);
    }

    #[test]
    fn marker_output_below_limit_succeeds() {
        let text = format!("RAW {}\n7 CLOSED\n8 OPEN\n", MARKER_LIMIT - 1);
        assert_eq!(parse_marker_output(&text, 1).unwrap(), vec![(7, true), (8, false)]);
    }

    #[test]
    fn marker_output_at_limit_bails_even_when_filter_dropped_rows() {
        // Raw count at the limit, zero rows survived the filter.
        let text = format!("RAW {MARKER_LIMIT}\n");
        assert!(parse_marker_output(&text, 1).is_err());
        let text = format!("RAW {MARKER_LIMIT}\n7 CLOSED\n");
        assert!(parse_marker_output(&text, 1).is_err());
    }

    #[test]
    fn marker_output_without_raw_count_is_malformed() {
        assert!(parse_marker_output("7 CLOSED\n", 1).is_err());
        assert!(parse_marker_output("", 1).is_err());
        assert!(parse_marker_output("RAW x\n", 1).is_err());
    }

    #[cfg(unix)]
    fn mocked_split(
        native: &str,
        graphql_diagnostic: &str,
        pages: &[serde_json::Value],
    ) -> Result<SplitState> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let gh = dir.path().join("gh-marker-fixture.sh");
        let script = r#"#!/bin/sh
case "$1 $2" in
  "api repos/o/r/issues/12/sub_issues?per_page=100&state=all") cat native ;;
  "issue list") cat diagnostic >&2; exit 1 ;;
  "api search/issues")
    test "$3" = "--method" && test "$4" = "GET" &&
    test "$6" = 'q="loom:parent #12" in:body repo:o/r is:issue' &&
    test "$8" = "per_page=100" || exit 76
    if test -f "${10}"; then cat "${10}"; else exit 1; fi
    ;;
  *) exit 76 ;;
esac"#;
        std::fs::write(&gh, script)?;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700))?;
        std::fs::write(dir.path().join("native"), native)?;
        std::fs::write(dir.path().join("diagnostic"), graphql_diagnostic)?;
        for (index, page) in pages.iter().enumerate() {
            std::fs::write(dir.path().join(format!("page={}", index + 1)), page.to_string())?;
        }
        split_state_with_gh(&gh, dir.path(), "o/r", 12)
    }

    #[cfg(unix)]
    const GRAPHQL_EXHAUSTED: &str =
        "GraphQL: API rate limit already exceeded for installation ID 1";

    #[cfg(unix)]
    fn marker_row(number: u32, state: &str, body: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "number": number, "state": state, "body": body,
            "repository_url": "https://api.github.com/repos/o/r"
        })
    }

    #[cfg(unix)]
    fn marker_page(total: usize, items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"total_count": total, "incomplete_results": false, "items": items})
    }

    #[cfg(unix)]
    #[test]
    fn rest_marker_search_preserves_the_exact_union_and_filters_self_links() {
        assert_eq!(
            mocked_split("", GRAPHQL_EXHAUSTED, &[marker_page(0, Vec::new())]).unwrap(),
            SplitState::NoChildren,
        );
        let closed = marker_row(7, "closed", parent_marker(12).into());
        let matches = marker_page(
            4,
            vec![
                closed.clone(),
                marker_row(8, "open", parent_marker(123).into()),
                marker_row(12, "open", parent_marker(12).into()),
                marker_row(99, "open", serde_json::Value::Null),
            ],
        );
        assert_eq!(
            mocked_split("7 CLOSED\n9 CLOSED\n", GRAPHQL_EXHAUSTED, &[matches]).unwrap(),
            SplitState::AllClosed(vec![7, 9])
        );
        let open = marker_page(1, vec![marker_row(7, "open", parent_marker(12).into())]);
        assert_eq!(
            mocked_split("7 CLOSED\n9 CLOSED\n", GRAPHQL_EXHAUSTED, &[open]).unwrap(),
            SplitState::OpenChildren(1)
        );
    }

    #[cfg(unix)]
    #[test]
    fn rest_marker_search_checks_both_pages_before_classifying() {
        let first = marker_page(
            CHILD_PAGE + 1,
            (1..=100)
                .map(|n| marker_row(n, "open", serde_json::Value::Null))
                .collect(),
        );
        let second = marker_page(101, vec![marker_row(101, "open", parent_marker(12).into())]);
        assert_eq!(
            mocked_split("", GRAPHQL_EXHAUSTED, &[first.clone(), second]).unwrap(),
            SplitState::OpenChildren(1)
        );
        assert!(mocked_split("", GRAPHQL_EXHAUSTED, std::slice::from_ref(&first)).is_err());
        let duplicate = marker_page(101, vec![marker_row(100, "open", parent_marker(12).into())]);
        assert!(mocked_split("", GRAPHQL_EXHAUSTED, &[first, duplicate]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rest_marker_search_fails_closed_on_missing_or_truncated_evidence() {
        let mut wrong_repo = marker_row(7, "open", parent_marker(12).into());
        wrong_repo["repository_url"] = "https://api.github.com/repos/other/r".into();
        let mut malformed = marker_row(7, "open", parent_marker(12).into());
        malformed.as_object_mut().unwrap().remove("body");
        let mut wrong_kind = marker_row(7, "open", parent_marker(12).into());
        wrong_kind["pull_request"] = serde_json::json!({});
        let mut malformed_kind = wrong_kind.clone();
        malformed_kind["pull_request"] = serde_json::Value::Null;
        for page in [
            marker_page(MARKER_LIMIT, Vec::new()),
            marker_page(1, Vec::new()),
            marker_page(1, vec![wrong_repo]),
            marker_page(1, vec![malformed]),
            marker_page(1, vec![wrong_kind]),
            marker_page(1, vec![malformed_kind]),
            serde_json::json!({"total_count": 0, "incomplete_results": true, "items": []}),
            serde_json::json!({"total_count": 0, "items": []}),
        ] {
            assert!(mocked_split("7 CLOSED\n", GRAPHQL_EXHAUSTED, &[page]).is_err());
        }
        assert!(mocked_split("7 CLOSED\n", GRAPHQL_EXHAUSTED, &[]).is_err());
        assert!(mocked_split(
            "",
            "HTTP 403: Resource not accessible by integration",
            &[marker_page(0, Vec::new())],
        )
        .is_err());
    }

    #[test]
    fn body_gets_the_parent_marker() {
        let body = child_body("Do the thing.", 7, None);
        assert!(body.starts_with("Do the thing.\n\n"));
        assert!(body.contains(&parent_marker(7)));
    }

    #[test]
    fn marker_is_not_repeated_and_empty_body_works() {
        let once = child_body("", 7, None);
        assert_eq!(once, format!("{}\n", parent_marker(7)));
        assert_eq!(child_body(&once, 7, None), once);
    }

    #[test]
    fn red_main_marker_is_copied_only_when_the_parent_has_it() {
        let parent = format!("Fix main.\n{RED_MAIN_MARKER}\n");
        let with = child_body("Part.", 7, Some(&parent));
        assert!(with.contains(RED_MAIN_MARKER));
        assert_eq!(child_body(&with, 7, Some(&parent)), with, "idempotent");
        let without = child_body("Part.", 7, Some("plain parent"));
        assert!(!without.contains(RED_MAIN_MARKER));
        // Mentioned in prose, not at a line start: not a marker.
        let prose = format!("see {RED_MAIN_MARKER} for the idea");
        assert!(!child_body("Part.", 7, Some(&prose)).contains(RED_MAIN_MARKER));
    }

    #[test]
    fn sub_issue_link_uses_the_database_id() {
        let args = sub_issue_link_args("o/r", 7, 123_456);
        assert_eq!(args[3], "repos/o/r/issues/7/sub_issues");
        assert_eq!(args[5], "sub_issue_id=123456");
    }
}
