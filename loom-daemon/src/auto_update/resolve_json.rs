//! `loom-daemon-update.sh --resolve-json` invocation + parsing (Issue #7609),
//! split out of `auto_update.rs` to respect the file-size ratchet
//! (`.loom/docs/file-size-policy.md`) rather than grown in place.
//!
//! #7818: a stale/incompatible `loom-daemon-update.sh` on the *workspace*
//! checkout (a host whose `LOOM_WORKSPACE` points at a consumer repo whose
//! own copy hadn't picked up `--resolve-json` yet) printed nothing on stdout,
//! and the daemon logged the maximally unhelpful `no artifact
//! (\`loom-daemon-update.sh --resolve-json\` printed no JSON object) ->
//! source path: no source checkout` — naming neither which of the (up to)
//! two candidate script paths [`super::ScriptAutoUpdateProbe::script_root`]
//! resolved to, nor anything the script itself said on stderr about why. Both
//! error paths here now name the exact resolved script path and fold in a
//! truncated stderr tail, so the log always answers "which script, and what
//! did it say" instead of leaving that to be reconstructed by hand.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{truncate_tail, ArtifactInfo, ArtifactResolution, REBUILD_POLL_INTERVAL};

/// Run `<script> --resolve-json` in `cwd` and return its captured stdout and
/// stderr — or an `Err` reason when the process itself could not be run.
///
/// Read-only by contract on the script's side (no download of the binary, no
/// `git fetch`, no build/provision/restart), so this is safe to call on every
/// tick. Both streams are captured to separate temp files rather than pipes,
/// for the same reason `run_update_script` does: a chatty child on a pipe
/// with nobody draining it deadlocks. **The exit code is deliberately
/// ignored** — the script exits `1` for the entirely ordinary "no release
/// resolved" case and still prints the JSON, so the JSON (stdout) is the
/// contract, not the status; stderr is carried along purely as diagnostic
/// context for [`parse_resolve_json`]'s error paths.
pub(super) fn run_resolve_json(
    script: &Path,
    cwd: &Path,
    timeout: Duration,
) -> Result<(String, String), String> {
    let out_path = std::env::temp_dir()
        .join(format!("loom-auto-update-resolve-{}.json", uuid::Uuid::new_v4()));
    let err_path = std::env::temp_dir()
        .join(format!("loom-auto-update-resolve-{}.stderr", uuid::Uuid::new_v4()));
    let out_file = std::fs::File::create(&out_path)
        .map_err(|e| format!("could not create the resolve output file: {e}"))?;
    let err_file = match std::fs::File::create(&err_path) {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            return Err(format!("could not create the resolve stderr file: {e}"));
        }
    };

    let mut command = Command::new(script);
    command
        .arg("--resolve-json")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file));

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            let _ = std::fs::remove_file(&err_path);
            return Err(format!("could not spawn `{} --resolve-json`: {e}", script.display()));
        }
    };

    let start = Instant::now();
    let result = loop {
        match child.try_wait() {
            Ok(Some(_status)) => break Ok(()),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!(
                        "`{} --resolve-json` timed out after {}s",
                        script.display(),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(REBUILD_POLL_INTERVAL);
            }
            Err(e) => break Err(format!("could not poll `{}`: {e}", script.display())),
        }
    };
    let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);
    result.map(|()| (stdout, stderr))
}

/// Parse `--resolve-json`'s single JSON object into an [`ArtifactResolution`].
/// Any shape surprise (unparseable, `ok:false`, a missing version) becomes
/// `Unresolved` with a reason rather than an error: "we could not learn about
/// a newer artifact" must always degrade to the source path, never to a
/// failure that stalls the loop.
///
/// `script` and `stderr` are diagnostic-only context (#7818): the "no JSON
/// object" / "unparseable JSON" cases name the exact resolved `script` path
/// and fold in a truncated `stderr` tail, so a stale/incompatible script that
/// doesn't understand `--resolve-json` is distinguishable in the log from the
/// ordinary "no release published yet" case, which always still prints a
/// valid (if `ok:false`) JSON object and so never reaches these branches.
#[must_use]
pub(super) fn parse_resolve_json(stdout: &str, stderr: &str, script: &Path) -> ArtifactResolution {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with('{'))
        .unwrap_or("");
    if line.is_empty() {
        return ArtifactResolution::Unresolved(format!(
            "`{} --resolve-json` printed no JSON object{}",
            script.display(),
            stderr_suffix(stderr)
        ));
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return ArtifactResolution::Unresolved(format!(
            "`{} --resolve-json` printed unparseable JSON{}",
            script.display(),
            stderr_suffix(stderr)
        ));
    };
    let string_field = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return ArtifactResolution::Unresolved(
            string_field("reason").unwrap_or_else(|| "no release artifact resolved".to_string()),
        );
    }
    let Some(version) = string_field("version") else {
        return ArtifactResolution::Unresolved(
            "release resolution reported ok but no version".to_string(),
        );
    };
    ArtifactResolution::Resolved(ArtifactInfo {
        tag: string_field("tag").unwrap_or_else(|| version.clone()),
        version,
        published_at: string_field("published_at"),
        asset_sha256: string_field("asset_sha256"),
        target: string_field("target"),
        // The script reports the literal string "unknown" for a commit it
        // could not read; a version it could not read is already `null`.
        installed_version: string_field("installed_version").filter(|v| v != "unknown"),
        installed_sha256: string_field("installed_sha256"),
    })
}

