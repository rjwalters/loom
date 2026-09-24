//! Synthetic Codex CLI drives the real adapters; Git and Docker are real.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;

pub(super) struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Environment {
    pub(super) fn set(values: &[(&'static str, Option<std::ffi::OsString>)]) -> Self {
        let old = values
            .iter()
            .map(|(key, value)| {
                let old = std::env::var_os(key);
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
                (*key, old)
            })
            .collect();
        Self(old)
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

pub(super) fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        // defaults/roles contains symlinks to canonical runtime instructions;
        // installed consumer files carry their contents inside the clone.
        let metadata = std::fs::metadata(entry.path()).unwrap();
        if metadata.is_dir() {
            copy_tree(&entry.path(), &dest);
        } else if metadata.is_file() {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

pub(super) const CODEX: &str = r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == login ]]; then echo 'Logged in using an API key'; exit 0; fi
echo "fixture-private-cwd=$PWD"
test "$PWD" = /workspace/repo
test "$LOOM_WORKSPACE" = /workspace/repo
test "$LOOM_PROJECT_ROOT" = /workspace/repo
test "$LOOM_WORKTREE_ROOT" = /workspace/repo/.loom/worktrees
test -z "${LOOM_RUN_JOB_HOST:-}"
test -z "${LOOM_DAEMON_SOCKET:-}"
test -r .loom/hooks/guard-codex-bridge.sh
test -r .claude/commands/loom/guide.md
test -r .claude/commands/loom/sweep.md
if [[ "$*" == *retain* ]]; then
  echo unpushed > retained.txt
  echo fixture-retained-ready
  sleep 30
  exit 0
fi
if [[ "$*" == *guarded* ]]; then
  # Emulate Codex consulting its installed pre_tool_use hook (#8787): the
  # command comes from the profile's managed hooks.json, unmodified.
  test "${LOOM_ROLE:-}" = builder
  test -n "${LOOM_PRIVATE_CONTAINER_ID:-}"
  hook="$(jq -r '[.hooks.PreToolUse[].hooks[].command | select(contains("guard-codex-bridge.sh"))][0]' "$CODEX_HOME/hooks.json")"
  test -n "$hook" && test "$hook" != null
  wt=/workspace/repo/.loom/worktrees/issue-8787
  git worktree add -b feature/issue-8787 "$wt" HEAD
  printf '# Loom-managed worktree marker\n' > "$wt/.loom-managed"
  decide() {
    jq -nc --arg t "$1" --argjson i "$2" --arg c "$wt" '{hook_event_name:"PreToolUse",session_id:"11111111-2222-3333-4444-555555555555",transcript_path:null,turn_id:"turn-1",tool_use_id:"call-1",model:"fixture",permission_mode:"default",agent_id:"agent-1",agent_type:"primary",cwd:$c,tool_name:$t,tool_input:$i}' \
      | sh -c "$hook" | jq -rs 'if length == 0 then "allow" else .[0].hookSpecificOutput.permissionDecision end'
  }
  test "$(decide shell '{"command":["bash","-lc","git push --force origin main"]}')" = deny
  test "$(decide shell '{"command":["bash","-lc","gh pr merge 1 --squash"]}')" = deny
  test "$(decide write_stdin '{"session_id":1,"chars":"true\\n"}')" = deny
  test "$(decide apply_patch "$(jq -nc '{input:"*** Begin Patch\n*** Update File: /workspace/repo/file\n@@\n-base\n+tampered\n*** End Patch"}')")" = deny
  test "$(decide apply_patch "$(jq -nc --arg p "$wt/change.txt" '{input:("*** Begin Patch\n*** Add File: " + $p + "\n+issue change\n*** End Patch")}')")" = allow
  echo 'issue change' > "$wt/change.txt"
  git -C "$wt" add change.txt
  git -C "$wt" commit -m 'fixture guarded change'
  git -C "$wt" push -u origin feature/issue-8787
  echo fixture-guarded-complete
  exit 0
fi
if [[ "$*" == *mutate* ]]; then
  git switch -c feature/issue-8786
  echo 'private issue mutation' > mutation.txt
  git add mutation.txt
  git commit -m 'fixture issue mutation'
  git push -u origin feature/issue-8786
  .loom/scripts/create-pr.sh --repo fixture/repo --head feature/issue-8786 --base main --title 'Fixture change' --body 'Part of #8786' --label loom:review-requested
  .loom/scripts/sweep-checkpoint.sh write 8786 builder-done --task-id fixture-sweep --pr-number 17
fi
if .loom/scripts/run-job.sh --image alpine -- true >/tmp/run-job.log 2>&1; then exit 91; fi
grep -q 'unsupported in private-clone v1' /tmp/run-job.log
echo fixture-private-complete
echo 'session id: 12345678-1234-1234-1234-123456789abc' >&2
"#;

// This is a local forge CLI fixture, not a model or forge credential. It
// exercises the production create-pr helper's normal adoption/create flow.
pub(super) const GH: &str = r#"#!/usr/bin/env python3
import json, os, sys, urllib.request, ssl
a=sys.argv[1:]
if a[:2]==['pr','list']: sys.exit(0)
if a[:2]==['repo','view']: print('main'); sys.exit(0)
if a[:2]==['pr','create']:
    endpoint=os.environ['LOOM_PRIVATE_REPOSITORY'].split('/github/')[0]+'/fixture/pr'
    req=urllib.request.Request(endpoint, data=json.dumps({'head':'feature/issue-8786','number':17}).encode(), headers={'Content-Type':'application/json'})
    with urllib.request.urlopen(req,context=ssl._create_unverified_context()) as r: print(json.loads(r.read())['url'])
    sys.exit(0)
if a[:2]==['auth','status']: sys.exit(0)
if a and a[0]=='api': print('{}'); sys.exit(0)
sys.exit(1)
"#;

impl Fixture {
    pub(super) fn adapter(&self, name: &str, prompt: &str) -> Command {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_owned();
        let mut command = Command::new("bash");
        command
            .process_group(0)
            .arg(source.join("defaults/scripts/spawn-worker.sh"))
            .args(["-p", prompt, "--dangerously-skip-permissions"])
            .current_dir(self.root.path().join("registry"))
            .env(
                "LOOM_DAEMON_SELF_BIN",
                std::env::var_os("LOOM_TEST_ADAPTER_BIN")
                    .unwrap_or_else(|| self.host.clone().into_os_string()),
            )
            .env("LOOM_WORKSPACE", self.root.path().join("registry"))
            .env("LOOM_CODEX_PROFILE_ROOT", self.root.path().join("profiles"))
            .env("LOOM_CODEX_PROFILE", name)
            .env("LOOM_RUNTIME", "codex")
            .env("LOOM_CODEX_AUTH_MODE_CHECK", "0")
            .env("LOOM_RUN_JOB_HOST", "forbidden-host")
            .env("GH_CONFIG_DIR", self.root.path().join("forge"))
            .env("GITEA_USERNAME", auth::USERNAME)
            .env("GITEA_TOKEN", auth::PASSWORD);
        for key in [
            "CODEX_HOME",
            "LOOM_CODEX_HOME",
            "LOOM_ROLE",
            "LOOM_PRIVATE_LEASE_FD",
            "LOOM_CODEX_NO_EXEC",
            "LOOM_SPAWN_NO_EXPORT",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "FORGE_TOKEN",
            "LOOM_ACCOUNT_NAME",
            "LOOM_ACCOUNT_PROVIDER",
        ] {
            command.env_remove(key);
        }
        command
    }
}

#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn adapter_chain_pushes_private_branch_and_preserves_host_logs_and_work() {
    let mut f = Fixture::with_adapters(true);
    f.repository = f.repository.replace("/gitea/", "/github/");
    let name = &f.names[0];
    let gh_host = reqwest::Url::parse(&f.repository)
        .unwrap()
        .host_str()
        .unwrap()
        .to_owned();
    checked(
        f.command(&[
            "session",
            "start",
            name,
            "--private-clone",
            &f.repository,
            "--image",
            &f.image,
        ])
        .env("GH_HOST", &gh_host)
        .env("GH_TOKEN", auth::GH_TOKEN)
        .output()
        .unwrap(),
    );
    let root = f.root.path().join("registry");
    checked(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&root)
            .output()
            .unwrap(),
    );
    checked(
        Command::new("git")
            .args(["remote", "add", "origin", &f.repository])
            .current_dir(&root)
            .output()
            .unwrap(),
    );
    let defaults = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("defaults");
    for (src, dest) in [
        ("scripts", ".loom/scripts"),
        ("runtimes", ".loom/runtimes"),
        ("roles", ".loom/roles"),
        (".claude/commands/loom", ".claude/commands/loom"),
    ] {
        copy_tree(&defaults.join(src), &root.join(dest));
    }
    let _environment = Environment::set(&[
        ("LOOM_WORKSPACE", Some(root.clone().into_os_string())),
        ("LOOM_CODEX_PROFILE_ROOT", Some(f.root.path().join("profiles").into_os_string())),
        ("LOOM_CODEX_PROFILE", Some(name.into())),
        ("LOOM_RUNTIME", Some("codex".into())),
        ("LOOM_DAEMON_SELF_BIN", Some(f.host.clone().into_os_string())),
        ("GH_CONFIG_DIR", Some(f.root.path().join("forge").into_os_string())),
        ("GH_HOST", Some(gh_host.clone().into())),
        ("GH_TOKEN", Some(auth::GH_TOKEN.into())),
        ("LOOM_CODEX_AUTH_MODE_CHECK", Some("0".into())),
        ("CODEX_HOME", None),
        ("LOOM_CODEX_HOME", None),
        ("LOOM_PRIVATE_LEASE_FD", None),
        ("LOOM_ROLE", None),
        ("LOOM_CODEX_NO_EXEC", None),
        ("LOOM_SPAWN_NO_EXPORT", None),
    ]);
    // An unrelated private account must not force legacy explicit homes into
    // the inventory or touch Docker for their selection.
    let legacy = f.root.path().join("legacy-explicit");
    std::fs::create_dir(&legacy).unwrap();
    {
        let _legacy = Environment::set(&[("LOOM_CODEX_HOME", Some(legacy.into_os_string()))]);
        assert!(loom_daemon::tokens_pool::private_workspace::dispatch::Selection::prepare(
            &root,
            "codex",
            None,
            loom_daemon::tokens_pool::private_workspace::JobKind::Role,
            None,
            "legacy"
        )
        .unwrap()
        .is_none());
    }
    // #8787: containment is attempted for the sweep (Builder requirements) but
    // refused before any claim: the managed hook installed in the clone has
    // not been trusted, so the remote-operation/lifecycle obligations remain
    // unproven. No manifest is promoted and no trust is fabricated here.
    let mut config = loom_daemon::sweep_registry::SweepRegistryConfig::new(root.clone());
    config.journal_path = Some(root.join("sweeps.json"));
    let registry = std::sync::Arc::new(std::sync::Mutex::new(
        loom_daemon::sweep_registry::SweepRegistry::new(config),
    ));
    let rejected = loom_daemon::sweep_registry::SweepRegistry::dispatch_unlocked(
        &registry,
        &loom_daemon::types::SweepKind::Issue(8786),
        None,
        None,
        None,
        None,
    )
    .unwrap_err();
    assert!(rejected.to_string().contains("capabilit"), "{rejected}");
    assert!(rejected.to_string().contains("pre_tool_use hook"), "{rejected}");
    assert!(!root.join(".loom/locks/issue-8786").exists());
    // A read-only synthetic profile must fail before any role/model launch.
    // Restore its original owner-only mode before asserting or continuing.
    let profile = f.root.path().join("profiles").join(name);
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o500)).unwrap();
    let inaccessible = loom_daemon::tokens_pool::private_workspace::dispatch::Selection::prepare(
        &root,
        "codex",
        None,
        loom_daemon::tokens_pool::private_workspace::JobKind::Role,
        None,
        "fixture-inaccessible",
    )
    .err();
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(inaccessible
        .unwrap()
        .to_string()
        .contains("profile is not readable and writable"));
    use loom_daemon::role_runner::RoleInvocationRunner;
    let mut runner = loom_daemon::role_runner::ScriptRoleInvocationRunner::new(root.clone())
        .with_timeout(Duration::from_secs(30));
    let outcome = runner.invoke("guide", "fixture guide");
    let role_log = std::fs::read_to_string(root.join(".loom/logs/role-guide.log"))
        .unwrap_or_else(|error| format!("role log unavailable: {error}"));
    assert!(
        matches!(outcome, loom_daemon::role_runner::RoleTickOutcome::Success),
        "scheduled private guide failed: {outcome:?}\n{role_log}"
    );
    assert!(role_log.contains("fixture-private-complete"));
    use loom_daemon::tokens_pool::private_workspace::{dispatch::Selection, JobKind};
    let rejected_spawn = Selection::prepare(
        &root,
        "codex",
        None,
        JobKind::Sweep,
        Some(9999),
        "fixture-rejected-spawn",
    )
    .unwrap()
    .unwrap();
    let mut missing_command = Command::new("/nonexistent/private-fixture");
    rejected_spawn.apply(&mut missing_command);
    assert!(missing_command.spawn().is_err());
    drop(rejected_spawn);
    assert!(!root.join(".loom/private-jobs/issue-9999.json").exists());
    // Same production preflight as scheduled/explicit dispatch; the fake
    // free-form worker tests transport without claiming mutable capability.
    let selection =
        Selection::prepare(&root, "codex", None, JobKind::Sweep, Some(8786), "fixture-sweep")
            .unwrap()
            .unwrap();
    let busy = Selection::prepare(&root, "codex", None, JobKind::Role, None, "fixture-concurrent");
    assert!(busy.err().unwrap().to_string().contains("busy"));
    let mut command = f.adapter(name, "mutate");
    command
        .env("GH_HOST", &gh_host)
        .env("GH_TOKEN", auth::GH_TOKEN);
    selection.apply(&mut command);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    selection.spawned();
    drop(selection);
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = checked(output);
    assert!(stdout.contains("fixture-private-complete"));
    assert!(stdout.contains("/pull/17"));
    assert!(stderr.contains("LOOM_ACCOUNT"));
    assert!(stderr.contains("LOOM_TERMINAL_RESULT"));
    assert!(!stderr.contains(auth::GH_TOKEN));
    assert!(!root.join("mutation.txt").exists());
    let checkpoint: Value = serde_json::from_slice(
        &std::fs::read(root.join(".loom/sweep-checkpoint/issue-8786.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(checkpoint["pr_number"], 17);
    let record: Value = serde_json::from_slice(
        &std::fs::read(root.join(".loom/private-jobs/issue-8786.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["outcome"], "completed");
    assert_eq!(record["owner"], "fixture-sweep");
    assert_eq!(record["snapshot"]["published"], true);
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("host-sibling/keep")).unwrap(),
        "host fixture"
    );
    let state: Value =
        serde_json::from_str(&f.cli(&["session", "status", name, "--json"])).unwrap();
    assert!(state["lease"].is_null());
    assert!(f
        .exec(name, "git -C /workspace/repo log -1 --format=%s origin/feature/issue-8786")
        .contains("fixture issue mutation"));
    let checkpoint_time = std::fs::metadata(root.join(".loom/sweep-checkpoint/issue-8786.json"))
        .unwrap()
        .modified()
        .unwrap();
    let retained_log = root.join("retained.log");
    let interrupted =
        Selection::prepare(&root, "codex", None, JobKind::Sweep, Some(8786), "fixture-interrupted")
            .unwrap()
            .unwrap();
    let mut retained_command = f.adapter(name, "retain");
    interrupted.apply(&mut retained_command);
    let result = retained_command
        .env("GH_HOST", &gh_host)
        .env("GH_TOKEN", auth::GH_TOKEN)
        .stdout(std::fs::File::create(&retained_log).unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    interrupted.spawned();
    drop(interrupted);
    let pid = result.id();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(&retained_log)
        .unwrap_or_default()
        .contains("fixture-retained-ready")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "worker did not reach retained-work readiness"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    let retained = result.wait_with_output().unwrap();
    assert!(!retained.status.success());
    let restarted =
        Selection::prepare(&root, "claude", None, JobKind::Sweep, Some(8786), "fixture-restart");
    assert!(restarted
        .err()
        .unwrap()
        .to_string()
        .contains("requires recovery"));
    {
        let _fallback = Environment::set(&[("LOOM_RUNTIME", Some("claude".into()))]);
        let restarted_registry = std::sync::Arc::new(std::sync::Mutex::new(
            loom_daemon::sweep_registry::SweepRegistry::new(
                loom_daemon::sweep_registry::SweepRegistryConfig::new(root.clone()),
            ),
        ));
        let refusal = loom_daemon::sweep_registry::SweepRegistry::dispatch_unlocked(
            &restarted_registry,
            &loom_daemon::types::SweepKind::Issue(8786),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(refusal.to_string().contains("requires recovery"), "{refusal}");
    }

    let record: Value = serde_json::from_slice(
        &std::fs::read(root.join(".loom/private-jobs/issue-8786.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["owner"], "fixture-interrupted");
    assert_ne!(record["outcome"], "completed");
    assert_eq!(record["snapshot"]["published"], false);
    assert_eq!(
        std::fs::metadata(root.join(".loom/sweep-checkpoint/issue-8786.json"))
            .unwrap()
            .modified()
            .unwrap(),
        checkpoint_time
    );
    let refused = f
        .adapter(name, "next")
        .env("GH_HOST", &gh_host)
        .env("GH_TOKEN", auth::GH_TOKEN)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("dirty"));
    assert!(f
        .exec(name, "cat /workspace/repo/retained.txt")
        .contains("unpushed"));
    assert!(std::fs::read_to_string(&retained_log)
        .unwrap()
        .contains("fixture-retained-ready"));
    assert!(role_log.contains("LOOM_TERMINAL_RESULT"));
    // A disappearing container must leave a durable, non-success outcome and
    // host logs; the next process must refuse runtime failover after restart.
    let second = &f.names[1];
    checked(
        f.command(&[
            "session",
            "start",
            second,
            "--private-clone",
            &f.repository,
            "--image",
            &f.image,
        ])
        .env("GH_HOST", &gh_host)
        .env("GH_TOKEN", auth::GH_TOKEN)
        .output()
        .unwrap(),
    );
    let _second = Environment::set(&[("LOOM_CODEX_PROFILE", Some(second.into()))]);
    let selection = Selection::prepare(
        &root,
        "codex",
        None,
        JobKind::Sweep,
        Some(8788),
        "fixture-container-loss",
    )
    .unwrap()
    .unwrap();
    let lost_log = root.join("container-loss.log");
    let mut command = f.adapter(second, "retain");
    selection.apply(&mut command);
    let child = command
        .env("GH_HOST", &gh_host)
        .env("GH_TOKEN", auth::GH_TOKEN)
        .stdout(std::fs::File::create(&lost_log).unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    selection.spawned();
    drop(selection);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(&lost_log)
        .unwrap_or_default()
        .contains("fixture-retained-ready")
    {
        assert!(std::time::Instant::now() < deadline, "container-loss worker did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    docker(&["rm", "-f", &format!("loom-codex-session-{second}")]);
    assert!(!child.wait_with_output().unwrap().status.success());
    let record: Value = serde_json::from_slice(
        &std::fs::read(root.join(".loom/private-jobs/issue-8788.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["account"], second.as_str());
    assert_ne!(record["outcome"], "completed");
    assert!(
        Selection::prepare(&root, "claude", None, JobKind::Sweep, Some(8788), "restarted").is_err()
    );
    assert!(std::fs::read_to_string(&lost_log)
        .unwrap()
        .contains("fixture-retained-ready"));
}
