//! The two demonstrated escalations of issue #8839, reproduced against the real
//! production guard bridge and the real hook wire protocol inside a real private
//! session, plus the image/identity refusals that gate admission.
//!
//! Credential-free: the only "credential" anywhere is the synthetic disposable
//! string in `synthetic-auth.json`, and this module asserts it never reaches the
//! control bundle, the manifest, or the clone.
use super::*;
use loom_daemon::tokens_pool::private_workspace::bundle;

/// Stage the image-owned control bundle into a build context, mirroring
/// `docker/session/Dockerfile`: the production guards, the libraries they
/// source, and the generic guard installed under the dispatcher's name so no
/// decision can be delegated to a worker-writable canonical guard.
pub(super) fn stage(into: &std::path::Path) {
    let defaults = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("defaults");
    std::fs::create_dir_all(into.join("hooks")).unwrap();
    std::fs::create_dir_all(into.join("scripts/lib")).unwrap();
    for name in [
        "guard-codex-bridge.sh",
        "guard-destructive-generic.sh",
        "guard-loom-workflow.sh",
        "guard-worktree-paths.sh",
    ] {
        std::fs::copy(defaults.join("hooks").join(name), into.join("hooks").join(name)).unwrap();
    }
    std::fs::copy(
        defaults.join("hooks/guard-destructive-generic.sh"),
        into.join("hooks/guard-destructive.sh"),
    )
    .unwrap();
    for name in [
        "canonical-path.sh",
        "config-resolver.sh",
        "default-branch.sh",
        "installed-file-guard.sh",
        "worktree-root.sh",
    ] {
        std::fs::copy(defaults.join("scripts/lib").join(name), into.join("scripts/lib").join(name))
            .unwrap();
    }
    std::fs::copy(
        defaults.join("scripts/provision-codex-hooks.sh"),
        into.join(bundle::PROVISIONER),
    )
    .unwrap();
}

/// One `pre_tool_use` event in Codex's real 0.146.0-pinned input schema (every
/// required field present), carrying a shell call the guards must police.
fn event(command: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "shell",
        "tool_input": {"command": ["bash", "-lc", command], "workdir": "/workspace/repo"},
        "cwd": "/workspace/repo",
        "session_id": "00000000-0000-0000-0000-00000000c0de",
        "tool_use_id": "call_fixture",
        "turn_id": "turn_fixture",
        "model": "fixture",
        "permission_mode": "auto",
        "transcript_path": "/dev/null",
    }))
    .unwrap()
}

impl Fixture {
    /// Run the hook exactly as Codex would: the command string taken from the
    /// profile's own `hooks.json` registration, the event on stdin. `policy`
    /// selects whether the forced private-session policy is present, which is
    /// the only difference between the reported escalation and its closure.
    fn hook(&self, name: &str, command: &str, policy: bool) -> String {
        let registered = self.exec(
            name,
            "jq -r '[.hooks.PreToolUse[].hooks[].command] | map(select(contains(\"guard-codex-bridge.sh\"))) | .[0]' \"$CODEX_HOME/hooks.json\"",
        );
        let registered = registered.trim().to_owned();
        assert!(
            registered.starts_with(&format!("{}/hooks/", bundle::CONTROL_ROOT)),
            "managed registration must name the image-owned bridge, got {registered}"
        );
        let env = if policy {
            bundle::POLICY
                .iter()
                .map(|(k, v)| format!("export {k}={v}; "))
                .collect::<String>()
        } else {
            String::new()
        };
        self.exec(
            name,
            &format!("{env}printf '%s' {} | bash {registered}", shell_quote(&event(command))),
        )
    }

