//! Heal the forge-visible claim: re-apply `loom:building` to OPEN issues
//! whose sweep is live on this host but whose claim label never made it to
//! the forge (Issue #9464).
//!
//! # The measured gap
//!
//! 48% of landed issues were never labelled `loom:building` at all, rising
//! to 80–94% of landings in the weeks before the report. A dispatch whose
//! label flip was skipped (observe mode), refused, or lost to a failed `gh`
//! still builds the issue — the daemon's in-memory registry knows, but the
//! forge (the thing humans and OTHER hosts' agents read) shows a free issue.
//! The label state machine and CLAUDE.md's Builder workflow both lean on
//! that label being true.
//!
//! # The mechanism
//!
//! One pass per reconciliation cycle, piggybacking on the existing
//! [`super::pass_loop`] cadence (no new scheduler): read this host's sweep
//! journal for **live** sweeps (a PID `kill(pid, 0)` probe, the same liveness
//! the reaper uses), and for each issue they name, read the forge state:
//!
//! - OPEN **without** `loom:building` → re-apply it (idempotent: a single
//!   `--add-label` on an issue that has it is a no-op) and count the heal.
//! - OPEN **with** the label → nothing (the normal claimed shape).
//! - CLOSED → touch nothing (#9463's rule: a closed issue never gains a
//!   queue label) — but the live-sweep-on-a-closed-issue shape is logged at
//!   `warn`, because it means a sweep is building a done issue.
//!
//! Bounded per pass (`MAX_PROBES_PER_PASS`) so a pathological journal cannot
//! turn one reconciliation tick into an unbounded `gh` fan-out, and
//! fail-open everywhere: every `gh` failure is logged and skipped, never
//! fatal to the pass.

use std::path::Path;

/// The most issues one pass will probe. A healthy host has at most a handful
/// of live sweeps (dispatch caps are single digits); the cap exists so a
/// corrupted journal full of zombie PIDs cannot fan out.
const MAX_PROBES_PER_PASS: usize = 20;

/// Outcome of one heal pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BuildingHealStats {
    /// Issues with a live local sweep whose forge state was read.
    pub checked: usize,
    /// Issues where `loom:building` was (re-)applied.
    pub healed: usize,
    /// Live-sweep issues skipped because the issue is CLOSED (#9463).
    pub closed_skipped: usize,
}

/// Run one heal pass against `root`'s workspace using `gh_bin`.
pub fn heal_missing_building_labels(gh_bin: &Path, root: &Path) -> BuildingHealStats {
    let mut stats = BuildingHealStats::default();
    let journal_path = match crate::sweep_journal::default_journal_path() {
        Ok(path) => path,
        Err(error) => {
            log::debug!("building_heal: no journal path: {error}");
            return stats;
        }
    };
    let journal = crate::sweep_journal::load(&journal_path);
    // Distinct issues with a LIVE sweep on this host (in journal order — the
    // oldest claim first), rooted at this workspace.
    let mut live_issues: Vec<u32> = Vec::new();
    for entry in &journal.entries {
        if entry.repo != root.display().to_string() {
            continue;
        }
        if !crate::sweep_registry::crash_signals::is_pid_alive(entry.pid) {
            continue;
        }
        if !live_issues.contains(&entry.issue) {
            live_issues.push(entry.issue);
        }
    }
    for issue in live_issues.into_iter().take(MAX_PROBES_PER_PASS) {
        let Some((state, labels)) = read_issue_state_and_labels(gh_bin, root, issue) else {
            continue;
        };
        stats.checked += 1;
        let state = state.to_ascii_lowercase();
        if state == "closed" {
            // #9463: a closed issue never gains a queue label — and a live
            // sweep building one is worth a host-visible warn.
            stats.closed_skipped += 1;
            log::warn!(
                "building_heal: issue #{issue} is CLOSED but a live sweep on this host is \
                 still working it (#9464/#9463) — not re-applying any queue label"
            );
            continue;
        }
        if labels.iter().any(|label| label == "loom:building") {
            continue;
        }
        if apply_building_label(gh_bin, root, issue) {
            stats.healed += 1;
            log::info!(
                "building_heal: re-applied `loom:building` to open issue #{issue} — its \
                 sweep is live on this host but the forge-visible claim was missing \
                 (#9464)"
            );
        }
    }
    if stats.checked > 0 {
        log::info!(
            "building_heal: checked {} live-sweep issue(s), healed {}, closed-skipped {} \
             (#9464)",
            stats.checked,
            stats.healed,
            stats.closed_skipped
        );
    }
    stats
}

/// `(state, labels)` for one issue, best-effort — `None` on any `gh` failure.
fn read_issue_state_and_labels(
    gh_bin: &Path,
    root: &Path,
    issue: u32,
) -> Option<(String, Vec<String>)> {
    let n = issue.to_string();
    let stdout = crate::claim_reconciliation::gh_call::ok_stdout(
        crate::claim_reconciliation::gh_call::read("heal.issue_view", gh_bin, root).args([
            "issue",
            "view",
            &n,
            "--json",
            "state,labels",
        ]),
    )?;
    let value: serde_json::Value = serde_json::from_slice(&stdout).ok()?;
    let state = value.get("state")?.as_str()?.to_string();
    let labels = value
        .get("labels")?
        .as_array()?
        .iter()
        .filter_map(|label| label.get("name")?.as_str().map(ToString::to_string))
        .collect();
    Some((state, labels))
}

