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

/// One `pre_tool_use` event in Codex's 0.146.0-pinned input schema (every
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

/// The same call in the shape the shipped **0.149.1** engine actually emits,
/// transcribed field-for-field from a payload captured by an always-allow
/// recorder hook during a real `codex exec` turn against the real session image
/// (`docker/session/test-image.sh` §12, which re-captures and re-asserts it on
/// every image build). It differs from the 0.146.0 form above in both places
/// the bridge has to understand: the tool is named `Bash`, not `shell`, and
/// `tool_input.command` is a **string** rather than an argv array. Exercising
/// only the older shape would mean the fixtures never met a payload this CLI
/// can produce.
fn engine_event(command: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "session_id": "00000000-0000-0000-0000-00000000c0de",
        "turn_id": "turn_fixture",
        "transcript_path": "/dev/null",
        "cwd": "/workspace/repo",
        "hook_event_name": "PreToolUse",
        "model": "fixture-model",
        "permission_mode": "bypassPermissions",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "tool_use_id": "call_fixture",
    }))
    .unwrap()
}

impl Fixture {
    /// Run the hook exactly as Codex would: the command string taken from the
    /// profile's own `hooks.json` registration, the event on stdin. `policy`
    /// selects whether the forced private-session policy is present, which is
    /// the only difference between the reported escalation and its closure.
    /// `shape` builds the payload, so the same assertion can be made against
    /// both the 0.146.0 schema and the shape the shipped engine emits today.
    fn hook_with(
        &self,
        name: &str,
        shape: fn(&str) -> String,
        command: &str,
        policy: bool,
    ) -> String {
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
            &format!("{env}printf '%s' {} | bash {registered}", shell_quote(&shape(command))),
        )
    }

    /// The 0.146.0-schema payload, which every existing assertion uses.
    fn hook(&self, name: &str, command: &str, policy: bool) -> String {
        self.hook_with(name, event, command, policy)
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

/// Every route a worker has to a file in a directory it owns, tried against
/// each of the profile's control files in turn. On a read-only **mount point**
/// all of them fail — writes with `EROFS`, unlink and rename with `EBUSY` —
/// while the directory around them stays writable, which is what keeps the
/// canonical atomic `auth.json` refresh working. `ESCALATED` on stdout, or a
/// nonzero exit, means the protection did not hold.
const FREEZE_PROBE: &str = r#"
set -u
escalated() { echo "ESCALATED: $1"; exit 1; }
for control in hooks.json config.toml loom-codex-hooks.json; do
    path="$CODEX_HOME/$control"
    test -f "$path" || escalated "$control is absent"
    printf attack > "$path" 2>/dev/null && escalated "wrote $control"
    chmod 666 "$path" 2>/dev/null && escalated "chmod $control"
    rm -f "$path" 2>/dev/null && escalated "removed $control"
    mv "$path" "$path.stolen" 2>/dev/null && escalated "renamed $control away"
    printf attack > "$CODEX_HOME/decoy" || escalated "profile directory is not writable"
    mv "$CODEX_HOME/decoy" "$path" 2>/dev/null && escalated "renamed over $control"
    rm -f "$CODEX_HOME/decoy"
    ln -sf /dev/null "$path" 2>/dev/null && escalated "symlinked over $control"
    ln -f "$CODEX_HOME/auth.json" "$path" 2>/dev/null && escalated "hardlinked over $control"
done
printf 'refreshed-synthetic\n' > "$CODEX_HOME/auth.json.tmp" \
    && mv "$CODEX_HOME/auth.json.tmp" "$CODEX_HOME/auth.json" \
    || escalated "the canonical atomic auth refresh stopped working"
echo FROZEN
"#;

/// The demonstrated escalation this increment closes, stated as the worker
/// would: remove the registration the engine reads, and every later turn of
/// this session runs unhooked no matter what any earlier check proved.
#[test]
#[ignore = "requires Docker; explicitly run by CI"]
fn profile_control_files_are_frozen_by_mount_and_an_unprotected_session_is_refused() {
    let f = Fixture::with_adapters(true);
    let name = &f.names[0];
    f.start(name);
    let profile = f.root.path().join("profiles").join(name);

    // The registration exists, names the image-owned bridge, and the boundary
    // reports the whole thing ready and bound.
    let report: Value =
        serde_json::from_str(&f.exec(name, "loom-daemon private-workspace control")).unwrap();
    assert_eq!(report["status"], "ready", "{report}");
    assert_eq!(report["control_version"], bundle::CONTROL_VERSION);
    let registered = f.exec(
        name,
        "jq -r '[.hooks.PreToolUse[].hooks[].command] | map(select(contains(\"guard-codex-bridge.sh\"))) | .[0]' \"$CODEX_HOME/hooks.json\"",
    );
    assert_eq!(registered.trim(), bundle::registration());

    // Docker's own inventory: each control file is a read-only bind of the
    // canonical profile file onto its own path, and the profile DIRECTORY is
    // still read-write (that is what auth refresh needs).
    let container = format!("loom-codex-session-{name}");
    let mounts: Value =
        serde_json::from_str(&docker(&["inspect", "--format", "{{json .Mounts}}", &container]))
            .unwrap();
    let mounts = mounts.as_array().unwrap();
    for control in bundle::PROFILE_CONTROLS {
        let destination = bundle::control_destination(control);
        let mount = mounts
            .iter()
            .find(|mount| mount["Destination"] == destination.as_str())
            .unwrap_or_else(|| panic!("{control} is not bound at all: {mounts:?}"));
        assert_eq!(mount["Type"], "bind", "{control}");
        assert_eq!(mount["RW"], false, "{control}");
        assert_eq!(mount["Source"], profile.join(control).to_str().unwrap(), "{control}");
    }
    assert!(mounts
        .iter()
        .any(|mount| mount["Destination"] == "/home/loom/.codex-profile" && mount["RW"] == true));

    // The probe itself: the registration and Codex's trust state cannot be
    // written, removed, renamed away, renamed over, chmod-ed, symlinked over or
    // hardlinked over from inside the session — and auth refresh still works.
    let sealed: Vec<Vec<u8>> = bundle::PROFILE_CONTROLS
        .iter()
        .map(|control| std::fs::read(profile.join(control)).unwrap())
        .collect();
    assert_eq!(f.exec(name, FREEZE_PROBE).trim(), "FROZEN");
    for (control, before) in bundle::PROFILE_CONTROLS.iter().zip(&sealed) {
        assert_eq!(
            &std::fs::read(profile.join(control)).unwrap(),
            before,
            "{control} changed on the host"
        );
    }
    assert_eq!(
        std::fs::read_to_string(profile.join("auth.json"))
            .unwrap()
            .trim(),
        "refreshed-synthetic"
    );
    // Neither the probe nor the protection moved a byte of the session's own
    // identity: the boundary still reports the same bound identity.
    let after: Value =
        serde_json::from_str(&f.exec(name, "loom-daemon private-workspace control")).unwrap();
    assert_eq!(after["identity"], report["identity"]);

    // ---- a session that does NOT carry the protection is refused ----------
    // Built by hand with every other containment setting identical, because
    // that is exactly the shape an older daemon (or a hand-started container)
    // produces: the profile bound read-write and nothing else. Both halves of
    // the enforcement must refuse it independently.
    let unprotected = &f.names[1];
    f.start(unprotected);
    f.cli(&["session", "stop", unprotected, "--json"]);
    let peer = format!("loom-codex-session-{unprotected}");
    docker(&[
        "run",
        "-d",
        "--name",
        &peer,
        "--user",
        "1000:1000",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=512m",
        "--mount",
        &format!("type=volume,src=loom-codex-workspace-{unprotected},dst=/workspace"),
        "--mount",
        &format!(
            "type=bind,src={},dst=/home/loom/.codex-profile",
            f.root.path().join("profiles").join(unprotected).display()
        ),
        "--mount",
        &format!(
            "type=bind,src={},dst=/run/loom-gh,readonly",
            f.root.path().join("forge").display()
        ),
        "--label",
        "loom.workspace-mode=private-clone",
        "--label",
        "loom.workspace=/workspace/repo",
        "--label",
        &format!("loom.account={unprotected}"),
        "--label",
        &format!("loom.repository={}", f.repository),
        "--env",
        "CODEX_HOME=/home/loom/.codex-profile",
        "--env",
        "GH_CONFIG_DIR=/run/loom-gh",
        "--entrypoint",
        "/usr/bin/tini",
        &f.image,
        "--",
        "/bin/sleep",
        "infinity",
    ]);
    // 1. The host refuses it on Docker's own mount inventory, before any
    //    in-container code is trusted to describe itself.
    let refused = f.job(unprotected, "role", "true").output().unwrap();
    assert!(!refused.status.success(), "an unprotected session was admitted");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("bound read-only"), "{stderr}");
    // 2. And the boundary refuses it from inside, on the live mount table,
    //    even though the registration's CONTENT is still perfectly correct —
    //    which is the whole point: content that can be replaced mid-session is
    //    not enforcement, and a check that only detects it runs too late.
    let exposed: Value = serde_json::from_str(&docker(&[
        "exec",
        &peer,
        "loom-daemon",
        "private-workspace",
        "control",
    ]))
    .unwrap();
    assert_eq!(exposed["status"], "profile-mutable", "{exposed}");
    assert_eq!(exposed["identity"], "");
    assert_eq!(
        docker(&[
            "exec",
            &peer,
            "sh",
            "-c",
            "jq -r '[.hooks.PreToolUse[].hooks[].command] | map(select(contains(\"guard-codex-bridge.sh\"))) | .[0]' \"$CODEX_HOME/hooks.json\"",
        ])
        .trim(),
        bundle::registration(),
        "the unprotected session's registration content is correct; only its reachability differs"
    );
    // ...and the escalation is real there: the same probe that is refused
    // above succeeds against the unprotected profile.
    let escalated = Command::new("docker")
        .args(["exec", &peer, "sh", "-c", "rm -f \"$CODEX_HOME/hooks.json\" && test ! -e \"$CODEX_HOME/hooks.json\" && echo UNHOOKED"])
        .output()
        .unwrap();
    assert!(
        checked(escalated).contains("UNHOOKED"),
        "the unprotected shape must still reproduce the escalation, or the check above proves nothing"
    );
    docker(&["rm", "-f", &peer]);
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
    // The same call in the shape the SHIPPED engine emits (`Bash`, string
    // command — see `engine_event`). Both halves again: the worker's own
    // configuration still allows it, the forced policy still denies it. A
    // fixture that only ever used the older argv shape could pass while the
    // bridge had stopped classifying what this CLI version actually sends.
    assert!(
        f.hook_with(name, engine_event, force, false)
            .trim()
            .is_empty(),
        "the escalation must reproduce on the engine's own payload shape too"
    );
    let denied = f.hook_with(name, engine_event, force, true);
    assert!(
        denied.contains("\"permissionDecision\": \"deny\"")
            || denied.contains("\"permissionDecision\":\"deny\""),
        "the engine's own payload shape must fail closed as well: {denied}"
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
        // A bundle that is present but unusable (mutated, stale) is refused
        // after the container exists, and deliberately leaves it in place
        // rather than resetting an account's session behind the operator; an
        // image with no bundle at all is refused one step earlier still, at the
        // pre-create provisioning of the profile's control files, so no
        // container is created for it. `session stop` is correct in both cases
        // — it is what that refusal's own message instructs ("stop the idle
        // container before recreating it with the requested image"), and it is
        // what lets the next unsupported image be admitted far enough to be
        // refused on its own merits.
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
