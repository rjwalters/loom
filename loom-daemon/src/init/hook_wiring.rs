//! Project-level hook wiring for `.claude/settings.json` (#13793).
//!
//! The daemon resync used to write only payload files, so a hook script that
//! landed under `.loom/hooks/` stayed unwired in every fleet repo until an
//! operator ran `resync-installed.sh`. This module owns the same
//! `(event, matcher, script)` table as `scripts/install/provision-hooks.sh`
//! (a drift test compares the two) and adds the missing entries additively.
//!
//! Rules, identical to `ensure_project_hook_wiring`:
//! * only a hook whose script exists under `.loom/hooks/` counts;
//! * "wired" means some command under the same event contains
//!   `.loom/hooks/<name>` (and is not the machine-level `defaults/hooks/`
//!   wrapper). A Repo Skills `.claude/skills/repo/hooks/…` path never matches;
//! * a missing entry is appended as `"${CLAUDE_PROJECT_DIR}/.loom/hooks/<name>"`
//!   (quoted, #6544); nothing is removed or reordered;
//! * a file that is not a JSON object, or whose `hooks` shape is unexpected,
//!   is left untouched and reported.

use serde_json::{json, Map, Value};

/// `(event, matcher, script)`; mirrors `_PHOOK_TYPES/_MATCHERS/_NAMES`.
pub(crate) const HOOKS: &[(&str, &str, &str)] = &[
    ("PreToolUse", "Bash", "guard-destructive.sh"),
    ("PreToolUse", "Bash", "guard-loom-workflow.sh"),
    ("PreToolUse", "Edit|Write", "guard-worktree-paths.sh"),
    ("PreToolUse", "mcp__loom__.*", "guard-mcp-tools.sh"),
    ("UserPromptSubmit", "", "skill-router.sh"),
    ("UserPromptSubmit", "", "methodology-inject.sh"),
    ("Stop", "", "guard-background-subagents.sh"),
    ("SessionStart", "", "gh-front-env.sh"),
    ("PreToolUse", "*", "roll-pause.sh"),
    ("PostToolUse", "*", "roll-pause.sh"),
    ("PostToolUseFailure", "*", "roll-pause.sh"),
    ("Stop", "", "guard-uncommitted-work.sh"),
    ("SubagentStop", "", "guard-uncommitted-work.sh"),
];

/// The result of [`wire`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Wired {
    /// Every present hook is already wired.
    Unchanged,
    /// The new file text.
    Changed(String),
    /// Left as is: why.
    Unusable(String),
}

fn is_wired(entries: &[Value], name: &str) -> bool {
    let marker = format!(".loom/hooks/{name}");
    entries
        .iter()
        .filter_map(|e| e.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|h| h.get("command").and_then(Value::as_str))
        .any(|c| c.contains(&marker) && !c.contains("defaults/hooks/"))
}