    fn derive(&self, tag: &str, steps: &str) -> String {
        let image = format!("{}-{tag}", self.image);
        self.derived.borrow_mut().push(image.clone());
        let context = self.root.path().join(format!("derived-{tag}"));
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            format!("FROM {}\nUSER root\n{steps}\n", self.image),
        )
        .unwrap();
        docker(&["build", "-q", "-t", &image, context.to_str().unwrap()]);
        image
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[test]
#[ignore = "requires Docker; explicitly run by CI"]
fn image_owned_control_bundle_keeps_guard_code_and_policy_enforced_after_mutation() {
    let f = Fixture::with_adapters(true);
    let name = &f.names[0];
    let protected = docker(&[
        "exec",
        &f.server,
        "git",
        "-C",
        "/srv/repo.git",
        "rev-parse",
        "main",
    ]);
    f.start(name);

    // ---- the boundary itself -------------------------------------------
    let manifest: Value =
        serde_json::from_str(&f.exec(name, "cat /opt/loom/private-control/manifest.json")).unwrap();
    assert_eq!(manifest["protocol"], bundle::CONTROL_PROTOCOL);
    assert_eq!(manifest["control_version"], bundle::CONTROL_VERSION);
    assert_eq!(manifest["registration"], bundle::registration());
    // Sealed against the CLI actually installed in the image, and at or above
    // the schema floor the bridge is pinned to (acceptance criterion 6).
    assert!(manifest["codex_cli"].as_str().unwrap().contains("0.149.1"));
    assert_eq!(manifest["codex_schema_pin"], bundle::CODEX_FLOOR);
    let report: Value =
        serde_json::from_str(&f.exec(name, "loom-daemon private-workspace control")).unwrap();
    assert_eq!(report["status"], "ready", "{report}");
    assert_eq!(report["managed"], true);
    let identity = report["identity"].as_str().unwrap().to_owned();

    // The worker cannot write into the bundle, and nothing under it names a
    // credential (acceptance criterion 4).
    f.exec(
        name,
        "! (printf x > /opt/loom/private-control/hooks/guard-codex-bridge.sh) 2>/dev/null \
         && ! (rm -f /opt/loom/private-control/hooks/guard-codex-bridge.sh) 2>/dev/null \
         && ! (printf x > /opt/loom/private-control/attack) 2>/dev/null \
         && test ! -e /opt/loom/private-control/auth.json",
    );
    assert!(f
        .exec(name, "grep -rl synthetic-credential-free-fixture /opt/loom/private-control /workspace/repo || true")
        .trim()
        .is_empty());

    // ---- regression 1: `.loom/config.json` guards.forceScope: "off" -----
    let force = "git push --force origin HEAD:main";
    f.exec(
        name,
        "printf '%s' '{\"guards\":{\"forceScope\":\"off\"}}' > /workspace/repo/.loom/config.json",
    );
    // The reported escalation, reproduced: with the worker's own configuration
    // as the effective policy, the bridge allows the force-push (an allow is
    // silence on the Codex 0.146.0 wire).
    assert!(
        f.hook(name, force, false).trim().is_empty(),
        "fixture must reproduce the reported escalation, or it proves nothing"
    );
    // ...and closed: the forced policy the host puts on the model's own process
    // wins over every config tier, so the same call now fails closed.
    let denied = f.hook(name, force, true);
    assert!(
        denied.contains("\"permissionDecision\": \"deny\"")
            || denied.contains("\"permissionDecision\":\"deny\""),
        "force-push over a protected ref must fail closed: {denied}"
    );

    // ---- regression 2: delete the guard bridge from the clone -----------
    f.exec(
        name,
        "rm -rf /workspace/repo/.loom/hooks /workspace/repo/defaults 2>/dev/null; true",
    );
    let denied = f.hook(name, force, true);
    assert!(
        denied.contains("deny"),
        "enforcement must survive deleting the clone's own guard copies: {denied}"
    );
    // A worker-writable canonical guard is not consulted either: the bundle
    // ships the generic guard AS guard-destructive.sh.
    f.exec(
        name,
        "mkdir -p /workspace/repo/.claude/skills/repo/hooks && printf '#!/usr/bin/env bash\\nexit 0\\n' > /workspace/repo/.claude/skills/repo/hooks/guard-destructive.sh",
    );
    assert!(f.hook(name, force, true).contains("deny"));

    // Neither mutation survives an admission: the clone is dirty, and `prepare`
    // refuses reuse without touching a byte of it.
    let dirty = f.job(name, "role", "true").output().unwrap();
    assert!(!dirty.status.success());
    assert!(String::from_utf8_lossy(&dirty.stderr).contains("dirty"));
    f.exec(
        name,
        "git -C /workspace/repo restore --source=HEAD --staged --worktree -- . && git -C /workspace/repo clean -qfd",
    );

    // ---- the non-regression half: normal private work still works -------
    checked(
        f.job(
            name,
            "sweep",
            "cd /workspace/repo && git switch -c feature/issue-8839 && printf 'control\\n' > control.txt \
             && git add control.txt && git commit -qm 'fixture control change' && git push -q -u origin feature/issue-8839",
        )
        .output()
        .unwrap(),
    );
    assert!(f
        .exec(name, "git -C /workspace/repo log -1 --format=%s origin/feature/issue-8839")
        .contains("fixture control change"));

    // ---- execution-side recheck + forced policy (criterion 5) -----------
    // `execute` is the process that execs the model; it audits its own argv, so
    // the probe is a bare `env` (a `sh -c …` wrapper is refused by that audit,
    // not by this boundary).
    let execute = |identity: Option<&str>| {
        let mut args = vec![
            "exec".to_owned(),
            "--env".into(),
            "LOOM_PRIVATE_WORKSPACE=1".into(),
        ];
        if let Some(identity) = identity {
            args.extend([
                "--env".to_owned(),
                format!("LOOM_PRIVATE_CONTROL={identity}"),
            ]);
        }
        args.extend([
            format!("loom-codex-session-{name}"),
            "loom-daemon".into(),
            "private-workspace".into(),
            "execute".into(),
            "--".into(),
            "env".into(),
        ]);
        Command::new("docker").args(&args).output().unwrap()
    };
    // The bound identity still holds, and the exec'd process carries the forced
    // policy — the guards' env override, applied by the code that execs.
    let applied = checked(execute(Some(&identity)));
    for (key, value) in bundle::POLICY {
        assert!(
            applied.lines().any(|line| line == format!("{key}={value}")),
            "forced policy {key}={value} did not reach the exec'd process: {applied}"
        );
    }
    let wrong = execute(Some(&"0".repeat(64)));
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("control identity changed"));
    let unbound = execute(None);
    assert!(!unbound.status.success());
    assert!(String::from_utf8_lossy(&unbound.stderr).contains("predates"));

