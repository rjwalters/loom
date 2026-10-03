#![allow(clippy::unwrap_used)]

use super::*;
use serial_test::serial;

/// Saves and clears every input of [`resolve`] (plus `$HOSTNAME`, which must
/// no longer matter), restoring the ambient values on drop.
struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn clear() -> Self {
        let keys = [
            HOST_ID_ENV,
            HOST_ID_FILE_ENV,
            "HOSTNAME",
            crate::config_resolver::PRIVATE_DEFAULTS_ENV,
        ];
        let saved = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for k in keys {
            std::env::remove_var(k);
        }
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

fn system_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// AC: with `LOOM_HOST_ID` unset and `$HOSTNAME`/`hostname` set to distinct
/// values, the id equals neither, survives a "restart" (a fresh read of the
/// file with the in-process cache bypassed), and does not follow `$HOSTNAME`.
#[test]
#[serial(loom_config_env)]
fn persisted_id_is_stable_and_never_the_hostname() {
    let _guard = EnvGuard::clear();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state").join(HOST_ID_FILENAME);
    std::env::set_var(HOST_ID_FILE_ENV, &path);
    std::env::set_var("HOSTNAME", "shell-exported-name");

    let first = resolve();
    assert_eq!(first.source, HostIdSource::Generated);
    assert!(first.id.starts_with(GENERATED_PREFIX), "{first:?}");
    assert_ne!(first.id, "shell-exported-name");
    assert_ne!(first.id, system_hostname());
    assert_eq!(first.path.as_deref(), Some(path.as_path()));

    // $HOSTNAME changing (a different launch context) changes nothing.
    std::env::set_var("HOSTNAME", "launchd-has-none");
    assert_eq!(host_identity(), first.id);

    // A restart re-reads the file: same id, reported as persisted.
    let (reread, created) = load_or_create(&path).unwrap();
    assert!(!created);
    assert_eq!(reread, first.id);
}

#[cfg(unix)]
#[test]
fn persisted_id_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(HOST_ID_FILENAME);
    let (_, created) = load_or_create(&path).unwrap();
    assert!(created);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "persisted host id must be owner-only, got {mode:o}");
    // No temp file left behind.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name() != HOST_ID_FILENAME)
        .collect();
    assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");
}

/// Concurrent first use (two processes starting at once) converges on ONE id:
/// exactly one creator wins and every other caller reads its value back.
#[test]
fn concurrent_creation_converges_on_one_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(HOST_ID_FILENAME);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || load_or_create(&path).unwrap())
        })
        .collect();
    let results: Vec<(String, bool)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let first = &results[0].0;
    assert!(results.iter().all(|(id, _)| id == first), "{results:?}");
    assert_eq!(results.iter().filter(|(_, created)| *created).count(), 1, "{results:?}");
}

#[test]
fn an_invalid_persisted_file_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(HOST_ID_FILENAME);
    std::fs::write(&path, "  \n").unwrap();
    let (id, created) = load_or_create(&path).unwrap();
    assert!(created);
    assert!(is_valid_id(&id));
    assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), id);
}

#[test]
fn an_existing_operator_written_id_is_kept_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(HOST_ID_FILENAME);
    std::fs::write(&path, "joseph-superset\n").unwrap();
    assert_eq!(load_or_create(&path).unwrap(), ("joseph-superset".to_string(), false));
}

/// Precedence: env > configured roster id > persisted id, blanks ignored.
#[test]
#[serial(loom_config_env)]
fn precedence_is_env_then_config_then_persisted() {
    let _guard = EnvGuard::clear();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(HOST_ID_FILENAME);
    std::fs::write(&path, "persisted-id\n").unwrap();
    std::env::set_var(HOST_ID_FILE_ENV, &path);
    let defaults = dir.path().join("defaults.json");
    std::fs::write(&defaults, r#"{"fleet": {"hostId": "roster-id"}}"#).unwrap();
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, &defaults);
    std::env::set_var(HOST_ID_ENV, "env-id");

    assert_eq!(resolve().id, "env-id");
    assert_eq!(resolve().source, HostIdSource::Env);

    std::env::set_var(HOST_ID_ENV, "   ");
    assert_eq!(resolve().id, "roster-id", "a blank LOOM_HOST_ID must not shadow the roster id");
    assert_eq!(resolve().source, HostIdSource::Config);

    std::fs::write(&defaults, r#"{"fleet": {"hostId": "  "}}"#).unwrap();
    assert_eq!(resolve().id, "persisted-id", "a blank roster id falls through");
    assert_eq!(resolve().source, HostIdSource::Persisted);
}

#[test]
fn generated_ids_are_valid_and_distinct_from_opaque_ids() {
    let id = generate_id();
    assert!(is_valid_id(&id), "{id}");
    assert!(id.starts_with(GENERATED_PREFIX));
    assert_eq!(id.len(), GENERATED_PREFIX.len() + 12);
    assert_ne!(generate_id(), id);
    assert!(!id.starts_with("host-"), "must not look like an opaque lease id");
}

#[test]
fn id_validation_rejects_unsafe_values() {
    assert!(is_valid_id("loom-worker-1"));
    assert!(is_valid_id("mac.studio_2"));
    assert!(!is_valid_id(""));
    assert!(!is_valid_id("two words"));
    assert!(!is_valid_id("a/b"));
    assert!(!is_valid_id(&"x".repeat(129)));
}
