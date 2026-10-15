//! The Dependabot-side `loom:review-requested` routing contract (#7577),
//! behind `defaults/scripts/check-dependabot-labels.sh`.
//!
//! # Why it exists
//!
//! `labels:` in `.github/dependabot.yml` is scoped to the ONE `updates:` entry
//! it appears in. Dependabot matches every PR it opens (version updates *and*
//! alert-driven security updates) against the entry whose `package-ecosystem`
//! plus directory contain the manifest being patched. A manifest with no
//! matching entry gets Dependabot's DEFAULT labels, never
//! `loom:review-requested`, and sits in the Judge's unlabeled fallback queue,
//! which cannot clear it (the #5455 livelock; #7474/#7473/#7396/#7395/#7138/
//! #7137/#6890 sat inert for weeks inside nested npm packages).
//!
//! # What it asserts
//!
//! 1. Every `updates:` entry, for every directory it names, lists
//!    [`REQUIRED_LABEL`] in its `labels:`.
//! 2. Every tracked npm manifest with a committed lockfile beside it is covered
//!    by an npm entry for its directory, unless it declares no dependencies of
//!    any kind AND is not a workspace root (no `workspaces` key, no
//!    `pnpm-workspace.yaml` beside it). Such a manifest has nothing for an
//!    entry to route; it is reported as a note. A workspace root is never
//!    exempt: its lockfile carries the members' dependencies.
//! 3. Every npm entry points at a directory that has a `package.json` (catches
//!    an entry left behind by a moved package). Glob directories are not
//!    expanded, so a glob npm entry fails here rather than being trusted.
//! 4. Advisory only: a manifest that declares dependencies, has no lockfile and
//!    no entry (a scaffold such as `quickstarts/`) is a note.
//!
//! No `.github/dependabot.yml` is a clean no-op.
//!
//! # The shape it reads
//!
//! The file is a flat block-style list, and this reader understands only that
//! shape (failing loudly on zero parsed entries). It accepts both location
//! spellings GitHub documents, `directory: "/x"` and a `directories:` list
//! (block items or an inline `["/a", "/b"]` flow list), and `labels:` as a
//! block or flow list. A render of an organisation-wide dependency policy
//! writes `directories:` and inline `labels: [...]`.
//!
//! # Why this is Rust
//!
//! The script is `contract`-category shell, which epic #7810's `shell-budget`
//! gate ratchets down, never up; new logic goes into `loom-daemon`, the same
//! move [`crate::renovate_labels`] made for #9418.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// The label that routes a dependency PR into the Judge's review queue.
pub const REQUIRED_LABEL: &str = "loom:review-requested";

/// Repo-relative location of the config.
pub const CONFIG: &str = ".github/dependabot.yml";

const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "npm-shrinkwrap.json",
];

/// One `(entry, directory)` row of the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub ecosystem: String,
    /// Empty when the entry names no directory (a violation).
    pub directory: String,
    pub labels: Vec<String>,
}

/// Outcome of a run.
#[derive(Debug, Default)]
pub struct Report {
    /// False when there is no config: nothing was checked.
    pub config_present: bool,
    pub errors: Vec<String>,
    pub notes: Vec<String>,
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(s);
    let s = s
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .unwrap_or(s);
    s.to_string()
}

fn flow_list(s: &str) -> Vec<String> {
    let s = s.trim().trim_start_matches('[').trim_end_matches(']');
    s.split(',')
        .map(unquote)
        .filter(|i| !i.is_empty())
        .collect()
}

fn value(line: &str) -> &str {
    line.split_once(':').map(|(_, v)| v).unwrap_or("")
}

/// Parse the config into one row per `(entry, directory)`; an entry with no
/// directory yields one row with an empty `directory`.
pub fn parse_entries(text: &str) -> Vec<Entry> {
    #[derive(PartialEq)]
    enum List {
        None,
        Dirs,
        Labels,
    }
    struct Cur {
        eco: String,
        dirs: Vec<String>,
        labels: Vec<String>,
    }
    fn emit(cur: Option<Cur>, out: &mut Vec<Entry>) {
        let Some(c) = cur else { return };
        let dirs = if c.dirs.is_empty() {
            vec![String::new()]
        } else {
            c.dirs
        };
        for d in dirs {
            out.push(Entry {
                ecosystem: c.eco.clone(),
                directory: d,
                labels: c.labels.clone(),
            });
        }
    }

    let mut out = Vec::new();
    let mut cur: Option<Cur> = None;
    let mut list = List::None;
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with("  - package-ecosystem:") {
            emit(cur.take(), &mut out);
            cur = Some(Cur {
                eco: unquote(value(line)),
                dirs: vec![],
                labels: vec![],
            });
            list = List::None;
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        if line.starts_with("    directory:") {
            c.dirs = vec![unquote(value(line))];
            list = List::None;
        } else if line.starts_with("    directories:") || line.starts_with("    labels:") {
            let is_dirs = line.starts_with("    directories:");
            let v = value(line);
            let items = if v.contains('[') {
                flow_list(v)
            } else {
                vec![]
            };
            list = if v.contains('[') {
                List::None
            } else if is_dirs {
                List::Dirs
            } else {
                List::Labels
            };
            if is_dirs {
                c.dirs = items
            } else {
                c.labels = items
            }
        } else if let Some(item) = line.strip_prefix("      - ") {
            match list {
                List::Dirs => c.dirs.push(unquote(item)),
                List::Labels => c.labels.push(unquote(item)),
                List::None => {}
            }
        } else if line.starts_with("    ")
            && line[4..].starts_with(|ch: char| ch.is_ascii_alphabetic())
        {
            list = List::None;
        }
    }
    emit(cur, &mut out);
    out
}

