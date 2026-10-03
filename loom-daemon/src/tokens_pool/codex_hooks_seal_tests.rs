//! Issue #10102: the sealed registration, clause by clause. Every tamper must
//! refuse the seal (no trust waiver), and the untampered profile must be
//! ready only when the caller opted in, in a session container.

use super::*;
use crate::tokens_pool::codex_hooks::{Check, Registration};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Loom's registration exactly as `provision-codex-hooks.sh install` writes it.
fn loom_hooks() -> serde_json::Value {
    json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
        {"type": "command", "command": SHARED_COMMAND, "timeout": 30}
    ]}]}})
}

/// A session-managed profile carrying only Loom's registration, its receipt,
/// an empty `config.toml` and NO recorded trust, plus a git checkout with a
/// readable bridge to launch in.
fn sealed(dir: &Path) -> (PathBuf, PathBuf) {
    let profile = dir.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    write_hooks(&profile, &loom_hooks());
    std::fs::write(
        profile.join(RECEIPT),
        json!({"loomManagedHook": {
            "command": SHARED_COMMAND,
            "commandSha256": sha256_hex(SHARED_COMMAND.as_bytes()),
            "trustBaselineHashes": []
        }})
        .to_string(),
    )
    .unwrap();
    std::fs::write(profile.join("config.toml"), "model = \"gpt\"\n").unwrap();
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join(".loom/hooks")).unwrap();
    std::fs::write(repo.join(".loom/hooks/guard-codex-bridge.sh"), "#!/bin/sh\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    (profile, repo)
}

