//! Format-preserving edit of `repos.yml` for `propose priority`.
//!
//! Finds the `repos[]` record whose `name:` is the requested repo and
//! sets/inserts its `fleet_priority:` line only — every other line in the
//! record (including its `fleet`/`firewall` flags, which this command must
//! never touch) and every other record are left byte-for-byte alone.

use anyhow::{anyhow, Result};

use super::block::{self, Block};

/// Edit `text` (the current `repos.yml`) to set `repo`'s `fleet_priority`.
pub fn edit(text: &str, repo: &str, priority: u32) -> Result<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let trailing_newline = text.is_empty() || text.ends_with('\n');

    let repos = block::find_block(&lines, 0, lines.len(), 0, "repos")
        .ok_or_else(|| anyhow!("repos.yml has no top-level `repos:` list"))?;
    let dash_indent = repos.child_indent;

    let starts: Vec<usize> = (repos.start..repos.end)
        .filter(|&i| {
            !block::is_blank_or_comment(&lines[i])
                && block::indent_of(&lines[i]) == dash_indent
                && lines[i].trim_start().starts_with("- ")
        })
        .collect();

    let name_needle = [
        format!("name: {repo}"),
        format!("name: \"{repo}\""),
        format!("name: '{repo}'"),
    ];
    let mut found = None;
    for (idx, &start) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).copied().unwrap_or(repos.end);
        let has_name = (start..end).any(|i| {
            let content = if i == start {
                lines[i]
                    .trim_start()
                    .trim_start_matches("- ")
                    .trim()
                    .to_string()
            } else {
                lines[i].trim().to_string()
            };
            name_needle.contains(&content)
        });
        if has_name {
            found = Some((start, end));
            break;
        }
    }
    let (start, end) =
        found.ok_or_else(|| anyhow!("repos.yml has no repos[] record named `{repo}`"))?;

    let mut item = Block {
        start,
        end,
        child_indent: dash_indent + 2,
    };
    block::set_scalar(&mut lines, &mut item, "fleet_priority", &priority.to_string());

    let mut out = lines.join("\n");
    if trailing_newline && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
#[path = "tests/priority_edit_tests.rs"]
mod tests;
