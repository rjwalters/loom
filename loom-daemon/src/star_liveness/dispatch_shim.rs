//! Legacy star readers kept **only** for `star_liveness/` (#11104) and the
//! non-dispatch readers #11510 removes (`forge_priority_labels`).
//!
//! #11508 took every star key out of dispatch: the work finder no longer
//! lists, orders or admits by `loom:operator-priority`, its levels, or an
//! inherited star. These items moved here unchanged from
//! `work_finder/operator_priority.rs` so the subsystem that still reads them
//! keeps compiling; `work_finder::operator_priority` re-exports the three its
//! callers import by path. Nothing in `work_finder` calls them. #11104
//! deletes this file with the rest of `star_liveness/`.

use std::path::PathBuf;

use anyhow::{anyhow, Result};

use crate::work_finder::WorkItem;

impl WorkItem {
    /// True when the issue is starred at any level (#9244, #10307): it
    /// carries a level label ([`crate::operator_levels`], own or inherited),
    /// or blocks a starred issue and inherits its star (#9244 C). Star-liveness
    /// treats them alike.
    #[must_use]
    pub fn is_operator_priority(&self) -> bool {
        self.operator_level() >= 1
    }

    /// The issue's effective operator priority level (#10307): the highest
    /// of its own level labels, its inherited level labels, and the
    /// in-memory inherited star (level 1). 0 = not starred.
    #[must_use]
    pub fn operator_level(&self) -> u8 {
        self.operator_level_in(crate::operator_levels::table())
    }

    /// [`Self::operator_level`] against an explicit level `table`.
    #[must_use]
    pub fn operator_level_in(&self, table: &[crate::operator_levels::PriorityLevel]) -> u8 {
        let in_memory = u8::from(self.operator_priority_inherited_from.is_some());
        crate::operator_levels::level_in(table, &self.labels).max(in_memory)
    }
}

/// Fold star-liveness's starred rows into `ready`, deduped by issue number,
/// dropping the rows
/// [`crate::work_finder::operator_priority::excluded_from_side_listing`]
/// drops (claimed, being curated, or Champion-path).
#[must_use]
pub fn merge_starred(mut ready: Vec<WorkItem>, starred: Vec<WorkItem>) -> Vec<WorkItem> {
    let listed: std::collections::HashSet<u32> = ready.iter().map(|i| i.number).collect();
    ready.extend(starred.into_iter().filter(|i| {
        !listed.contains(&i.number)
            && !crate::work_finder::operator_priority::excluded_from_side_listing(i)
    }));
    ready
}

/// Where a starred issue's starred-at comes from.
///
/// The production source reads the issue's forge timeline
/// ([`GhTimelineStarredAt`]); tests pass a fake. Slice C of #9244 adds the
/// loom-ui intent `requested_at` as a higher-precedence source by wrapping
/// this trait: a wrapper that answers from the intent marker first and falls
/// through to the timeline is the whole seam.
pub trait StarredAtSource {
    /// When `issue` was last labeled
    /// [`crate::work_finder::OPERATOR_PRIORITY_LABEL`], or `None` when the
    /// timeline carries no such event.
    ///
    /// # Errors
    ///
    /// Returns an error when the read itself failed; callers treat that like
    /// `None`.
    fn starred_at(&mut self, issue: u32) -> Result<Option<String>>;

    /// A starred-at that outranks everything cached for `issue`, answered
    /// without a forge read (the loom-ui intent's `requested_at`). Consulted
    /// before the restart store, so a persisted value never overrides an
    /// intent. The default has none.
    fn authoritative(&mut self, _issue: u32) -> Option<String> {
        None
    }
}

