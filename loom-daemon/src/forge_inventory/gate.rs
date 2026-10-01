//! The change gate for **unclassified direct forge calls** (Issue #9777).
//!
//! A manifest that nobody is obliged to update decays the day it lands. So the
//! gate measures the other direction: every file in the tree that makes a
//! direct forge call must either be a **declared caller** of some inventoried
//! operation, or be listed in the bypass baseline with an owner and a removal
//! issue. A file that is neither is an unclassified call site, and a baselined
//! file whose call count GREW is a new bypass hiding inside an old one.
//!
//! It is a one-way ratchet, like `scripts/check-file-size-budget.sh`: shrinking
//! is always fine, growth is a failure, and `--update` rewrites the baseline so
//! the ledger only ever gets shorter.
//!
//! # What is a "direct forge call"
//!
//! A literal or dynamically-assembled `gh <noun> <verb>` / `gh api` invocation
//! in an executable file. Deliberately NOT counted:
//!
//! - **Installed mirrors** (`.loom/**`) — resync copies of `defaults/`,
//!   measured at their source, exactly as the file-size budget does.
//! - **Tests and fixtures** — `tests/**`, `*_tests.rs`, `tests.rs`,
//!   `test-*.sh`. #9777 classifies fixtures rather than counting them as
//!   active operations.
//! - **Prohibitions** — `defaults/hooks/guard-*.sh` match forge command text in
//!   order to *deny* it. A guard that recognises `gh pr merge` is the opposite
//!   of a caller, and counting it would make the gate fight its own safety net.
//! - **Role prompts** (`*.md`) — instructions to an LLM, not call sites. They
//!   are inventoried in each operation's `callers` as `kind = "role-prompt"`,
//!   which is how a prompt-only operation still gets an owner and a test.
//!
//! Scanning is lexical on purpose. A gate that needed to resolve a shell
//! variable to decide whether a line calls the forge would be unable to answer
//! for exactly the dynamic construction #9777 asks to be counted; an
//! over-counting lexical scan costs one baseline entry, an under-counting
//! clever one costs an invisible bypass.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// `gh` subcommand nouns whose invocation is a forge API call.
const FORGE_NOUNS: &[&str] = &[
    "api", "issue", "pr", "label", "repo", "run", "release", "auth", "search", "secret",
    "variable", "workflow", "cache", "browse", "ruleset",
];

/// One baselined bypass: a file that makes direct forge calls today and has not
/// been migrated behind an inventoried operation yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bypass {
    /// Repo-relative path.
    pub path: String,
    /// Direct call sites counted at baseline time. The ratchet's number.
    pub calls: usize,
    /// Owner area from `manifest.toml`'s `[owners]` table.
    pub owner: String,
    /// The issue that retires this entry.
    pub removal_issue: u64,
}

/// `defaults/forge/call-bypass-baseline.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    /// The issue that owns elimination of the whole ledger.
    #[serde(default)]
    pub removal_epic: Option<u64>,
    #[serde(default, rename = "bypass")]
    pub bypasses: Vec<Bypass>,
}

impl Baseline {
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&Bypass> {
        self.bypasses.iter().find(|b| b.path == path)
    }
}

/// How a scanned file was disposed of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Declared as a caller of at least one inventoried operation.
    Classified,
    /// Baselined, and its call count did not grow.
    Baselined,
    /// Baselined, but it grew — a new bypass inside an old one.
    BaselineGrew { recorded: usize },
    /// Neither classified nor baselined: a new unclassified call site.
    Unclassified,
}

/// One scanned file's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileResult {
    pub path: String,
    pub calls: usize,
    pub verdict: Verdict,
    /// First few `line:text` samples, for the failure message.
    pub samples: Vec<String>,
}

impl FileResult {
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(self.verdict, Verdict::Unclassified | Verdict::BaselineGrew { .. })
    }
}

