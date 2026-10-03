//! Guard-hook readiness as Codex availability for merging roles (#9390
//! follow-up), against the SHIPPED `provision-codex-hooks.sh`: the readiness
//! verdict is that script's, never a second implementation.
use super::*;
use crate::runtime_preference::availability::{availability, Availability, CredentialSource};
use crate::runtime_preference::resolve::Tap;
use std::fs;

const ENV: [&str; 6] = [
    "LOOM_CODEX_PROFILE_ROOT",
    "LOOM_CODEX_PROFILE",
    "LOOM_CODEX_HOME",
    "CODEX_HOME",
    "LOOM_SPAWN_NO_EXPORT",
    "LOOM_CODEX_NO_EXEC",
];

/// A workspace laid out like an installed repo (real role/runtime manifests,
/// the real provisioner and bridge, an executable `spawn-codex.sh` stub) with
/// four Codex accounts: `ready`, `untrusted`, `missing` (no hook installed),
/// and `private` (a private-clone session's profile).
struct Fixture {
    root: tempfile::TempDir,
    profiles: tempfile::TempDir,
    shared: tempfile::TempDir,
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
        fs::copy(
            repo.join("defaults/scripts/provision-codex-hooks.sh"),
            loom.join("scripts/provision-codex-hooks.sh"),
        )
        .unwrap();
        fs::copy(
            repo.join("defaults/hooks/guard-codex-bridge.sh"),
            loom.join("hooks/guard-codex-bridge.sh"),
        )
        .unwrap();
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
        let prior = ENV
            .iter()
            .chain([&crate::tokens_pool::paths::SHARED_ACCOUNTS_ROOT_ENV])
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in ENV {
            std::env::remove_var(key);
        }
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
        std::env::set_var(crate::tokens_pool::paths::SHARED_ACCOUNTS_ROOT_ENV, shared.path());
        let fixture = Self {
            root,
            profiles,
            shared,
            prior,
        };
        fixture.install("ready");
        fixture.trust("ready");
        fixture.install("untrusted");
        fixture
    }

    fn profile(&self, name: &str) -> PathBuf {
        self.profiles.path().join(name)
    }

    fn install(&self, name: &str) {
        let status = Command::new("bash")
            .arg(
                self.root
                    .path()
                    .join(".loom/scripts/provision-codex-hooks.sh"),
            )
            .args(["install", "--codex-home"])
            .arg(self.profile(name))
            .arg("--workspace")
            .arg(self.root.path())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "install {name}");
    }

    /// The operator's one-time trust decision, keyed the way Codex keys it for
    /// a bare-metal run of this profile.
    fn trust(&self, name: &str) {
        let home = self.profile(name).canonicalize().unwrap();
        fs::write(
            self.profile(name).join("config.toml"),
            format!(
                "[hooks.state.\"{}/hooks.json:pre_tool_use:0:0\"]\ntrusted_hash = \"sha256:op\"\n",
                home.display()
            ),
        )
        .unwrap();
    }

    fn codex_for(&self, role: &str) -> Availability {
        let admitted =
            crate::runtime_admission::resolve_and_admit(self.root.path(), role, Some("codex"))
                .unwrap();
        availability(self.root.path(), &Tap::runtime("codex"), &admitted, 0)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.shared.path();
        for (key, value) in self.prior.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn have_jq() -> bool {
    Command::new("jq")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
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
    if !have_jq() {
        eprintln!("skipping: jq unavailable");
        return;
    }
    let f = Fixture::new();
    let mut unready = unready_profiles(f.root.path()).unwrap();
    unready.sort();
    assert_eq!(unready, ["missing", "untrusted"], "the private profile is not judged here");
    f.install("missing");
    f.trust("missing");
    f.trust("untrusted");
    assert_eq!(unready_profiles(f.root.path()).unwrap(), Vec::<String>::new());
    // A profile going stale closes the gate again.
    fs::write(f.profile("missing").join("hooks.json"), "{}").unwrap();
    assert_eq!(unready_profiles(f.root.path()).unwrap(), ["missing"]);
}

#[test]
#[serial_test::serial]
fn no_installed_provisioner_is_never_read_as_ready() {
    let f = Fixture::new();
    fs::remove_file(f.root.path().join(".loom/scripts/provision-codex-hooks.sh")).unwrap();
    assert!(unready_profiles(f.root.path()).is_err());
}

/// The #9390 fallback: while any shared seat is unguarded, Codex is
/// unavailable to Champion and Judge — so the preference walk moves on to the
/// next tap — and stays available to every read-only role.
#[test]
#[serial_test::serial]
fn codex_is_unavailable_to_merging_roles_until_every_seat_is_guarded() {
    if !have_jq() {
        eprintln!("skipping: jq unavailable");
        return;
    }
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
    f.install("missing");
    f.trust("missing");
    f.trust("untrusted");
    for role in ["champion", "judge"] {
        assert!(f.codex_for(role).is_spawnable(), "{role}");
    }
}
