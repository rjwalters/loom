//! The launch-injected pause hook wiring (#11049).

use super::*;

fn ws_with_hook(rel: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let hook = tmp.path().join(rel);
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/usr/bin/env bash\n").unwrap();
    tmp
}

fn write_settings(dir: &Path, rel: &str, body: &serde_json::Value) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body.to_string()).unwrap();
}

fn parsed(args: &[String]) -> serde_json::Value {
    assert_eq!(args.len(), 2, "{args:?}");
    assert_eq!(args[0], "--settings");
    serde_json::from_str(&args[1]).unwrap()
}

#[test]
fn a_consumer_install_gets_all_three_events_wired_to_its_installed_hook() {
    let ws = ws_with_hook(".loom/hooks/roll-pause.sh");
    // A consumer's own settings: guards on Bash only, nothing for the pause hook.
    write_settings(
        ws.path(),
        ".claude/settings.json",
        &serde_json::json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "command", "command": "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh\""}]}]}}),
    );
    let v = parsed(&settings_args(Some("sweep-issue-7-a"), ws.path(), ws.path(), None));
    let hook = ws.path().join(".loom/hooks/roll-pause.sh");
    for event in EVENTS {
        let entries = v["hooks"][event].as_array().unwrap();
        assert_eq!(entries.len(), 1, "{event}");
        assert_eq!(entries[0]["matcher"], "*", "{event}: every tool, not only Bash");
        let cmd = entries[0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(cmd, format!("exec bash '{}'", hook.display()), "{event}");
    }
}

#[test]
fn nothing_is_injected_for_an_attended_launch_or_a_bad_item_id() {
    let ws = ws_with_hook(".loom/hooks/roll-pause.sh");
    assert!(settings_args(None, ws.path(), ws.path(), None).is_empty());
    assert!(settings_args(Some("../escape"), ws.path(), ws.path(), None).is_empty());
}

#[test]
fn nothing_is_injected_when_no_hook_is_installed() {
    let ws = tempfile::tempdir().unwrap();
    assert!(settings_args(Some("item-1"), ws.path(), ws.path(), None).is_empty());
    let missing = ws.path().join("nope.sh");
    assert!(settings_args(Some("item-1"), ws.path(), ws.path(), Some(&missing)).is_empty());
}

#[test]
fn a_project_that_already_wires_the_hook_is_left_alone() {
    // The Loom repo's own settings run roll-pause.sh through hook-wiring.sh.
    let ws = ws_with_hook("defaults/hooks/roll-pause.sh");
    write_settings(
        ws.path(),
        ".claude/settings.json",
        &serde_json::json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
            {"type": "command", "command": "bash -c 'exec bash \"$L\" PreToolUse roll-pause.sh'"}]}]}}),
    );
    assert!(settings_args(Some("item-1"), ws.path(), ws.path(), None).is_empty());
    // A mention outside `hooks` (a permission rule, say) is not wiring.
    write_settings(
        ws.path(),
        ".claude/settings.json",
        &serde_json::json!({"permissions": {"allow": ["Bash(.loom/hooks/roll-pause.sh)"]}}),
    );
    assert_eq!(settings_args(Some("item-1"), ws.path(), ws.path(), None).len(), 2);
    // settings.local.json counts too.
    write_settings(
        ws.path(),
        ".claude/settings.local.json",
        &serde_json::json!({"hooks": {"PostToolUse": [{"matcher": "*", "hooks": [
            {"type": "command", "command": "x/roll-pause.sh"}]}]}}),
    );
    assert!(settings_args(Some("item-1"), ws.path(), ws.path(), None).is_empty());
}

#[test]
fn the_source_tree_hook_is_the_fallback_and_an_override_wins() {
    let ws = ws_with_hook("defaults/hooks/roll-pause.sh");
    let v = parsed(&settings_args(Some("item-1"), ws.path(), ws.path(), None));
    let cmd = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(cmd.ends_with("/defaults/hooks/roll-pause.sh'"), "{cmd}");

    let other = ws_with_hook("x/roll-pause.sh");
    let o = other.path().join("x/roll-pause.sh");
    let v = parsed(&settings_args(Some("item-1"), ws.path(), ws.path(), Some(&o)));
    let cmd = v["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert_eq!(cmd, format!("exec bash '{}'", o.display()));
}

#[test]
fn a_hook_path_with_a_quote_stays_one_shell_word() {
    let json = settings_json(Path::new("/tmp/it's here/roll-pause.sh"));
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let cmd = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert_eq!(cmd, r"exec bash '/tmp/it'\''s here/roll-pause.sh'");
}
