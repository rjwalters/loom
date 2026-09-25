//! End-to-end coverage for `loom-daemon accounts provision` (issue #8672),
//! driven through the real binary.
//!
//! In-crate unit tests already cover the sharing rules, the ledger, and the
//! denylist. What only a real-binary test can show is the wiring:
//!
//! - `accounts add` provisions the profile it just created, **without being
//!   asked** — the "a rotated account is not a blank install" claim.
//! - `accounts provision --all` is idempotent across separate *processes*,
//!   not merely across two calls sharing one in-memory ledger.
//! - The credential the login wrote is still byte-identical afterwards.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    _root: tempfile::TempDir,
    workspace: PathBuf,
    profiles: PathBuf,
    default_home: PathBuf,
    path: std::ffi::OsString,
}

impl Fixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let profiles = root.path().join("codex-profiles");
        let default_home = root.path().join("operator-home").join(".codex");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&bin).unwrap();

        // A fake `codex login` that behaves like the real one: it creates the
        // credential inside whatever CODEX_HOME it is handed, and nothing else.
        let codex = bin.join("codex");
        std::fs::write(
            &codex,
            "#!/bin/sh\nprintf 'FAKE-CREDENTIAL' > \"$CODEX_HOME/auth.json\"\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();

        // The operator's own profile: prompts to share, instructions to copy,
        // settings to merge, and trust state that must never be shared.
        std::fs::create_dir_all(default_home.join("prompts")).unwrap();
        std::fs::write(default_home.join("prompts").join("review.md"), "review").unwrap();
        std::fs::write(default_home.join("AGENTS.md"), "# operator instructions\n").unwrap();
        std::fs::write(
            default_home.join("config.toml"),
            concat!(
                "model = \"gpt-5-codex\"\n",
                "\n",
                "[mcp_servers.loom]\n",
                "command = \"mcp-loom\"\n",
                "\n",
                "[hooks.state.\"id\"]\n",
                "trusted_hash = \"OPERATOR-TRUST-HASH\"\n",
                "\n",
                "[projects.\"/home/operator/private\"]\n",
                "trust_level = \"trusted\"\n",
            ),
        )
        .unwrap();
        std::fs::write(default_home.join("auth.json"), "OPERATOR-CREDENTIAL").unwrap();

        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let path =
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&inherited)))
                .unwrap();

        Self {
            _root: root,
            workspace,
            profiles,
            default_home,
            path,
        }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
            .arg("accounts")
            .arg("--workspace")
            .arg(&self.workspace)
            .args(args)
            .env("LOOM_CODEX_PROFILE_ROOT", &self.profiles)
            .env("LOOM_CODEX_DEFAULT_HOME", &self.default_home)
            // No hook-bridge provisioner in this synthetic workspace; the
            // bridge has its own dedicated test against the real script.
            .env("LOOM_CODEX_HOOKS_SCRIPT", "")
            .env("PATH", &self.path)
            .output()
            .unwrap()
    }

    fn profile(&self, name: &str) -> PathBuf {
        self.profiles.join(name)
    }
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

#[test]
fn accounts_add_provisions_the_profile_it_just_created() {
    let fixture = Fixture::new();
    let output = fixture.run(&["add", "codex", "agent-1"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");

    let profile = fixture.profile("agent-1");

    // The login's own credential is intact and was never re-read or rewritten
    // by provisioning — and the OPERATOR's credential never leaked into it.
    assert_eq!(std::fs::read_to_string(profile.join("auth.json")).unwrap(), "FAKE-CREDENTIAL");

    // The blank-install gap is closed: instructions, prompts and settings are
    // all present without anyone running a second command.
    assert_eq!(
        std::fs::read_to_string(profile.join("AGENTS.md")).unwrap(),
        "# operator instructions\n"
    );
    assert!(is_symlink(&profile.join("prompts")));
    assert!(profile.join("prompts").join("review.md").is_file());
    let config = std::fs::read_to_string(profile.join("config.toml")).unwrap();
    assert!(config.contains("gpt-5-codex"), "{config}");
    assert!(config.contains("mcp-loom"), "{config}");

    // …while trust state and identity stayed behind, per the denylist.
    assert!(!config.contains("OPERATOR-TRUST-HASH"), "{config}");
    assert!(!config.contains("trust_level"), "{config}");
    assert!(!config.contains("private"), "{config}");

    // No credential, from either side, is quoted anywhere in the profile.
    for entry in std::fs::read_dir(&profile).unwrap().flatten() {
        if entry.file_name() == "auth.json" || !entry.path().is_file() {
            continue;
        }
        let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
        assert!(!body.contains("OPERATOR-CREDENTIAL"), "{:?}", entry.path());
        assert!(!body.contains("FAKE-CREDENTIAL"), "{:?}", entry.path());
    }

    assert!(profile.join(".loom-profile.json").is_file());
}

#[test]
fn provision_all_is_idempotent_across_processes_and_respects_a_local_edit() {
    let fixture = Fixture::new();
    assert_eq!(fixture.run(&["add", "codex", "agent-1"]).status.code(), Some(0));
    let profile = fixture.profile("agent-1");

    // A second, fully independent process must change nothing.
    let output = fixture.run(&["provision", "--all", "--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    let reports: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(reports.as_array().unwrap().len(), 1, "{stdout}");
    assert_eq!(reports[0]["changed"], false, "{stdout}");
    assert_eq!(reports[0]["profile"], "agent-1");
    // The report names a profile, never a credential path.
    assert!(!stdout.contains("auth.json"), "{stdout}");

    // Now the operator hand-tunes this pooled account…
    std::fs::write(profile.join("AGENTS.md"), "# hand-written for this pooled account\n").unwrap();
    // …and their own default profile moves on.
    std::fs::write(fixture.default_home.join("AGENTS.md"), "# new default\n").unwrap();

    let output = fixture.run(&["provision", "agent-1"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(profile.join("AGENTS.md")).unwrap(),
        "# hand-written for this pooled account\n",
        "a local edit inside a pooled profile must win forever"
    );
}

#[test]
fn provision_a_dry_run_writes_nothing() {
    let fixture = Fixture::new();
    assert_eq!(fixture.run(&["add", "codex", "agent-1"]).status.code(), Some(0));
    let profile = fixture.profile("agent-1");
    std::fs::remove_file(profile.join("AGENTS.md")).unwrap();

    let output = fixture.run(&["provision", "agent-1", "--dry-run"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(!profile.join("AGENTS.md").exists(), "--dry-run must not write");
}

#[test]
fn provision_without_a_name_or_all_is_a_usage_error() {
    let fixture = Fixture::new();
    let output = fixture.run(&["provision"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--all"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn provision_rejects_a_provider_with_no_sharing_table() {
    let fixture = Fixture::new();
    let output = fixture.run(&["provision", "--provider", "claude", "--all"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no profile sharing table"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
