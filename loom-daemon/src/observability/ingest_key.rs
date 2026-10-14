//! The shared ingest key: where its file path comes from, how it is read,
//! and — since #11555 — which exporter entries actually need it.
//!
//! Pre-#11555 the key was a hard prerequisite for *any* exporting: a config
//! without a readable key file disabled every exporter. #11555 makes the
//! prerequisite per-entry: an `otlp` entry carrying a `headers_file`
//! (#10961) authenticates with that file's headers, so it starts with no
//! ingest key configured at all. [`resolve_for_planned`] is the decision
//! [`super::spawn_task`] runs after its policy pass.
//!
//! Issue #7815's rule survives in per-entry form: the key file is read only
//! when some entry that would use it survived the policy pass — never for a
//! destination export would refuse, and never for a config whose every entry
//! carries its own credentials.

use std::path::Path;

use super::{env_nonempty, ExporterEntry, ExporterKind, ObservabilityConfig, INGEST_KEY_FILE_ENV};

/// The `misconfigured` detail when no ingest key file is configured at all.
/// Issue #5337's wording, byte-compatible with the pre-#11555 status.
pub(super) const NOT_CONFIGURED_DETAIL: &str = "observability.ingestKeyFile not configured \
     (set observability.ingestKeyFile or $LOOM_OBSERVABILITY_INGEST_KEY_FILE)";

/// **env > config > default** (`$HOME/.loom/observability/ingest.key`).
///
/// The default keeps resolution host-relative even when no tier sets
/// `ingestKeyFile` explicitly, so no host ever *needs* a value copied
/// verbatim from a different host's `$HOME` in its config (#5336: a macOS
/// `ingestKeyFile` landed in the shared, committed `.loom/config.json` and
/// was inherited unreadable by a Linux worker via a plain `git pull` — the
/// copy path, not the value, was the defect). Provisioning a host now needs
/// only to place its key at this conventional path (or set an explicit
/// override via `.loom-local/local.json` / `$LOOM_OBSERVABILITY_INGEST_KEY_FILE`
/// for a non-default location, e.g. a system path like
/// `/etc/loom/observability-ingest.key`).
#[must_use]
pub fn resolve_ingest_key_file(config: &ObservabilityConfig) -> Option<String> {
    env_nonempty(INGEST_KEY_FILE_ENV)
        .or_else(|| config.ingest_key_file.clone())
        .or_else(default_ingest_key_file)
}

/// Join `home` with the conventional relative ingest-key-file location. Pure
/// and unit-testable independent of the real `$HOME` (see
/// [`default_ingest_key_file`] for why the `$HOME` lookup itself is not
/// unit-tested in-process).
fn ingest_key_file_under(home: &Path) -> String {
    home.join(".loom")
        .join("observability")
        .join("ingest.key")
        .to_string_lossy()
        .to_string()
}

/// This crate's single `src/` test binary links every `#[test]` together, and
/// several modules (`tokens_pool::paths::shared_tokens_dir`, `fleet::drain`,
/// `fleet::path_bootstrap`) already `set_var`/`remove_var("HOME")` for their
/// own isolation — a real ambient `$HOME` must never leak into a *different*
/// module's resolved default mid-test-run. Refusing the default under
/// `cfg(test)` closes that race structurally, mirroring
/// `shared_tokens_dir`'s `#4657` fix exactly: production behavior reads the
/// real `$HOME`, every in-process test observes `None` from this function
/// and exercises [`ingest_key_file_under`] directly instead.
#[cfg(not(test))]
fn default_ingest_key_file() -> Option<String> {
    dirs::home_dir().map(|h| ingest_key_file_under(&h))
}

#[cfg(test)]
fn default_ingest_key_file() -> Option<String> {
    None
}