/// Add the missing entries for every hook `present` reports to `text`.
pub(crate) fn wire(text: &str, present: impl Fn(&str) -> bool) -> Wired {
    let mut root: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return Wired::Unusable(format!("not valid JSON: {e}")),
    };
    let Some(obj) = root.as_object_mut() else {
        return Wired::Unusable("top level is not a JSON object".to_string());
    };
    let mut changed = false;
    for (event, matcher, name) in HOOKS {
        if !present(name) {
            continue;
        }
        let hooks = obj
            .entry("hooks")
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(hooks) = hooks.as_object_mut() else {
            return Wired::Unusable("`hooks` is not an object".to_string());
        };
        let entries = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(entries) = entries.as_array_mut() else {
            return Wired::Unusable(format!("`hooks.{event}` is not an array"));
        };
        if is_wired(entries, name) {
            continue;
        }
        let command = format!("\"${{CLAUDE_PROJECT_DIR}}/.loom/hooks/{name}\"");
        let new_hook = json!({"type": "command", "command": command});
        let slot = entries.iter_mut().find(|e| {
            e.get("matcher").and_then(Value::as_str).unwrap_or("") == *matcher
                && e.get("hooks").is_none_or(Value::is_array)
        });
        match slot {
            Some(entry) => {
                let list = entry
                    .as_object_mut()
                    .map(|o| o.entry("hooks").or_insert_with(|| Value::Array(Vec::new())))
                    .and_then(Value::as_array_mut);
                match list {
                    Some(list) => list.push(new_hook),
                    None => return Wired::Unusable(format!("`hooks.{event}` entry is malformed")),
                }
            }
            None => entries.push(json!({"matcher": matcher, "hooks": [new_hook]})),
        }
        changed = true;
    }
    if !changed {
        return Wired::Unchanged;
    }
    match serde_json::to_string_pretty(&root) {
        Ok(mut s) => {
            s.push('\n');
            Wired::Changed(s)
        }
        Err(e) => Wired::Unusable(format!("cannot serialize: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = include_str!("../../../scripts/install/provision-hooks.sh");

    fn all(_: &str) -> bool {
        true
    }

    fn words(block: &str) -> Vec<String> {
        block
            .split_whitespace()
            .map(|w| w.trim_matches('"').to_string())
            .collect()
    }

    /// Text between `NAME=(` and the closing `)`.
    fn array(name: &str) -> String {
        let start = SCRIPT.find(&format!("{name}=(")).expect(name) + name.len() + 2;
        SCRIPT[start..start + SCRIPT[start..].find(')').unwrap()].to_string()
    }

    #[test]
    fn table_matches_provision_hooks_sh() {
        let types = words(&array("_PHOOK_TYPES"));
        let names = words(&array("_PHOOK_NAMES"));
        // Matchers mix bare and quoted words, and may be empty strings.
        let mut matchers: Vec<String> = Vec::new();
        let (mut cur, mut quoted, mut open) = (String::new(), false, false);
        for c in array("_PHOOK_MATCHERS").chars() {
            match c {
                '"' => {
                    quoted = !quoted;
                    open = true;
                }
                c if c.is_whitespace() && !quoted => {
                    if open {
                        matchers.push(std::mem::take(&mut cur));
                        open = false;
                    }
                }
                c => {
                    cur.push(c);
                    open = true;
                }
            }
        }
        if open {
            matchers.push(cur);
        }
        let rust: Vec<_> = HOOKS.iter().collect();
        assert_eq!(types.len(), rust.len());
        assert_eq!(names.len(), rust.len());
        assert_eq!(matchers.len(), rust.len());
        for (i, (e, m, n)) in HOOKS.iter().enumerate() {
            assert_eq!((types[i].as_str(), matchers[i].as_str(), names[i].as_str()), (*e, *m, *n));
        }
    }

    #[test]
    fn wires_everything_into_repo_skills_only_settings_and_keeps_order() {
        let before = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[
            {"type":"command","command":".claude/skills/repo/hooks/guard-destructive.sh"}]}]}}"#;
        let Wired::Changed(out) = wire(before, all) else {
            panic!("expected change")
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let bash = &v["hooks"]["PreToolUse"][0]["hooks"];
        assert_eq!(bash[0]["command"], ".claude/skills/repo/hooks/guard-destructive.sh");
        assert_eq!(
            bash[1]["command"],
            "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh\""
        );
        let count: usize = v["hooks"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|e| e.as_array().unwrap())
            .map(|e| e["hooks"].as_array().unwrap().len())
            .sum();
        assert_eq!(count, 1 + HOOKS.len());
    }

    #[test]
    fn second_pass_is_a_no_op() {
        let Wired::Changed(out) = wire("{}", all) else {
            panic!()
        };
        assert_eq!(wire(&out, all), Wired::Unchanged);
    }

    #[test]
    fn absent_scripts_are_not_wired() {
        assert_eq!(wire("{}", |_| false), Wired::Unchanged);
        let Wired::Changed(out) = wire("{}", |n| n == "gh-front-env.sh") else {
            panic!()
        };
        assert!(out.contains("gh-front-env.sh") && !out.contains("skill-router.sh"));
    }

    #[test]
    fn unusable_files_are_left_alone() {
        for text in [
            "not json",
            "[]",
            r#"{"hooks":[]}"#,
            r#"{"hooks":{"Stop":{}}}"#,
            "",
        ] {
            assert!(matches!(wire(text, all), Wired::Unusable(_)), "{text}");
        }
    }
}
