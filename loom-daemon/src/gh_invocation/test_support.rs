//! Helpers shared by the `gh_invocation` test modules (2am#12523).

use crate::forge_call_stats;
use crate::types::ForgeCallCounts;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Write an executable `#!/bin/sh` stub named `name` into `dir`.
pub(crate) fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Run `body` with THIS thread's call-stats sink at a fresh dir, returning the
/// host-window rows it produced.
pub(crate) fn rows_after(body: impl FnOnce()) -> Vec<ForgeCallCounts> {
    let sink = tempfile::tempdir().unwrap();
    forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    let report = forge_call_stats::status_report(chrono::Utc::now(), None);
    forge_call_stats::set_test_sink_dir(None);
    report.host_window.unwrap_or_default()
}

/// Total calls (ok + error) booked for `caller`.
pub(crate) fn calls(rows: &[ForgeCallCounts], caller: &str) -> u64 {
    rows.iter()
        .filter(|r| r.caller == caller)
        .map(|r| r.ok + r.error)
        .sum()
}
