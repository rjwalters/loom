//! Tests for the `guards.enabled` opt-out (issue #10335).
use super::*;
use tempfile::TempDir;

#[test]
fn test_remove_loom_guard_hooks_only_strips_guard_scripts() {
    // Issue #10335: guards.enabled:false strips the three guard hooks but keeps
    // other Loom hooks and all foreign hooks.
    let mut settings: serde_json::Value = serde_json::from_str(
        r#"{
            "hooks": {
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [
                        {"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"},
                        {"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-loom-workflow.sh"},
                        {"type": "command", "command": ".claude/hooks/custom-guard.sh"}
                    ]},
                    {"matcher": "Edit|Write", "hooks": [
                        {"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-worktree-paths.sh"}
                    ]}
                ],
                "UserPromptSubmit": [{"matcher": "", "hooks": [
                    {"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/skill-router.sh"}
                ]}]
            }
        }"#,
    )
    .unwrap();

    remove_loom_guard_hooks(&mut settings);

    let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 1, "empty Edit|Write matcher dropped");
    let bash = pre[0]["hooks"].as_array().unwrap();
    assert_eq!(bash.len(), 1);
    assert_eq!(bash[0]["command"], ".claude/hooks/custom-guard.sh");
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn test_apply_guard_opt_out_respects_config() {
    let temp = TempDir::new().unwrap();
    let ws = temp.path();
    fs::create_dir_all(ws.join(".claude")).unwrap();
    fs::create_dir_all(ws.join(".loom")).unwrap();
    let settings_path = ws.join(".claude/settings.json");
    let body = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"}]}]}}"#;

    // No opt-out: untouched.
    fs::write(&settings_path, body).unwrap();
    fs::write(ws.join(".loom/config.json"), "{}").unwrap();
    apply_guard_opt_out(ws, &settings_path);
    assert!(fs::read_to_string(&settings_path)
        .unwrap()
        .contains("guard-destructive.sh"));

    // Opt-out: guard hook stripped, and stays stripped on a second run.
    fs::write(ws.join(".loom/config.json"), r#"{"guards":{"enabled":false}}"#).unwrap();
    apply_guard_opt_out(ws, &settings_path);
    apply_guard_opt_out(ws, &settings_path);
    assert!(!fs::read_to_string(&settings_path)
        .unwrap()
        .contains("guard-destructive.sh"));
}