/// The `--jq` program for [`GhTimelineStarredAt`]: `L <created_at>` per
/// `labeled` event for any level label ([`crate::operator_levels`], #10307),
/// and `C <created_at> <requested_at> <author_association> <login>` per
/// loom-ui star-intent audit comment (#9244 C, any level), whose
/// `requested_at` is the authoritative starred-at when its author is
/// trusted. Parsed by
/// [`crate::star_liveness::intents::starred_at_from_timeline`].
///
/// The starred-at is therefore when the issue last *reached* a level, and
/// the cache re-reads it when the level changes: a level-2 issue orders
/// among level-2 issues by when it was raised. A demotion back to the star
/// keeps the later (level-2) time, which only orders it later among stars.
#[must_use]
pub fn starred_at_jq() -> String {
    let labels = crate::operator_levels::starred_labels(crate::operator_levels::table())
        .iter()
        .map(|l| format!(".label.name == \"{l}\""))
        .collect::<Vec<_>>()
        .join(" or ");
    format!(
        r#".[] | if (.event == "labeled" and ({labels})) then "L \(.created_at)" elif (.event == "commented" and ((.body // "") | contains("loom:operator-priority-intent=") and contains("action=star"))) then "C \(.created_at) \((.body | capture("requested_at=(?<t>[^ >]+)") | .t) // "-") \(.author_association // "-") \(.actor.login // .user.login // "-")" else empty end"#
    )
}

/// The production [`StarredAtSource`]: a REST `gh api …/issues/<n>/timeline`
/// read (the larger pool, not GraphQL), made only for starred issues the
/// cache does not already know.
pub struct GhTimelineStarredAt {
    /// The `gh` binary.
    pub gh_bin: PathBuf,
    /// The workspace root `gh` resolves `{owner}/{repo}` from.
    pub cwd: Option<PathBuf>,
    /// An explicit `owner/repo`, when the caller has one.
    pub repo: Option<String>,
}

/// The facade target and REST path for a timeline read of `issue`.
///
/// The slug is the explicit `repo`, else the machine-global `LOOM_REPO`
/// (`loom_repo`) — exactly what gh's `{owner}/{repo}` placeholder resolves to,
/// since the facade sets `GH_REPO` from it — so the call is typed
/// ([`crate::gh_invocation::GhTarget::Repo`]) and its URL names the repo.
/// With neither, the placeholder stays and gh resolves it from the working
/// directory as before.
#[must_use]
pub fn timeline_target(
    repo: Option<&str>,
    loom_repo: Option<&str>,
    issue: u32,
) -> (crate::gh_invocation::GhTarget, String) {
    let typed = repo.or(loom_repo).map(str::trim).and_then(|slug| {
        crate::gh_invocation::GhTarget::repo(slug)
            .ok()
            .map(|t| (t, slug))
    });
    match typed {
        Some((target, slug)) => (target, format!("repos/{slug}/issues/{issue}/timeline")),
        None => (
            crate::gh_invocation::GhTarget::None,
            format!("repos/{{owner}}/{{repo}}/issues/{issue}/timeline"),
        ),
    }
}

impl StarredAtSource for GhTimelineStarredAt {
    fn starred_at(&mut self, issue: u32) -> Result<Option<String>> {
        if crate::rate_limit_breaker::global_skip_pass("work_finder") {
            return Err(anyhow!("rate-limit breaker is suppressing forge reads"));
        }
        let loom_repo = std::env::var("LOOM_REPO")
            .ok()
            .filter(|r| !r.trim().is_empty());
        let (target, url) = timeline_target(self.repo.as_deref(), loom_repo.as_deref(), issue);
        // #10089: counted via the facade (`work_finder.starred_at`); with no
        // cwd the facade runs in the daemon's own directory.
        let mut inv = crate::gh_invocation::GhInvocation::new(
            crate::gh_invocation::Operation::new("work_finder.starred_at"),
            crate::gh_invocation::AccessIntent::Read,
            target,
            crate::claim_reconciliation::gh_call::GH_TIMEOUT,
        )
        .forge_op(crate::forge_call_stats::ops::TIMELINE_READ)
        .program(&self.gh_bin)
        .args(["api", &url])
        .args(["--paginate", "--jq", &starred_at_jq()]);
        if let Some(dir) = self.cwd.as_deref() {
            inv = inv.current_dir(dir);
        }
        let out = crate::claim_reconciliation::gh_call::output(inv)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            crate::rate_limit_breaker::global_observe_failure(&stderr, "work_finder_starred_at");
            return Err(anyhow!("gh api timeline for #{issue} failed: {stderr}"));
        }
        Ok(crate::star_liveness::intents::starred_at_from_timeline(
            &String::from_utf8_lossy(&out.stdout),
        ))
    }
}