/// `" — stderr: <truncated tail>"`, or empty when there was nothing on
/// stderr — appended to the "no JSON object" / "unparseable JSON" reasons.
fn stderr_suffix(stderr: &str) -> String {
    if stderr.trim().is_empty() {
        String::new()
    } else {
        format!(" — stderr: {}", truncate_tail(stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script_path() -> std::path::PathBuf {
        std::path::PathBuf::from("/fake/loom-daemon-update.sh")
    }

    #[test]
    fn test_parse_resolve_json_happy_path() {
        let stdout = r#"{"ok":true,"reason":null,"repo":"rjwalters/loom","target":"aarch64-apple-darwin","tag":"v0.19.24","version":"0.19.24","published_at":"2026-09-13T12:00:00Z","asset_sha256":"abc123","installed_bin":"/x/loom-daemon","installed_version":"0.19.21","installed_commit":"deadbee","installed_sha256":"def456","source_version":"0.19.25","source_commit":"88116c7"}"#;
        match parse_resolve_json(stdout, "", &script_path()) {
            ArtifactResolution::Resolved(info) => {
                assert_eq!(info.version, "0.19.24");
                assert_eq!(info.tag, "v0.19.24");
                assert_eq!(info.published_at.as_deref(), Some("2026-09-13T12:00:00Z"));
                assert_eq!(info.asset_sha256.as_deref(), Some("abc123"));
                assert_eq!(info.installed_version.as_deref(), Some("0.19.21"));
                assert_eq!(info.installed_sha256.as_deref(), Some("def456"));
                assert_eq!(info.target.as_deref(), Some("aarch64-apple-darwin"));
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_resolve_json_not_ok_carries_the_reason() {
        let stdout =
            r#"{"ok":false,"reason":"'gh release view' found no latest release","version":null}"#;
        match parse_resolve_json(stdout, "", &script_path()) {
            ArtifactResolution::Unresolved(reason) => {
                assert!(reason.contains("no latest release"), "reason: {reason}");
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_resolve_json_garbage_is_unresolved_not_a_panic() {
        assert!(matches!(
            parse_resolve_json("", "", &script_path()),
            ArtifactResolution::Unresolved(_)
        ));
        assert!(matches!(
            parse_resolve_json("not json at all", "", &script_path()),
            ArtifactResolution::Unresolved(_)
        ));
        assert!(matches!(
            parse_resolve_json("{oops", "", &script_path()),
            ArtifactResolution::Unresolved(_)
        ));
        // ok:true but no version — a shape surprise must degrade, not fetch.
        assert!(matches!(
            parse_resolve_json(r#"{"ok":true,"version":null}"#, "", &script_path()),
            ArtifactResolution::Unresolved(_)
        ));
        // The script's "unknown" installed-commit sentinel must not become a
        // plausible-looking installed VERSION.
        match parse_resolve_json(
            r#"{"ok":true,"version":"0.19.24","installed_version":"unknown"}"#,
            "",
            &script_path(),
        ) {
            ArtifactResolution::Resolved(info) => assert_eq!(info.installed_version, None),
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_resolve_json_ignores_leading_noise_lines() {
        // Defensive: a shell that leaks a line onto stdout before the JSON
        // must not break resolution.
        let stdout = "warning: something\n{\"ok\":true,\"version\":\"0.19.24\"}\n";
        assert!(matches!(
            parse_resolve_json(stdout, "", &script_path()),
            ArtifactResolution::Resolved(_)
        ));
    }

    // ---- #7818: script path + stderr are named in the diagnostic reasons --

    #[test]
    fn test_parse_resolve_json_no_object_names_the_script_path() {
        let script =
            std::path::PathBuf::from("/opt/checkout/.loom/scripts/cli/loom-daemon-update.sh");
        match parse_resolve_json("", "", &script) {
            ArtifactResolution::Unresolved(reason) => {
                assert!(
                    reason.contains("/opt/checkout/.loom/scripts/cli/loom-daemon-update.sh"),
                    "reason does not name the script path: {reason}"
                );
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_resolve_json_no_object_folds_in_stderr() {
        match parse_resolve_json(
            "",
            "loom-daemon-update.sh: unknown flag --resolve-json\n",
            &script_path(),
        ) {
            ArtifactResolution::Unresolved(reason) => {
                assert!(reason.contains("unknown flag --resolve-json"), "reason: {reason}");
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_resolve_json_no_object_no_stderr_is_not_suffixed() {
        match parse_resolve_json("", "", &script_path()) {
            ArtifactResolution::Unresolved(reason) => {
                assert!(!reason.contains("stderr"), "reason: {reason}");
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_resolve_json_unparseable_json_folds_in_stderr() {
        match parse_resolve_json("{oops", "disk full\n", &script_path()) {
            ArtifactResolution::Unresolved(reason) => {
                assert!(reason.contains("unparseable JSON"), "reason: {reason}");
                assert!(reason.contains("disk full"), "reason: {reason}");
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    }
}