fn write_hooks(profile: &Path, hooks: &serde_json::Value) {
    std::fs::write(profile.join("hooks.json"), serde_json::to_string_pretty(hooks).unwrap())
        .unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn session_check(profile: &Path, repo: &Path, request: Option<Request>) -> Check {
    Check {
        codex_home: profile.to_path_buf(),
        workspace: Some(repo.to_path_buf()),
        registration: Registration::WorkspaceIndependent,
        fallback_bridge: None,
        runtime_home: Some(PathBuf::from(SESSION_CODEX_HOME)),
        sealed: request,
    }
}

fn launch(repo: &Path) -> Request {
    Request {
        launch_dir: Some(repo.to_path_buf()),
        ..Request::default()
    }
}

fn reason(profile: &Path, repo: &Path) -> String {
    vet(profile, Path::new(SESSION_CODEX_HOME), &launch(repo)).unwrap_err()
}

#[test]
fn an_untrusted_but_sealed_session_seat_is_ready_only_when_the_caller_opts_in() {
    let tmp = tempfile::tempdir().unwrap();
    let (profile, repo) = sealed(tmp.path());

    let verdict = session_check(&profile, &repo, Some(launch(&repo))).verify();
    assert!(verdict.ready, "{}", verdict.reason);
    assert!(verdict.trusted);
    assert_eq!(verdict.trust_signal, "sealed-registration");
    assert!(verdict.bypass_hook_trust, "the launch must pass the waiver");
    assert!(!verdict.container_verified, "no container was named");
    assert_eq!(verdict.seal_reason, "sealed");

    // A caller that did not opt in (and so would not pass the waiver) sees the
    // same seat as untrusted: Codex would skip the entry without a word.
    let legacy = session_check(&profile, &repo, None).verify();
    assert!(!legacy.ready);
    assert!(!legacy.bypass_hook_trust);
    assert_eq!(legacy.trust_signal, "none");
    assert!(legacy.seal_reason.is_empty());
}

#[test]
fn the_host_never_gets_the_waiver() {
    let tmp = tempfile::tempdir().unwrap();
    let (profile, repo) = sealed(tmp.path());
    let mut check = session_check(&profile, &repo, Some(launch(&repo)));
    check.runtime_home = None; // bare metal: the canonical profile path
    let verdict = check.verify();
    assert!(!verdict.ready);
    assert!(!verdict.bypass_hook_trust);
    assert!(verdict
        .seal_reason
        .contains("only used inside a hardened session container"));
}

#[test]
fn a_pinned_registration_is_never_sealed() {
    let tmp = tempfile::tempdir().unwrap();
    let (profile, repo) = sealed(tmp.path());
    let mut check = session_check(&profile, &repo, Some(launch(&repo)));
    check.registration = Registration::Pinned {
        bridge: repo.join(".loom/hooks/guard-codex-bridge.sh"),
    };
    let verdict = check.verify();
    assert!(!verdict.bypass_hook_trust);
    assert!(verdict.seal_reason.contains("workspace-independent"));
}

#[test]
fn every_hooks_json_tamper_refuses_the_seal() {
    let handler = json!({"type": "command", "command": SHARED_COMMAND, "timeout": 30});
    let extra = json!({"type": "command", "command": "bash /tmp/x.sh", "timeout": 30});
    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "an extra handler",
            json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [handler, extra]}]}}),
        ),
        (
            "an extra group",
            json!({"hooks": {"PreToolUse": [
                {"matcher": "*", "hooks": [handler]},
                {"matcher": "*", "hooks": [extra]}
            ]}}),
        ),
        (
            "another event",
            json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [handler]}],
                             "SessionStart": [{"hooks": [extra]}]}}),
        ),
        (
            "a changed matcher",
            json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [handler]}]}}),
        ),
        (
            "an async handler",
            json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
                {"type": "command", "command": SHARED_COMMAND, "timeout": 30, "async": true}
            ]}]}}),
        ),
        (
            "a short timeout",
            json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
                {"type": "command", "command": SHARED_COMMAND, "timeout": 1}
            ]}]}}),
        ),
        (
            "a different command",
            json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
                {"type": "command", "command": format!("{SHARED_COMMAND} ; true"), "timeout": 30}
            ]}]}}),
        ),
        (
            "a top-level description",
            json!({"description": "x", "hooks": {"PreToolUse": [{"matcher": "*", "hooks": [handler]}]}}),
        ),
        ("no registration", json!({"hooks": {}})),
    ];
    for (label, hooks) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let (profile, repo) = sealed(tmp.path());
        write_hooks(&profile, &hooks);
        assert!(
            vet(&profile, Path::new(SESSION_CODEX_HOME), &launch(&repo)).is_err(),
            "{label} must refuse the seal"
        );
        assert!(
            !session_check(&profile, &repo, Some(launch(&repo)))
                .verify()
                .bypass_hook_trust,
            "{label} must never yield the waiver"
        );
    }

    // A duplicated key: a lenient reader keeps the last value, while Codex
    // refuses the whole file and so runs no hook at all.
    let tmp = tempfile::tempdir().unwrap();
    let (profile, repo) = sealed(tmp.path());
    let text = serde_json::to_string(&loom_hooks()).unwrap().replacen(
        "\"timeout\":30",
        "\"timeout\":30,\"timeout\":30",
        1,
    );
    std::fs::write(profile.join("hooks.json"), text).unwrap();
    assert!(reason(&profile, &repo).contains("duplicated key"));
}

#[test]
fn a_receipt_that_does_not_pin_loom_refuses_the_seal() {
    let tmp = tempfile::tempdir().unwrap();
    let (profile, repo) = sealed(tmp.path());
    std::fs::write(
        profile.join(RECEIPT),
        json!({"loomManagedHook": {"commandSha256": "0".repeat(64)}}).to_string(),
    )
    .unwrap();
    assert!(reason(&profile, &repo).contains("receipt"));
    std::fs::remove_file(profile.join(RECEIPT)).unwrap();
    assert!(reason(&profile, &repo).contains("missing or unreadable"));
}

