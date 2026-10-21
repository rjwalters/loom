//! Shipped prompts and docs must not trip Claude Code's rate-limit hint (#11411).
//!
//! Claude Code's shell tool appends a "sleep until reset" reminder to any
//! result whose command calls `gh` and whose output matches a GitHub
//! rate-limit phrase, whatever printed it. An agent that prints its own role
//! file in the same command as a `gh` call is then told, falsely, that the
//! quota is spent. The patterns below mirror that matcher (case-insensitive).
//! Signature tables spell the phrases with `·` for a space instead; the code
//! that classifies real rate limits keeps the literal phrases.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

/// Phrases the hint matches anywhere in printed text. `[ ]` keeps this source
/// file from matching itself.
const PHRASE_PATTERN: &str =
    r"(?i)API rate limit (already )?exceeded|exceeded a secondary rate[ ]limit";

/// The bare word the hint also matches. Checked only on prompt surfaces: the
/// docs use the same word in lower case as a real status name.
const WORD_PATTERN: &str = r"(?i)\bRATE_LIMITED\b";

/// Agent-read surfaces under `defaults/` (role prompts, slash commands,
/// generated skills).
const PROMPT_DIRS: &[&str] = &["roles", ".claude", ".agents"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon has a parent directory")
        .to_path_buf()
}

fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if path.file_name().is_some_and(|name| name == "scripts") {
                continue;
            }
            markdown_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
}

fn offending_lines(path: &Path, re: &Regex) -> Vec<String> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.lines()
        .enumerate()
        .filter(|(_, line)| re.is_match(line))
        .map(|(n, line)| format!("{}:{}: {}", path.display(), n + 1, line.trim()))
        .collect()
}

#[test]
fn shipped_markdown_has_no_rate_limit_hint_trigger() {
    let root = repo_root();
    let defaults = root.join("defaults");
    let mut files = vec![root.join("CLAUDE.md"), root.join("AGENTS.md")];
    markdown_files(&defaults, &mut files);
    assert!(
        files.iter().any(|p| p.ends_with("judge.md")),
        "the walk found no judge.md under {}; the guard is checking nothing",
        defaults.display()
    );

    let phrase = Regex::new(PHRASE_PATTERN).unwrap();
    let word = Regex::new(WORD_PATTERN).unwrap();
    let mut hits = Vec::new();
    for file in &files {
        hits.extend(offending_lines(file, &phrase));
        let rel = file.strip_prefix(&defaults).unwrap_or(file);
        if PROMPT_DIRS.iter().any(|dir| rel.starts_with(dir)) {
            hits.extend(offending_lines(file, &word));
        }
    }

    assert!(
        hits.is_empty(),
        "these lines would make Claude Code tell an agent that printed them in a \
         `gh` command to sleep for a rate-limit reset (#11411). Spell the \
         signature with `·` for the space, as judge.md's table does:\n{}",
        hits.join("\n")
    );
}

#[test]
fn patterns_match_the_real_messages() {
    let phrase = Regex::new(PHRASE_PATTERN).unwrap();
    let word = Regex::new(WORD_PATTERN).unwrap();
    for joined in [
        "api rate limit·exceeded",
        "API rate limit·already exceeded for user ID 1",
    ] {
        let real = joined.replace('·', " ");
        assert!(phrase.is_match(&real), "{real}");
        assert!(!phrase.is_match(joined), "{joined}");
    }
    assert!(phrase.is_match(&"You have exceeded a secondary rate·limit".replace('·', " ")));
    assert!(word.is_match(&"outcome=rate·limited".replace('·', "_")));
    assert!(!word.is_match("rate_limited_count"));
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
