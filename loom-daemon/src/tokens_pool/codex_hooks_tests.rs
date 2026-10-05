//! Unit tests for the native readiness check. The full behavioural contract
//! (install + verify together, executing the registered command the way Codex
//! does, `--all-profiles`) is `defaults/scripts/tests/test-provision-codex-hooks.sh`,
//! which drives this code through the shell stub.
use super::*;
use std::fs;

fn shell_constant() -> String {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let script =
        fs::read_to_string(repo.join("defaults/scripts/provision-codex-hooks.sh")).unwrap();
    let line = script
        .lines()
        .find(|l| l.starts_with("LOOM_SHARED_HOOK_COMMAND='"))
        .expect("provision-codex-hooks.sh defines LOOM_SHARED_HOOK_COMMAND");
    line.trim_start_matches("LOOM_SHARED_HOOK_COMMAND='")
        .trim_end_matches('\'')
        .to_owned()
}

/// The shell writes the command and this module checks for it: one string.
#[test]
fn the_shared_command_is_the_one_the_provisioner_installs() {
    assert_eq!(shell_constant(), SHARED_COMMAND);
    assert!(SHARED_COMMAND.contains(MARKER));
    assert!(SHARED_COMMAND.contains(&format!("--loom-hook-version {SHARED_VERSION}")));
    assert!(SHARED_COMMAND.contains("exit 2"));
}

/// A profile holding `command` as Loom's entry, a receipt pinning it with the
/// given baseline, and `trusted` hashes in config.toml.
fn profile(dir: &Path, command: &str, baseline: Option<&[&str]>, trusted: &[&str]) -> PathBuf {
    let profile = dir.join("acct");
    fs::create_dir_all(&profile).unwrap();
    fs::write(
        profile.join("hooks.json"),
        serde_json::json!({"hooks": {"PreToolUse": [
            {"matcher": "shell", "hooks": [{"type": "command", "command": "/opt/operator/audit.sh"}]},
            {"matcher": "*", "hooks": [{"type": "command", "command": command, "timeout": 30}]}
        ]}})
        .to_string(),
    )
    .unwrap();
    let mut receipt = serde_json::json!({"loomManagedHook": {
        "command": command, "commandSha256": sha256_hex(command.as_bytes())
    }});
    if let Some(baseline) = baseline {
        receipt["loomManagedHook"]["trustBaselineHashes"] = serde_json::json!(baseline);
    }
    fs::write(profile.join(RECEIPT), receipt.to_string()).unwrap();
    // Codex keeps one hash per key. Loom's entry (group 1, after the
    // operator's) carries the LAST, most recent decision; the others are
    // trust for other hooks.
    let home = profile.canonicalize().unwrap();
    let config: String = trusted
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let key = if i + 1 == trusted.len() {
                format!("{}/hooks.json:pre_tool_use:1:0", home.display())
            } else {
                format!("{}/hooks.json:pre_tool_use:0:{i}", home.display())
            };
            format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"{h}\"\n")
        })
        .collect();
    fs::write(profile.join("config.toml"), format!("# trusted_hash = \"comment\"\n{config}"))
        .unwrap();
    profile
}

fn workspace(dir: &Path) -> PathBuf {
    let ws = dir.join("ws");
    fs::create_dir_all(ws.join(".loom/hooks")).unwrap();
    fs::write(ws.join(".loom/hooks").join(MARKER), "#!/bin/sh\n").unwrap();
    ws
}

fn shared(codex_home: PathBuf, workspace: Option<PathBuf>) -> Check {
    Check {
        codex_home,
        workspace,
        registration: Registration::WorkspaceIndependent,
        fallback_bridge: None,
        runtime_home: None,
        sealed: None,
    }
}

#[test]
fn a_trusted_shared_registration_is_ready_from_any_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let home = profile(dir.path(), SHARED_COMMAND, Some(&["old"]), &["old", "fresh"]);
    for name in ["a", "b"] {
        let ws = workspace(&dir.path().join(name));
        let verdict = shared(home.clone(), Some(ws)).verify();
        assert!(verdict.ready, "{verdict:?}");
        assert_eq!(verdict.trust_signal, "baseline-diff");
        assert_eq!(verdict.registration, "workspace-independent");
        assert_eq!(verdict.version, SHARED_VERSION);
        assert_eq!(verdict.profile, "acct");
    }
}

