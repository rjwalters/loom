//! Content scan for credential-shaped values (#9133).
//!
//! Every other credential defence Loom has is PATH-based: the managed
//! `.gitignore` block, [`crate::init::post_init`]'s `CREDENTIAL_PATTERNS`, and
//! their shell mirrors in `land-resync-commit.sh` / `resync-installed.sh`.
//! They protect the paths somebody predicted. A renamed copy of the token pool
//! (`.loom/tokens.<suffix>/`) sits outside all of them at once, and a `git add`
//! of a whole directory, by hand or by automation, invokes none of the script
//! guards. This module is
//! the layer that does not care what the path is called: it reads the CONTENT
//! about to be committed or pushed.
//!
//! **It never emits a matched value.** A [`Finding`] carries path, line,
//! class and fingerprint — `sha256(value)[:8]`, the fingerprint the token-pool
//! tooling already uses — so a report is safe to paste into a public issue.
//!
//! Only ADDED lines are scanned, so an existing synthetic fixture is not
//! re-reported every time its file is touched. Commits are scanned one at a
//! time rather than as a net diff: a value added in one commit and deleted in
//! the next is gone from the tree and still public in history.
//!
//! Allowlist: `.loom/secret-scan-allow`, one fingerprint per line. Fingerprint
//! rather than path or inline marker on purpose: allowing a value takes a
//! reviewed line naming exactly that value, and a path allowlist is the
//! failure this module exists to cover.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use regex::Regex;
use sha2::{Digest, Sha256};

/// The credential shapes, `(class, regex)`. Each is tuned to the REAL length
/// of the credential so the short, obviously synthetic fixtures in test code do
/// not fire: a real Claude OAuth token is `sk-ant-oat01-` plus 95 characters,
/// where the fixtures in this repo are 20-50.
pub const SECRET_CLASSES: &[(&str, &str)] = &[
    ("anthropic-oauth", r"sk-ant-o[ar]t01-[A-Za-z0-9_-]{80,}"),
    ("anthropic-api-key", r"sk-ant-(?:api03|admin01)-[A-Za-z0-9_-]{80,}"),
    ("github-token", r"gh[pousr]_[A-Za-z0-9]{36,}"),
    ("github-pat", r"github_pat_[A-Za-z0-9_]{60,}"),
    (
        "tailscale-key",
        r"tskey-(?:auth|api|client|scim|webhook)-[A-Za-z0-9]+-[A-Za-z0-9]{20,}",
    ),
    ("aws-access-key-id", r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b"),
    ("slack-token", r"xox[abposr]-[A-Za-z0-9-]{20,}"),
    // The header alone is ordinary code (PEM parsers, test stubs); a key needs
    // its base64 body. This is the one-line form (`\n`-escaped, as in JSON or
    // an env file); a header on a line of its own is handled in `scan_lines`.
    (
        "private-key",
        r"-----BEGIN [A-Z ]{0,20}PRIVATE KEY-----(?:\\n|\s)+[A-Za-z0-9+/=]{40,}",
    ),
    ("zai-api-key", r"\b[0-9a-f]{32}\.[A-Za-z0-9]{16}\b"),
];

/// Repo-relative location of the fingerprint allowlist.
pub const ALLOW_FILE: &str = ".loom/secret-scan-allow";

/// Files larger than this are skipped when scanned whole (untracked files).
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

/// `sha256(value.trim())[:8]`, lowercase hex.
pub fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.trim().as_bytes());
    hex::encode(digest)[..8].to_string()
}

/// One credential-shaped value. Deliberately has no field for the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: String,
    pub line: usize,
    /// Abbreviated commit, when the finding came from history.
    pub commit: Option<String>,
    pub class: &'static str,
    pub fingerprint: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.path, self.line)?;
        if let Some(c) = &self.commit {
            write!(f, " @{c}")?;
        }
        write!(f, ": {} (fp {})", self.class, self.fingerprint)
    }
}

pub struct Scanner {
    classes: Vec<(&'static str, Regex)>,
    allowed: HashSet<String>,
    /// A PEM private-key header alone on its line, and the base64 body line
    /// that must follow it for the pair to be a key.
    pem_header: Regex,
    pem_body: Regex,
}

impl Scanner {
    pub fn new(allowed: HashSet<String>) -> Self {
        let classes = SECRET_CLASSES
            .iter()
            .map(|(name, re)| (*name, Regex::new(re).expect("SECRET_CLASSES regex compiles")))
            .collect();
        Self {
            classes,
            allowed,
            pem_header: Regex::new(r"^\s*-----BEGIN [A-Z ]{0,20}PRIVATE KEY-----\s*$")
                .expect("regex"),
            pem_body: Regex::new(r"^\s*[A-Za-z0-9+/=]{40,}\s*$").expect("regex"),
        }
    }

