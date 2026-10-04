//! **Which top-level turn a claim belongs to** (#10129).
//!
//! A top-level session's transcript is the whole conversation, not one
//! agent's work. Two shapes of caller look alike from the transcript, and only
//! one of them can be scoped:
//!
//! - **A slash-command run.** A person types `/loom:builder 42`. That turn is
//!   the role's own work, and the next prompt ends it, so the run can own
//!   exactly the turn ([`super::segment`] ends it at that prompt).
//! - **An operator's main agent claiming inline.** One of its turns can last
//!   hours and span many issues, PR checks and other repos with no prompt in
//!   between. No boundary scopes that, so it stays refused.
//!
//! The difference is what opened the turn the claim sits in. This module reads
//! the transcript up to the claim line and reports whether the last prompt
//! before it was a `<command-name>/loom:<role></command-name>` line whose
//! arguments name the issue. It is a second condition on top of the binding
//! the caller already proved through its parent shells; neither alone starts a
//! run.

use std::io::{BufRead as _, BufReader, Read as _};
use std::path::Path;

use serde_json::Value;

use super::segment::starts_next_task;

/// `Ok(())` when the turn holding the claim line at byte `claim_from` of the
/// top-level transcript `path` was opened by a `/loom:<role>` command naming
/// `issue`; otherwise the reason it was not, for the one-line diagnostic.
///
/// # Errors
///
/// The reason the turn does not qualify. Unreadable transcripts do not
/// qualify either: a shorter feed, never a mislabeled one.
pub fn opened_by_role_command(path: &Path, claim_from: u64, issue: u32) -> Result<(), String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("transcript unreadable: {error}"))?;
    let mut lines = BufReader::new(file.take(claim_from));
    let mut opener: Option<Vec<u8>> = None;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match lines.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {
                if starts_next_task(&buffer) {
                    opener = Some(std::mem::take(&mut buffer));
                }
            }
            Err(error) => return Err(format!("transcript unreadable: {error}")),
        }
    }
    let Some(opener) = opener else {
        return Err("no prompt precedes the claim".to_string());
    };
    let text = prompt_text(&opener);
    let Some(role) = crate::activity::transcript_parse::slash_command_role(&text) else {
        return Err(
            "the turn holding the claim was not opened by a /loom:<role> command".to_string()
        );
    };
    if command_args(&text).is_some_and(|args| names_issue(args, issue)) {
        Ok(())
    } else {
        Err(format!(
            "the turn was opened by /loom:{role}, but its arguments do not name issue #{issue}"
        ))
    }
}

/// The text of a `user` line's message, whether it is a string or text blocks.
fn prompt_text(line: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return String::new();
    };
    match value.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The body of the command's `<command-args>` element.
fn command_args(text: &str) -> Option<&str> {
    let rest = &text[text.find("<command-args>")? + "<command-args>".len()..];
    Some(rest.split("</command-args>").next().unwrap_or(rest))
}

/// Whether `args` holds `issue` as a whole number (`42`, `#42`, or inside a
/// URL), never as part of a longer one.
fn names_issue(args: &str, issue: u32) -> bool {
    args.split(|c: char| !c.is_ascii_digit())
        .any(|digits| digits.parse::<u32>().ok() == Some(issue))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "user", "message": {"role": "user", "content": text}})
        )
    }

    fn assistant(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
        )
    }

    fn check(lines: &[String], issue: u32) -> Result<(), String> {
        let dir = std::env::temp_dir().join(format!(
            "loom-turn-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let body = lines.concat();
        std::fs::write(&path, &body).unwrap();
        let result = opened_by_role_command(&path, body.len() as u64, issue);
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    const COMMAND: &str =
        "<command-name>/loom:builder</command-name><command-args>#42</command-args>";

    #[test]
    fn a_command_naming_the_issue_opens_the_turn() {
        assert_eq!(check(&[user(COMMAND), assistant("on it")], 42), Ok(()));
    }

    #[test]
    fn a_command_for_another_issue_does_not() {
        assert!(check(&[user(COMMAND)], 4).is_err());
        assert!(check(&[user(COMMAND)], 420).is_err());
    }

    #[test]
    fn a_later_plain_prompt_replaces_the_command() {
        assert!(check(&[user(COMMAND), user("now check my other repo")], 42).is_err());
    }

    #[test]
    fn a_transcript_with_no_prompt_does_not_qualify() {
        assert!(check(&[assistant("hello")], 42).is_err());
    }
}
