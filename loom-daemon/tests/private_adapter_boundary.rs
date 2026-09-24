//! Credential-free adapter regressions: neither a model nor Docker is needed
//! to prove private dispatch refuses host-execution escape flags before spawn.
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::{fd::AsRawFd, unix::process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    dir: tempfile::TempDir,
    profile: PathBuf,
    scripts: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles/private");
        for path in [&profile, &dir.path().join("bin"), &dir.path().join("repo")] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(profile.join("auth.json"), r#"{"synthetic":true}"#).unwrap();
        std::fs::write(profile.join(".session-managed.json"), "{}").unwrap();
        let state = dir.path().join("profiles/.private-sessions/private");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("workspace.json"), "{}").unwrap();
        let codex = dir.path().join("bin/codex");
        std::fs::write(&codex, "#!/bin/sh\nprintf invoked > \"$TEST_HOST_CODEX_TOUCH\"\n").unwrap();
        std::fs::set_permissions(codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        // No usable ambient installation: the inherited-lease refusal must
        // use SELF_BIN before provider discovery can consult the host PATH.
        let ambient = dir.path().join("bin/loom-daemon");
        std::fs::write(&ambient, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(ambient, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&profile, dir.path().join("profile-link")).unwrap();
        Self {
            dir,
            profile,
            scripts: Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts"),
        }
    }
    fn command(&self, adapter: &str) -> Command {
        let mut command = Command::new("bash");
        command
            .arg(self.scripts.join(adapter))
            .args(["-p", "fixture", "--dangerously-skip-permissions"])
            .current_dir(self.dir.path().join("repo"))
            .env_clear()
            .env("HOME", self.dir.path())
            .env("TMPDIR", self.dir.path())
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.dir.path().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("LOOM_WORKSPACE", self.dir.path().join("repo"))
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env("LOOM_RUNTIME", "codex")
            .env("LOOM_CODEX_AUTH_MODE_CHECK", "0")
            .env("LOOM_DAEMON_SELF_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
            .env("LOOM_CODEX_PROFILE_ROOT", self.dir.path().join("profiles"))
            .env("TEST_HOST_CODEX_TOUCH", self.dir.path().join("host-invoked"));
        command
    }
    fn refused(&self, command: &mut Command, flag: &str) {
        let output = command.output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(flag) && stderr.contains("forbidden"), "{stderr}");
        assert!(!self.dir.path().join("host-invoked").exists());
        assert!(!self
            .dir
            .path()
            .join("profiles/.private-sessions/private/job.json")
            .exists());
        assert!(!self.dir.path().join("repo/.loom/private-jobs").exists());
    }
}

#[test]
fn private_profiles_refuse_host_escape_flags_through_both_adapters() {
    let f = Fixture::new();
    for adapter in ["spawn-worker.sh", "spawn-codex.sh"] {
        for (flag, value) in [
            ("LOOM_CODEX_SESSION_EXEC", "0"),
            ("LOOM_SPAWN_NO_EXPORT", "1"),
        ] {
            for pin in [
                "CODEX_HOME",
                "LOOM_CODEX_HOME",
                "LOOM_CODEX_PROFILE",
                "symlink",
            ] {
                let mut command = f.command(adapter);
                command.env(flag, value);
                match pin {
                    "LOOM_CODEX_PROFILE" => {
                        command.env(pin, "private");
                    }
                    "symlink" => {
                        command.env("CODEX_HOME", f.dir.path().join("profile-link"));
                    }
                    _ => {
                        command.env(pin, &f.profile);
                    }
                }
                f.refused(&mut command, flag);
            }
            // Already-prepared dispatch cannot bypass the direct adapter guard.
            let mut command = f.command(adapter);
            command.env("LOOM_PRIVATE_LEASE_FD", "198").env(flag, value);
            let lock = std::fs::File::create(f.dir.path().join("account.lock")).unwrap();
            let fd = lock.as_raw_fd();
            assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(fd, 198) < 0 || libc::fcntl(198, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            f.refused(&mut command, flag);
        }
    }
}

#[test]
fn private_profile_missing_adoption_marker_does_not_fall_back_to_host() {
    let f = Fixture::new();
    std::fs::remove_file(f.profile.join(".session-managed.json")).unwrap();
    for adapter in ["spawn-worker.sh", "spawn-codex.sh"] {
        let output = f
            .command(adapter)
            .env("CODEX_HOME", &f.profile)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("recover the owned session"));
        assert!(!f.dir.path().join("host-invoked").exists());
    }
}

#[test]
fn skip_export_checks_the_home_codex_still_inherits() {
    let f = Fixture::new();
    let legacy = f.dir.path().join("legacy");
    std::fs::create_dir(&legacy).unwrap();
    symlink(&f.profile, f.dir.path().join(".codex")).unwrap();
    for adapter in ["spawn-worker.sh", "spawn-codex.sh"] {
        let mut command = f.command(adapter);
        command
            .env("LOOM_CODEX_HOME", &legacy)
            .env("CODEX_HOME", &f.profile)
            .env("LOOM_SPAWN_NO_EXPORT", "1");
        f.refused(&mut command, "LOOM_SPAWN_NO_EXPORT");
        // With CODEX_HOME unset, skip-export still leaves Codex's ambient
        // HOME/.codex active despite the Loom-only nonprivate pin.
        let mut ambient = f.command(adapter);
        ambient
            .env("LOOM_CODEX_HOME", &legacy)
            .env("LOOM_SPAWN_NO_EXPORT", "1");
        f.refused(&mut ambient, "LOOM_SPAWN_NO_EXPORT");
    }
}

#[test]
fn dry_runs_and_legacy_explicit_profiles_keep_escape_flag_behavior() {
    let f = Fixture::new();
    let legacy = f.dir.path().join("legacy");
    std::fs::create_dir(&legacy).unwrap();
    std::fs::write(legacy.join("auth.json"), r#"{"synthetic":true}"#).unwrap();
    for adapter in ["spawn-worker.sh", "spawn-codex.sh"] {
        let preview = f
            .command(adapter)
            .env("CODEX_HOME", &f.profile)
            .env("LOOM_CODEX_NO_EXEC", "1")
            .env("LOOM_CODEX_SESSION_EXEC", "0")
            .env("LOOM_SPAWN_NO_EXPORT", "1")
            .output()
            .unwrap();
        assert!(preview.status.success(), "{}", String::from_utf8_lossy(&preview.stderr));
        assert!(!f.dir.path().join("host-invoked").exists());
        // Normal resolution gives LOOM_CODEX_HOME precedence over a stale
        // private CODEX_HOME; that inactive candidate must not block it.
        let overridden = f
            .command(adapter)
            .args(["--json", "-m", "gpt-5"])
            .env("LOOM_CODEX_HOME", &legacy)
            .env("CODEX_HOME", &f.profile)
            .env("LOOM_CODEX_SESSION_EXEC", "0")
            .output()
            .unwrap();
        assert!(overridden.status.success(), "{}", String::from_utf8_lossy(&overridden.stderr));
        assert!(f.dir.path().join("host-invoked").exists());
        std::fs::remove_file(f.dir.path().join("host-invoked")).unwrap();
        for (flag, value) in [
            ("LOOM_CODEX_SESSION_EXEC", "0"),
            ("LOOM_SPAWN_NO_EXPORT", "1"),
        ] {
            let output = f
                .command(adapter)
                .env("CODEX_HOME", &legacy)
                .env(flag, value)
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(f.dir.path().join("host-invoked").exists());
            std::fs::remove_file(f.dir.path().join("host-invoked")).unwrap();
        }
    }
    // Direct argv previews never require the new daemon subcommand.
    let preview = f
        .command("spawn-codex.sh")
        .env("CODEX_HOME", &f.profile)
        .env("LOOM_CODEX_NO_EXEC", "1")
        .env("LOOM_CODEX_SESSION_EXEC", "0")
        .env("LOOM_DAEMON_SELF_BIN", "/nonexistent/private-adapter-helper")
        .output()
        .unwrap();
    assert!(preview.status.success(), "{}", String::from_utf8_lossy(&preview.stderr));
}