#[test]
fn every_user_config_tamper_refuses_the_seal() {
    let loom_key = format!("{SESSION_CODEX_HOME}/hooks.json:pre_tool_use:0:0");
    let cases = [
        ("toml hooks", "[[hooks.PreToolUse]]\nmatcher = \"*\"\n".to_owned()),
        ("loom disabled", format!("[hooks.state.\"{loom_key}\"]\nenabled = false\n")),
        ("hooks off", "[features]\nhooks = false\n".to_owned()),
        ("legacy hooks off", "[features]\ncodex_hooks = false\n".to_owned()),
        ("moved project root", "project_root_markers = []\n".to_owned()),
        ("a profile with features", "[profiles.x.features]\nhooks = false\n".to_owned()),
        ("unparsable", "this is = = not toml\n".to_owned()),
    ];
    for (label, config) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let (profile, repo) = sealed(tmp.path());
        std::fs::write(profile.join("config.toml"), config).unwrap();
        assert!(
            vet(&profile, Path::new(SESSION_CODEX_HOME), &launch(&repo)).is_err(),
            "{label} must refuse the seal"
        );
    }

    // Recorded trust, enabled = true, other projects and other features are
    // all fine: they add no hook source.
    let tmp = tempfile::tempdir().unwrap();
    let (profile, repo) = sealed(tmp.path());
    std::fs::write(
        profile.join("config.toml"),
        format!(
            "model = \"m\"\n[features]\nhooks = true\nweb_search = false\n\
             [projects.\"/x\"]\ntrust_level = \"trusted\"\n\
             [hooks.state.\"{loom_key}\"]\nenabled = true\ntrusted_hash = \"sha256:old\"\n"
        ),
    )
    .unwrap();
    vet(&profile, Path::new(SESSION_CODEX_HOME), &launch(&repo)).unwrap();
}

#[test]
fn every_hook_source_in_the_launch_argv_refuses_the_seal() {
    let refused: [&[&str]; 18] = [
        &["-C", "/elsewhere"],
        &["-C/elsewhere"],
        &["--cd", "/elsewhere"],
        &["--cd=/elsewhere"],
        &["--worktree"],
        &["-pwork"],
        &["--profile=work"],
        &["-c", "hooks.PreToolUse=[]"],
        &["--config", "features.hooks=false"],
        &["--config=plugins.x.enabled=true"],
        &["-chooks.state={}"],
        &["-c", "profile=other"],
        &["-c", "bypass_hook_trust=true"],
        &["--enable", "plugins"],
        &["--disable=hooks"],
        &["--profile", "x"],
        &["-p", "x"],
        &[BYPASS_FLAG],
    ];
    for args in refused {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        assert!(vet_session_flags(&args).is_err(), "{args:?} must refuse the seal");
    }
    // What spawn-codex.sh itself passes is fine.
    let ok: Vec<String> = [
        "-m",
        "gpt-5",
        "-c",
        "model_reasoning_effort=high",
        "-c",
        "sandbox_workspace_write.network_access=true",
        "--skip-git-repo-check",
        "-s",
        "danger-full-access",
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect();
    vet_session_flags(&ok).unwrap();
}

#[test]
fn a_project_hook_source_anywhere_codex_looks_refuses_the_seal() {
    // In the launch directory's own checkout.
    let tmp = tempfile::tempdir().unwrap();
    let (profile, repo) = sealed(tmp.path());
    let sub = repo.join("src/deep");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(repo.join(".codex")).unwrap();
    std::fs::write(repo.join(".codex/hooks.json"), "{}").unwrap();
    let request = Request {
        launch_dir: Some(sub.clone()),
        ..Request::default()
    };
    assert!(vet(&profile, Path::new(SESSION_CODEX_HOME), &request)
        .unwrap_err()
        .contains("project .codex/hooks.json"));

    // A project config.toml that touches hooks, or does not parse.
    std::fs::remove_file(repo.join(".codex/hooks.json")).unwrap();
    std::fs::write(repo.join(".codex/config.toml"), "[features]\nhooks = true\n").unwrap();
    assert!(vet(&profile, Path::new(SESSION_CODEX_HOME), &request).is_err());
    std::fs::write(repo.join(".codex/config.toml"), "= nope").unwrap();
    assert!(vet(&profile, Path::new(SESSION_CODEX_HOME), &request).is_err());

    // The fleet's existing template (sandbox and approval settings only) adds
    // no hook source.
    std::fs::write(
        repo.join(".codex/config.toml"),
        "sandbox = \"workspace-write\"\nask_for_approval = \"never\"\n\
         [shell_environment_policy]\ninherit = \"all\"\n",
    )
    .unwrap();
    vet(&profile, Path::new(SESSION_CODEX_HOME), &request).unwrap();

    // A config.toml that is not a regular file refuses (never a blocking read).
    std::fs::remove_file(repo.join(".codex/config.toml")).unwrap();
    std::fs::create_dir(repo.join(".codex/config.toml")).unwrap();
    assert!(vet(&profile, Path::new(SESSION_CODEX_HOME), &request)
        .unwrap_err()
        .contains("not a regular file"));
    std::fs::remove_dir(repo.join(".codex/config.toml")).unwrap();

    // A `.codex/` Loom cannot look inside is not "absent".
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dot = repo.join(".codex");
        std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o000)).unwrap();
        let blind = std::fs::read_dir(&dot).is_err(); // false when running as root
        let verdict = vet(&profile, Path::new(SESSION_CODEX_HOME), &request);
        std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o755)).unwrap();
        if blind {
            assert!(verdict.unwrap_err().contains("cannot be inspected"));
        }
    }
    vet(&profile, Path::new(SESSION_CODEX_HOME), &request).unwrap();

    // Above the project root Codex does not look, so neither does Loom.
    std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
    std::fs::write(tmp.path().join(".codex/hooks.json"), "{}").unwrap();
    vet(&profile, Path::new(SESSION_CODEX_HOME), &request).unwrap();
}