/// Read `path` and return its trimmed contents as the ingest key. Every
/// failure (missing file, unreadable, empty after trimming) is logged **by
/// path only** — the key itself never reaches a log line — and yields `Err`
/// with a detail string safe to surface on `loom-daemon status` (Issue
/// #5337): the offending path and, for I/O failures, the [`std::io::Error`]'s
/// `Display` (which includes the OS errno on platforms that report one, e.g.
/// `No such file or directory (os error 2)`). Its callers treat `Err` as
/// "not configured" — registering the detail as a
/// [`crate::types::ObservabilityExportState::Misconfigured`] status rather
/// than dropping it.
pub(super) fn read_ingest_key(path: &str) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let key = contents.trim().to_string();
            if key.is_empty() {
                let detail = format!("ingest key file {path} is empty after trimming whitespace");
                log::warn!("observability: {detail} — export off for the exporters that need it");
                Err(detail)
            } else {
                Ok(key)
            }
        }
        Err(error) => {
            let detail = format!("could not read ingest key file {path}: {error}");
            log::warn!("observability: {detail} — export off for the exporters that need it");
            Err(detail)
        }
    }
}

/// Whether `entry` authenticates with the shared ingest key (#11555): the
/// native HTTPS sink always does, and so does an `otlp` entry without a
/// `headers_file`. An `otlp` entry with one carries its own credentials — a
/// file-supplied `Authorization`, or headers an identity-aware proxy accepts
/// in place of a Bearer — and can start with no ingest key configured at
/// all.
pub(super) fn entry_needs_key(entry: &ExporterEntry) -> bool {
    match entry.kind {
        ExporterKind::Https => true,
        ExporterKind::Otlp => entry.headers_file.is_none(),
    }
}

/// What [`resolve_for_planned`] decided for a `spawn_task` call's planned
/// entries. Deliberately not derived `Debug`: [`PlannedKey::Key`] holds a
/// credential, and a derived rendering would put it in any `{:?}` log line
/// (#5337's posture — the manual impl redacts it).
pub(super) enum PlannedKey {
    /// No planned entry authenticates with the ingest key: none is read
    /// (#7815, per-entry) and the exporters run without a Bearer header.
    NotNeeded,
    /// The key was read and is threaded into every key-needing entry.
    Key(String),
    /// Some planned entry needs the key but it did not resolve; the string
    /// is the detail a `misconfigured` status reports. Only the entries
    /// that need the key are refused — the rest still start.
    Missing(String),
}

impl std::fmt::Debug for PlannedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlannedKey::NotNeeded => f.write_str("NotNeeded"),
            // The key is a credential: never rendered.
            PlannedKey::Key(_) => f.write_str("Key(<redacted>)"),
            PlannedKey::Missing(detail) => write!(f, "Missing({detail})"),
        }
    }
}