    /// The multi-line private-key form. `pending` is true when the previous
    /// line was a bare header; this line then decides. The finding points at
    /// the header and fingerprints the first body line.
    fn scan_pem_line(
        &self,
        pending: &mut bool,
        path: &str,
        line: usize,
        commit: Option<&str>,
        text: &str,
        out: &mut Vec<Finding>,
    ) {
        if *pending && self.pem_body.is_match(text) {
            let fp = fingerprint(text);
            if !self.allowed.contains(&fp) {
                out.push(Finding {
                    path: path.to_string(),
                    line: line.saturating_sub(1),
                    commit: commit.map(str::to_string),
                    class: "private-key",
                    fingerprint: fp,
                });
            }
        }
        *pending = self.pem_header.is_match(text);
    }

    /// Reads the allowlist at `<repo>/.loom/secret-scan-allow` (or `file`
    /// when given). A missing file is an empty allowlist.
    pub fn for_repo(repo: &Path, file: Option<&Path>) -> Self {
        let path = file.map_or_else(|| repo.join(ALLOW_FILE), Path::to_path_buf);
        let text = std::fs::read_to_string(path).unwrap_or_default();
        Self::new(parse_allowlist(&text))
    }

    pub fn scan_line(
        &self,
        path: &str,
        line: usize,
        commit: Option<&str>,
        text: &str,
        out: &mut Vec<Finding>,
    ) {
        for (class, re) in &self.classes {
            for m in re.find_iter(text) {
                let fp = fingerprint(m.as_str());
                if self.allowed.contains(&fp) {
                    continue;
                }
                out.push(Finding {
                    path: path.to_string(),
                    line,
                    commit: commit.map(str::to_string),
                    class,
                    fingerprint: fp,
                });
            }
        }
    }

    pub fn scan_text(&self, path: &str, text: &str, out: &mut Vec<Finding>) {
        let mut pem = false;
        for (i, line) in text.lines().enumerate() {
            self.scan_line(path, i + 1, None, line, out);
            self.scan_pem_line(&mut pem, path, i + 1, None, line, out);
        }
    }

