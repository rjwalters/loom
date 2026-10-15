#![allow(clippy::unwrap_used)]

use super::*;
use serial_test::serial;

/// Saves and clears every input of [`resolve`] and [`lease_host_identity`]
/// (`$HOSTNAME` included, which must no longer matter to [`resolve`]),
/// restoring the ambient values on drop.
pub(crate) struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    pub(crate) fn clear() -> Self {
        let keys = [
            HOST_ID_ENV,
            LEASE_HOST_ID_ENV,
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

/// Declares a test holding both env locks its inputs need: `loom_config_env`
/// (outer, for `LOOM_CONFIG_DEFAULTS_FILE`) then the crate-default group
/// (inner, for `LOOM_HOST_ID`/`HOSTNAME`, shared with `sweep_registry`'s
/// tests). Multi-key `#[serial(a, b)]` cannot name the default group, so it
/// nests, in the order `role_collision`'s nested test documents.
macro_rules! env_test {
    ($(#[$m:meta])* fn $name:ident() $body:block) => {
        $(#[$m])*
        #[test]
        #[serial(loom_config_env)]
        fn $name() {
            #[serial]
            fn inner() $body
            inner();
        }
    };
}

fn system_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Write a machine-tier fixture and point `LOOM_CONFIG_DEFAULTS_FILE` at it.
fn machine_tier(dir: &Path, json: &str) -> PathBuf {
    let path = dir.join("defaults.json");
    std::fs::write(&path, json).unwrap();
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, &path);
    path
}

env_test! {
    /// AC: `fleet.hostId` beats `LOOM_HOST_ID`.
    fn config_beats_env() {
        let _guard = EnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(HOST_ID_FILE_ENV, dir.path().join(HOST_ID_FILENAME));
        machine_tier(dir.path(), r#"{"fleet":{"hostId":"loom-worker-1"}}"#);
        std::env::set_var(HOST_ID_ENV, "other");

        let resolved = resolve();
        assert_eq!(resolved.id, "loom-worker-1");
        assert_eq!(resolved.source, HostIdSource::Config);
        assert_eq!(host_identity(), "loom-worker-1");

        // Surrounding whitespace in the roster value is trimmed.
        machine_tier(dir.path(), r#"{"fleet":{"hostId":"  loom-worker-2 \n"}}"#);
        assert_eq!(host_identity(), "loom-worker-2");
    }
}

env_test! {
    /// AC: with no usable `fleet.hostId`, `LOOM_HOST_ID` wins; a whitespace-only
    /// `LOOM_HOST_ID` is ignored.
    fn env_wins_without_a_usable_config_value() {
        let _guard = EnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        let id_file = dir.path().join(HOST_ID_FILENAME);
        std::fs::write(&id_file, "persisted-id\n").unwrap();
        std::env::set_var(HOST_ID_FILE_ENV, &id_file);
        std::env::set_var(HOST_ID_ENV, "robb-studio");

        // Missing file.
        std::env::set_var(
            crate::config_resolver::PRIVATE_DEFAULTS_ENV,
            dir.path().join("does-not-exist.json"),
        );
        assert_eq!(resolve().id, "robb-studio");
        assert_eq!(resolve().source, HostIdSource::Env);

        for tier in [
            r#"{"fleet":{"captain":"x"}}"#, // missing key
            r#"{"fleet":{"hostId":"   "}}"#, // blank
            r#"{"fleet":{"hostId":42}}"#,    // non-string
            r#"{"fleet":{"hostId":null}}"#,  // null
            "not json",                      // malformed
        ] {
            machine_tier(dir.path(), tier);
            assert_eq!(resolve().id, "robb-studio", "tier {tier}");
        }

        // An explicitly-empty override disables the tier entirely.
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        assert_eq!(resolve().id, "robb-studio");

        std::env::set_var(HOST_ID_ENV, "   ");
        assert_eq!(resolve().id, "persisted-id", "a blank LOOM_HOST_ID must not shadow the file");
        assert_eq!(resolve().source, HostIdSource::Persisted);
    }
}

env_test! {
    /// AC (no hostname fallback): with no config and no `LOOM_HOST_ID`, the id
    /// is the persisted generated one — neither `$HOSTNAME` nor the `hostname`
    /// binary — and it does not follow `$HOSTNAME`.
    fn no_hostname_fallback() {
        let _guard = EnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join(HOST_ID_FILENAME);
        std::env::set_var(HOST_ID_FILE_ENV, &path);
        std::env::set_var("HOSTNAME", "studio");

        let first = resolve();
        assert_eq!(first.source, HostIdSource::Generated);
        assert!(first.id.starts_with(GENERATED_PREFIX), "{first:?}");
        assert_eq!(first.id.len(), GENERATED_PREFIX.len() + 16, "{first:?}");
        assert_ne!(first.id, "studio");
        assert_ne!(first.id, system_hostname());
        assert_eq!(first.path.as_deref(), Some(path.as_path()));

        std::env::set_var("HOSTNAME", "launchd-has-another");
        assert_eq!(host_identity(), first.id);
        std::env::remove_var("HOSTNAME");
        assert_eq!(host_identity(), first.id);
    }
}

env_test! {
    /// AC (persistence): repeated calls, and a call after dropping the in-process
    /// cache, return the same id; the file is owner-only with a trailing newline.
    fn persisted_id_survives_cache_drop_and_is_owner_only() {
        let _guard = EnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HOST_ID_FILENAME);
        std::env::set_var(HOST_ID_FILE_ENV, &path);

        let first = host_identity();
        assert_eq!(host_identity(), first);
        clear_cache();
        let after_restart = resolve();
        assert_eq!(after_restart.id, first);
        assert_eq!(after_restart.source, HostIdSource::Persisted);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{first}\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "persisted host id must be owner-only, got {mode:o}");
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != HOST_ID_FILENAME)
            .collect();
        assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");
    }
}

env_test! {
    /// AC: a pre-seeded file's value is returned verbatim (trimmed).
    fn a_pre_seeded_id_is_returned_verbatim() {
        let _guard = EnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HOST_ID_FILENAME);
        std::fs::write(&path, "  joseph-superset\n\n").unwrap();
        std::env::set_var(HOST_ID_FILE_ENV, &path);
        assert_eq!(resolve().id, "joseph-superset");
        assert_eq!(load_or_create(&path).unwrap(), ("joseph-superset".to_string(), false));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "  joseph-superset\n\n", "untouched");
    }
}

