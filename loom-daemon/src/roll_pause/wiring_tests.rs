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
    let v = parsed(&settings_args(Some("sweep-issue-7-a"), ws.path(), ws.path(), None, None));
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
    assert!(settings_args(None, ws.path(), ws.path(), None, None).is_empty());
    assert!(settings_args(Some("../escape"), ws.path(), ws.path(), None, None).is_empty());
}

#[test]
fn nothing_is_injected_when_no_hook_is_installed() {
    let ws = tempfile::tempdir().unwrap();
    assert!(settings_args(Some("item-1"), ws.path(), ws.path(), None, None).is_empty());
    let missing = ws.path().join("nope.sh");
    assert!(settings_args(Some("item-1"), ws.path(), ws.path(), None, Some(&missing)).is_empty());
}

/// Hook entries running `roll-pause.sh` on `events`, all with `matcher`.
fn wiring(events: &[&str], matcher: &str) -> serde_json::Value {
    let entry = serde_json::json!([{"matcher": matcher, "hooks": [
        {"type": "command", "command": "bash -c 'exec bash \"$L\" roll-pause.sh'"}]}]);
    let hooks: serde_json::Map<String, serde_json::Value> = events
        .iter()
        .map(|e| ((*e).to_string(), entry.clone()))
        .collect();
    serde_json::json!({ "hooks": hooks })
}

fn injects(ws: &Path, user: Option<&Path>) -> bool {
    !settings_args(Some("item-1"), ws, ws, user, None).is_empty()
}

#[test]
fn full_match_all_coverage_in_the_project_settings_is_left_alone() {
    let ws = ws_with_hook("defaults/hooks/roll-pause.sh");
    for matcher in ["*", "", ".*"] {
        write_settings(ws.path(), ".claude/settings.json", &wiring(&EVENTS, matcher));
        assert!(!injects(ws.path(), None), "matcher {matcher:?}");
    }
    // An absent matcher is match-all too.
    let mut v = wiring(&EVENTS, "*");
    for e in EVENTS {
        v["hooks"][e][0].as_object_mut().unwrap().remove("matcher");
    }
    write_settings(ws.path(), ".claude/settings.json", &v);
    assert!(!injects(ws.path(), None));
}

#[test]
fn the_loom_repos_own_settings_count_as_fully_wired() {
    let real = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.claude/settings.json");
    let body: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
    let ws = ws_with_hook("defaults/hooks/roll-pause.sh");
    write_settings(ws.path(), ".claude/settings.json", &body);
    assert!(!injects(ws.path(), None), "{}", real.display());
}

#[test]
fn a_registration_missing_an_event_still_gets_the_injection() {
    let ws = ws_with_hook("defaults/hooks/roll-pause.sh");
    // PostToolUse only: no pre-tool pause hook at all.
    write_settings(ws.path(), ".claude/settings.json", &wiring(&["PostToolUse"], "*"));
    assert!(injects(ws.path(), None));
    // PreToolUse only: the ledger never closes.
    write_settings(ws.path(), ".claude/settings.json", &wiring(&["PreToolUse"], "*"));
    assert!(injects(ws.path(), None));
    // Pre and Post but no PostToolUseFailure: a failed call stays in the ledger.
    write_settings(
        ws.path(),
        ".claude/settings.json",
        &wiring(&["PreToolUse", "PostToolUse"], "*"),
    );
    assert!(injects(ws.path(), None));
    // The same in settings.local.json.
    std::fs::remove_file(ws.path().join(".claude/settings.json")).unwrap();
    write_settings(ws.path(), ".claude/settings.local.json", &wiring(&["PostToolUse"], "*"));
    assert!(injects(ws.path(), None));
}

#[test]
fn a_narrow_matcher_still_gets_the_injection() {
    let ws = ws_with_hook("defaults/hooks/roll-pause.sh");
    for matcher in ["Bash", "Edit|Write", "mcp__.*"] {
        write_settings(ws.path(), ".claude/settings.json", &wiring(&EVENTS, matcher));
        assert!(injects(ws.path(), None), "matcher {matcher:?}");
    }
    // Narrow for one event only is still a gap.
    let mut v = wiring(&EVENTS, "*");
    v["hooks"]["PreToolUse"][0]["matcher"] = "Bash".into();
    write_settings(ws.path(), ".claude/settings.json", &v);
    assert!(injects(ws.path(), None));
}

#[test]
fn a_mention_outside_a_hook_command_is_not_wiring() {
    let ws = ws_with_hook("defaults/hooks/roll-pause.sh");
    write_settings(
        ws.path(),
        ".claude/settings.json",
        &serde_json::json!({"permissions": {"allow": ["Bash(.loom/hooks/roll-pause.sh)"]}}),
    );
    assert!(injects(ws.path(), None));
    // Full coverage by a different hook script is not this hook.
    let mut v = wiring(&EVENTS, "*");
    for e in EVENTS {
        v["hooks"][e][0]["hooks"][0]["command"] = "x/guard-destructive.sh".into();
    }
    write_settings(ws.path(), ".claude/settings.json", &v);
    assert!(injects(ws.path(), None));
}

#[test]
fn coverage_merges_across_the_project_files_and_the_user_settings() {
    let ws = ws_with_hook("defaults/hooks/roll-pause.sh");
    let user = tempfile::tempdir().unwrap();
    // Split across settings.json and settings.local.json: complete.
    write_settings(ws.path(), ".claude/settings.json", &wiring(&["PreToolUse"], "*"));
    write_settings(
        ws.path(),
        ".claude/settings.local.json",
        &wiring(&["PostToolUse", "PostToolUseFailure"], ""),
    );
    assert!(!injects(ws.path(), None));
    // Project covers only PreToolUse; the user settings cover the rest.
    std::fs::remove_file(ws.path().join(".claude/settings.local.json")).unwrap();
    assert!(injects(ws.path(), Some(user.path())));
    write_settings(
        user.path(),
        "settings.json",
        &wiring(&["PostToolUse", "PostToolUseFailure"], "*"),
    );
    assert!(!injects(ws.path(), Some(user.path())));
    // User settings alone, fully wired, suppress too.
    std::fs::remove_file(ws.path().join(".claude/settings.json")).unwrap();
    write_settings(user.path(), "settings.json", &wiring(&EVENTS, "*"));
    assert!(!injects(ws.path(), Some(user.path())));
    // ...but a narrow user matcher does not.
    write_settings(user.path(), "settings.json", &wiring(&EVENTS, "Bash"));
    assert!(injects(ws.path(), Some(user.path())));
}

#[test]
fn the_source_tree_hook_is_the_fallback_and_an_override_wins() {
    let ws = ws_with_hook("defaults/hooks/roll-pause.sh");
    let v = parsed(&settings_args(Some("item-1"), ws.path(), ws.path(), None, None));
    let cmd = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(cmd.ends_with("/defaults/hooks/roll-pause.sh'"), "{cmd}");

    let other = ws_with_hook("x/roll-pause.sh");
    let o = other.path().join("x/roll-pause.sh");
    let v = parsed(&settings_args(Some("item-1"), ws.path(), ws.path(), None, Some(&o)));
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
