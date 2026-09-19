//! Reading the `autonomy-desired` marker, and resolving the pid file the same
//! way the daemon itself does.
//!
//! The marker is operator INTENT: present ⇒ a daemon is expected; absent ⇒ it
//! was deliberately stopped (or never started) ⇒ stay silent. That is why the
//! detector keys on it rather than on "is the pid file / launchd job present",
//! which cannot tell a deliberate stop from a silent death.

use std::path::{Path, PathBuf};

/// First `key=value` line's value, or `None`. Mirrors the shell's
/// `grep -E "^key=" | head -n1 | cut -d= -f2-`, including that a value
/// containing `=` is preserved whole and that a missing file is not an error.
#[must_use]
pub fn get(marker: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(marker).ok()?;
    let prefix = format!("{key}=");
    text.lines()
        .find(|l| l.starts_with(&prefix))
        .map(|l| l[prefix.len()..].to_string())
}

/// Same, but treating an empty value as absent — the shell's `[[ -z ... ]]`
/// checks collapsed the two, and several fields default on empty.
#[must_use]
pub fn get_nonempty(marker: &Path, key: &str) -> Option<String> {
    get(marker, key).filter(|v| !v.is_empty())
}

/// Resolve the pid file, in the shell's documented five-tier precedence.
///
/// The shell's own comment says this "mirrors the daemon's own
/// `daemon_pidfile::resolve_pid_file_path_from` EXACTLY so the two ends can
/// never mean different files", and records that #5118 — where the script read
/// `<socket dir>/.daemon.pid` while the daemon wrote
/// `<workspace>/.loom/.daemon.pid` — was possible *only because each side
/// derived its own path*.
///
/// So this does not re-derive it. The two watchdog-only tiers (the marker's own
/// `pid_file=` and `repo_root=`) fold into the shared function's arguments, and
/// the precedence itself stays where it is already tested.
#[must_use]
pub fn resolve_pid_file(
    pid_file_env: Option<String>,
    marker_pid_file: Option<String>,
    machine_checkout: Option<String>,
    workspace: Option<String>,
    marker_repo_root: Option<String>,
    loom_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    crate::daemon_pidfile::resolve_pid_file_path_from(
        // Tiers 1-2: the explicit override, then the path the start script
        // chose on THIS host (which is what it exported as LOOM_PID_FILE).
        pid_file_env.filter(|s| !s.is_empty()).or(marker_pid_file),
        machine_checkout,
        // Tier 4: repo mode — the live workspace, else the marker's record of it.
        workspace.filter(|s| !s.is_empty()).or(marker_repo_root),
        loom_dir,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn marker_with(body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("autonomy-desired");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(body.as_bytes()).expect("write");
        (dir, path)
    }

    #[test]
    fn reads_the_first_match_only() {
        let (_d, p) = marker_with("pid_file=/a\npid_file=/b\n");
        assert_eq!(get(&p, "pid_file").as_deref(), Some("/a"));
    }

    #[test]
    fn a_value_containing_equals_survives_whole() {
        // `cut -d= -f2-` keeps everything after the FIRST `=`.
        let (_d, p) = marker_with("started_at=2026-01-01T00:00:00Z\nargs=--flag=value\n");
        assert_eq!(get(&p, "args").as_deref(), Some("--flag=value"));
    }

    #[test]
    fn a_missing_marker_is_absence_not_an_error() {
        assert_eq!(get(Path::new("/nonexistent/marker"), "pid_file"), None);
    }

    #[test]
    fn a_key_that_is_only_a_prefix_of_another_does_not_match_it() {
        let (_d, p) = marker_with("pid_file_backup=/wrong\npid_file=/right\n");
        assert_eq!(get(&p, "pid_file").as_deref(), Some("/right"));
    }

    #[test]
    fn explicit_env_outranks_the_marker() {
        let got = resolve_pid_file(
            Some("/from/env".into()),
            Some("/from/marker".into()),
            None,
            None,
            None,
            None,
        );
        assert_eq!(got, Some(PathBuf::from("/from/env")));
    }

    #[test]
    fn the_marker_pid_file_outranks_workspace_derivation() {
        let got = resolve_pid_file(
            None,
            Some("/from/marker".into()),
            None,
            Some("/ws".into()),
            None,
            None,
        );
        assert_eq!(got, Some(PathBuf::from("/from/marker")));
    }

    #[test]
    fn workspace_derives_the_repo_mode_path() {
        let got = resolve_pid_file(None, None, None, Some("/ws".into()), None, None);
        assert_eq!(got, Some(PathBuf::from("/ws/.loom/.daemon.pid")));
    }

    #[test]
    fn the_marker_repo_root_stands_in_for_an_unset_workspace() {
        let got = resolve_pid_file(None, None, None, None, Some("/repo".into()), None);
        assert_eq!(got, Some(PathBuf::from("/repo/.loom/.daemon.pid")));
    }
}