#[test]
fn a_linked_worktree_is_vetted_against_its_main_checkouts_codex_dir_too() {
    let tmp = tempfile::tempdir().unwrap();
    let (profile, repo) = sealed(tmp.path());
    std::fs::write(repo.join("f"), "x").unwrap();
    git(&repo, &["add", "f"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "base",
        ],
    );
    let worktree = tmp.path().join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            worktree.to_str().unwrap(),
            "-b",
            "w",
        ],
    );
    vet(&profile, Path::new(SESSION_CODEX_HOME), &launch(&worktree)).unwrap();

    // Codex reads a worktree's project hooks from the main checkout's
    // matching `.codex/` (merge_root_checkout_project_hooks).
    std::fs::create_dir_all(repo.join(".codex")).unwrap();
    std::fs::write(repo.join(".codex/hooks.json"), "{}").unwrap();
    assert!(vet(&profile, Path::new(SESSION_CODEX_HOME), &launch(&worktree)).is_err());
}

#[test]
fn no_launch_directory_means_no_seal() {
    let tmp = tempfile::tempdir().unwrap();
    let (profile, _repo) = sealed(tmp.path());
    let err = vet(&profile, Path::new(SESSION_CODEX_HOME), &Request::default()).unwrap_err();
    assert!(err.contains("no launch directory"));
}

/// A stand-in `docker` that answers `exec … sha256sum` with fixed lines.
#[cfg(unix)]
fn fake_docker(dir: &Path, lines: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("docker");
    std::fs::write(&path, format!("#!/bin/sh\ncat <<'EOF'\n{lines}EOF\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

#[cfg(unix)]
#[test]
fn the_container_must_see_exactly_the_vetted_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let (profile, repo) = sealed(tmp.path());
    let seal = vet(&profile, Path::new(SESSION_CODEX_HOME), &launch(&repo)).unwrap();
    let mut lines = String::new();
    for (name, hex) in &seal.controls {
        lines.push_str(&format!("{hex}  {SESSION_CODEX_HOME}/{name}\n"));
    }
    let container = |docker: String| Container {
        docker,
        name: "loom-codex-session-x".into(),
    };
    let same = fake_docker(tmp.path(), &lines);
    container_sees(&container(same.clone()), &seal).unwrap();

    let mut request = launch(&repo);
    request.container = Some(container(same));
    let verdict = session_check(&profile, &repo, Some(request.clone())).verify();
    assert!(verdict.bypass_hook_trust && verdict.container_verified, "{verdict:?}");

    // A stale bind (the container still reads an older hooks.json) or a
    // missing control is not the vetted registration.
    let stale_dir = tmp.path().join("stale");
    std::fs::create_dir_all(&stale_dir).unwrap();
    let stale =
        fake_docker(&stale_dir, &lines.replacen(&seal.controls["hooks.json"], &"0".repeat(64), 1));
    assert!(container_sees(&container(stale.clone()), &seal)
        .unwrap_err()
        .contains("hooks.json"));
    request.container = Some(container(stale));
    let verdict = session_check(&profile, &repo, Some(request)).verify();
    assert!(!verdict.ready && !verdict.bypass_hook_trust, "{verdict:?}");

    let empty_dir = tmp.path().join("empty");
    std::fs::create_dir_all(&empty_dir).unwrap();
    assert!(container_sees(&container(fake_docker(&empty_dir, "")), &seal).is_err());
}
