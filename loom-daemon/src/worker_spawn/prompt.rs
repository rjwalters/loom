//! Expand Loom's role invocation without relying on a Claude slash-command parser.
use super::LaunchError;
use std::path::Path;

pub fn role_invocation(prompt: &str) -> Option<(&str, &str)> {
    let rest = prompt.strip_prefix("/loom:")?;
    let (role, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some((role, arguments.trim()))
}

pub fn expand(root: &Path, prompt: &str) -> Result<String, LaunchError> {
    let Some((role, arguments)) = role_invocation(prompt) else {
        return Ok(prompt.to_string());
    };
    let canonical = crate::runtime_admission::canonical_role(role)
        .ok_or_else(|| LaunchError::config("unknown Loom role in prompt"))?;
    let (path, role_text) = if canonical == "sweep-lifecycle" {
        (
            std::path::PathBuf::from("bundled native-sweep.md"),
            include_str!("../../../defaults/docs/native-sweep.md").to_string(),
        )
    } else {
        let installed = root.join(".loom/roles").join(format!("{canonical}.md"));
        let path = if installed.is_file() {
            installed
        } else {
            root.join("defaults/roles").join(format!("{canonical}.md"))
        };
        let text = std::fs::read_to_string(&path).map_err(|e| {
            LaunchError::config(format!("cannot read role instructions {}: {e}", path.display()))
        })?;
        (path, text)
    };
    let mut expanded = format!(
        "Loom role instructions (source: {}):\n{}",
        path.display(),
        role_text.replace("$ARGUMENTS", arguments)
    );
    // Both harnesses discover repository rules, but explicit inclusion also covers
    // launches from worktrees whose primary checkout owns the installed role files.
    for file in ["AGENTS.md", "CLAUDE.md"] {
        let path = root.join(file);
        if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| LaunchError::config(format!("cannot read {}: {e}", path.display())))?;
            expanded
                .push_str(&format!("\n\nRepository instructions ({}):\n{text}", path.display()));
        }
    }
    expanded.push_str("\n\nNative runtime: use loom_read/loom_edit/loom_write/loom_bash and Loom's CLI helpers. Do not invoke model workers or unavailable MCP/Task tools. Execute this role in the current session. Guard denials are failures, not approval prompts.\n");
    Ok(expanded)
}
