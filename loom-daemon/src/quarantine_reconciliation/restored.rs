//! Startup reconciliation that keeps a live park (Issue #11930).
//!
//! # The gap
//!
//! The #4110 pass released **every** daemon-marked `loom:blocked` at startup,
//! on the reasoning that a fresh daemon's quarantine memory is empty, so
//! nothing would ever release the label otherwise. That was true for a
//! one-host daemon. On a fleet it undid parks wholesale: a park set by a peer
//! a minute earlier, or by this host just before a roll, was released by the
//! next daemon to start, and with several workers restarting per release the
//! fast-fail park (#11854) rarely lasted more than a few minutes.
//!
//! # The rule
//!
//! The forge is the source of truth for a park: `loom:blocked`, the body park
//! record, and the anchored quarantine marker comment. At startup this pass
//! releases a marked park **only when it can prove the park has expired**: its
//! marker is at least the base quarantine TTL old. A younger park, whoever set
//! it, stays on the forge, and its marker's timestamp is handed to this
//! host's [`crate::sweep_registry::SweepRegistry`] for the workspace
//! ([`take`]), which rebuilds the in-memory quarantine from it. The ordinary
//! TTL expiry then releases the park when the TTL has run, exactly as if this
//! host had set it. A park the pass cannot date at all (no marker or label
//! timestamp) is released as before, the #4206 fail-open: an undatable park
//! held in memory could never be released.
//!
//! The TTL used is the workspace's base quarantine TTL. The forge does not
//! record an escalated generation (vibesql#6639), so a restarted host serves
//! a relapse park for the base TTL, never for less.

use super::{decide, BlockedIssue, ReconcileAction};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The startup decision for one `loom:blocked` issue (Issue #11930).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupAction {
    /// Not the daemon's park (see [`decide`]): leave it alone.
    Keep,
    /// The daemon's park, and its TTL has run: release it now.
    Release,
    /// The daemon's park, still inside its TTL: keep `loom:blocked` and
    /// rebuild the in-memory quarantine as applied at `quarantined_at`.
    Restore { quarantined_at: DateTime<Utc> },
}

/// The startup decision for `issue` at `now` with base quarantine TTL `ttl`.
/// Pure.
#[must_use]
pub fn decide_at_startup(issue: &BlockedIssue, now: DateTime<Utc>, ttl: Duration) -> StartupAction {
    if decide(issue) == ReconcileAction::Keep {
        return StartupAction::Keep;
    }
    // The marker comment is posted right after the label edit; the label
    // event is the fallback anchor when the comment could not be dated.
    let Some(quarantined_at) = issue
        .last_quarantine_comment_at
        .or(issue.last_blocked_labeled_at)
    else {
        return StartupAction::Release;
    };
    let age = (now - quarantined_at).to_std().unwrap_or_default();
    if age >= ttl {
        StartupAction::Release
    } else {
        StartupAction::Restore { quarantined_at }
    }
}

/// Parks kept by the startup pass, awaiting adoption by their workspace's
/// registry, keyed by normalized workspace root.
type Pending = HashMap<PathBuf, Vec<(u32, DateTime<Utc>)>>;

fn pending() -> &'static Mutex<Pending> {
    static PENDING: OnceLock<Mutex<Pending>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(root: &Path) -> PathBuf {
    crate::workspace_registry::normalize_path(root)
}

/// Hand a kept park to `root`'s registry (Issue #11930).
pub(crate) fn stash(root: &Path, issue: u32, quarantined_at: DateTime<Utc>) {
    let mut map = pending()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let parks = map.entry(key(root)).or_default();
    parks.retain(|(n, _)| *n != issue);
    parks.push((issue, quarantined_at));
}

