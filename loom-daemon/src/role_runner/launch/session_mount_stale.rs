//! A Codex tick refused because its account's session container did not
//! mount the tick's working directory (#10364).
//!
//! A host-mode session container bind-mounts each registered repository
//! separately, and that set is fixed when the container is created. A repo
//! registered afterwards is not inside it, so `docker exec --workdir <repo>`
//! fails with `chdir to cwd … no such file or directory`. `session-exec host`
//! now checks the mounts before exec and refuses with exit 78 and a marker;
//! `spawn-codex.sh` turns that into `category=SESSION_MOUNT_STALE`. This module
//! turns the tick's own terminal record into a failure reason with a stable
//! [`REASON_PREFIX`], which `observability::lifecycle::admission_attributes`
//! projects to `loom.admission.reason = "session-mount-stale"`. The account
//! records no hold: see `TerminalClassification::SessionMountStale`.

use crate::tokens_pool::health::TerminalClassification;

use crate::role_tick_telemetry::SESSION_MOUNT_STALE_REASON_PREFIX as REASON_PREFIX;

/// The failure reason for the tick whose header carries `tick_anchor`, when
/// its own terminal record says the session container lacked the mount.
pub(super) fn reason_in(contents: &str, tick_anchor: &str) -> Option<String> {
    if tick_anchor.is_empty() {
        return None;
    }
    let result = crate::sweep_registry::parse_terminal_result_after(contents, tick_anchor)?;
    (result.category == TerminalClassification::SessionMountStale).then(|| {
        format!(
            "{REASON_PREFIX}: the Codex session container for account {} does not mount this \
             repository; recreate it when idle (#10364)",
            result.account
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::role_tick_telemetry::{is_session_down_reason, is_session_mount_stale_reason};

    const ANCHOR: &str = "=== tick 1 ===";

    fn log(category: &str) -> String {
        format!(
            "{ANCHOR}\n# LOOM_TERMINAL_RESULT v=2 provider=codex account=agent-3 \
             category={category} exit_code=78 model=none\n"
        )
    }

    #[test]
    fn a_mount_stale_record_gets_the_distinct_reason() {
        let reason = reason_in(&log("SESSION_MOUNT_STALE"), ANCHOR).unwrap();
        assert!(is_session_mount_stale_reason(&reason), "{reason}");
        assert!(!is_session_down_reason(&reason), "{reason}");
        assert!(reason.contains("agent-3"));
    }

    #[test]
    fn session_down_and_recoverable_records_do_not() {
        assert!(reason_in(&log("SESSION_DOWN"), ANCHOR).is_none());
        assert!(reason_in(&log("RECOVERABLE"), ANCHOR).is_none());
        assert!(reason_in("", ANCHOR).is_none());
        assert!(reason_in(&log("SESSION_MOUNT_STALE"), "").is_none());
    }
}