/// Apply the `loom:building` label; `true` when `gh` exited 0.
fn apply_building_label(gh_bin: &Path, root: &Path, issue: u32) -> bool {
    let n = issue.to_string();
    crate::claim_reconciliation::gh_call::ok_stdout(
        crate::claim_reconciliation::gh_call::write("heal.issue_add_label", gh_bin, root).args([
            "issue",
            "edit",
            &n,
            "--add-label",
            "loom:building",
        ]),
    )
    .is_some()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake `gh` whose `issue view` reports `{"state":…,"labels":[…]}` in
    /// the real response shape (`labels` is an array of `{name}` objects)
    /// and which records every invocation, so the heal path can be driven
    /// end-to-end without the forge.
    fn fake_gh(
        dir: &Path,
        state: &str,
        labels: &[&str],
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let fake_gh = dir.join("fake-gh.sh");
        let gh_log = dir.join("gh-calls.log");
        let labels_json = serde_json::json!(labels
            .iter()
            .map(|name| serde_json::json!({ "name": name }))
            .collect::<Vec<_>>())
        .to_string();
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> {}\n\
             if [ \"$1\" = \"issue\" ] && [ \"$2\" = \"view\" ]; then\n\
             printf '%s' '{{\"state\":\"{state}\",\"labels\":{labels_json}}}'\n\
             exit 0\nfi\nexit 0\n",
            gh_log.display()
        );
        std::fs::write(&fake_gh, script).unwrap();
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        (fake_gh, gh_log)
    }

    /// Seed one live-sweep journal entry at `journal_path` (a tempdir-scoped
    /// path the caller has already pointed [`crate::sweep_journal::JOURNAL_PATH_ENV`]
    /// at) — never the real `~/.loom/sweeps.json` (#9907: these tests used to
    /// read/write that file unconditionally via `default_journal_path()`,
    /// racing a live `loom-daemon` on the same host).
    fn seed_live_sweep(journal_path: &Path, root: &Path, issue: u32) {
        let mut journal = crate::sweep_journal::load(journal_path);
        journal.entries.push(crate::sweep_journal::JournalEntry {
            repo: root.display().to_string(),
            issue,
            // This test process's own PID: alive by construction, no
            // `kill` permissions needed (the probe on a live owned PID
            // succeeds).
            pid: std::process::id(),
            started_at: chrono::Utc::now(),
        });
        crate::sweep_journal::save(journal_path, &journal).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn heals_an_open_issue_whose_claim_label_is_missing() {
        let journal_path_dir = tempfile::tempdir().unwrap();
        let journal_path = journal_path_dir.path().join("sweeps.json");
        std::env::set_var(crate::sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        let dir = tempfile::tempdir().unwrap();
        seed_live_sweep(&journal_path, dir.path(), 9464);
        let (fake_gh, gh_log) = fake_gh(dir.path(), "open", &[]);

        let stats = heal_missing_building_labels(&fake_gh, dir.path());

        std::env::remove_var(crate::sweep_journal::JOURNAL_PATH_ENV);

        assert_eq!(stats.checked, 1);
        assert_eq!(stats.healed, 1);
        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(calls.contains("issue edit 9464 --add-label loom:building"), "{calls}");
    }

    #[test]
    #[serial_test::serial]
    fn never_labels_a_closed_issue_and_warns_instead() {
        let journal_path_dir = tempfile::tempdir().unwrap();
        let journal_path = journal_path_dir.path().join("sweeps.json");
        std::env::set_var(crate::sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        let dir = tempfile::tempdir().unwrap();
        seed_live_sweep(&journal_path, dir.path(), 9463);
        let (fake_gh, gh_log) = fake_gh(dir.path(), "closed", &["loom:building"]);

        let stats = heal_missing_building_labels(&fake_gh, dir.path());

        std::env::remove_var(crate::sweep_journal::JOURNAL_PATH_ENV);

        assert_eq!(stats.checked, 1);
        assert_eq!(stats.healed, 0);
        assert_eq!(stats.closed_skipped, 1);
        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            !calls.contains("--add-label loom:building"),
            "a closed issue must never gain the queue label (#9463): {calls}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn an_already_claimed_issue_is_untouched() {
        let journal_path_dir = tempfile::tempdir().unwrap();
        let journal_path = journal_path_dir.path().join("sweeps.json");
        std::env::set_var(crate::sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        let dir = tempfile::tempdir().unwrap();
        seed_live_sweep(&journal_path, dir.path(), 42);
        let (fake_gh, gh_log) = fake_gh(dir.path(), "open", &["loom:building"]);

        let stats = heal_missing_building_labels(&fake_gh, dir.path());

        std::env::remove_var(crate::sweep_journal::JOURNAL_PATH_ENV);

        assert_eq!(stats.checked, 1);
        assert_eq!(stats.healed, 0);
        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(!calls.contains("issue edit"), "{calls}");
    }
}