/// Is this path outside the gate's reach (mirror, test, prohibition, doc)?
#[must_use]
pub fn is_excluded(path: &str) -> bool {
    // Installed mirrors, wherever they sit. `.loom/` at the repo root is the
    // common case, but the quickstart template repos carry their own nested
    // `quickstarts/<kind>/.loom/` trees — mirrors just the same, measured at
    // their `defaults/` source. Matching only the root prefix listed three
    // copies of `worktree.sh` in the first generated baseline.
    if path.starts_with(".loom/") || path.contains("/.loom/") || path.starts_with("target/") {
        return true;
    }
    if path.starts_with("defaults/hooks/guard-") || path.starts_with("tests/") {
        return true;
    }
    if path.contains("/tests/") || path.ends_with("_tests.rs") || path.ends_with("/tests.rs") {
        return true;
    }
    if let Some(name) = path.rsplit('/').next() {
        if name.starts_with("test-") && name.ends_with(".sh") {
            return true;
        }
    }
    false
}

/// Does the gate scan this file type at all? `.sh`/`.rs` everywhere, `.yml`
/// only under a `workflows/` directory (a label catalogue mentioning `gh api`
/// in a description is prose, not a call site).
#[must_use]
pub fn is_scannable(path: &str) -> bool {
    if path.ends_with(".sh") || path.ends_with(".rs") {
        return true;
    }
    (path.ends_with(".yml") || path.ends_with(".yaml")) && path.contains("workflows/")
}

/// Is `b` a byte that can continue an identifier/path token? Used to reject
/// `--gh api`, `loom-gh api`, `tough api`, `$gh api`-style false positives
/// where the `gh` is part of a longer word.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-')
}

/// Count direct forge call sites in `text`, returning `(count, samples)` with
/// up to `max_samples` `line:text` strings.
#[must_use]
pub fn scan_text(text: &str, max_samples: usize) -> (usize, Vec<String>) {
    let mut count = 0usize;
    let mut samples = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let hits = count_line(line);
        if hits == 0 {
            continue;
        }
        count += hits;
        if samples.len() < max_samples {
            samples.push(format!("{}:{}", idx + 1, line.trim()));
        }
    }
    (count, samples)
}

/// How many `gh <forge-noun>` invocations one line contains.
fn count_line(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut hits = 0usize;
    let mut i = 0usize;
    while let Some(rel) = line[i..].find("gh ") {
        let start = i + rel;
        let after = start + 3;
        // `gh` must start a token: nothing identifier-ish immediately before.
        let preceded_ok = start == 0 || !is_token_byte(bytes[start - 1]);
        if preceded_ok {
            let rest = line[after..].trim_start();
            let noun = rest
                .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .next()
                .unwrap_or_default();
            if FORGE_NOUNS.contains(&noun) {
                hits += 1;
            }
        }
        i = after;
    }
    hits
}