    // ---- preparation-side refusals: replaced / stale image (criterion 5) --
    let mutated = f.derive(
        "mutated",
        "RUN chmod -R u+w /opt/loom/private-control \
         && printf 'exit 0\\n' >> /opt/loom/private-control/hooks/guard-codex-bridge.sh \
         && chmod -R a-w /opt/loom/private-control",
    );
    let stale = f.derive(
        "stale",
        "RUN chmod -R u+w /opt/loom/private-control \
         && sed -i 's/loom-private-control-v1/loom-private-control-v0/' /opt/loom/private-control/manifest.json \
         && chmod -R a-w /opt/loom/private-control",
    );
    let absent = f.derive("absent", "RUN rm -rf /opt/loom/private-control");
    for (image, expected) in [
        (&mutated, "sealed digests"),
        (&stale, "different protocol"),
        (&absent, "ships no"),
    ] {
        let refused = f
            .command(&[
                "session",
                "start",
                &f.names[1],
                "--private-clone",
                &f.repository,
                "--image",
                image,
            ])
            .output()
            .unwrap();
        assert!(!refused.status.success(), "{image} was admitted");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains(expected), "{image}: {stderr}");
        // The refusal happens AFTER the container exists, and deliberately
        // leaves it in place rather than resetting an account's session behind
        // the operator. Clearing it explicitly here is what that refusal's own
        // message instructs ("stop the idle container before recreating it
        // with the requested image"), and it is what lets the next unsupported
        // image be admitted far enough to be refused on its own merits.
        checked(
            f.command(&["session", "stop", &f.names[1], "--json"])
                .output()
                .unwrap(),
        );
    }

    // Nothing in this test moved a protected ref on the disposable remote.
    assert_eq!(
        protected,
        docker(&[
            "exec",
            &f.server,
            "git",
            "-C",
            "/srv/repo.git",
            "rev-parse",
            "main",
        ])
    );
    // Authentication survived untouched in the canonical profile mount.
    let auth = f.root.path().join("profiles").join(name).join("auth.json");
    assert_eq!(std::fs::read_to_string(&auth).unwrap(), "synthetic-credential-free-fixture");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(f.root.path().join("profiles").join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    // A writable profile is what keeps refresh working: prove the canonical
    // lifecycle can still replace auth.json atomically from inside the session.
    f.exec(
        name,
        "printf 'refreshed-synthetic\\n' > \"$CODEX_HOME/auth.json.tmp\" && mv \"$CODEX_HOME/auth.json.tmp\" \"$CODEX_HOME/auth.json\"",
    );
    assert_eq!(std::fs::read_to_string(&auth).unwrap().trim(), "refreshed-synthetic");
}