    /// Scans the ADDED lines of a `-U0` unified diff. `commit <sha>` lines (as
    /// `git log --format='commit %H'` emits them) set the commit column.
    pub fn scan_diff(&self, diff: &str, out: &mut Vec<Finding>) {
        let mut commit: Option<String> = None;
        let mut path = String::new();
        let mut line = 0usize;
        // Lines still owed by the current hunk. Counting them (rather than
        // keying on a leading `+++`) is what keeps an added line whose content
        // starts with `++ ` from being mistaken for a file header.
        let (mut old_left, mut new_left) = (0usize, 0usize);
        let mut pem = false;
        for raw in diff.lines() {
            if old_left > 0 || new_left > 0 {
                if let Some(text) = raw.strip_prefix('+') {
                    self.scan_line(&path, line, commit.as_deref(), text, out);
                    self.scan_pem_line(&mut pem, &path, line, commit.as_deref(), text, out);
                    line += 1;
                    new_left = new_left.saturating_sub(1);
                } else if raw.starts_with('-') {
                    pem = false;
                    old_left = old_left.saturating_sub(1);
                } else if !raw.starts_with('\\') {
                    pem = false;
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    line += 1;
                }
                continue;
            }
            pem = false;
            if let Some(sha) = raw.strip_prefix("commit ") {
                commit = Some(sha.chars().take(12).collect());
            } else if let Some(p) = raw.strip_prefix("+++ ") {
                path = p.strip_prefix("b/").unwrap_or(p).to_string();
            } else if let Some(hunk) = raw.strip_prefix("@@ ") {
                if let Some((old, new, start)) = parse_hunk(hunk) {
                    (old_left, new_left, line) = (old, new, start);
                }
            }
        }
    }
}

/// `-a,b +c,d @@ …` -> (old count, new count, new start). A missing count is 1.
fn parse_hunk(hunk: &str) -> Option<(usize, usize, usize)> {
    let mut parts = hunk.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let count = |s: &str| -> Option<(usize, usize)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (_, old_n) = count(old)?;
    let (new_start, new_n) = count(new)?;
    Some((old_n, new_n, new_start))
}

pub fn parse_allowlist(text: &str) -> HashSet<String> {
    text.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|fp| fp.len() == 8 && fp.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// What to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Index vs `HEAD` (a git pre-commit hook).
    Staged,
    /// Staged + unstaged + untracked-unignored: everything a
    /// `git add -A && git commit` could sweep in. Used by the PreToolUse
    /// guard, which runs BEFORE the command and so before its `git add`.
    Pending,
    /// Every commit in `git log <revs>`.
    Range(Vec<String>),
    /// Commits reachable from `HEAD` but from no remote-tracking ref.
    Unpushed,
    /// git's pre-push stdin: `<local ref> <local sha> <remote ref> <remote sha>`.
    PrePush(String),
}

/// The modes a shell command needs, read from its text: `git … commit` ->
/// [`Mode::Pending`], `git … push` -> [`Mode::Unpushed`]. Empty when the
/// command does neither. The caller passes a text with quoted data already
/// masked, so a commit MESSAGE that says "push" does not count.
pub fn modes_for_command(command: &str) -> Vec<Mode> {
    let mut modes = Vec::new();
    for segment in command.split(['&', '|', ';', '\n', '(', ')']) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        let Some(git) = words
            .iter()
            .position(|w| *w == "git" || w.ends_with("/git"))
        else {
            continue;
        };
        // First non-option word after `git`, skipping `-C <dir>` / `-c <k=v>`.
        let mut i = git + 1;
        while i < words.len() && words[i].starts_with('-') {
            i += if matches!(words[i], "-C" | "-c") {
                2
            } else {
                1
            };
        }
        match words.get(i) {
            Some(&"commit") if !modes.contains(&Mode::Pending) => modes.push(Mode::Pending),
            Some(&"push") if !modes.contains(&Mode::Unpushed) => modes.push(Mode::Unpushed),
            _ => {}
        }
    }
    modes
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("could not run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

const DIFF_FLAGS: &[&str] = &[
    "-U0",
    "--no-color",
    "--no-ext-diff",
    "--no-renames",
    "--no-textconv",
];

fn scan_git_diff(
    scanner: &Scanner,
    repo: &Path,
    args: &[&str],
    out: &mut Vec<Finding>,
) -> Result<()> {
    let text = git(repo, args)?;
    scanner.scan_diff(&String::from_utf8_lossy(&text), out);
    Ok(())
}

fn scan_log(scanner: &Scanner, repo: &Path, revs: &[&str], out: &mut Vec<Finding>) -> Result<()> {
    let mut args = vec!["log", "-p", "--no-merges", "--format=commit %H"];
    args.extend_from_slice(DIFF_FLAGS);
    args.extend_from_slice(revs);
    scan_git_diff(scanner, repo, &args, out)
}

fn scan_untracked(scanner: &Scanner, repo: &Path, out: &mut Vec<Finding>) -> Result<()> {
    let list = git(repo, &["ls-files", "-z", "--others", "--exclude-standard"])?;
    for rel in list.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let rel = String::from_utf8_lossy(rel);
        let full = repo.join(rel.as_ref());
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        if bytes.contains(&0) {
            continue; // binary
        }
        scanner.scan_text(&rel, &String::from_utf8_lossy(&bytes), out);
    }
    Ok(())
}

/// Runs one mode against the repo at `repo` (its top level).
pub fn scan(scanner: &Scanner, repo: &Path, mode: &Mode) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    match mode {
        Mode::Staged | Mode::Pending => {
            let has_head = git(repo, &["rev-parse", "-q", "--verify", "HEAD"]).is_ok();
            let empty_tree;
            let base = if has_head {
                "HEAD"
            } else {
                empty_tree = String::from_utf8_lossy(&git(
                    repo,
                    &["hash-object", "-t", "tree", "/dev/null"],
                )?)
                .trim()
                .to_string();
                empty_tree.as_str()
            };
            let mut args = vec!["diff", "--cached"];
            args.extend_from_slice(DIFF_FLAGS);
            args.push(base);
            scan_git_diff(scanner, repo, &args, &mut out)?;
            if *mode == Mode::Pending {
                let mut args = vec!["diff"];
                args.extend_from_slice(DIFF_FLAGS);
                scan_git_diff(scanner, repo, &args, &mut out)?;
                scan_untracked(scanner, repo, &mut out)?;
            }
        }
        Mode::Range(revs) => {
            let revs: Vec<&str> = revs.iter().map(String::as_str).collect();
            scan_log(scanner, repo, &revs, &mut out)?;
        }
        Mode::Unpushed => scan_log(scanner, repo, &["HEAD", "--not", "--remotes"], &mut out)?,
        Mode::PrePush(stdin) => {
            for line in stdin.lines() {
                let f: Vec<&str> = line.split_whitespace().collect();
                let [_, local, _, remote] = f[..] else {
                    continue;
                };
                if local == ZERO_SHA {
                    continue; // branch deletion
                }
                if remote == ZERO_SHA {
                    scan_log(scanner, repo, &[local, "--not", "--remotes"], &mut out)?;
                } else {
                    scan_log(scanner, repo, &[&format!("{remote}..{local}")], &mut out)?;
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