#[test]
fn the_named_workspace_must_have_its_own_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let home = profile(dir.path(), SHARED_COMMAND, None, &["any"]);
    let bare = dir.path().join("bare");
    fs::create_dir_all(&bare).unwrap();
    let verdict = Check {
        // A fallback bridge must NOT stand in for a named workspace's.
        fallback_bridge: Some(workspace(dir.path()).join(".loom/hooks").join(MARKER)),
        ..shared(home, Some(bare))
    }
    .verify();
    assert!(!verdict.ready && !verdict.bridge_readable, "{verdict:?}");
    assert!(verdict.reason.contains("no readable"), "{}", verdict.reason);
}

#[test]
fn legacy_private_and_tampered_entries_are_stale_under_the_shared_check() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    for (command, expect) in [
        ("/x/.loom/hooks/guard-codex-bridge.sh --project-root /x --loom-hook-version 1", "pre-#9390"),
        (
            "/opt/loom/private-control/hooks/guard-codex-bridge.sh --project-root /workspace/repo --loom-hook-version 1",
            "private-clone",
        ),
    ] {
        let sub = tempfile::tempdir_in(dir.path()).unwrap();
        let home = profile(sub.path(), command, None, &["t"]);
        let verdict = shared(home, Some(ws.clone())).verify();
        assert!(verdict.stale && !verdict.ready, "{verdict:?}");
        assert!(verdict.reason.contains(expect), "{}", verdict.reason);
    }
    // Hand-edited after install: the receipt no longer pins it.
    let sub = tempfile::tempdir_in(dir.path()).unwrap();
    let home = profile(sub.path(), SHARED_COMMAND, None, &["t"]);
    let hooks = fs::read_to_string(home.join("hooks.json"))
        .unwrap()
        .replace("exit 2;", "exit 0;");
    fs::write(home.join("hooks.json"), hooks).unwrap();
    let verdict = shared(home, Some(ws)).verify();
    assert!(verdict.stale && !verdict.ready, "{verdict:?}");
}

#[test]
fn trust_follows_the_install_time_baseline_diff() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    for (baseline, trusted, ready, signal) in [
        (Some(&[][..]), &[][..], false, "none"),
        (Some(&["old"][..]), &["old"][..], false, "baseline-diff-no-new-trust"),
        (Some(&["old"][..]), &["old", "new"][..], true, "baseline-diff"),
        (None, &["whenever"][..], true, "legacy-coarse"),
    ] {
        let sub = tempfile::tempdir_in(dir.path()).unwrap();
        let home = profile(sub.path(), SHARED_COMMAND, baseline, trusted);
        let verdict = shared(home, Some(ws.clone())).verify();
        assert_eq!((verdict.ready, verdict.trust_signal), (ready, signal), "{verdict:?}");
    }
}

#[test]
fn a_pinned_check_keeps_the_pre_9390_rules() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    let bridge = ws.join(".loom/hooks").join(MARKER);
    let canonical = ws.canonicalize().unwrap().join(".loom/hooks").join(MARKER);
    let command =
        format!("{} --project-root /workspace/repo --loom-hook-version 1", canonical.display());
    let home = profile(dir.path(), &command, None, &["t"]);
    let pinned = |bridge: PathBuf| Check {
        codex_home: home.clone(),
        workspace: None,
        registration: Registration::Pinned { bridge },
        fallback_bridge: None,
        runtime_home: None,
        sealed: None,
    };
    let verdict = pinned(bridge).verify();
    assert!(verdict.ready, "{verdict:?}");
    assert_eq!((verdict.registration, verdict.version), ("pinned", PINNED_VERSION));
    let other = workspace(&dir.path().join("other"))
        .join(".loom/hooks")
        .join(MARKER);
    let verdict = pinned(other).verify();
    assert!(verdict.reason.contains("different bridge"), "{}", verdict.reason);
}

#[test]
fn pooled_profiles_skip_bookkeeping_and_private_sessions_for_a_shared_check() {
    let root = tempfile::tempdir().unwrap();
    for name in ["a", "b", "private", ".private-sessions/private"] {
        fs::create_dir_all(root.path().join(name)).unwrap();
    }
    fs::write(root.path().join(".private-sessions/private/workspace.json"), "{}").unwrap();
    let names = |registration: &Registration| {
        pooled_profiles(root.path(), registration)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&Registration::WorkspaceIndependent), ["a", "b"]);
    assert_eq!(
        names(&Registration::Pinned {
            bridge: PathBuf::from("/b")
        }),
        ["a", "b", "private"]
    );
}

