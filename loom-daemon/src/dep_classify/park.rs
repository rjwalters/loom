//! Parking a dependency-only finding on `loom:blocked` (#11861).
//!
//! A proposal whose only recurring finding is an open dependency is waiting,
//! not stuck on a human. Before #11861 the shape could end on `loom:operator`
//! (an N=2 escalation the dependency-timing gate did not catch, healed later by
//! Champion's Pass 0 un-escalation), or stay `loom:curated` with no record at
//! all (the gate's `DEFER`). `loom:operator` is now a no-op for every role, so
//! neither is a resting state: the finding becomes `loom:blocked` with one park
//! record per open blocker (`park-record.md`), which stays until a human,
//! Guide or Champion lifts it once the blockers close (#11301). Champion
//! re-evaluates the released proposal through the same gate (`REEVALUATE`).
//!
//! The write goes through [`crate::park_record::apply`], the one shared way to
//! apply `loom:blocked`: body record first, then the label, so there is never
//! a label-only park.
//!
//! # Cross-repo blockers are not parked
//!
//! The release pass resolves local blockers only and skips a park naming
//! another repo's artifact (`Skip::CrossRepo`), so such a park would never
//! lift. [`plan`] refuses it; the caller leaves the item where it was.

use crate::park_record::BlockerRef;

/// The park record `reason` this module writes.
pub const PARK_REASON: &str = "dependency-only finding";

/// The park record `by` this module writes.
pub const PARK_BY: &str = "champion";

/// What a park would do, decided from the open blocker set alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Park on these local blockers.
    Park(Vec<BlockerRef>),
    /// At least one open blocker is in another repo: do not park.
    CrossRepo(Vec<String>),
    /// No open blocker, or one that does not parse: nothing to park on.
    Nothing,
}

/// Plan a park on `open` (`OWNER/REPO#N` references, as the classifier
/// prints them), for an issue in `repo`.
#[must_use]
pub fn plan(repo: &str, open: &[String]) -> Plan {
    let mut local = Vec::new();
    let mut foreign = Vec::new();
    for r in open {
        match r.parse::<BlockerRef>() {
            Ok(b) if b.is_local(Some(repo)) => local.push(BlockerRef::local(b.number)),
            Ok(_) => foreign.push(r.clone()),
            Err(_) => return Plan::Nothing,
        }
    }
    if !foreign.is_empty() {
        return Plan::CrossRepo(foreign);
    }
    if local.is_empty() {
        return Plan::Nothing;
    }
    local.sort();
    local.dedup();
    Plan::Park(local)
}

/// Apply a [`Plan::Park`]: one park record per blocker, then `loom:blocked`,
/// then drop `remove_labels`. `Err` carries the apply output on failure.
///
/// # Errors
/// The park-record apply refused or a forge write failed.
pub fn apply(
    repo_root: &std::path::Path,
    repo: &str,
    issue: u64,
    blockers: &[BlockerRef],
    remove_labels: &[String],
) -> Result<(), String> {
    let req = crate::park_record::apply::ApplyRequest {
        number: issue,
        repo: Some(repo.to_string()),
        blocked_by: blockers.to_vec(),
        reason: Some(PARK_REASON.to_string()),
        by: Some(PARK_BY.to_string()),
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        remove_labels: remove_labels.to_vec(),
        dry_run: false,
    };
    let mut forge = crate::operator_decision::cli::GhForge::new(
        repo_root.to_path_buf(),
        Some(repo.to_string()),
    )
    .with_call_site("dep_classify.park");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::park_record::apply::apply(&mut forge, &req, &mut out, &mut err);
    if code == crate::park_record::apply::exit::OK {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&err).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn local_blockers_park_deduplicated_and_sorted() {
        assert_eq!(
            plan("o/r", &s(&["o/r#9", "O/R#3", "o/r#9"])),
            Plan::Park(vec![BlockerRef::local(3), BlockerRef::local(9)])
        );
    }

    #[test]
    fn a_cross_repo_blocker_refuses_the_park() {
        assert_eq!(plan("o/r", &s(&["o/r#9", "x/y#2"])), Plan::CrossRepo(s(&["x/y#2"])));
    }

    #[test]
    fn nothing_open_or_unparseable_parks_nothing() {
        assert_eq!(plan("o/r", &[]), Plan::Nothing);
        assert_eq!(plan("o/r", &s(&["not a ref"])), Plan::Nothing);
    }
}
