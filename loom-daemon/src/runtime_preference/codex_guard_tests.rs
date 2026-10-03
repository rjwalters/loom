//! Guard-hook readiness as Codex availability for merging roles (#9390).
use super::*;
use crate::runtime_preference::availability::{availability, Availability, CredentialSource};
use crate::runtime_preference::resolve::Tap;
use crate::tokens_pool::codex_hooks::test_support::guard_ready;
use std::fs;
use std::path::PathBuf;

const ENV: [&str; 6] = [
    "LOOM_CODEX_PROFILE_ROOT",
    "LOOM_CODEX_PROFILE",
    "LOOM_CODEX_HOME",
    "CODEX_HOME",
    "LOOM_SPAWN_NO_EXPORT",
    "LOOM_CODEX_NO_EXEC",
];

/// A workspace laid out like an installed repo (real role/runtime manifests,
/// an installed bridge, an executable `spawn-codex.sh` stub) with four Codex
/// accounts: `ready`, `untrusted`, `missing` (no hook installed), and
/// `private` (a private-clone session's profile, never judged here).
struct Fixture {
    root: tempfile::TempDir,
    profiles: tempfile::TempDir,
    _shared: tempfile::TempDir,
    prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl Fixture {
    fn new() -> Self {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let root = tempfile::tempdir().unwrap();
        let profiles = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        let loom = root.path().join(".loom");
        for (sub, from) in [
            ("roles", "defaults/roles"),
            ("runtimes", "defaults/runtimes"),
        ] {
            fs::create_dir_all(loom.join(sub)).unwrap();
            for entry in fs::read_dir(repo.join(from)).unwrap().flatten() {
                fs::copy(entry.path(), loom.join(sub).join(entry.file_name())).unwrap();
            }
        }
        fs::create_dir_all(loom.join("scripts")).unwrap();
        fs::create_dir_all(loom.join("hooks")).unwrap();
        fs::write(loom.join("hooks/guard-codex-bridge.sh"), "#!/bin/sh\n").unwrap();
        let adapter = loom.join("scripts/spawn-codex.sh");
        fs::write(&adapter, "#!/bin/sh\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let names = ["ready", "untrusted", "missing", "private"];
        for name in names {
            fs::create_dir_all(profiles.path().join(name)).unwrap();
        }
        let private_state = profiles.path().join(".private-sessions/private");
        fs::create_dir_all(&private_state).unwrap();
        fs::write(private_state.join("workspace.json"), "{}").unwrap();
        let accounts = names
            .iter()
            .map(|name| {
                format!(
                    r#"{{"provider":"codex","name":"{name}","credential_kind":"codex_home","credential_reference":"{name}","enabled":true}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            loom.join("accounts.json"),
            format!(r#"{{"version":1,"accounts":[{accounts}]}}"#),
        )
        .unwrap();
        let shared_env = crate::tokens_pool::paths::SHARED_ACCOUNTS_ROOT_ENV;
        let prior = ENV
            .iter()
            .chain([&shared_env])
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in ENV {
            std::env::remove_var(key);
        }
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
        std::env::set_var(shared_env, shared.path());
        let fixture = Self {
            root,
            profiles,
            _shared: shared,
            prior,
        };
        guard_ready(&fixture.profile("ready"), None);
        // Installed, but its only trust was taken somewhere Codex won't look.
        guard_ready(&fixture.profile("untrusted"), Some(Path::new("/elsewhere")));
        fixture
    }

    fn profile(&self, name: &str) -> PathBuf {
        self.profiles.path().join(name)
    }

    fn codex_for(&self, role: &str) -> Availability {
        let admitted =
            crate::runtime_admission::resolve_and_admit(self.root.path(), role, Some("codex"))
                .unwrap();
        availability(self.root.path(), &Tap::runtime("codex"), &admitted, 0)
    }

    fn ready_everything(&self) {
        guard_ready(&self.profile("missing"), None);
        guard_ready(&self.profile("untrusted"), None);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (key, value) in self.prior.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn only_the_merging_roles_are_guarded() {
    for role in ["champion", "judge"] {
        assert!(guarded(role), "{role}");
    }
    for role in [
        "curator",
        "hermit",
        "guide",
        "auditor",
        "architect",
        "builder",
        "doctor",
    ] {
        assert!(!guarded(role), "{role}");
    }
}

#[test]
#[serial_test::serial]
fn every_shared_profile_must_be_ready_and_private_ones_are_left_to_the_container() {
    let f = Fixture::new();
    let mut unready = unready_profiles(f.root.path()).unwrap();
    unready.sort();
    assert_eq!(unready, ["missing", "untrusted"], "the private profile is not judged here");
    f.ready_everything();
    assert_eq!(unready_profiles(f.root.path()).unwrap(), Vec::<String>::new());
    // A profile going stale closes the gate again.
    fs::write(f.profile("missing").join("hooks.json"), "{}").unwrap();
    assert_eq!(unready_profiles(f.root.path()).unwrap(), ["missing"]);
    // ...and so does a checkout with no bridge for the entry to run.
    f.ready_everything();
    fs::remove_file(f.root.path().join(".loom/hooks/guard-codex-bridge.sh")).unwrap();
    assert_eq!(unready_profiles(f.root.path()).unwrap().len(), 3);
}

/// The #9390 fallback: while any shared seat is unguarded, Codex is
/// unavailable to Champion and Judge (so the preference walk moves on to the
/// next tap) and stays available to every read-only role.
#[test]
#[serial_test::serial]
fn codex_is_unavailable_to_merging_roles_until_every_seat_is_guarded() {
    let f = Fixture::new();
    for role in ["champion", "judge"] {
        match f.codex_for(role) {
            Availability::Exhausted { source, detail, .. } => {
                assert_eq!(source, CredentialSource::CodexAccounts);
                assert!(detail.contains("missing") && detail.contains("untrusted"), "{detail}");
                assert!(detail.contains("#9390"), "{detail}");
            }
            other => panic!("{role}: expected Exhausted, got {other:?}"),
        }
    }
    assert!(f.codex_for("curator").is_spawnable());
    f.ready_everything();
    for role in ["champion", "judge"] {
        assert!(f.codex_for(role).is_spawnable(), "{role}");
    }
}
