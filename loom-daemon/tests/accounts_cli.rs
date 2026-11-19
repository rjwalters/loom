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

// =========================================================================
// Which registry a verb acts on (issue #8540)
// =========================================================================

/// One account named in a registry file, in the on-disk v1 shape.
fn registry_json(name: &str, enabled: bool) -> String {
    format!(
        r#"{{"version":1,"accounts":[{{"provider":"codex","name":"{name}","credential_kind":"codex_home","credential_reference":"{name}","enabled":{enabled}}}]}}"#
    )
}

/// A fixture whose layout mirrors the one #8540 was reported on: a shared
/// machine-level root (the stand-in for `$HOME`) that *contains* the Codex
/// profile root, exactly the containment that used to make every verb run
/// from there fail with "Codex profile root must not be repository-local".
struct RegistryFixture {
    _dir: tempfile::TempDir,
    shared_root: std::path::PathBuf,
    profiles: std::path::PathBuf,
}

impl RegistryFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let shared_root = dir.path().join("home");
        let profiles = shared_root.join(".loom").join("codex-profiles");
        std::fs::create_dir_all(profiles.join("alpha")).unwrap();
        std::fs::write(profiles.join("alpha/auth.json"), "{}").unwrap();
        std::fs::write(
            shared_root.join(".loom").join("accounts.json"),
            registry_json("alpha", true),
        )
        .unwrap();
        Self {
            _dir: dir,
            shared_root,
            profiles,
        }
    }

    fn command(&self, cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
            .arg("accounts")
            .args(args)
            .current_dir(cwd)
            .env("LOOM_CODEX_PROFILE_ROOT", &self.profiles)
            .env("LOOM_SHARED_ACCOUNTS_ROOT", &self.shared_root)
            .output()
            .unwrap()
    }

    fn shared_registry(&self) -> String {
        std::fs::read_to_string(self.shared_root.join(".loom").join("accounts.json")).unwrap()
    }
}

/// Acceptance criterion 1 + 2: from a cwd inside no Loom workspace, `list`
/// reads the **shared** machine-level registry (rather than erroring, and
/// rather than inventing `<cwd>/.loom/accounts.json`), says so, and reports
/// the rows it found as shared rather than repo-local.
#[test]
fn accounts_list_from_a_non_repo_cwd_uses_the_shared_registry() {
    let fixture = RegistryFixture::new();
    let elsewhere = tempfile::tempdir().unwrap();

    let output = fixture.command(elsewhere.path(), &["list"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("codex/alpha"), "{stdout}");
    // Provenance names the registry the row came from, not the file shape.
    assert!(stdout.contains("Shared"), "{stdout}");
    assert!(
        stderr.contains(&format!(
            "Registry: shared: {}",
            fixture
                .shared_root
                .join(".loom")
                .join("accounts.json")
                .display()
        )),
        "{stderr}"
    );
    assert!(!stderr.contains("repository-local"), "{stderr}");
}

/// The original #8540 reproduction: run from the shared root itself (`$HOME`
/// in the report), whose `.loom/codex-profiles` the profile root lives under.
/// That containment used to be read as "the profile root is inside the
/// workspace" and failed every verb.
#[test]
fn accounts_list_from_the_shared_root_itself_does_not_claim_a_repo_local_profile_root() {
    let fixture = RegistryFixture::new();

    let output = fixture.command(&fixture.shared_root.clone(), &["list"]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("repository-local"), "{stderr}");
    assert!(stderr.contains("Registry: shared: "), "{stderr}");
}

/// Acceptance criteria 2 + 3 + 4: a `disable` run from a repo cwd acts on the
/// repo-local registry **only** — the shared entry of the same name stays
/// enabled — and the command says both that it was repo-scoped and that the
/// shared entry it shadows was left alone.
#[test]
fn accounts_disable_from_a_repo_cwd_is_repo_scoped_and_says_so() {
    let fixture = RegistryFixture::new();
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".loom")).unwrap();
    let repo_registry = repo.path().join(".loom").join("accounts.json");
    std::fs::write(&repo_registry, registry_json("alpha", true)).unwrap();

    let output = fixture.command(repo.path(), &["disable", "codex", "alpha"]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!(
            "Registry: repo: {}",
            repo.path()
                .canonicalize()
                .unwrap()
                .join(".loom")
                .join("accounts.json")
                .display()
        )),
        "{stderr}"
    );
    // The shadow note names the account whose shared entry this did not touch.
    assert!(stderr.contains("shadows the shared machine-level registry"), "{stderr}");
    assert!(stderr.contains("alpha"), "{stderr}");

    assert!(
        std::fs::read_to_string(&repo_registry)
            .unwrap()
            .contains("\"enabled\": false"),
        "the repo-local entry must be disabled"
    );
    assert!(
        fixture.shared_registry().contains("\"enabled\":true"),
        "the shared entry must be untouched: {}",
        fixture.shared_registry()
    );
}