/// Take every park kept for `root` (Issue #11930). Cheap when nothing is
/// pending: the reaper calls it every tick.
pub(crate) fn take(root: &Path) -> Vec<(u32, DateTime<Utc>)> {
    let mut map = pending()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.is_empty() {
        return Vec::new();
    }
    map.remove(&key(root)).unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn marked(at: Option<DateTime<Utc>>, labeled: Option<DateTime<Utc>>) -> BlockedIssue {
        BlockedIssue {
            number: 11930,
            has_quarantine_comment: true,
            last_quarantine_comment_at: at,
            last_blocked_labeled_at: labeled,
        }
    }

    /// The acceptance case: a park younger than its TTL, whoever set it, is
    /// kept on the forge and restored from the marker's own timestamp.
    #[test]
    fn a_park_inside_its_ttl_is_restored_not_released() {
        let now = Utc::now();
        let at = now - chrono::Duration::minutes(5);
        let issue = marked(Some(at), Some(at));
        assert_eq!(
            decide_at_startup(&issue, now, HOUR),
            StartupAction::Restore { quarantined_at: at }
        );
    }

    /// A park whose TTL has run is still released at startup.
    #[test]
    fn a_ttl_expired_park_is_still_released() {
        let now = Utc::now();
        let at = now - chrono::Duration::minutes(61);
        assert_eq!(
            decide_at_startup(&marked(Some(at), Some(at)), now, HOUR),
            StartupAction::Release
        );
        let exactly = now - chrono::Duration::hours(1);
        assert_eq!(
            decide_at_startup(&marked(Some(exactly), None), now, HOUR),
            StartupAction::Release
        );
    }

    /// Not the daemon's park: untouched, as before.
    #[test]
    fn a_manual_block_is_kept() {
        let now = Utc::now();
        let mut issue = marked(None, None);
        issue.has_quarantine_comment = false;
        assert_eq!(decide_at_startup(&issue, now, HOUR), StartupAction::Keep);
        // A label applied long after an old marker is a manual re-park (#4206).
        let old = now - chrono::Duration::days(2);
        assert_eq!(
            decide_at_startup(&marked(Some(old), Some(now)), now, HOUR),
            StartupAction::Keep
        );
    }

    /// Dating: the marker first, the label event as a fallback, and an
    /// undatable park released (the #4206 fail-open).
    #[test]
    fn dating_falls_back_to_the_label_then_releases() {
        let now = Utc::now();
        let labeled = now - chrono::Duration::minutes(2);
        assert_eq!(
            decide_at_startup(&marked(None, Some(labeled)), now, HOUR),
            StartupAction::Restore {
                quarantined_at: labeled
            }
        );
        assert_eq!(decide_at_startup(&marked(None, None), now, HOUR), StartupAction::Release);
    }

    /// End to end through `reconcile_workspace`: a fresh park on the forge is
    /// kept (no `issue edit`) and handed to the workspace's registry.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn the_startup_pass_keeps_a_fresh_park_and_hands_it_over() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let gh_log = dir.path().join("gh.log");
        let at = (Utc::now() - chrono::Duration::minutes(3)).format("%Y-%m-%dT%H:%M:%SZ");
        let script = format!(
            r#"#!/bin/bash
printf '%s\n' "$*" >> "{log}"
case "$*" in *issues?labels=loom:blocked*)
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[{{"number":98,"state":"open","updated_at":"{at}","labels":[{{"name":"loom:blocked"}}]}}]'
  exit 0 ;; esac
case "$*" in */issues/98/comments*)
  echo '[{{"body":"Auto-quarantined by loom-daemon (#3939): failed fast","created_at":"{at}","user":{{"login":"loom-fleet-dispatch[bot]","type":"Bot"}}}}]'
  exit 0 ;; esac
case "$*" in */issues/98/timeline*)
  echo '"{at}"'
  exit 0 ;; esac
exit 0
"#,
            log = gh_log.display(),
        );
        let fake_gh = dir.path().join("fake-gh.sh");
        std::fs::write(&fake_gh, script).unwrap();
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();

        let ws =
            crate::write_scope_test_support::WritableRoot::register_with_gh(&repo_root, &fake_gh);
        let (checked, released) = super::super::forge::reconcile_workspace(&ws.gh, &repo_root);
        assert_eq!((checked, released), (1, 0), "a park inside its TTL is not released");
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(!calls.contains("issue edit"), "no label write: {calls}");
        let kept = take(&repo_root);
        assert_eq!(kept.len(), 1, "handed to the registry");
        assert_eq!(kept[0].0, 98);
    }

    /// The handoff is per workspace, replaces a repeat, and drains once.
    #[test]
    fn stash_and_take_are_per_root() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let t1 = Utc::now();
        let t2 = t1 + chrono::Duration::seconds(1);
        stash(a.path(), 1, t1);
        stash(a.path(), 1, t2);
        stash(b.path(), 2, t1);
        assert_eq!(take(a.path()), vec![(1, t2)]);
        assert!(take(a.path()).is_empty(), "drained");
        assert_eq!(take(b.path()), vec![(2, t1)]);
    }
}
