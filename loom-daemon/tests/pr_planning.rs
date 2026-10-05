//! CLI contract: use the real queue policy with forge-shaped offline input.
use std::process::Command;
#[cfg(unix)]
#[path = "support/write_scope_root.rs"]
mod write_scope_root;

#[test]
fn interactive_creation_is_explicit_and_unknown_is_the_default() {
    let root = tempfile::tempdir().unwrap();
    let marker = |origin: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
        cmd.args(["provenance", "pr-marker", "--repo-root"])
            .arg(root.path())
            .env_remove("LOOM_WORK_ORIGIN")
            .env_remove("GITHUB_ACTIONS");
        if let Some(origin) = origin {
            cmd.args(["--origin", origin]);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(marker(None).contains("origin=unknown"));
    assert!(marker(Some("interactive")).contains("origin=interactive"));
    assert!(marker(Some("autonomous")).contains("origin=autonomous"));
}

fn record(origin: &str) -> String {
    format!("<!-- loom:provenance v1 build=unknown unknown unknown prompts=unknown unknown sweep=none story=none trace=unknown host=unknown base=unknown run=none origin={origin} -->")
}
fn row(number: u64, origin: &str, label: &str) -> serde_json::Value {
    serde_json::json!({"number":number,"state":"open","draft":false,"created_at":format!("2026-09-{number:02}"),"body":record(origin),"user":{"login":"operator","type":"User"},"author_association":"OWNER","labels":[{"name":label}]})
}
fn cli(root: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    c.current_dir(root)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env_remove("LOOM_REPO")
        .env_remove("GH_REPO");
    c
}
fn output(mut cmd: Command) -> serde_json::Value {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn actual_role_entrypoints_use_shared_preference_and_effective_config() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(".loom")).unwrap();
    let defaults = root.join("defaults.json");
    std::fs::write(&defaults, r#"{"planning":{"preferHumanPrs":false}}"#).unwrap();
    for (role, label) in [
        ("judge", "loom:review-requested"),
        ("doctor", "loom:changes-requested"),
        ("champion", "loom:pr"),
    ] {
        let mut star = row(3, "autonomous", label);
        star["labels"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"name":"loom:operator-priority"}));
        let data = root.join("pulls.json");
        std::fs::write(
            &data,
            serde_json::to_vec(&vec![
                row(1, "autonomous", label),
                row(2, "interactive", label),
                star,
            ])
            .unwrap(),
        )
        .unwrap();
        for (override_on, expected) in [(false, vec![3, 1, 2]), (true, vec![3, 2, 1])] {
            std::fs::write(
                root.join(".loom/config.json"),
                if override_on {
                    r#"{"planning":{"preferHumanPrs":true}}"#
                } else {
                    "{}"
                },
            )
            .unwrap();
            let mut cmd = cli(root);
            cmd.env("LOOM_CONFIG_DEFAULTS_FILE", &defaults)
                .args(["pr-queue", "--role", role, "--input"])
                .arg(&data);
            let rows = output(cmd);
            let ids: Vec<_> = rows
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["number"].as_u64().unwrap())
                .collect();
            assert_eq!(ids, expected, "{role}, override={override_on}");
            assert_eq!(
                rows.as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r["number"] == 2)
                    .unwrap()["origin"],
                "interactive"
            );
        }
    }
}

#[cfg(unix)]
fn script(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn real_creation_adoption_and_autonomous_repair_preserve_interactive_origin() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let gh = root.join("gh");
    script(
        &gh,
        r#"
case "$1 $2" in
 'pr list') if test -f existing; then echo https://github.com/test/repo/pull/7; fi; exit 0;;
 'pr create') while test $# -gt 0; do if test "$1" = --body; then shift; printf '%s' "$1" > body.txt; fi; shift; done; echo https://github.com/test/repo/pull/7; exit 0;;