/// AC: concurrent first use converges on one id, with exactly one creator.
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
    let on_disk = std::fs::read_to_string(&path).unwrap().trim().to_string();
    assert!(results.iter().all(|(id, _)| *id == on_disk), "{results:?} vs {on_disk}");
    assert_eq!(results.iter().filter(|(_, created)| *created).count(), 1, "{results:?}");
}

/// An empty file (a crashed writer) is replaced, and concurrent replacers
/// converge on the one surviving id.
#[test]
fn concurrent_replacement_of_an_empty_file_converges() {
    const THREADS: usize = 8;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(HOST_ID_FILENAME);
    std::fs::write(&path, "\n").unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let link = |from: &Path, to: &Path| {
                    let result = std::fs::hard_link(from, to);
                    barrier.wait();
                    result
                };
                load_or_create_with(&path, link).unwrap()
            })
        })
        .collect();
    let results: Vec<(String, bool)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let on_disk = std::fs::read_to_string(&path).unwrap().trim().to_string();
    assert!(is_valid_id(&on_disk));
    assert!(results.iter().all(|(id, _)| *id == on_disk), "{results:?} vs {on_disk}");
    assert_eq!(results.iter().filter(|(_, created)| *created).count(), 1, "{results:?}");
}

/// A filesystem that refuses hard links still gets a persisted id via the
/// `O_EXCL` fallback, and a second caller reads it back.
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
    assert_eq!(load_or_create_with(&path, refuse).unwrap(), (id, false));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "temp file leaked");
}

/// A non-empty file that is not an id is an error, and is never overwritten.
#[test]
fn a_garbage_file_is_an_error_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(HOST_ID_FILENAME);
    std::fs::write(&path, "two words\n").unwrap();
    let err = load_or_create(&path).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "two words\n");
}

env_test! {
    /// AC: an unwritable id-file location resolves to `unknown-host`, never a
    /// hostname.
    fn an_unwritable_id_path_is_unknown_host() {
        let _guard = EnvGuard::clear();
        std::env::set_var("HOSTNAME", "studio");
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();
        std::env::set_var(HOST_ID_FILE_ENV, blocker.join(HOST_ID_FILENAME));
        let resolved = resolve();
        assert_eq!(resolved.source, HostIdSource::Unknown);
        assert_eq!(resolved.id, UNKNOWN_HOST);
        assert_eq!(host_identity(), UNKNOWN_HOST);
    }
}

#[test]
fn generated_ids_are_valid_and_distinct_from_opaque_lease_ids() {
    let id = generate_id();
    assert!(is_valid_id(&id), "{id}");
    assert!(id.starts_with(GENERATED_PREFIX));
    assert_ne!(generate_id(), id);
    assert!(!id.starts_with("host-"), "must not look like an opaque lease id");
}

#[test]
fn id_validation() {
    assert!(is_valid_id("loom-worker-1"));
    assert!(is_valid_id("mac.studio_2"));
    assert!(!is_valid_id(""));
    assert!(!is_valid_id("two words"));
    assert!(!is_valid_id("tab\there"));
    assert!(!is_valid_id(&"x".repeat(129)));
}