/// Scan `paths` (repo-relative) under `root` and classify each against the
/// manifest's declared callers and the baseline.
#[must_use]
pub fn evaluate(
    root: &Path,
    paths: &[String],
    declared: &BTreeSet<String>,
    baseline: &Baseline,
) -> Vec<FileResult> {
    let mut out = Vec::new();
    for path in paths {
        if !is_scannable(path) || is_excluded(path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(path)) else {
            continue;
        };
        let (calls, samples) = scan_text(&text, 3);
        if calls == 0 {
            continue;
        }
        let verdict = if declared.contains(path) {
            Verdict::Classified
        } else {
            match baseline.get(path) {
                Some(b) if calls > b.calls => Verdict::BaselineGrew { recorded: b.calls },
                Some(_) => Verdict::Baselined,
                None => Verdict::Unclassified,
            }
        };
        out.push(FileResult {
            path: path.clone(),
            calls,
            verdict,
            samples,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Baseline entries whose file no longer makes any direct forge call — the
/// ledger shrinking, which `--update` should drop.
#[must_use]
pub fn resolved_entries(results: &[FileResult], baseline: &Baseline) -> Vec<String> {
    let still: BTreeSet<&str> = results.iter().map(|r| r.path.as_str()).collect();
    baseline
        .bypasses
        .iter()
        .filter(|b| !still.contains(b.path.as_str()))
        .map(|b| b.path.clone())
        .collect()
}

/// Structural problems in the baseline itself: a bypass with no owner, no
/// removal issue, or duplicated. #9777 requires each baselined bypass to carry
/// an explicit owner and removal issue so the ledger cannot become anonymous.
#[must_use]
pub fn validate_baseline(
    baseline: &Baseline,
    known_owners: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for b in &baseline.bypasses {
        if !seen.insert(b.path.as_str()) {
            problems.push(format!("{}: listed more than once", b.path));
        }
        if b.owner.trim().is_empty() {
            problems.push(format!("{}: no owner recorded", b.path));
        } else if !known_owners.is_empty() && !known_owners.contains_key(&b.owner) {
            problems.push(format!(
                "{}: owner `{}` is not declared in manifest.toml's [owners] table",
                b.path, b.owner
            ));
        }
        if b.removal_issue == 0 {
            problems.push(format!("{}: no removal_issue recorded", b.path));
        }
    }
    problems
}

/// Render a baseline back to the TOML the repo stores, sorted by path so a
/// regeneration never produces a reordering diff.
#[must_use]
pub fn render_baseline(baseline: &Baseline) -> String {
    let mut sorted = baseline.bypasses.clone();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let total: usize = sorted.iter().map(|b| b.calls).sum();
    let mut out = String::new();
    out.push_str(
        "# call-bypass-baseline.toml — generated by `loom-daemon forge-inventory gate --update`\n#\n\
         # Every file that still makes a DIRECT forge call without being a declared\n\
         # caller of an inventoried operation (defaults/forge/operations/*.toml).\n\
         # This is a debt ledger, not a target: `calls` may only go DOWN and entries\n\
         # may only be removed. The gate fails when a listed file grows, or when an\n\
         # unlisted file makes a direct call at all.\n#\n\
         # Eliminating these entries is the caller-migration issue's job, not this\n\
         # ledger's — see `removal_epic` below and each entry's `removal_issue`.\n#\n",
    );
    out.push_str(&format!(
        "# {} file(s), {total} direct call site(s) at baseline.\n\n",
        sorted.len()
    ));
    if let Some(epic) = baseline.removal_epic {
        out.push_str(&format!("removal_epic = {epic}\n\n"));
    }
    for b in &sorted {
        out.push_str("[[bypass]]\n");
        out.push_str(&format!("path = {}\n", toml_string(&b.path)));
        out.push_str(&format!("calls = {}\n", b.calls));
        out.push_str(&format!("owner = {}\n", toml_string(&b.owner)));
        out.push_str(&format!("removal_issue = {}\n\n", b.removal_issue));
    }
    out
}

/// Minimal TOML basic-string quoting (paths and owner names here are ASCII
/// without control characters, but never hand-splice a quote).
fn toml_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Map a repo-relative path to the owner area that answers for it. Deterministic
/// and reviewable: a baseline entry's owner is derived from the path, never from
/// whoever happened to run `--update`.
#[must_use]
pub fn owner_for_path(path: &str) -> &'static str {
    const PREFIXES: &[(&str, &str)] = &[
        ("loom-daemon/src/ci_telemetry", "daemon-ci-telemetry"),
        ("loom-daemon/src/fleet", "daemon-fleet"),
        ("loom-daemon/src/release", "daemon-delivery"),
        ("loom-daemon/src/auto_update", "daemon-delivery"),
        ("loom-daemon/src/daemon_update", "daemon-delivery"),
        ("loom-daemon/src/merge_pr", "daemon-landing"),
        ("loom-daemon/src/sweep", "daemon-sweep"),
        ("loom-daemon/src/claim_reconciliation", "daemon-sweep"),
        ("loom-daemon/src/quarantine", "daemon-sweep"),
        ("loom-daemon/src/work_finder", "daemon-sweep"),
        ("loom-daemon/src/forge", "daemon-forge"),
        ("loom-daemon/src/rate_limit", "daemon-forge"),
        ("loom-daemon/src/credential", "daemon-forge"),
        ("loom-daemon/src/comment_trust", "daemon-forge"),
        ("loom-daemon/src/cli", "daemon-cli"),
        ("loom-daemon", "daemon-other"),
        ("mcp-loom", "mcp-server"),
        ("defaults/scripts", "role-scripts"),
        ("defaults/optional", "role-scripts"),
        ("defaults/.claude", "role-prompts"),
        ("defaults/roles", "role-prompts"),
        ("scripts/install", "installer"),
        ("scripts", "repo-tooling"),
        (".github", "ci-workflows"),
        ("dashboard", "dashboard"),
        ("docker", "delivery-images"),
        ("quickstarts", "installer"),
        // The repo-root install/uninstall entry points.
        ("install.sh", "installer"),
        ("uninstall.sh", "installer"),
    ];
    for (prefix, owner) in PREFIXES {
        if path.starts_with(prefix) {
            return owner;
        }
    }
    "unassigned"
}