esac
exit 1
"#,
    );
    let gate = root.join("version-gate");
    script(&gate, "exit 0");
    let create =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/create-pr.sh");
    // create-pr.sh vets its target through `loom-daemon forge may-write`
    // (#9548); the real verb decides on a managed, writable fixture checkout.
    let write_scope = write_scope_root::writable_env(root);
    let run = |origin: &str, body: &str| {
        let out = Command::new("bash")
            .arg(&create)
            .args([
                "--title",
                "fix: fixture",
                "--head",
                "session-branch",
                "--body",
                body,
            ])
            .current_dir(root)
            .env("PATH", format!("{}:{}", root.display(), std::env::var("PATH").unwrap()))
            .env("LOOM_FORGE_TYPE", "github")
            .env("LOOM_VERSION_CHECK_SCRIPT", &gate)
            .env("LOOM_DAEMON_SELF_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
            .env("LOOM_DAEMON_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
            .envs(write_scope.iter().cloned())
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env("LOOM_WORK_ORIGIN", origin)
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GH_REPO")
            .env_remove("LOOM_REPO")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    run("interactive", "Human-driven change");
    let body = std::fs::read_to_string(root.join("body.txt")).unwrap();
    assert!(body.contains("origin=interactive"));
    // Repair/recreation preserves the body marker despite autonomous context.
    run("autonomous", &body);
    assert_eq!(std::fs::read_to_string(root.join("body.txt")).unwrap(), body);
    // Existing-PR adoption never writes a replacement origin/body.
    std::fs::write(root.join("existing"), "").unwrap();
    run("autonomous", "replacement body");
    assert_eq!(std::fs::read_to_string(root.join("body.txt")).unwrap(), body);
    let mut pr = row(7, "interactive", "loom:changes-requested");
    pr["body"] = serde_json::json!(body);
    let input = root.join("pulls.json");
    for (role, label) in [
        ("doctor", "loom:changes-requested"),
        ("judge", "loom:review-requested"),
        ("champion", "loom:pr"),
    ] {
        pr["labels"] = serde_json::json!([{"name":label}]);
        std::fs::write(&input, serde_json::to_vec(&vec![pr.clone()]).unwrap()).unwrap();
        let mut cmd = cli(root);
        cmd.env("LOOM_WORK_ORIGIN", "autonomous")
            .args(["pr-queue", "--role", role, "--input"])
            .arg(&input);
        assert_eq!(output(cmd)[0]["origin"], "interactive");
    }
}

#[cfg(unix)]
#[test]
fn production_discovery_paginates_and_keeps_fallback_guard_and_off_behavior() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".loom/scripts")).unwrap();
    let fleet: Vec<_> = (1..=100)
        .map(|n| row(n, "autonomous", "loom:review-requested"))
        .collect();
    let mut interactive = row(101, "interactive", "");
    interactive["labels"] = serde_json::json!([]);
    std::fs::write(root.join("page1"), serde_json::to_vec(&fleet).unwrap()).unwrap();
    std::fs::write(root.join("page2"), serde_json::to_vec(&vec![interactive]).unwrap()).unwrap();
    script(
        &root.join("gh"),
        r#"
printf 'HTTP/2 200 OK\r\n\r\n'
case "$*" in *page=2*) cat page2;; *) cat page1;; esac
"#,
    );
    script(
        &root.join(".loom/scripts/judge-fallback-guard.sh"),
        "echo \"$1\" >> guarded; exit \"${GUARD_EXIT:-0}\"",
    );
    let queue = |disabled: bool, guard: &str| {
        std::fs::write(
            root.join(".loom/config.json"),
            format!(r#"{{"planning":{{"preferHumanPrs":{}}}}}"#, !disabled),
        )
        .unwrap();
        let mut cmd = cli(root);
        cmd.args(["pr-queue", "--role", "judge"])
            .env("LOOM_GH_BIN", root.join("gh"))
            .env("LOOM_GH_NO_POLICY_LAUNCHER", "1")
            .env("GUARD_EXIT", guard);
        output(cmd)
    };
    let planned = queue(false, "0");
    assert_eq!(planned[0]["number"], 101);
    assert_eq!(planned[0]["mode"], "fallback");
    assert_eq!(planned.as_array().unwrap().len(), 101);
    assert_eq!(std::fs::read_to_string(root.join("guarded")).unwrap(), "101\n");
    assert_eq!(queue(true, "0").as_array().unwrap().len(), 100);
    for code in ["10", "11", "12"] {
        assert_eq!(queue(false, code).as_array().unwrap().len(), 100);
    }
}

#[test]
fn actions_capture_is_autonomous_even_under_interactive_environment() {
    let dir = tempfile::tempdir().unwrap();
    let out = cli(dir.path())
        .args(["provenance", "pr-marker"])
        .env("GITHUB_ACTIONS", "true")
        .env("LOOM_WORK_ORIGIN", "interactive")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("origin=autonomous"));
}

#[cfg(unix)]
#[test]
fn real_fallback_guard_preserves_velocity_alerts_and_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".loom/scripts")).unwrap();
    let guard = root.join(".loom/scripts/judge-fallback-guard.sh");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../defaults/scripts/judge-fallback-guard.sh"),
        &guard,
    )
    .unwrap();
    let mut pr = row(123, "interactive", "");
    pr["labels"] = serde_json::json!([]);
    std::fs::write(root.join("pulls.json"), serde_json::to_vec(&vec![pr]).unwrap()).unwrap();
    script(
        &root.join("gh"),
        r#"
case "$*" in
 *'api repos/{owner}/{repo}/pulls/'*) cat pr.json;;
 *pulls*) printf 'HTTP/2 200 OK\r\n\r\n'; cat pulls.json;;
 *comments*)
   if test "${FAIL_GUARD:-0}" = 1; then echo 'fixture forge failure' >&2; exit 1; fi
   cat comments.json;;
 *) echo 'unexpected forge call' >&2; exit 1;;