env_test! {
    /// The lease chain is unchanged (`LOOM_HOST_ID` > `$HOSTNAME`) and ignores
    /// both `fleet.hostId` and the persisted id.
    fn lease_identity_keeps_the_legacy_chain() {
        let _guard = EnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        machine_tier(dir.path(), r#"{"fleet":{"hostId":"roster-name"}}"#);
        let id_file = dir.path().join(HOST_ID_FILENAME);
        std::fs::write(&id_file, "persisted-id\n").unwrap();
        std::env::set_var(HOST_ID_FILE_ENV, &id_file);

        std::env::set_var(HOST_ID_ENV, "robb-studio");
        std::env::set_var("HOSTNAME", "studio");
        assert_eq!(lease_host_identity(), "robb-studio");
        std::env::set_var(HOST_ID_ENV, "  ");
        assert_eq!(lease_host_identity(), "studio");
        std::env::remove_var(HOST_ID_ENV);
        assert_eq!(lease_host_identity(), "studio");
        std::env::set_var("HOSTNAME", "  ");
        let fallback = lease_host_identity();
        assert_ne!(fallback, "");
        assert_ne!(fallback, "roster-name");
        assert_ne!(fallback, "persisted-id");
    }
}

env_test! {
    /// #11740: a generated persisted id is replaced by the roster name, and the
    /// new id is what later resolutions (and a restart) return.
    fn adopt_roster_name_replaces_only_a_generated_id() {
        let _guard = EnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HOST_ID_FILENAME);
        std::env::set_var(HOST_ID_FILE_ENV, &path);

        let generated = host_identity();
        assert!(generated.starts_with(GENERATED_PREFIX));
        assert!(adopt_roster_name("robb-pro").unwrap());
        assert_eq!(host_identity(), "robb-pro");
        clear_cache();
        assert_eq!(resolve().id, "robb-pro");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "robb-pro\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // Already a roster name: never replaced again.
        assert!(!adopt_roster_name("robb-studio").unwrap());
        assert_eq!(host_identity(), "robb-pro");
    }
}

env_test! {
    /// #11740: an explicit identity (env, config, operator-written file) is
    /// never second-guessed by a hostname match.
    fn adopt_roster_name_never_overrides_an_explicit_identity() {
        let _guard = EnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HOST_ID_FILENAME);
        std::env::set_var(HOST_ID_FILE_ENV, &path);

        std::env::set_var(HOST_ID_ENV, "loom-worker-1");
        assert!(!adopt_roster_name("robb-pro").unwrap());
        assert_eq!(host_identity(), "loom-worker-1");
        std::env::remove_var(HOST_ID_ENV);

        std::fs::write(&path, "joseph-superset\n").unwrap();
        clear_cache();
        assert!(!adopt_roster_name("robb-pro").unwrap());
        assert_eq!(host_identity(), "joseph-superset");
        assert!(!adopt_roster_name("bad name").unwrap(), "invalid ids are refused");
    }
}

#[test]
fn adopt_at_refuses_when_the_file_changed_underneath() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(HOST_ID_FILENAME);
    std::fs::write(&path, "loom-host-1111111111111111\n").unwrap();
    // The caller resolved a different generated id than the file now holds.
    assert!(!adopt_at(&path, "loom-host-2222222222222222", "robb-pro").unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "loom-host-1111111111111111\n");
    assert!(adopt_at(&path, "loom-host-1111111111111111", "robb-pro").unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "robb-pro\n");
}

#[test]
#[serial]
fn hostname_candidates_are_short_and_deduplicated() {
    let saved = std::env::var("HOSTNAME").ok();
    std::env::set_var("HOSTNAME", "robb-pro.local");
    let got = hostname_candidates();
    match saved {
        Some(v) => std::env::set_var("HOSTNAME", v),
        None => std::env::remove_var("HOSTNAME"),
    }
    assert_eq!(got.first().map(String::as_str), Some("robb-pro"), "{got:?}");
    assert!(got.iter().all(|c| !c.is_empty() && !c.contains('.')), "{got:?}");
    for (i, a) in got.iter().enumerate() {
        assert!(!got[i + 1..].iter().any(|b| b.eq_ignore_ascii_case(a)), "{got:?}");
    }
}

env_test! {
    /// `LOOM_LEASE_HOST_ID` pins only the lease chain (#11906): a step child
    /// that carries it keeps the telemetry `host_identity()` of the daemon.
    fn lease_pin_leaves_the_telemetry_identity_alone() {
        let _guard = EnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        let id_file = dir.path().join("host-id");
        std::fs::write(&id_file, "persisted-studio\n").unwrap();
        std::env::set_var(HOST_ID_FILE_ENV, &id_file);
        std::env::set_var("HOSTNAME", "container-9f2c");
        let before = host_identity();
        assert_eq!(before, "persisted-studio");
        std::env::set_var(LEASE_HOST_ID_ENV, "daemon-host");
        assert_eq!(lease_host_identity(), "daemon-host");
        assert_eq!(host_identity(), before, "the lease pin never reaches host.id");
        std::env::set_var(HOST_ID_ENV, "explicit");
        assert_eq!(lease_host_identity(), "daemon-host", "lease pin beats LOOM_HOST_ID");
    }
}
