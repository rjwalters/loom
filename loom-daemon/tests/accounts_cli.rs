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