esac
"#,
    );
    let head = "a".repeat(40);
    let old_head = "b".repeat(40);
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let prepare = |cmd: &mut Command| {
        cmd.current_dir(root)
            .env("PATH", format!("{}:{}", root.display(), std::env::var("PATH").unwrap()))
            .env("LOOM_GH_BIN", root.join("gh"))
            .env("LOOM_GH_NO_POLICY_LAUNCHER", "1")
            // The guard authenticates marker authors through
            // `loom-daemon forge trusted-comments` (#9548/#9716); without a
            // reachable daemon it reads every marker as absent.
            .env("LOOM_DAEMON_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
            .env("FAIL_GUARD", "0");
    };
    // Exercise actual guard outcomes, including an empty queue with an alert.
    for (case, code, count, same_head, recent, bot) in [
        ("sha-alert", 12, 8, true, true, false),
        ("cap-alert", 11, 20, false, true, false),
        ("eligible-alert", 0, 8, false, true, false),
        ("eligible-quiet", 0, 0, false, true, false),
        ("sha-quiet", 12, 1, true, true, false),
        ("cap-quiet", 11, 20, false, false, false),
        ("bot-quiet", 10, 0, false, true, true),
    ] {
        std::fs::write(
            root.join("pr.json"),
            serde_json::to_vec(&serde_json::json!({
                // The guard's Step 1 is REST `pulls/<N>` (#9340), never GraphQL.
                "user":{"login":"operator","type":if bot { "Bot" } else { "User" }},
                "head":{"sha":head}
            }))
            .unwrap(),
        )
        .unwrap();
        // Markers in the REST shape the Judge's App actually posts them in:
        // a fleet-family `[bot]` login of type Bot, association NONE.
        let mut comments: Vec<_> = (0..count)
            .map(|_| serde_json::json!({
                "body":format!("<!-- loom:fallback-evaluated sha={} -->", if same_head { &head } else { &old_head }),
                "created_at":if recent { now.as_str() } else { "2020-01-01T00:00:00Z" },
                "user":{"login":"loom-fleet-dispatch[bot]","type":"Bot"},
                "author_association":"NONE"
            }))
            .collect();
        // An outsider's well-formed head-SHA marker is content, not control
        // (#9548/#9716): it must change neither the decision nor the counts.
        comments.push(serde_json::json!({
            "body":format!("<!-- loom:fallback-evaluated sha={head} -->"),
            "created_at":now,
            "user":{"login":"outsider","type":"User"},
            "author_association":"NONE"
        }));
        std::fs::write(root.join("comments.json"), serde_json::to_vec(&comments).unwrap()).unwrap();
        let mut direct = Command::new(&guard);
        prepare(&mut direct);
        let out = direct.arg("123").output().unwrap();
        assert_eq!(out.status.code(), Some(code), "{case}: {out:?}");
        let alert = recent && count >= 8;
        assert!(
            String::from_utf8_lossy(&out.stdout)
                .contains(&format!("VELOCITY_ALERT={}", u8::from(alert))),
            "{case}"
        );

        let mut queue = cli(root);
        prepare(&mut queue);
        let out = queue
            .args(["pr-queue", "--role", "judge"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{case}: {out:?}");
        let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), usize::from(code == 0), "{case}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        if alert {
            assert!(stderr.contains("PR #123"), "{case}: {stderr}");
            assert!(stderr.contains("VELOCITY_ALERT=1"), "{case}: {stderr}");
            assert!(stderr.contains(&format!("VELOCITY_COUNT={count}")), "{case}: {stderr}");
        } else {
            assert!(stderr.is_empty(), "{case}: {stderr}");
        }
    }

    // A failed guard remains an error, never a successful empty queue.
    std::fs::write(
        root.join("pr.json"),
        format!(r#"{{"user":{{"login":"operator","type":"User"}},"head":{{"sha":"{head}"}}}}"#),
    )
    .unwrap();
    let mut queue = cli(root);
    prepare(&mut queue);
    let out = queue
        .args(["pr-queue", "--role", "judge"])
        .env("FAIL_GUARD", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("Judge fallback guard failed for #123"), "{stderr}");
    assert!(stderr.contains("fixture forge failure"), "{stderr}");
}
