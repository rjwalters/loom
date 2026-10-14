//! The `loom:frozen` write gate for the registry's in-flight writers (#11743).
//!
//! The operator ruling on #11743: no agent or daemon path writes to a
//! `loom:frozen` item — no label, no comment, no body edit, not even
//! removing its own label. The dispatch-side sets already refuse a frozen
//! item, so the writers that remain are the ones that act on work already in
//! flight: a human can freeze an issue while its sweep runs, and the reaper,
//! the watchdogs and the PR-less retry bound then reach it.
//!
//! Writers that already read the item's labels check
//! [`crate::work_finder::is_frozen`] on that read. This module is the read for
//! the ones that do not: one REST label read
//! ([`SweepRegistry::current_labels_via_rest`]) right before the write.
//!
//! # Fail direction
//!
//! Each caller keeps its path's existing convention for an unverifiable read.
//! [`SweepRegistry::frozen_blocks_write`] is the fail-open form (a failed read
//! writes as before), used by the best-effort notices whose own probes already
//! fail open (the watchdog comments, the PR-less attempt note). A caller that
//! fails closed on a failed read uses [`SweepRegistry::issue_frozen_via_rest`]
//! and decides on `None` itself.

use super::SweepRegistry;

impl SweepRegistry {
    /// Whether `issue` carries `loom:frozen`: `Some(true)` / `Some(false)` on
    /// a successful REST label read, `None` when the read failed (or forge
    /// writes are off, so nothing would be written anyway).
    pub(crate) fn issue_frozen_via_rest(&self, issue: u32) -> Option<bool> {
        if self.config.skip_label_flip {
            return None;
        }
        self.current_labels_via_rest(issue)
            .map(|labels| crate::work_finder::is_frozen(&labels))
    }

    /// Whether the write `what` on `issue` must be skipped because the issue
    /// is `loom:frozen` (#11743). Fail-open: a failed read returns `false`,
    /// and the caller writes exactly as it did before the gate existed.
    pub(crate) fn frozen_blocks_write(&self, issue: u32, what: &str) -> bool {
        let frozen = self.issue_frozen_via_rest(issue) == Some(true);
        if frozen {
            log::info!("sweep_registry: #{issue} is `loom:frozen` — skipping the {what} (#11743)");
        }
        frozen
    }
}

/// Test seam shared by the writers' frozen tests (#11743).
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod test_support {
    use super::SweepRegistry;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// Wrap `reg`'s fake `gh` so the REST label read
    /// ([`SweepRegistry::current_labels_via_rest`]) answers `labels` (one per
    /// line; `None` makes it fail) and is logged to the returned path; every
    /// other call reaches the original fake unchanged.
    pub(crate) fn wrap_label_reads(
        reg: &mut SweepRegistry,
        ws: &Path,
        labels: Option<&str>,
    ) -> PathBuf {
        let inner = reg.config.gh_bin.clone().expect("a fake gh to wrap");
        let log = ws.join("frozen-label-reads.log");
        let outer = ws.join("fake-gh-frozen-wrap.sh");
        let answer = labels
            .map_or_else(|| "exit 1".to_string(), |l| format!("printf '%s\\n' '{l}'; exit 0"));
        let script = format!(
            "#!/usr/bin/env bash\n\
             case \"$*\" in\n\
             api\\ repos/*'--jq .labels[].name') printf '%s\\n' \"$*\" >> \"{log}\"; {answer} ;;\n\
             esac\n\
             exec \"{inner}\" \"$@\"\n",
            log = log.display(),
            inner = inner.display(),
        );
        std::fs::write(&outer, script).unwrap();
        std::fs::set_permissions(&outer, std::fs::Permissions::from_mode(0o755)).unwrap();
        reg.config.gh_bin = Some(outer);
        log
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::test_support::wrap_label_reads;
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use std::os::unix::fs::PermissionsExt;

    fn registry(ws: &std::path::Path) -> SweepRegistry {
        let gh = ws.join("fake-gh-base.sh");
        std::fs::write(&gh, "#!/usr/bin/env bash\nexit 0\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = SweepRegistryConfig::new(ws.to_path_buf());
        config.gh_bin = Some(gh);
        config.skip_label_flip = false;
        SweepRegistry::new(config)
    }

    #[test]
    #[serial_test::serial]
    fn a_frozen_issue_blocks_the_write_and_a_failed_read_does_not() {
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let dir = tempfile::tempdir().unwrap();
        let mut reg = registry(dir.path());
        let log = wrap_label_reads(&mut reg, dir.path(), Some("loom:building\nloom:frozen"));
        assert_eq!(reg.issue_frozen_via_rest(5), Some(true));
        assert!(reg.frozen_blocks_write(5, "test comment"));
        assert!(std::fs::read_to_string(&log)
            .unwrap()
            .contains("/issues/5 "));

        let dir = tempfile::tempdir().unwrap();
        let mut reg = registry(dir.path());
        wrap_label_reads(&mut reg, dir.path(), Some("loom:building"));
        assert!(!reg.frozen_blocks_write(5, "test comment"));

        let dir = tempfile::tempdir().unwrap();
        let mut reg = registry(dir.path());
        wrap_label_reads(&mut reg, dir.path(), None);
        assert_eq!(reg.issue_frozen_via_rest(5), None);
        assert!(!reg.frozen_blocks_write(5, "test comment"), "fail-open on a failed read");
        std::env::remove_var("LOOM_REPO");
    }
}
