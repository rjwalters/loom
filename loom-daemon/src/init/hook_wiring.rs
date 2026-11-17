//! Project-level hook wiring for `.claude/settings.json` (#13793).
//!
//! The daemon resync used to write only payload files, so a hook script that
//! landed under `.loom/hooks/` stayed unwired in every fleet repo until an
//! operator ran `resync-installed.sh`. This module owns the same
//! `(event, matcher, script, timeout_s)` table as
//! `scripts/install/provision-hooks.sh` (a drift test compares the two) and
//! adds the missing entries additively.
//!
//! Rules, identical to `ensure_project_hook_wiring`:
//! * only a hook whose script exists under `.loom/hooks/` counts;
//! * "wired" means some command under the same event contains
//!   `.loom/hooks/<name>` (and is not the machine-level `defaults/hooks/`
//!   wrapper). A Repo Skills `.claude/skills/repo/hooks/…` path never matches;
//! * a missing entry is appended as `"${CLAUDE_PROJECT_DIR}/.loom/hooks/<name>"`
//!   (quoted, #6544); nothing is removed or reordered;
//! * a timeout-bearing hook converges an already-wired entry to the table's
//!   `timeout_s` in place (#13959) instead of leaving pre-timeout wiring
//!   unbounded — only the exact command this module emits counts as
//!   Loom-authored, so a hand-written variant is never touched;
//! * a file that is not a JSON object, or whose `hooks` shape is unexpected,
//!   is left untouched and reported.

use serde_json::{json, Map, Value};