/// A shared disable works from a checkout and vetoes both discovered and
/// locally enabled seats. Re-enabling the shared seat lifts that veto.
#[test]
fn shared_disable_is_honored_from_both_kinds_of_workspace() {
    let fixture = RegistryFixture::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let profile = fixture.profiles.join("alpha");
        std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(profile.join("auth.json"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
    }
    let repo = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    for root in [repo.path(), other.path()] {
        std::fs::create_dir(root.join(".loom")).unwrap();
    }
    let local = other.path().join(".loom/accounts.json");
    let original =
        registry_json("alpha", true).replace(r#""name":"alpha""#, r#""name":"local-alias""#);
    std::fs::write(&local, &original).unwrap();
    for (verb, selectable) in [("disable", false), ("enable", true)] {
        let output = fixture.command(repo.path(), &[verb, "codex", "alpha", "--shared"]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(!repo.path().join(".loom/accounts.json").exists());
        assert_eq!(std::fs::read_to_string(&local).unwrap(), original);
        for root in [repo.path(), other.path()] {
            let draw = std::process::Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
                .args(["tokens", "select", "--provider", "codex", "--workspace"])
                .arg(root)
                .env("LOOM_ROLE", "curator")
                .env("LOOM_CODEX_PROFILE_ROOT", &fixture.profiles)
                .env("LOOM_SHARED_ACCOUNTS_ROOT", &fixture.shared_root)
                .output()
                .unwrap();
            assert_eq!(
                draw.status.success(),
                selectable,
                "{}",
                String::from_utf8_lossy(&draw.stderr)
            );
            if selectable {
                let account: serde_json::Value = serde_json::from_slice(&draw.stdout).unwrap();
                assert_eq!(
                    account["name"],
                    if root == other.path() {
                        "local-alias"
                    } else {
                        "alpha"
                    }
                );
            }
        }
    }
}

/// The launch script supplies LOOM_ROLE to the ordinary tokens selector.
#[test]
fn tokens_select_names_the_skipped_seat_for_guarded_roles() {
    let fixture = RegistryFixture::new();
    let repo = tempfile::tempdir().unwrap();
    for role in ["builder", "doctor", "champion", "judge", "curator"] {
        let draw = std::process::Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
            .args(["tokens", "select", "--provider", "codex", "--workspace"])
            .arg(repo.path())
            .env("LOOM_ROLE", role)
            .env("LOOM_CODEX_PROFILE_ROOT", &fixture.profiles)
            .env("LOOM_SHARED_ACCOUNTS_ROOT", &fixture.shared_root)
            .output()
            .unwrap();
        assert_eq!(draw.status.success(), role == "curator");
        if role != "curator" {
            let stderr = String::from_utf8_lossy(&draw.stderr);
            assert!(
                stderr.contains(&format!("Skipping Codex account alpha for {role}")),
                "{stderr}"
            );
        }
    }
}

// =========================================================================
// Issue #14298: `accounts list` reports private-clone repos and session image
// =========================================================================

/// A host-mode account — no private-workspace state, no session marker —
/// gains neither `private_clone` nor `image` in `accounts list --json`, and
/// its human line carries no `repos=`/`image=` suffix: the contract change
/// is purely additive (issue #14298 acceptance criteria 1 and 4).
#[test]
fn accounts_list_json_for_a_host_mode_account_has_no_session_additions() {
    let fixture = RegistryFixture::new();
    let elsewhere = tempfile::tempdir().unwrap();

    let output = fixture.command(elsewhere.path(), &["list", "--json"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert!(entry.get("private_clone").is_none(), "{entry}");
    assert!(entry.get("image").is_none(), "{entry}");

    let human = fixture.command(elsewhere.path(), &["list"]);
    let human_stdout = String::from_utf8_lossy(&human.stdout);
    assert!(human_stdout.contains("codex/alpha"), "{human_stdout}");
    assert!(!human_stdout.contains("repos="), "{human_stdout}");
    assert!(!human_stdout.contains("image="), "{human_stdout}");
}

/// A private-clone account lists its primary and additional registered
/// repositories in both output forms; the image stays absent because no
/// running container can be observed from a test (issue #14298 acceptance
/// criteria 1 and 3 — and the degrade-to-absent rule of criterion 5).
#[test]
fn accounts_list_reports_a_private_clone_accounts_registered_repositories() {
    let fixture = RegistryFixture::new();
    let profile = fixture.profiles.join("alpha");
    let state = fixture.profiles.join(".private-sessions").join("alpha");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("workspace.json"),
        format!(
            r#"{{
  "schema_version": 1,
  "account": "alpha",
  "container": "loom-codex-session-alpha",
  "engine": "test-engine",
  "repository": "https://forge.example/a/b",
  "base": "main",
  "repositories": [
    {{"repository": "https://forge.example/c/d", "base": "main", "slug": "c-d"}},
    {{"repository": "https://forge.example/e/f", "base": "release", "slug": "e-f"}}
  ],
  "volume": "loom-codex-workspace-alpha",
  "profile": {:?},
  "gh_config": null
}}"#,
            profile.display().to_string()
        ),
    )
    .unwrap();
    let elsewhere = tempfile::tempdir().unwrap();

    let output = fixture.command(elsewhere.path(), &["list", "--json"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["private_clone"]["repository"], "https://forge.example/a/b", "{entry}");
    assert_eq!(
        entry["private_clone"]["additional_repositories"],
        serde_json::json!(["https://forge.example/c/d", "https://forge.example/e/f"]),
        "{entry}"
    );
    assert!(entry.get("image").is_none(), "{entry}");

    let human = fixture.command(elsewhere.path(), &["list"]);
    let human_stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        human_stdout.contains(
            "repos=https://forge.example/a/b,https://forge.example/c/d,https://forge.example/e/f"
        ),
        "{human_stdout}"
    );
    assert!(!human_stdout.contains("image="), "{human_stdout}");
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
