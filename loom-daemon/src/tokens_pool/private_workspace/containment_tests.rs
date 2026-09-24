//! Worker-side containment verification with disposable local Git fixtures
//! (#8787). Docker-bound evidence is covered by `tests/private_workspace_docker`.
use super::containment::*;
use super::*;
use std::process::Command;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// A clone shaped like a consumer install: committed hooks, guard libraries,
/// hook provisioner and a Loom config layer.
fn clone() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for (path, body) in [
        (".loom/hooks/guard-codex-bridge.sh", "#!/bin/sh\necho bridge\n"),
        (".loom/hooks/guard-destructive.sh", "#!/bin/sh\necho destructive\n"),
        (".loom/hooks/tests/fixture.sh", "#!/bin/sh\n"),
        (".loom/scripts/provision-codex-hooks.sh", "#!/bin/sh\necho verify\n"),
        (".loom/scripts/lib/canonical-path.sh", "canonical() { :; }\n"),
        (".loom/config.json", "{\"guards\":{}}\n"),
        ("file", "base\n"),
    ] {
        std::fs::create_dir_all(repo.join(path).parent().unwrap()).unwrap();
        std::fs::write(repo.join(path), body).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let revision = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    (dir, revision)
}

#[test]
fn an_unmodified_guard_bundle_matches_its_base_revision() {
    let (dir, revision) = clone();
    let repo = dir.path().canonicalize().unwrap();
    guard_integrity(&repo, &revision).unwrap();
    // Ordinary work outside the guard bundle is the worker's own business.
    std::fs::write(repo.join("file"), "edited\n").unwrap();
    std::fs::create_dir_all(repo.join(".loom/worktrees/issue-1")).unwrap();
    guard_integrity(&repo, &revision).unwrap();
}

type Mutation<'a> = (&'a str, &'a dyn Fn(&Path));

#[test]
fn every_policy_input_is_bound_to_the_base_revision() {
    let mutations: [Mutation<'_>; 8] = [
        ("edited bridge", &|repo| {
            std::fs::write(repo.join(".loom/hooks/guard-codex-bridge.sh"), "exit 0\n").unwrap();
        }),
        ("untracked hook", &|repo| {
            std::fs::write(repo.join(".loom/hooks/zz-override.sh"), "exit 0\n").unwrap();
        }),
        ("deleted guard", &|repo| {
            std::fs::remove_file(repo.join(".loom/hooks/guard-destructive.sh")).unwrap();
        }),
        ("edited library", &|repo| {
            std::fs::write(repo.join(".loom/scripts/lib/canonical-path.sh"), "x\n").unwrap();
        }),
        ("edited provisioner", &|repo| {
            std::fs::write(repo.join(".loom/scripts/provision-codex-hooks.sh"), "exit 0\n")
                .unwrap();
        }),
        ("guard toggle", &|repo| {
            std::fs::write(repo.join(".loom/config.json"), "{\"guards\":{\"x\":false}}\n").unwrap();
        }),
        ("new local config layer", &|repo| {
            std::fs::create_dir_all(repo.join(".loom-local")).unwrap();
            std::fs::write(repo.join(".loom-local/local.json"), "{}\n").unwrap();
        }),
        ("committed guard change", &|repo| {
            std::fs::write(repo.join(".loom/hooks/guard-codex-bridge.sh"), "exit 0\n").unwrap();
            git(repo, &["commit", "-q", "-am", "disable"]);
        }),
    ];
    for (what, mutate) in mutations {
        let (dir, revision) = clone();
        let repo = dir.path().canonicalize().unwrap();
        mutate(&repo);
        assert!(guard_integrity(&repo, &revision).is_err(), "{what} was not detected");
    }
}

#[test]
fn index_tricks_and_symlinks_cannot_hide_a_modified_guard() {
    let (dir, revision) = clone();
    let repo = dir.path().canonicalize().unwrap();
    git(
        &repo,
        &[
            "update-index",
            "--assume-unchanged",
            ".loom/hooks/guard-codex-bridge.sh",
        ],
    );
    std::fs::write(repo.join(".loom/hooks/guard-codex-bridge.sh"), "exit 0\n").unwrap();
    assert!(guard_integrity(&repo, &revision).is_err(), "assume-unchanged hid an edit");

    let (dir, revision) = clone();
    let repo = dir.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("bridge.sh"), "#!/bin/sh\necho bridge\n").unwrap();
    std::fs::remove_file(repo.join(".loom/hooks/guard-codex-bridge.sh")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("bridge.sh"),
        repo.join(".loom/hooks/guard-codex-bridge.sh"),
    )
    .unwrap();
    assert!(guard_integrity(&repo, &revision).is_err(), "bridge escaped the clone");
}

#[test]
fn only_repository_mutating_roles_need_the_managed_policy() {
    for role in [
        "builder",
        "doctor",
        "sweep-lifecycle",
        "sweep",
        "development-worker",
        "pr-fixer",
    ] {
        assert!(mutable(role), "{role}");
    }
    for role in ["judge", "curator", "champion", "guide", "", "unknown"] {
        assert!(!mutable(role), "{role}");
    }
}

#[test]
fn a_bare_host_process_cannot_produce_worker_evidence() {
    // Malformed identities are refused before any file is read, and a host
    // has no bound container: there is no /workspace clone identity, and its
    // hostname is not the container's short ID.
    assert!(evidence("not-hex", &"b".repeat(40)).is_err());
    assert!(evidence(&"a".repeat(64), "HEAD").is_err());
    assert!(evidence(&"a".repeat(64), &"b".repeat(40)).is_err());
    assert_eq!(verify_local(&"a".repeat(64), &"b".repeat(40)), PolicyStatus::ContextInvalid);
}

#[test]
fn obligations_are_fixed_secret_free_text() {
    for status in [
        PolicyStatus::ContextInvalid,
        PolicyStatus::GuardModified,
        PolicyStatus::HooksNotReady,
        PolicyStatus::Failed,
    ] {
        let text = status.obligation();
        assert!(!text.is_empty());
        assert!(!text.contains(PROFILE), "{text}");
    }
    let report = serde_json::to_string(&PolicyReport {
        protocol: PROTOCOL.into(),
        status: PolicyStatus::HooksNotReady,
    })
    .unwrap();
    assert_eq!(
        report,
        format!("{{\"protocol\":\"{PROTOCOL}\",\"status\":\"hooks-not-ready\"}}")
    );
}