#[test]
fn verdicts_never_carry_a_credential_or_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let home = profile(dir.path(), SHARED_COMMAND, None, &["t"]);
    fs::write(home.join("auth.json"), "sk-FAKE-9390").unwrap();
    let json = serde_json::to_string(&shared(home, Some(workspace(dir.path()))).verify()).unwrap();
    assert!(!json.contains("sk-FAKE-9390"), "{json}");
    assert!(!json.contains(&dir.path().display().to_string()), "{json}");
}

/// Trust counts only under the key Codex looks Loom's entry up under, at the
/// CODEX_HOME it runs with (#9390 follow-up).
#[test]
fn only_trust_keyed_to_loom_where_codex_runs_counts() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    let home = profile(dir.path(), SHARED_COMMAND, Some(&[]), &[]);
    let host = home.canonicalize().unwrap().display().to_string();
    let check = |runtime_home: Option<&str>| {
        Check {
            runtime_home: runtime_home.map(PathBuf::from),
            sealed: None,
            ..shared(home.clone(), Some(ws.clone()))
        }
        .verify()
    };
    let write = |body: String| fs::write(home.join("config.toml"), body).unwrap();

    // Another profile's path (the robb-studio shape), and another position.
    for key in [
        "/elsewhere/r.j.walters/hooks.json:pre_tool_use:1:0".to_owned(),
        format!("{host}/hooks.json:pre_tool_use:0:0"),
    ] {
        write(format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"h\"\n"));
        let verdict = check(None);
        assert!(!verdict.trusted, "{key}: {verdict:?}");
        assert_eq!(verdict.trust_signal, "wrong-location");
        assert!(!verdict.reason.contains("/elsewhere"), "{}", verdict.reason);
    }
    // Every TOML spelling of the right key.
    let key = format!("{host}/hooks.json:pre_tool_use:1:0");
    for body in [
        format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"h\"\n"),
        format!("hooks.state.\"{key}\".trusted_hash = \"h\"\n"),
        format!("[hooks.state]\n\"{key}\" = {{ trusted_hash = \"h\", enabled = true }}\n"),
        format!("[hooks]\nstate.\"{key}\".trusted_hash = \"h\"\n"),
    ] {
        write(body.clone());
        assert!(check(None).ready, "{body}");
    }
    write(format!("# [hooks.state.\"{key}\"]\n# trusted_hash = \"h\"\n"));
    assert_eq!(check(None).trust_signal, "none");
    // Codex drops a handler whose state says `enabled = false`, trusted or not.
    write(format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"h\"\nenabled = false\n"));
    let verdict = check(None);
    assert_eq!((verdict.ready, verdict.trust_signal), (false, "disabled"));
    assert!(verdict.reason.contains("DISABLED"), "{}", verdict.reason);

    // An adopted profile runs in its session container: host-keyed trust
    // does not count there, container-keyed trust does, and an explicit
    // runtime home overrides the derivation either way.
    fs::write(home.join(".session-managed.json"), "{}").unwrap();
    write(format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"h\"\n"));
    let verdict = check(None);
    assert_eq!(
        (verdict.trusted, verdict.trust_signal, verdict.trust_location.as_str()),
        (false, "wrong-location", "the session container")
    );
    assert!(check(Some(&host)).ready);
    write(format!(
        "[hooks.state.\"{SESSION_CODEX_HOME}/hooks.json:pre_tool_use:1:0\"]\ntrusted_hash = \"h\"\n"
    ));
    assert!(check(None).ready);
    assert!(!check(Some(&host)).trusted);
}

#[test]
fn a_profile_without_loom_entry_carries_no_loom_trust() {
    let dir = tempfile::tempdir().unwrap();
    let home = profile(dir.path(), SHARED_COMMAND, None, &["h"]);
    fs::write(
        home.join("hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"/opt/operator/audit.sh"}]}]}}"#,
    )
    .unwrap();
    let runtime = home.canonicalize().unwrap();
    assert_eq!(trust_at(&home, &runtime), (false, "wrong-location"));
    assert!(keyed_trusted_hashes(&home.join("config.toml"), &BTreeSet::new()).is_empty());
}
