//! The watchdogs' two forge comments: the startup watchdog's give-up notice
//! (#5302) and the stale-sweep backstop's reap notice (#7529).
//!
//! Moved out of `watchdog.rs`, which is at its file-size ratchet baseline,
//! when #11743 gave both a `loom:frozen` gate: a human can freeze an issue
//! while its sweep is in flight, and no daemon path may then comment on it.
//! Neither path read the issue's labels, so each pays one REST label read
//! ([`SweepRegistry::frozen_blocks_write`]) right before posting. Fail-open,
//! like every other step of these best-effort notices: a failed read posts the
//! comment as before. The claim restore the stale-sweep reap performs goes
//! through `restore_label_to_ready`, which already leaves a frozen issue alone.

use super::*;

impl SweepRegistry {
    /// Best-effort forge comment when the startup watchdog gives up (Issue
    /// #5302): the bounded auto-restart (#3887) already retried this issue's
    /// sweep exactly once and it is still stuck, so nothing further will
    /// happen automatically until an operator acts. Before this, "giving up"
    /// meant only a `log::error!` line — invisible unless someone was
    /// tailing the daemon log at the moment it fired. Mirrors
    /// [`SweepRegistry::apply_quarantine_label`]'s "post a comment so the
    /// pause is visible to a human on the forge, not just in the daemon
    /// log" pattern.
    ///
    /// Called from exactly one call site, already inside the
    /// `self.watchdog_gaveup.insert(issue)` dedup guard [`watchdog_once`]
    /// uses to log the give-up once — so this posts at most one comment per
    /// issue no matter how many subsequent ticks re-observe the same
    /// give-up (the same dedup fixture the existing
    /// `watchdog_restarts_hung_sweep_once_then_gives_up` test already
    /// exercises for the log line).
    ///
    /// Skipped when label flips are disabled (test fixtures /
    /// `skip_label_flip`), matching every other best-effort forge mutation
    /// in this registry. Every step is best-effort: a `gh` failure only logs
    /// (at `warn`, not `debug` — a silently-dropped give-up notice defeats
    /// the whole point of this method), it never affects the load-bearing
    /// `watchdog_gaveup` dedup state, which the caller already committed
    /// before this runs.
    pub(super) fn post_watchdog_gaveup_comment(&self, issue: u32, elapsed: Duration) {
        if self.config.skip_label_flip
            || self.frozen_blocks_write(issue, "watchdog give-up comment")
        {
            return;
        }
        let body = format!(
            "{marker}: this issue's sweep made no progress for {elapsed_secs}s, exhausted its \
             one bounded auto-restart (#3887), and is still stuck — the daemon has stopped \
             retrying and left it running for operator triage rather than looping forever. \
             Investigate the sweep log, then either cancel the stuck sweep and re-dispatch the \
             issue, or investigate the underlying hang (e.g. an MCP-init race, #3887). This \
             issue remains held at `loom:building` until you act.",
            marker = WATCHDOG_GAVEUP_COMMENT_MARKER,
            elapsed_secs = elapsed.as_secs(),
        );
        // Counted as `watchdog.gaveup_comment` (#10089), scoped to the
        // workspace (#5401 GH_CONFIG_DIR).
        let issue_arg = issue.to_string();
        let mut comment = vec!["issue", "comment", &issue_arg, "--body", &body];
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        comment.extend(repo_flag.iter().map(String::as_str));
        let timeout = reap_gh_timeout();
        match self.gh_write("watchdog.gaveup_comment", comment) {
            Ok(Some(output)) if output.status.success() => {}
            Ok(Some(output)) => log::warn!(
                "watchdog: give-up comment for #{issue} exited {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Ok(None) => log::warn!(
                "watchdog: give-up comment for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => log::warn!("watchdog: give-up comment for #{issue} failed: {e}"),
        }
    }

    /// Best-effort forge comment when the stale-sweep backstop reaps an
    /// untracked sweep (Issue #7529) — mirrors
    /// [`post_watchdog_gaveup_comment`](Self::post_watchdog_gaveup_comment)'s
    /// "make the daemon-log-only fact visible on the forge" pattern, except
    /// this backstop already acted (restored `loom:issue`) rather than
    /// leaving the issue held.
    pub(super) fn post_stale_sweep_comment(&self, finding: &StaleSweepFinding) {
        if self.config.skip_label_flip
            || self.frozen_blocks_write(finding.issue, "stale-sweep reap comment")
        {
            return;
        }
        let body = format!(
            "{marker}: this issue's sweep (pid {pid}) had been running {elapsed_secs}s with its \
             log silent and no daemon-retained process handle for it — a sweep that survived a \
             prior daemon restart, which strips it of coverage from both the startup-hang \
             (#3887) and review-stall (#3910) watchdogs (#7529). The daemon has cancelled the \
             process and restored `loom:issue` so this issue can be re-dispatched.",
            marker = STALE_SWEEP_COMMENT_MARKER,
            pid = finding.pid,
            elapsed_secs = finding.elapsed.as_secs(),
        );
        let issue_arg = finding.issue.to_string();
        let mut comment = vec!["issue", "comment", &issue_arg, "--body", &body];
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        comment.extend(repo_flag.iter().map(String::as_str));
        let timeout = reap_gh_timeout();
        match self.gh_write("watchdog.stale_comment", comment) {
            Ok(Some(output)) if output.status.success() => {}
            Ok(Some(output)) => log::warn!(
                "stale-sweep-watchdog: reap comment for #{} exited {:?}: {}",
                finding.issue,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Ok(None) => log::warn!(
                "stale-sweep-watchdog: reap comment for #{} exceeded {}s, killed (#3973)",
                finding.issue,
                timeout.as_secs()
            ),
            Err(e) => {
                log::warn!("stale-sweep-watchdog: reap comment for #{} failed: {e}", finding.issue)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::frozen_write::test_support::wrap_label_reads;
    use std::os::unix::fs::PermissionsExt;

    /// A registry with forge writes on and a fake `gh` that logs every argv.
    fn registry(ws: &Path) -> (SweepRegistry, PathBuf) {
        let log = ws.join("gh.log");
        let gh = ws.join("fake-gh-watchdog.sh");
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            log.display()
        );
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = SweepRegistryConfig::new(ws.to_path_buf());
        config.gh_bin = Some(gh);
        config.skip_label_flip = false;
        (SweepRegistry::new(config), log)
    }

    fn comments(log: &Path) -> usize {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("issue comment "))
            .count()
    }

    /// #11743: neither watchdog comment is posted on a `loom:frozen` issue,
    /// and both still post when the label read fails (fail-open).
    #[test]
    #[serial_test::serial]
    fn neither_watchdog_comment_is_posted_on_a_frozen_issue() {
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let finding = StaleSweepFinding {
            issue: 7529,
            sweep_id: "sweep-7529".to_string(),
            pid: 1,
            elapsed: Duration::from_secs(9000),
            log_idle: None,
        };
        for (labels, posted) in [(Some("loom:building\nloom:frozen"), 0), (None, 2)] {
            let dir = tempfile::tempdir().unwrap();
            let (mut reg, log) = registry(dir.path());
            let reads = wrap_label_reads(&mut reg, dir.path(), labels);
            reg.post_watchdog_gaveup_comment(7529, Duration::from_secs(600));
            reg.post_stale_sweep_comment(&finding);
            assert_eq!(comments(&log), posted, "labels {labels:?}");
            let reads = std::fs::read_to_string(&reads).unwrap();
            assert_eq!(reads.lines().count(), 2, "one label read per comment: {reads}");
        }
        std::env::remove_var("LOOM_REPO");
    }
}
