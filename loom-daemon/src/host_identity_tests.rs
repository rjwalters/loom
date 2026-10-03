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

/// A filesystem that refuses hard links (EPERM on some mounts) still gets a
/// persisted, owner-only id via the `O_EXCL` fallback — not `unknown-host`.
#[test]
fn hard_link_refused_falls_back_to_exclusive_create() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(HOST_ID_FILENAME);
    let refuse = |_: &Path, _: &Path| -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    };
    let (id, created) = load_or_create_with(&path, refuse).unwrap();
    assert!(created);
    assert!(id.starts_with(GENERATED_PREFIX), "{id}");
    assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), id);
    // A second caller on the same mount reads it back rather than re-keying.
    assert_eq!(load_or_create_with(&path, refuse).unwrap(), (id, false));
    let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(leftovers, 1, "temp file leaked");
}

/// Many replacers of one corrupt file: every caller gets a valid id read back
/// from disk (never an unplaced candidate), the file ends valid, and the
/// on-disk id is among those reported. The residual window — a replacer that
/// re-reads before a later one's `rename` lands — is documented on
/// [`load_or_create`]; it cannot recur once the file is valid.
#[test]
fn concurrent_replacement_of_an_invalid_file_reports_the_on_disk_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(HOST_ID_FILENAME);
    std::fs::write(&path, "not an id\n").unwrap();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || load_or_create(&path).unwrap())
        })
        .collect();
    let results: Vec<(String, bool)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let on_disk = std::fs::read_to_string(&path).unwrap().trim().to_string();
    assert!(is_valid_id(&on_disk));
    assert!(results.iter().all(|(id, _)| is_valid_id(id)), "{results:?}");
    assert!(results.iter().any(|(id, _)| *id == on_disk), "{results:?}");
    assert_eq!(load_or_create(&path).unwrap(), (on_disk, false));
}

/// An unwritable id location resolves to the `unknown` source (which
/// `loom-daemon host-id` turns into a non-zero exit), never a fake id.
#[test]
#[serial(loom_config_env)]
fn an_unwritable_id_path_is_unknown() {
    let _guard = EnvGuard::clear();
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, "").unwrap();
    std::env::set_var(HOST_ID_FILE_ENV, blocker.join(HOST_ID_FILENAME));
    let resolved = resolve();
    assert_eq!(resolved.source, HostIdSource::Unknown);
    assert_eq!(resolved.id, UNKNOWN_HOST);
}

/// The daemon exports a persisted id to its children; an env-sourced id is
/// left alone, and `unknown-host` is never exported as if it were an identity.
#[test]
#[serial(loom_config_env)]
fn export_for_children_exports_real_ids_only() {
    let _guard = EnvGuard::clear();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(HOST_ID_FILENAME);
    std::fs::write(&path, "persisted-for-export\n").unwrap();
    std::env::set_var(HOST_ID_FILE_ENV, &path);
    let resolved = export_for_children();
    assert_eq!(resolved.id, "persisted-for-export");
    assert_eq!(std::env::var(HOST_ID_ENV).unwrap(), "persisted-for-export");

    std::env::set_var(HOST_ID_ENV, "pinned");
    assert_eq!(export_for_children().source, HostIdSource::Env);
    assert_eq!(std::env::var(HOST_ID_ENV).unwrap(), "pinned");

    std::env::remove_var(HOST_ID_ENV);
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, "").unwrap();
    std::env::set_var(HOST_ID_FILE_ENV, blocker.join(HOST_ID_FILENAME));
    assert_eq!(export_for_children().source, HostIdSource::Unknown);
    assert!(std::env::var(HOST_ID_ENV).is_err(), "unknown-host must not be exported");
}
