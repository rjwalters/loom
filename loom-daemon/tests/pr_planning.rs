//! CLI contract: use the real queue policy with forge-shaped offline input.
use std::process::Command;

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
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env("LOOM_WORK_ORIGIN", origin)
            .env_remove("GITHUB_ACTIONS")
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