/// Resolve the shared ingest key for the entries that survived
/// [`super::spawn_task`]'s policy pass: read only when at least one of them
/// [`entry_needs_key`] needs it, so a config of only `headers_file`
/// entries never touches the key file at all (#7815 in per-entry form,
/// #11555).
pub(super) fn resolve_for_planned(
    config: &ObservabilityConfig,
    planned: &[(&ExporterEntry, String)],
) -> PlannedKey {
    if !planned.iter().any(|(entry, _)| entry_needs_key(entry)) {
        return PlannedKey::NotNeeded;
    }
    match resolve_ingest_key_file(config) {
        Some(path) => match read_ingest_key(&path) {
            Ok(key) => PlannedKey::Key(key),
            Err(detail) => PlannedKey::Missing(detail),
        },
        None => PlannedKey::Missing(NOT_CONFIGURED_DETAIL.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn clear_env() {
        std::env::remove_var(INGEST_KEY_FILE_ENV);
    }

    #[test]
    fn ingest_key_file_under_joins_the_conventional_relative_path() {
        assert_eq!(
            ingest_key_file_under(Path::new("/home/ubuntu")),
            "/home/ubuntu/.loom/observability/ingest.key"
        );
        assert_eq!(
            ingest_key_file_under(Path::new("/Users/robb-studio")),
            "/Users/robb-studio/.loom/observability/ingest.key",
            "must resolve against WHATEVER home is passed in — never a value \
             baked in from a different host"
        );
    }

    #[test]
    #[serial]
    fn resolve_ingest_key_file_config_wins_over_default_when_no_env_set() {
        clear_env();
        let config = ObservabilityConfig {
            ingest_key_file: Some("/etc/loom/observability-ingest.key".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_ingest_key_file(&config),
            Some("/etc/loom/observability-ingest.key".to_string())
        );
    }

    #[test]
    #[serial]
    fn resolve_ingest_key_file_env_overrides_config() {
        clear_env();
        std::env::set_var(INGEST_KEY_FILE_ENV, "/run/secrets/ingest.key");
        let config = ObservabilityConfig {
            ingest_key_file: Some("/etc/loom/observability-ingest.key".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_ingest_key_file(&config), Some("/run/secrets/ingest.key".to_string()));
        clear_env();
    }

    fn https_entry() -> ExporterEntry {
        ExporterEntry {
            kind: ExporterKind::Https,
            endpoint: None,
            headers_file: None,
        }
    }

    fn otlp_entry_with_headers_file() -> ExporterEntry {
        ExporterEntry {
            kind: ExporterKind::Otlp,
            endpoint: None,
            headers_file: Some("/run/secrets/otlp-headers".to_string()),
        }
    }

    #[test]
    #[serial]
    fn resolve_for_planned_skips_the_key_when_only_headers_file_entries_survive() {
        clear_env();
        let entry = otlp_entry_with_headers_file();
        let planned = vec![(&entry, "https://otlp.test-fixture.internal".to_string())];
        match resolve_for_planned(&ObservabilityConfig::default(), &planned) {
            PlannedKey::NotNeeded => {}
            other => panic!("expected NotNeeded, got {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn resolve_for_planned_reads_the_key_when_an_https_entry_survives() {
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("ingest.key");
        std::fs::write(&key_path, "fixture-key\n").unwrap();
        let config = ObservabilityConfig {
            ingest_key_file: Some(key_path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let https = https_entry();
        let otlp = otlp_entry_with_headers_file();
        let planned = vec![
            (&https, "https://ingest.test-fixture.internal".to_string()),
            (&otlp, "https://otlp.test-fixture.internal".to_string()),
        ];
        match resolve_for_planned(&config, &planned) {
            PlannedKey::Key(key) => assert_eq!(key, "fixture-key"),
            other => panic!("expected Key, got {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn resolve_for_planned_reports_the_not_configured_detail() {
        clear_env();
        let entry = https_entry();
        let planned = vec![(&entry, "https://ingest.test-fixture.internal".to_string())];
        match resolve_for_planned(&ObservabilityConfig::default(), &planned) {
            PlannedKey::Missing(detail) => assert_eq!(detail, NOT_CONFIGURED_DETAIL),
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn resolve_for_planned_reports_the_read_failure_detail() {
        clear_env();
        let config = ObservabilityConfig {
            ingest_key_file: Some("/nonexistent/ingest.key".to_string()),
            ..Default::default()
        };
        let entry = https_entry();
        let planned = vec![(&entry, "https://ingest.test-fixture.internal".to_string())];
        match resolve_for_planned(&config, &planned) {
            PlannedKey::Missing(detail) => {
                assert!(
                    detail.starts_with("could not read ingest key file"),
                    "detail was: {detail}"
                );
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    /// #7815's rule in per-entry form (#11555): a key file configured but
    /// broken is never even read when no surviving entry needs it.
    #[test]
    #[serial]
    fn resolve_for_planned_never_reads_a_key_no_surviving_entry_needs() {
        clear_env();
        let config = ObservabilityConfig {
            ingest_key_file: Some("/nonexistent/ingest.key".to_string()),
            ..Default::default()
        };
        let entry = otlp_entry_with_headers_file();
        let planned = vec![(&entry, "https://otlp.test-fixture.internal".to_string())];
        match resolve_for_planned(&config, &planned) {
            PlannedKey::NotNeeded => {}
            other => panic!("expected NotNeeded for a headers_file-only plan, got {other:?}"),
        }
    }
}