/// `(event, matcher, script, timeout_s)`; mirrors
/// `_PHOOK_TYPES/_MATCHERS/_NAMES/_TIMEOUTS`. `timeout_s` is the Claude Code
/// hook `timeout` (#13959): the seconds after which the client kills a hung
/// hook instead of letting it stall session start. `None` emits no field.
/// Only the SessionStart gh front carries one today: its worst measured step
/// is ~0.1 s (`gh-shim session-env`), the unresolved worst case (a binary
/// lookup that falls through to `cargo metadata`, a cold binary start under
/// load) is seconds, and the reported stall was 30–50 s — 10 s bounds the
/// pathology with two orders of magnitude of headroom over anything healthy.
/// The hook is fail-open and stdout-silent, so a killed run costs only the
/// PATH line, the same degradation as any other resolution failure.
pub(crate) const HOOKS: &[(&str, &str, &str, Option<u64>)] = &[
    ("PreToolUse", "Bash", "guard-destructive.sh", None),
    ("PreToolUse", "Bash", "guard-loom-workflow.sh", None),
    ("PreToolUse", "Edit|Write", "guard-worktree-paths.sh", None),
    ("PreToolUse", "mcp__loom__.*", "guard-mcp-tools.sh", None),
    ("UserPromptSubmit", "", "skill-router.sh", None),
    ("UserPromptSubmit", "", "methodology-inject.sh", None),
    ("Stop", "", "guard-background-subagents.sh", None),
    ("SessionStart", "", "gh-front-env.sh", Some(10)),
    ("PreToolUse", "*", "roll-pause.sh", None),
    ("PostToolUse", "*", "roll-pause.sh", None),
    ("PostToolUseFailure", "*", "roll-pause.sh", None),
    ("Stop", "", "guard-uncommitted-work.sh", None),
    ("SubagentStop", "", "guard-uncommitted-work.sh", None),
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

/// Converge an already-wired Loom-authored entry for `name` to `timeout`
/// (#13959). Returns whether anything changed. Only a command byte-identical
/// to the one this module emits counts as Loom-authored — a hand-written
/// variant that merely references the same script is left alone.
fn converge_timeout(entries: &mut [Value], name: &str, timeout: u64) -> bool {
    let ours = format!("\"${{CLAUDE_PROJECT_DIR}}/.loom/hooks/{name}\"");
    let mut changed = false;
    for hook in entries
        .iter_mut()
        .filter_map(|e| e.get_mut("hooks").and_then(Value::as_array_mut))
        .flatten()
    {
        let is_ours = hook.get("command").and_then(Value::as_str) == Some(ours.as_str());
        if is_ours && hook.get("timeout").and_then(Value::as_u64) != Some(timeout) {
            if let Some(obj) = hook.as_object_mut() {
                obj.insert("timeout".to_string(), json!(timeout));
                changed = true;
            }
        }
    }
    changed
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
    for (event, matcher, name, timeout) in HOOKS {
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
            if let Some(t) = timeout {
                changed |= converge_timeout(entries, name, *t);
            }
            continue;
        }
        let command = format!("\"${{CLAUDE_PROJECT_DIR}}/.loom/hooks/{name}\"");
        let new_hook = match timeout {
            Some(t) => json!({"type": "command", "command": command, "timeout": t}),
            None => json!({"type": "command", "command": command}),
        };
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

    /// Words of an array body, quote-aware: a mix of bare words (`Bash`) and
    /// quoted words, including quoted EMPTY strings (`""`).
    fn quoted_words(block: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let (mut cur, mut quoted, mut open) = (String::new(), false, false);
        for c in block.chars() {
            match c {
                '"' => {
                    quoted = !quoted;
                    open = true;
                }
                c if c.is_whitespace() && !quoted => {
                    if open {
                        out.push(std::mem::take(&mut cur));
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
            out.push(cur);
        }
        out
    }

    #[test]
    fn table_matches_provision_hooks_sh() {
        let types = words(&array("_PHOOK_TYPES"));
        let names = words(&array("_PHOOK_NAMES"));
        // Matchers and timeouts mix bare and quoted words, and may be empty.
        let matchers = quoted_words(&array("_PHOOK_MATCHERS"));
        let timeouts = quoted_words(&array("_PHOOK_TIMEOUTS"));
        let rust: Vec<_> = HOOKS.iter().collect();
        assert_eq!(types.len(), rust.len());
        assert_eq!(names.len(), rust.len());
        assert_eq!(matchers.len(), rust.len());
        assert_eq!(timeouts.len(), rust.len());
        for (i, (e, m, n, t)) in HOOKS.iter().enumerate() {
            assert_eq!((types[i].as_str(), matchers[i].as_str(), names[i].as_str()), (*e, *m, *n));
            assert_eq!(timeouts[i].parse::<u64>().ok(), *t, "timeout of {} at index {i}", names[i]);
        }
    }

    /// #13959: the SessionStart gh front is wired with the table's `timeout`;
    /// hooks whose table entry has none get no field at all.
    #[test]
    fn sessionstart_entry_carries_timeout_and_others_do_not() {
        let Wired::Changed(out) = wire("{}", all) else {
            panic!("expected change")
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let session: Vec<_> = v["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|e| e["hooks"].as_array().unwrap())
            .collect();
        assert_eq!(session.len(), 1);
        assert_eq!(session[0]["command"], "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/gh-front-env.sh\"");
        assert_eq!(session[0]["timeout"], 10);
        let bash: Vec<_> = v["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|e| e["hooks"].as_array().unwrap())
            .collect();
        assert!(bash.iter().all(|h| h.get("timeout").is_none()));
    }

    /// #13959: pre-timeout Loom wiring converges to the table's timeout in
    /// place (no duplicate), a hand-written entry referencing the same script
    /// is left alone, and an already-converged file is byte-stable.
    #[test]
    fn wired_entry_converges_to_timeout_without_touching_hand_written_entries() {
        let before = r#"{"hooks":{"SessionStart":[
            {"matcher":"","hooks":[
              {"type":"command","command":"\"${CLAUDE_PROJECT_DIR}/.loom/hooks/gh-front-env.sh\""},
              {"type":"command","command":"bash .loom/hooks/gh-front-env.sh"}]}]}}"#;
        let Wired::Changed(out) = wire(before, |n| n == "gh-front-env.sh") else {
            panic!("expected change")
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let list = v["hooks"]["SessionStart"][0]["hooks"].as_array().unwrap();
        assert_eq!(list.len(), 2, "no duplicate appended");
        assert_eq!(list[0]["timeout"], 10, "Loom-authored entry gained the timeout");
        assert_eq!(list[0]["command"], "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/gh-front-env.sh\"");
        assert!(list[1].get("timeout").is_none(), "hand-written entry untouched");
        // A second pass on the converged file changes nothing (byte-stable).
        assert_eq!(wire(&out, |n| n == "gh-front-env.sh"), Wired::Unchanged);
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
