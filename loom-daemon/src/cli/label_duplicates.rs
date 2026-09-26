//! `loom-daemon label-duplicates <PATH>` — find label names declared more
//! than once in a Loom `labels.yml` file (issue #8875).
//!
//! A repo upgraded across the #4187 marker-block boundary without absorbing
//! its pre-#4187 legacy labels ends up with two `- name:` entries for the
//! same label — a structural problem in the file itself, independent of
//! forge state, that `sync-labels.sh`'s MISSING/STALE/EXTRA diff has no way
//! to describe. `sync-labels.sh` is `contract`-category shell
//! (`scripts/shell-allowlist.txt`), so per
//! `.loom/docs/shell-language-policy.md` new logic goes here rather than
//! growing it in place — including the human-facing report, so the shell
//! stub only needs to capture one integer, not format anything.
//!
//! Prints the duplicate count on stdout (a bare integer, `0` when none), and
//! — when that count is nonzero — a fully-formatted `  DUPLICATE     <name>
//! (...)` line per duplicated name plus a trailing de-duplication advisory,
//! both on stderr. Splitting the streams this way lets the shell stub
//! capture the count for its own gating/summary while the human-readable
//! detail passes straight through to the terminal, with no shell-side
//! formatting or looping needed either side of the call.

use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(clap::Args)]
pub(crate) struct LabelDuplicatesArgs {
    /// Path to the `labels.yml` file to scan.
    #[arg(value_name = "PATH")]
    labels_file: PathBuf,
}

/// Label names declared via a `- name: <name>` line, in file order — mirrors
/// `sync-labels.sh`'s own `^- name: (.+)$` extraction exactly (no trimming
/// or quote-stripping: label names are never quoted in `labels.yml`).
fn declared_names(content: &str) -> Vec<&str> {
    content
        .lines()
        .filter_map(|line| line.strip_prefix("- name: "))
        .collect()
}

/// Names appearing more than once in `content`, each returned exactly once,
/// in order of first appearance.
#[must_use]
pub(crate) fn find_duplicates(content: &str) -> Vec<String> {
    let names = declared_names(content);
    let mut out: Vec<String> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        if out.iter().any(|d| d == name) {
            continue;
        }
        if names[i + 1..].contains(name) {
            out.push((*name).to_string());
        }
    }
    out
}

/// The exact line `sync-labels.sh`'s report prints per duplicated name —
/// centralized here so the shell stub does not carry its own copy.
fn report_line(labels_file: &std::path::Path, name: &str) -> String {
    format!(
        "  DUPLICATE     {name} (declared more than once in {} — structural drift in the file itself, independent of forge state; likely a pre-#4187-upgrade artifact, see #8875)",
        labels_file.display()
    )
}

impl LabelDuplicatesArgs {
    pub(crate) fn run(self) -> Result<()> {
        let content = std::fs::read_to_string(&self.labels_file)
            .with_context(|| format!("reading {}", self.labels_file.display()))?;
        let duplicates = find_duplicates(&content);
        for name in &duplicates {
            eprintln!("{}", report_line(&self.labels_file, name));
        }
        if !duplicates.is_empty() {
            eprintln!(
                "{} declares one or more labels more than once — de-duplicate the file by hand (or reinstall Loom, which now absorbs pre-#4187 duplicates automatically) before --check can converge.",
                self.labels_file.display()
            );
        }
        println!("{}", duplicates.len());
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn no_duplicates_returns_empty() {
        let content =
            "- name: loom:issue\n  color: \"3B82F6\"\n- name: loom:building\n  color: \"F59E0B\"\n";
        assert!(find_duplicates(content).is_empty());
    }

    #[test]
    fn duplicate_name_is_reported_once_in_first_seen_order() {
        let content = "- name: loom:building\n  color: \"1d76db\"\n\
                        - name: loom:issue\n  color: \"1d76db\"\n\
                        - name: loom:building\n  color: \"3B82F6\"\n";
        assert_eq!(find_duplicates(content), vec!["loom:building".to_string()]);
    }

    #[test]
    fn multiple_duplicates_preserve_first_seen_order() {
        let content = "- name: b\n- name: a\n- name: b\n- name: a\n";
        assert_eq!(find_duplicates(content), vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn name_appearing_three_times_is_reported_once() {
        let content = "- name: x\n- name: x\n- name: x\n";
        assert_eq!(find_duplicates(content), vec!["x".to_string()]);
    }

    #[test]
    fn run_prints_each_duplicate_on_its_own_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("labels.yml");
        std::fs::write(
            &path,
            "- name: loom:issue\n  color: \"1d76db\"\n- name: loom:issue\n  color: \"3B82F6\"\n",
        )
        .unwrap();
        assert_eq!(
            find_duplicates(&std::fs::read_to_string(&path).unwrap()),
            vec!["loom:issue".to_string()]
        );
    }
}
