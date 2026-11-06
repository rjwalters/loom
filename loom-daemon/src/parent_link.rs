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

/// Enumerate `parent`'s children from the native sub-issue list and from
/// `<!-- loom:parent #N -->` markers, and classify them.
///
/// # Errors
/// Either read failed, was malformed, or filled a whole page. An unreadable
/// list is never evidence that the parent is split and finished.
pub fn split_state(root: &Path, slug: &str, parent: u32) -> Result<SplitState> {
    let gh = gh_program();
    let mut all: Vec<(u32, bool)> = Vec::new();

    let path = format!("repos/{slug}/issues/{parent}/sub_issues?per_page={CHILD_PAGE}&state=all");
    let sub = gh_call::read("parent_link.sub_issues", &gh, root).args([
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
        r#".[] | select(.body | contains("{}")) | "\(.number) \(.state)""#,
        parent_marker(parent)
    );
    let limit = (CHILD_PAGE * 2).to_string();
    let marker = gh_call::read("parent_link.marker_children", &gh, root).args([
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
    let out = gh_call::ok_stdout(marker)
        .ok_or_else(|| anyhow::anyhow!("could not search marker children of #{parent}"))?;
    let rows = parse_child_lines(&String::from_utf8_lossy(&out))
        .ok_or_else(|| anyhow::anyhow!("malformed marker-child list for #{parent}"))?;
    all.extend(rows.into_iter().filter(|&(n, _)| n != parent));
    Ok(classify_children(&all))
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
