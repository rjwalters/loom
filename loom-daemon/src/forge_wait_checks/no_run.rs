//! `LOOM-CHECKS-NO-RUN <sha>`: no GitHub Actions run exists for the head
//! (#11389).
//!
//! A head commit whose message carries a GitHub skip-CI marker gets no
//! workflow run at all. Before this, the wait could not tell that from slow
//! CI: zero rows (or only other apps' checks) with required contexts still
//! missing ran to `LOOM-CHECKS-TIMEOUT` after up to 30 minutes.
//!
//! GitHub Actions creates one check suite per workflow run, so "no run" is
//! read as "zero check suites from the `github-actions` app". A suite in any
//! state, `queued` included, is a run: the wait goes on as before.
//!
//! The probe runs only when all of these hold, so it never cuts short a wait
//! that is still making progress:
//!
//! - the wait has run for at least the grace period (`LOOM_WAIT_CHECKS_NO_RUN_GRACE`,
//!   default [`DEFAULT_GRACE`] s), so Actions has had time to register a suite;
//! - the poll did not settle, and no observed check is still running (`idle`):
//!   the wait is only waiting on a check that has not registered;
//! - no earlier probe found a suite or failed. Either stops probing, so it
//!   costs one extra read per poll at most, and only in that stuck state.
//!
//! A failed read is never read as "no run": the wait falls back to its old
//! `TIMEOUT`.

use std::time::Duration;

use serde_json::Value;

use super::reads::GhReads;
use super::Outcome;

/// Default grace period before a missing Actions run counts, seconds.
pub const DEFAULT_GRACE: u64 = 120;

/// The GitHub Actions app's id (its `app.id` on every check suite it owns).
pub const ACTIONS_APP_ID: u64 = 15368;

const ACTIONS_SLUG: &str = "github-actions";

/// How many check suites in `payload` belong to GitHub Actions.
///
/// # Errors
/// `check_suites` is missing or not an array.
pub fn actions_suites(payload: &Value) -> Result<usize, String> {
    let suites = payload
        .get("check_suites")
        .and_then(Value::as_array)
        .ok_or("unreadable: `check_suites` is missing or not an array")?;
    Ok(suites
        .iter()
        .filter(|s| {
            let app = s.get("app");
            app.and_then(|a| a.get("slug")).and_then(Value::as_str) == Some(ACTIONS_SLUG)
                || app.and_then(|a| a.get("id")).and_then(Value::as_u64) == Some(ACTIONS_APP_ID)
        })
        .count())
}

/// The stderr hint printed after the sentinel.
#[must_use]
pub fn hint(sha: &str) -> String {
    format!(
        "wait-checks: no GitHub Actions run exists for {sha}. The usual cause is a skip-CI \
         marker in the head commit message (\"[skip ci]\", \"[ci skip]\", \"[no ci]\", \
         \"[skip actions]\", \"[actions skip]\" or a \"skip-checks: true\" trailer): reword \
         that commit and push again. Otherwise no workflow matches this event."
    )
}

/// Per-wait probe state.
#[derive(Debug)]
pub struct Probe {
    grace: Duration,
    /// A suite was seen, or a read failed: never probe again in this wait.
    done: bool,
}

impl Probe {
    #[must_use]
    pub fn new(grace: Duration) -> Self {
        Self { grace, done: false }
    }

    /// `Some(NoRun)` when the conditions in the module docs hold and the
    /// head has zero Actions suites.
    pub fn check(
        &mut self,
        reads: &mut GhReads,
        sha: &str,
        elapsed: Duration,
        idle: bool,
        notes: &mut Vec<String>,
    ) -> Option<Outcome> {
        if self.done || !idle || sha.is_empty() || elapsed < self.grace {
            return None;
        }
        let read = reads
            .actions_suites(sha, ACTIONS_APP_ID)
            .map_err(|e| e.reason().to_string())
            .and_then(|v| actions_suites(&v));
        match read {
            Ok(0) => {
                notes.push(hint(sha));
                Some(Outcome::NoRun {
                    sha: sha.to_string(),
                })
            }
            Ok(_) => {
                self.done = true;
                None
            }
            Err(why) => {
                self.done = true;
                notes.push(format!("wait-checks: GitHub Actions suite lookup failed: {why}"));
                None
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_only_the_actions_app() {
        let p = json!({"total_count": 3, "check_suites": [
            {"app": {"slug": "codecov", "id": 254}, "status": "queued"},
            {"app": {"slug": "github-actions", "id": 15368}, "status": "queued"},
            {"app": {"id": 15368}, "status": "completed"},
        ]});
        assert_eq!(actions_suites(&p).unwrap(), 2);
        let other = json!({"check_suites": [{"app": {"slug": "renovate"}}]});
        assert_eq!(actions_suites(&other).unwrap(), 0);
        assert!(actions_suites(&json!({"total_count": 0})).is_err());
    }

    #[test]
    fn hint_names_the_skip_marker_cause() {
        let h = hint("abc");
        assert!(h.contains("abc") && h.contains("skip-CI marker") && h.contains("[ci skip]"));
    }
}