/// `/` for the repo root, `/pkg` for `pkg/package.json`.
fn manifest_dir(manifest: &str) -> String {
    match Path::new(manifest).parent().and_then(|p| p.to_str()) {
        None | Some("") | Some(".") => "/".to_string(),
        Some(d) => format!("/{d}"),
    }
}

/// Number of declared dependencies of every kind.
fn declared_deps(v: &Value) -> usize {
    [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ]
    .iter()
    .filter_map(|k| v.get(*k).and_then(Value::as_object))
    .map(|o| o.len())
    .sum()
}

/// Tracked `package.json` files, as `git ls-files` lists them.
pub fn tracked_manifests(root: &Path) -> Vec<String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--", "package.json", "*/package.json"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Check the repo at `root`, enumerating manifests with git.
pub fn check(root: &Path) -> Report {
    check_with_manifests(root, &tracked_manifests(root))
}

/// Check the repo at `root` against an explicit manifest list.
pub fn check_with_manifests(root: &Path, manifests: &[String]) -> Report {
    let mut r = Report::default();
    let Ok(text) = std::fs::read_to_string(root.join(CONFIG)) else {
        return r;
    };
    r.config_present = true;

    let entries = parse_entries(&text);
    if entries.is_empty() {
        r.errors.push(format!(
            "ERROR: parsed zero entries from {CONFIG} — the file shape changed, update this check."
        ));
        return r;
    }

    // Check 1: every (entry, directory) carries the routing label.
    for e in &entries {
        if e.directory.is_empty() {
            r.errors.push(format!(
                "ERROR: {CONFIG} entry '{}' has no 'directory:' / 'directories:' (or it could not be parsed).",
                e.ecosystem
            ));
        } else if !e.labels.iter().any(|l| l == REQUIRED_LABEL) {
            r.errors.push(format!(
                "ERROR: {CONFIG} entry '{}' (directory '{}') does not list '{REQUIRED_LABEL}' in labels:.\n       \
                 Without it, Dependabot PRs for that directory land in the Judge's unlabeled fallback queue (#5455/#7577).",
                e.ecosystem, e.directory
            ));
        }
    }

    let npm_dirs: Vec<&str> = entries
        .iter()
        .filter(|e| e.ecosystem == "npm" && !e.directory.is_empty())
        .map(|e| e.directory.as_str())
        .collect();

    // Checks 2 and 4.
    for manifest in manifests.iter().filter(|m| !m.is_empty()) {
        let want = manifest_dir(manifest);
        if npm_dirs.contains(&want.as_str()) {
            continue;
        }
        let dir = root
            .join(manifest)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
        let has_lockfile = LOCKFILES.iter().any(|l| dir.join(l).is_file());
        let parsed: Option<Value> = std::fs::read_to_string(root.join(manifest))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());

        if has_lockfile {
            let is_workspace_root = dir.join("pnpm-workspace.yaml").is_file()
                || parsed
                    .as_ref()
                    .is_some_and(|v| v.get("workspaces").is_some());
            if !is_workspace_root && parsed.as_ref().is_some_and(|v| declared_deps(v) == 0) {
                r.notes.push(format!(
                    "note: {manifest} has a lockfile but declares no dependencies and is not a workspace root — no Dependabot entry needed."
                ));
                continue;
            }
            r.errors.push(format!(
                "ERROR: {manifest} has a committed lockfile but no npm entry for directory \"{want}\" in {CONFIG}.\n       \
                 Add one (with the same labels:) or Dependabot will open unlabeled, inert PRs for it (#7577)."
            ));
            continue;
        }
        if parsed.as_ref().is_some_and(|v| declared_deps(v) > 0) {
            r.notes.push(format!(
                "note: {manifest} declares dependencies and has no dependabot entry (no lockfile — treated as a scaffold, not an error)."
            ));
        }
    }

    // Check 3: no npm entry points at a directory with no manifest.
    for dir in &npm_dirs {
        let candidate = root.join(dir.trim_start_matches('/')).join("package.json");
        if !candidate.is_file() {
            r.errors.push(format!(
                "ERROR: {CONFIG} has an npm entry for directory \"{dir}\" but no package.json exists there."
            ));
        }
    }
    r
}

#[cfg(test)]
mod tests;
