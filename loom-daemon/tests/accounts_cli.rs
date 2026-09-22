#[cfg(unix)]
#[test]
fn login_child_exit_status_is_preserved_by_cli() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let fixture = tempfile::tempdir().unwrap();
    let bin_dir = fixture.path().join("bin");
    let workspace = fixture.path().join("workspace");
    let profiles = fixture.path().join("profiles");
    std::fs::create_dir(&bin_dir).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let codex = bin_dir.join("codex");
    std::fs::write(&codex, "#!/bin/sh\nexit 23\n").unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin_dir.clone()).chain(std::env::split_paths(&inherited_path)),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "accounts",
            "--workspace",
            workspace.to_str().unwrap(),
            "add",
            "codex",
            "alice",
        ])
        .env("LOOM_CODEX_PROFILE_ROOT", &profiles)
        .env("PATH", path)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(23));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Codex login failed or was cancelled"));
    assert!(!stderr.contains("auth.json"));
    assert!(!profiles.join("alice").exists());
}

/// Issue #8407 exit-code contract, end to end through the real binary.
///
/// A host with no Codex profiles **says so and exits 0** — an un-provisioned
/// host is not an outage, and a fleet script gating dispatch on this must not
/// read one as such.
#[test]
fn accounts_check_on_a_host_with_no_profiles_says_so_and_exits_zero() {
    use std::process::Command;

    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    let profiles = fixture.path().join("profiles");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&profiles).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "accounts",
            "--workspace",
            workspace.to_str().unwrap(),
            "check",
        ])
        .env("LOOM_CODEX_PROFILE_ROOT", &profiles)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No Codex accounts registered"), "{stdout}");
}

/// The other half of the contract: accounts exist but none is selectable —
/// every one disabled here — so the command reports them and exits 1.
/// Nothing in the output may name a credential file.
#[test]
fn accounts_check_exits_one_when_no_account_is_selectable() {
    use std::process::Command;

    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    let profiles = fixture.path().join("profiles");
    std::fs::create_dir_all(workspace.join(".loom")).unwrap();
    std::fs::create_dir_all(profiles.join("alpha")).unwrap();
    std::fs::write(profiles.join("alpha/auth.json"), "recognizable-secret").unwrap();
    std::fs::write(
        workspace.join(".loom/accounts.json"),
        r#"{"version":1,"accounts":[{"provider":"codex","name":"alpha","credential_kind":"codex_home","credential_reference":"alpha","enabled":false}]}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "accounts",
            "--workspace",
            workspace.to_str().unwrap(),
            "check",
        ])
        .env("LOOM_CODEX_PROFILE_ROOT", &profiles)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("alpha"), "{stdout}");
    assert!(stdout.contains("skipped"), "{stdout}");
    assert!(!stdout.contains("recognizable-secret"), "{stdout}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("recognizable-secret"));
}

/// Issue #8517: `accounts --workspace` is a *global* `String` argument, and
/// `session start` / `session shell` used to declare their own `workspace:
/// Option<PathBuf>` under the same clap id. clap propagates the global into
/// the subcommand's matches, derive then reads the nested field under that
/// id, and the type mismatch aborted every invocation with "Mismatch between
/// definition and access of `workspace`". The nested flag is now
/// `--mount-workspace` with its own id; this test only asserts the parser
/// gets past argument access — the fake `docker` on PATH fails, so the
/// command itself fails, but as an ordinary error, never a panic.
#[cfg(unix)]
#[test]
fn session_start_and_shell_parse_mount_workspace_without_panicking() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let fixture = tempfile::tempdir().unwrap();
    let bin_dir = fixture.path().join("bin");
    let workspace = fixture.path().join("workspace");
    let mount = fixture.path().join("mount");
    let profiles = fixture.path().join("profiles");
    for dir in [&bin_dir, &workspace, &mount, &profiles] {
        std::fs::create_dir(dir).unwrap();
    }
    let docker = bin_dir.join("docker");
    std::fs::write(&docker, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin_dir.clone()).chain(std::env::split_paths(&inherited_path)),
    )
    .unwrap();

    // The first case is the pre-fix repro: no nested flag at all, only the
    // global `--workspace` — the propagated global alone was enough to panic.
    for (sub, extra) in [
        ("start", vec!["--json"]),
        ("start", vec!["--mount-workspace", mount.to_str().unwrap(), "--json"]),
        ("shell", vec!["--mount-workspace", mount.to_str().unwrap()]),
    ] {
        let mut args = vec![
            "accounts",
            "--workspace",
            workspace.to_str().unwrap(),
            "session",
            sub,
            "alice",
        ];
        args.extend(extra);
        let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
            .args(&args)
            .env("LOOM_CODEX_PROFILE_ROOT", &profiles)
            .env("PATH", &path)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("panicked")
                && !stderr.contains("Mismatch between definition and access"),
            "session {sub} must not panic on argument access: {stderr}"
        );
        assert_ne!(
            output.status.code(),
            Some(101),
            "session {sub}: exit 101 is a Rust panic: {stderr}"
        );
    }
}
