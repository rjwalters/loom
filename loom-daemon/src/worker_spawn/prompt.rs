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
    let name = if canonical == "sweep-lifecycle" {
        "sweep"
    } else {
        canonical
    };
    let installed = root.join(".loom/roles").join(format!("{name}.md"));
    let path = if installed.is_file() {
        installed
    } else {
        root.join("defaults/roles").join(format!("{name}.md"))
    };
    let role_text = std::fs::read_to_string(&path).map_err(|e| {
        LaunchError::config(format!("cannot read role instructions {}: {e}", path.display()))
    })?;
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
    Ok(expanded)
}
