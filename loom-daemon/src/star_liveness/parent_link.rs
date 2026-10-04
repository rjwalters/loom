//! Creation-time parent link (#10012 §4): what `create-issue.sh --parent N`
//! does, as daemon logic the script only passes a flag to.
//!
//! Two steps around the create:
//!
//! 1. [`child_body`] (before): append `<!-- loom:parent #N -->` to the child's
//!    body, and copy the parent's `<!-- loom:main-red-fix -->` marker when it
//!    has one (§6: a body marker, not a label, so propagation never edits
//!    bodies later). Idempotent.
//! 2. [`link_created`] (after): when the parent is starred, put the star on
//!    the child in the same call and post the inherited audit comment
//!    ([`super::inherited_star::marker`], `requested_at` = the parent's
//!    starred-at). The native sub-issue link is the caller's best-effort
//!    extra ([`sub_issue_link_args`]); a failure there never un-files.
//!
//! The star write is a **seam**: [`star_child`] takes a [`StarForge`], so the
//! materialization pass (the §2 slice, built on #9975's `forge star` path)
//! can call the very same function. It is a no-op unless the parent carries
//! the star *now*, so the daemon never stars a child on its own judgment.

use anyhow::Result;

use super::forge::StarForge;
use super::inherited_star::marker;
use crate::work_finder::OPERATOR_PRIORITY_LABEL;

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

/// What [`star_child`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StarOutcome {
    /// The parent does not carry the star: nothing written.
    ParentNotStarred,
    /// The child carried it already (nothing written beyond a missing audit
    /// comment, which would be owned by whoever starred it).
    AlreadyStarred,
    /// Label added and audit comment posted.
    Starred,
}

/// Star `child` because `parent` is starred.
///
/// `starred_at` is the **parent's** starred-at (carried as the marker's
/// `requested_at`, so the child orders at the parent's star time).
/// Idempotent: a child that already carries the star is left untouched. The
/// label and its audit comment are written together or not at all: a failed
/// audit post rolls the label back.
///
/// # Errors
/// A forge read or write failed.
pub fn star_child(
    forge: &mut dyn StarForge,
    parent: u32,
    child: u32,
    starred_at: Option<&str>,
) -> Result<StarOutcome> {
    let starred = forge
        .issue(parent)?
        .is_some_and(|p| p.labels.iter().any(|l| l == OPERATOR_PRIORITY_LABEL));
    if !starred {
        return Ok(StarOutcome::ParentNotStarred);
    }
    let Some(existing) = forge.issue(child)? else {
        anyhow::bail!("#{child} does not exist");
    };
    // A star the child already has belongs to whoever put it there: leave it
    // (and its provenance) untouched.
    if existing.labels.iter().any(|l| l == OPERATOR_PRIORITY_LABEL) {
        return Ok(StarOutcome::AlreadyStarred);
    }
    forge.add_label(child, OPERATOR_PRIORITY_LABEL)?;
    // The star is ours (it was absent a line ago), so this labeling generation
    // has no audit yet: always post, never dedupe against an older comment
    // (a star -> unstar -> re-star cycle leaves the previous generation's
    // audit behind, and the owner rule would read it as pre-dating the label).
    if let Err(e) = forge.post_comment(child, &marker(parent, child, starred_at)) {
        // A star with no provenance would read as the operator's own, and the
        // next call would see it as `AlreadyStarred` and never repair it. Take
        // back the label we just wrote so a retry starts clean.
        return Err(match forge.remove_label(child, OPERATOR_PRIORITY_LABEL) {
            Ok(()) => e.context(format!("audit comment on #{child} failed; star rolled back")),
            Err(re) => e.context(format!(
                "audit comment on #{child} failed and the star could not be rolled back ({re}): \
                 remove {OPERATOR_PRIORITY_LABEL} from #{child} and retry"
            )),
        });
    }
    Ok(StarOutcome::Starred)
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

// ---------------------------------------------------------------------------
// Production glue (`loom-daemon forge parent body|link`).
// ---------------------------------------------------------------------------

use std::path::Path;

use super::forge::GhStarForge;
use crate::work_finder::operator_priority::{GhTimelineStarredAt, StarredAtSource};

/// The parent's body, `None` when it cannot be read (a rate limit must not
/// stop the filing: the marker is added regardless, only the red-main copy is
/// skipped).
#[must_use]
pub fn fetch_parent_body(root: &Path, slug: &str, parent: u32) -> Option<String> {
    GhStarForge::new(root, slug)
        .issue(parent)
        .ok()
        .flatten()
        .and_then(|p| p.body)
}

/// Post-create step: star the child when the parent is starred, then link it
/// natively. Both are best effort and independent; every failure is named in
/// the returned error so the script can print one note. Never un-files.
///
/// # Errors
/// One or more of the two steps failed (the other may have succeeded).
pub fn link_created(root: &Path, slug: &str, parent: u32, child: u32) -> Result<StarOutcome> {
    let mut forge = GhStarForge::new(root, slug);
    let mut problems: Vec<String> = Vec::new();
    let mut outcome = StarOutcome::ParentNotStarred;
    let parent_starred = forge
        .issue(parent)
        .map(|p| p.is_some_and(|p| p.labels.iter().any(|l| l == OPERATOR_PRIORITY_LABEL)));
    match parent_starred {
        Ok(true) => {
            let at = GhTimelineStarredAt {
                gh_bin: forge.gh_bin.clone(),
                cwd: Some(root.to_path_buf()),
                repo: Some(slug.to_string()),
            }
            .starred_at(parent)
            .ok()
            .flatten();
            match star_child(&mut forge, parent, child, at.as_deref()) {
                Ok(o) => outcome = o,
                Err(e) => problems.push(format!("star: {e}")),
            }
        }
        Ok(false) => {}
        Err(e) => problems.push(format!("star: could not read #{parent}: {e}")),
    }
    if let Err(e) = link_sub_issue(&forge, parent, child) {
        problems.push(format!("sub-issue link: {e}"));
    }
    if problems.is_empty() {
        Ok(outcome)
    } else {
        Err(anyhow::anyhow!(problems.join("; ")))
    }
}

fn link_sub_issue(forge: &GhStarForge, parent: u32, child: u32) -> Result<()> {
    use crate::claim_reconciliation::gh_call;
    let path = format!("repos/{}/issues/{child}", forge.slug);
    let id_inv = gh_call::read("parent_link.child_id", &forge.gh_bin, &forge.root)
        .args(["api", &path, "--jq", ".id"]);
    let out = gh_call::output(id_inv)?;
    if !out.status.success() {
        anyhow::bail!("could not read #{child}: {}", gh_call::stderr(&out));
    }
    let id: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("#{child} has no numeric id"))?;
    let args = sub_issue_link_args(&forge.slug, parent, id);
    let link = gh_call::write("parent_link.sub_issue", &forge.gh_bin, &forge.root).args(&args);
    let out = gh_call::output(link)?;
    if out.status.success() {
        return Ok(());
    }
    let err = gh_call::stderr(&out);
    // Already linked is the goal state.
    if err.contains("already") {
        return Ok(());
    }
    anyhow::bail!("{err}")
}
